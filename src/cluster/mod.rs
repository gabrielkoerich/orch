//! Raft between orch nodes. Only coordination state goes through the log, the design is in
//! `docs/specs/2026-10-26-multi-machine-cluster-design.md`.

mod command;
pub mod local;
mod network;
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
    /// no leader is known yet, so it waits for one and tries again
    pub async fn write(&self, req: Request) -> anyhow::Result<Reply> {
        for attempt in 0..2 {
            let err = match self.raft.client_write(req.clone()).await {
                Ok(r) => return Ok(r.data),
                Err(e) => e,
            };
            let RaftError::APIError(ClientWriteError::ForwardToLeader(f)) = &err else {
                return Err(err.into());
            };
            match (&f.leader_node, &self.secret) {
                (Some(leader), Some(secret)) => {
                    return network::forward(&leader.addr, secret, &req).await;
                }
                (None, Some(_)) if attempt == 0 => {
                    self.raft
                        .wait(Some(Duration::from_secs(5)))
                        .metrics(|m| m.current_leader.is_some(), "a leader is known")
                        .await?;
                }
                _ => return Err(err.into()),
            }
        }

        anyhow::bail!("no leader to send the write to")
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
    let secret = Arc::new(cfg.secret);
    let raft_store = RaftStore::open(raft_db, store.clone()).await?;
    let (log, sm) = Adaptor::new(raft_store);
    let config = openraft::Config {
        enable_elect: cfg.elect,
        ..config()?
    }
    .validate()?;
    let network = network::HttpNetwork {
        secret: secret.clone(),
    };
    let raft = Raft::new(cfg.id, Arc::new(config), network, log, sm).await?;
    let listener = tokio::net::TcpListener::bind(&cfg.addr).await?;
    let app = network::router(raft.clone(), secret.clone());
    let stop = Arc::new(tokio::sync::Notify::new());
    let stopped = stop.clone();
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
