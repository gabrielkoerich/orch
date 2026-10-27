//! Raft between orch nodes. Only coordination state goes through the log, the design is in
//! `docs/specs/2026-10-26-multi-machine-cluster-design.md`.

mod command;
pub mod local;
mod storage;

use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use openraft::error::{InstallSnapshotError, RPCError, RaftError, Unreachable};
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

/// Start a one-node cluster over `store`, with its log in `raft_db`, and route the store's
/// replicated writes through it
pub async fn start_single(store: &TaskStore, raft_db: &Path) -> anyhow::Result<Raft> {
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
    store.attach_raft(raft.clone());
    Ok(raft)
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
