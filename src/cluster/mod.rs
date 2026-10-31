//! Raft between orch nodes. Only coordination state goes through the log, the design is in
//! `docs/specs/2026-10-26-multi-machine-cluster-design.md`.

mod command;
pub mod config;
pub mod discovery;
pub mod local;
pub mod network;
pub mod progress;
mod storage;

use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use openraft::error::{ClientWriteError, InstallSnapshotError, RPCError, RaftError, Unreachable};
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use openraft::storage::Adaptor;
use openraft::{BasicNode, SnapshotPolicy};

use crate::store::TaskStore;

pub use command::{Command, Reply, Request};
pub use storage::RaftStore;

pub type NodeId = u64;

openraft::declare_raft_types!(
    pub TypeConfig:
        D = Request,
        R = Reply,
);

pub type Raft = openraft::Raft<TypeConfig>;

/// A `cluster:` section turns the cluster on. Without one, orch writes straight to SQLite as it always has
pub fn enabled() -> bool {
    crate::config::get("cluster.secret").is_ok_and(|s| !s.is_empty())
}

/// Start this machine's node from the `cluster:` section and the tailnet. A learner keeps asking
/// the cluster to add it until a member proves the secret and takes it
pub async fn start(store: &TaskStore, raft_db: &Path) -> anyhow::Result<Node> {
    let cfg = config::ClusterConfig::load()?
        .ok_or_else(|| anyhow::anyhow!("no cluster.secret in the config"))?;
    let tailnet = discovery::status().await?;
    let plan = config::plan(&cfg, &tailnet)?;
    tracing::info!(
        name = %plan.name,
        addr = %plan.addr,
        elect = plan.elect,
        learner = plan.learner,
        "starting cluster node"
    );
    let node = start_node(
        store,
        raft_db,
        NodeConfig {
            name: plan.name.clone(),
            id: plan.id,
            addr: plan.addr.clone(),
            secret: cfg.secret.clone(),
            voters: plan.voters.clone(),
            elect: plan.elect,
        },
    )
    .await?;
    if plan.learner {
        tokio::spawn(join_loop(node.clone(), plan, cfg));
    }

    Ok(node)
}

