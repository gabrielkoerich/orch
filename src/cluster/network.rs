//! Raft RPCs and forwarded writes between nodes, JSON over HTTP like `../agents-coordination`.
//! Every request carries the cluster secret, so reaching the port is not enough to join

// openraft's RPC errors are its own types, so their size is not ours to change
#![allow(clippy::result_large_err)]

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use openraft::error::{
    InstallSnapshotError, NetworkError, RPCError, RaftError, RemoteError, Unreachable,
};
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use openraft::BasicNode;
use serde::de::DeserializeOwned;
use serde::Serialize;
use subtle::ConstantTimeEq;

use super::{NodeId, Raft, Reply, Request, TypeConfig};

const SECRET_HEADER: &str = "x-orch-cluster-secret";

static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_default()
});

#[derive(Clone)]
struct App {
    raft: Raft,
    secret: Arc<String>,
}

fn authorized(app: &App, headers: &HeaderMap) -> bool {
    let given = headers
        .get(SECRET_HEADER)
        .map(|v| v.as_bytes())
        .unwrap_or_default();
    given.ct_eq(app.secret.as_bytes()).into()
}

pub(crate) fn router(raft: Raft, secret: Arc<String>) -> Router {
    Router::new()
        .route("/raft/append", post(append))
        .route("/raft/snapshot", post(snapshot))
        .route("/raft/vote", post(vote))
        .route("/cluster/write", post(write))
        .with_state(App { raft, secret })
}

async fn append(
    State(a): State<App>,
    headers: HeaderMap,
    Json(req): Json<AppendEntriesRequest<TypeConfig>>,
) -> Response {
    if !authorized(&a, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    Json(a.raft.append_entries(req).await).into_response()
}

async fn snapshot(
    State(a): State<App>,
    headers: HeaderMap,
    Json(req): Json<InstallSnapshotRequest<TypeConfig>>,
) -> Response {
    if !authorized(&a, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    Json(a.raft.install_snapshot(req).await).into_response()
}

async fn vote(
    State(a): State<App>,
    headers: HeaderMap,
    Json(req): Json<VoteRequest<NodeId>>,
) -> Response {
    if !authorized(&a, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    Json(a.raft.vote(req).await).into_response()
}

/// A write a follower forwarded. It is proposed here and never forwarded again, so writes cannot loop
async fn write(State(a): State<App>, headers: HeaderMap, Json(req): Json<Request>) -> Response {
    if !authorized(&a, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    match a.raft.client_write(req).await {
        Ok(r) => Json(r.data).into_response(),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    }
}

pub(crate) async fn forward(addr: &str, secret: &str, req: &Request) -> anyhow::Result<Reply> {
    let res = CLIENT
        .post(format!("http://{addr}/cluster/write"))
        .header(SECRET_HEADER, secret)
        .json(req)
        .send()
        .await?;
    let status = res.status();
    if !status.is_success() {
        anyhow::bail!(
            "leader at {addr} refused the write ({status}): {}",
            res.text().await?
        );
    }

    Ok(res.json().await?)
}

pub(crate) struct HttpNetwork {
    pub secret: Arc<String>,
}

impl RaftNetworkFactory<TypeConfig> for HttpNetwork {
    type Network = Peer;

    async fn new_client(&mut self, target: NodeId, node: &BasicNode) -> Peer {
        Peer {
            target,
            addr: node.addr.clone(),
            secret: self.secret.clone(),
        }
    }
}

pub(crate) struct Peer {
    target: NodeId,
    addr: String,
    secret: Arc<String>,
}

impl Peer {
    async fn send<Req, Resp, E>(
        &self,
        path: &str,
        req: Req,
        option: &RPCOption,
    ) -> Result<Resp, RPCError<NodeId, BasicNode, RaftError<NodeId, E>>>
    where
        Req: Serialize,
        Resp: DeserializeOwned,
        E: std::error::Error + DeserializeOwned,
    {
        let res = CLIENT
            .post(format!("http://{}/raft/{path}", self.addr))
            .header(SECRET_HEADER, self.secret.as_str())
            .timeout(option.hard_ttl())
            .json(&req)
            .send()
            .await
            .map_err(|e| {
                if e.is_connect() {
                    RPCError::Unreachable(Unreachable::new(&e))
                } else {
                    RPCError::Network(NetworkError::new(&e))
                }
            })?;
        let res = res
            .error_for_status()
            .map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        let body: Result<Resp, RaftError<NodeId, E>> = res
            .json()
            .await
            .map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        body.map_err(|e| RPCError::RemoteError(RemoteError::new(self.target, e)))
    }
}

impl RaftNetwork<TypeConfig> for Peer {
    async fn append_entries(
        &mut self,
        req: AppendEntriesRequest<TypeConfig>,
        option: RPCOption,
    ) -> Result<AppendEntriesResponse<NodeId>, RPCError<NodeId, BasicNode, RaftError<NodeId>>> {
        self.send("append", req, &option).await
    }

    async fn install_snapshot(
        &mut self,
        req: InstallSnapshotRequest<TypeConfig>,
        option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<NodeId>,
        RPCError<NodeId, BasicNode, RaftError<NodeId, InstallSnapshotError>>,
    > {
        self.send("snapshot", req, &option).await
    }

    async fn vote(
        &mut self,
        req: VoteRequest<NodeId>,
        option: RPCOption,
    ) -> Result<VoteResponse<NodeId>, RPCError<NodeId, BasicNode, RaftError<NodeId>>> {
        self.send("vote", req, &option).await
    }
}