async fn join_loop(node: Node, plan: config::Plan, cfg: config::ClusterConfig) {
    loop {
        let member = node
            .raft
            .metrics()
            .borrow()
            .membership_config
            .membership()
            .nodes()
            .any(|(id, _)| *id == plan.id);
        if member {
            return;
        }

        if let Ok(tailnet) = discovery::status().await {
            for peer in tailnet.peers.iter().filter(|p| p.online) {
                let addr = format!("{}:{}", peer.ip, cfg.port);
                if network::probe(&addr, &cfg.secret, Some(&peer.name))
                    .await
                    .is_none()
                {
                    continue;
                }

                let join = network::Join {
                    id: plan.id,
                    addr: plan.addr.clone(),
                };
                match network::send_join(&addr, &cfg.secret, &join).await {
                    Ok(()) => tracing::info!(via = %peer.name, "joined the cluster as a learner"),
                    Err(e) => tracing::warn!(via = %peer.name, err = %e, "join refused"),
                }

                break;
            }
        }

        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

/// One member as the leader sees it, for `orch nodes`
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Member {
    pub id: NodeId,
    pub addr: String,
    pub voter: bool,
    pub leader: bool,
    pub me: bool,
}

pub fn members(node: &Node) -> Vec<Member> {
    let m = node.raft.metrics().borrow().clone();
    let membership = m.membership_config.membership();
    let voters: std::collections::BTreeSet<NodeId> = membership.voter_ids().collect();
    membership
        .nodes()
        .map(|(id, n)| Member {
            id: *id,
            addr: n.addr.clone(),
            voter: voters.contains(id),
            leader: m.current_leader == Some(*id),
            me: *id == m.id,
        })
        .collect()
}

/// Id of the node when orch runs alone
pub const SINGLE_NODE_ID: NodeId = 1;

/// The proposer's clock, in the format the store writes timestamps in
pub fn now_stamp() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn config() -> anyhow::Result<openraft::Config> {
    Ok(openraft::Config {
        cluster_name: "orch".into(),
        heartbeat_interval: 250,
        election_timeout_min: 1000,
        election_timeout_max: 2000,
        snapshot_policy: SnapshotPolicy::LogsSinceLast(1000),
        ..Default::default()
    }
    .validate()?)
}

/// A running node. The store holds one and sends every replicated write through it
#[derive(Clone)]
pub struct Node {
    pub raft: Raft,
    /// None on a single node, which has no peers to forward to
    secret: Option<Arc<String>>,
    server: Option<Arc<tokio::sync::Notify>>,
}

impl Node {
    /// Propose on this node, or forward to the leader when this node follows. During an election
    /// the leader can be unknown or stale, so it waits and tries again, but only a write that was
    /// certainly not applied is sent again
    pub async fn write(&self, req: Request) -> anyhow::Result<Reply> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let err = match self.raft.client_write(req.clone()).await {
                Ok(r) => return Ok(r.data),
                Err(e) => e,
            };
            let RaftError::APIError(ClientWriteError::ForwardToLeader(f)) = &err else {
                return Err(err.into());
            };
            let Some(secret) = &self.secret else {
                return Err(err.into());
            };
            let reason = match &f.leader_node {
                Some(leader) => match network::forward(&leader.addr, secret, &req).await {
                    Ok(reply) => return Ok(reply),
                    Err(network::ForwardError::Unknown(e)) => return Err(e),
                    Err(network::ForwardError::NotApplied(why)) => why,
                },
                None => "no leader known".to_string(),
            };
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!("no leader took the write: {reason}");
            }

            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    pub fn is_leader(&self) -> bool {
        let m = self.raft.metrics();
        let m = m.borrow();
        m.current_leader == Some(m.id)
    }

    pub async fn stop(self) {
        if let Some(server) = self.server {
            server.notify_one();
        }

        let _ = self.raft.shutdown().await;
    }
}

/// How one node of a cluster starts: its id, the address it listens on, the shared secret, the
/// voters to initialize with, and whether it may lead (a witness may not)
pub struct NodeConfig {
    pub name: String,
    pub id: NodeId,
    pub addr: String,
    pub secret: String,
    pub voters: BTreeMap<NodeId, String>,
    pub elect: bool,
}

/// Start a one-node cluster over `store`, with its log in `raft_db`, and route the store's
/// replicated writes through it
pub async fn start_single(store: &TaskStore, raft_db: &Path) -> anyhow::Result<Node> {
    let raft_store = RaftStore::open(raft_db, store.clone()).await?;
    let (log, sm) = Adaptor::new(raft_store);
    let raft = Raft::new(SINGLE_NODE_ID, Arc::new(config()?), NoNetwork, log, sm).await?;
    let members = BTreeMap::from([(SINGLE_NODE_ID, BasicNode::default())]);
    // A restart finds the membership in the log, and initialize refuses a second time
    if let Err(e) = raft.initialize(members).await {
        tracing::debug!(err = %e, "raft already initialized");
    }

    raft.wait(Some(Duration::from_secs(10)))
        .current_leader(SINGLE_NODE_ID, "single node leads")
        .await?;
    let node = Node {
        raft,
        secret: None,
        server: None,
    };
    store.attach_node(node.clone());
    Ok(node)
}

/// Start one node of a cluster and serve its RPCs on `cfg.addr`. Every voter initializes with
/// the same membership, which openraft accepts, so no voter is special
pub async fn start_node(
    store: &TaskStore,
    raft_db: &Path,
    cfg: NodeConfig,
) -> anyhow::Result<Node> {
    let listener = tokio::net::TcpListener::bind(&cfg.addr).await?;
    start_node_on(store, raft_db, cfg, listener).await
}

/// Like `start_node`, with a listener the caller already bound. `cfg.addr` is the address peers use
pub async fn start_node_on(
    store: &TaskStore,
    raft_db: &Path,
    cfg: NodeConfig,
    listener: tokio::net::TcpListener,
) -> anyhow::Result<Node> {
    let secret = Arc::new(cfg.secret);
    let raft_store = RaftStore::open(raft_db, store.clone()).await?;
    let (log, sm) = Adaptor::new(raft_store);
    let config = openraft::Config {
        enable_elect: cfg.elect,
        ..config()?
    }
    .validate()?;
    let network = network::HttpNetwork {
        secret: std::sync::Arc::clone(&secret),
    };
    let raft = Raft::new(cfg.id, Arc::new(config), network, log, sm).await?;
    let me = network::Hello {
        name: cfg.name,
        id: cfg.id,
        witness: !cfg.elect,
        leader: None,
        term: 0,
        proof: String::new(),
    };
    let app = network::router(
        raft.clone(),
        std::sync::Arc::clone(&secret),
        me,
        cfg.addr.clone(),
    );
    let stop = Arc::new(tokio::sync::Notify::new());
    let stopped = std::sync::Arc::clone(&stop);
    tokio::spawn(async move {
        // Graceful shutdown also closes kept-alive connections, which would keep answering for a stopped node
        let served = axum::serve(listener, app)
            .with_graceful_shutdown(async move { stopped.notified().await })
            .await;
        if let Err(e) = served {
            tracing::warn!(err = %e, "cluster listener stopped");
        }
    });
    // initialize starts an election whatever enable_elect says, so a witness waits for the leader
    if cfg.elect && !cfg.voters.is_empty() {
        let members: BTreeMap<NodeId, BasicNode> = cfg
            .voters
            .into_iter()
            .map(|(id, addr)| (id, BasicNode::new(addr)))
            .collect();
        if let Err(e) = raft.initialize(members).await {
            tracing::debug!(err = %e, "raft already initialized");
        }
    }

    let node = Node {
        raft,
        secret: Some(secret),
        server: Some(stop),
    };
    store.attach_node(node.clone());
    Ok(node)
}

/// A single node never sends an RPC. The tailnet transport replaces this
struct NoNetwork;

impl RaftNetworkFactory<TypeConfig> for NoNetwork {
    type Network = NoNetwork;

    async fn new_client(&mut self, _target: NodeId, _node: &BasicNode) -> NoNetwork {
        NoNetwork
    }
}

fn unreachable<E: std::error::Error>() -> RPCError<NodeId, BasicNode, E> {
    RPCError::Unreachable(Unreachable::new(&std::io::Error::other(
        "no network on a single node",
    )))
}

impl RaftNetwork<TypeConfig> for NoNetwork {
    async fn append_entries(
        &mut self,
        _req: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<NodeId>, RPCError<NodeId, BasicNode, RaftError<NodeId>>> {
        Err(unreachable())
    }

    async fn install_snapshot(
        &mut self,
        _req: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<NodeId>,
        RPCError<NodeId, BasicNode, RaftError<NodeId, InstallSnapshotError>>,
    > {
        Err(unreachable())
    }

    async fn vote(
        &mut self,
        _req: VoteRequest<NodeId>,
        _option: RPCOption,
    ) -> Result<VoteResponse<NodeId>, RPCError<NodeId, BasicNode, RaftError<NodeId>>> {
        Err(unreachable())
    }
}

#[cfg(test)]
mod tests;
