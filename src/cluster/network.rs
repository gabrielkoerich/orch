//! Raft RPCs and forwarded writes between nodes, JSON over HTTP like `../agents-coordination`.
//! Every request carries the cluster secret, so reaching the port is not enough to join

// openraft's RPC errors are its own types, so their size is not ours to change
#![allow(clippy::result_large_err)]

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
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
const NONCE_HEADER: &str = "x-orch-nonce";
const PROOF_HEADER: &str = "x-orch-proof";

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
    me: Arc<Hello>,
    /// The address peers dial, bound into every proof so a proof relayed through another host fails
    addr: Arc<String>,
}

/// What a node says about itself to a peer that found it on the tailnet
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct Hello {
    pub name: String,
    pub id: NodeId,
    pub witness: bool,
    #[serde(default)]
    pub leader: Option<NodeId>,
    #[serde(default)]
    pub term: u64,
    /// HMAC of the prober's nonce and this name, so the prober knows the peer holds the secret
    #[serde(default)]
    pub proof: String,
}

/// HMAC-SHA256 under the cluster secret, in hex. Proves the secret without sending it
fn mac(secret: &str, parts: &[&str]) -> String {
    use hmac::{Hmac, Mac};
    let mut m =
        Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC takes any key");
    for part in parts {
        m.update(part.as_bytes());
        m.update(b"\0");
    }

    m.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A node asking the leader to add it as a learner
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct Join {
    pub id: NodeId,
    pub addr: String,
}

fn authorized(app: &App, headers: &HeaderMap) -> bool {
    let given = headers
        .get(SECRET_HEADER)
        .map(reqwest::header::HeaderValue::as_bytes)
        .unwrap_or_default();
    given.ct_eq(app.secret.as_bytes()).into()
}

pub(crate) fn router(raft: Raft, secret: Arc<String>, me: Hello, addr: String) -> Router {
    Router::new()
        .route("/raft/append", post(append))
        .route("/raft/snapshot", post(snapshot))
        .route("/raft/vote", post(vote))
        .route("/cluster/write", post(write))
        .route("/cluster/hello", get(hello))
        .route("/cluster/join", post(join))
        .with_state(App {
            raft,
            secret,
            me: Arc::new(me),
            addr: Arc::new(addr),
        })
}

/// Answers only a prober that proves the secret, and proves it back, so neither side sends it
async fn hello(State(a): State<App>, headers: HeaderMap) -> Response {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    };
    let nonce = header(NONCE_HEADER);
    let expected = mac(&a.secret, &["probe", &nonce, &a.addr]);
    let proven: bool = header(PROOF_HEADER)
        .as_bytes()
        .ct_eq(expected.as_bytes())
        .into();
    if nonce.len() < 16 || !proven {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let m = a.raft.metrics().borrow().clone();
    Json(Hello {
        leader: m.current_leader,
        term: m.current_term,
        proof: mac(&a.secret, &["hello", &nonce, &a.me.name, &a.addr]),
        ..(*a.me).clone()
    })
    .into_response()
}

/// Adds the caller as a learner on the leader, or sends it on to the leader
async fn join(State(a): State<App>, headers: HeaderMap, Json(req): Json<Join>) -> Response {
    if !authorized(&a, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let node = BasicNode::new(&req.addr);
    match a.raft.add_learner(req.id, node, false).await {
        Ok(_) => StatusCode::OK.into_response(),
        Err(RaftError::APIError(openraft::error::ClientWriteError::ForwardToLeader(f))) => {
            match f.leader_node {
                Some(leader) => match send_join(&leader.addr, &a.secret, &req).await {
                    Ok(()) => StatusCode::OK.into_response(),
                    Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
                },
                None => (StatusCode::SERVICE_UNAVAILABLE, "no leader yet").into_response(),
            }
        }
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    }
}

pub async fn send_join(addr: &str, secret: &str, req: &Join) -> anyhow::Result<()> {
    let res = CLIENT
        .post(format!("http://{addr}/cluster/join"))
        .header(SECRET_HEADER, secret)
        .json(req)
        .send()
        .await?;
    anyhow::ensure!(
        res.status().is_success(),
        "join refused by {addr} ({}): {}",
        res.status(),
        res.text().await?
    );
    Ok(())
}

/// Ask a host found on the tailnet who it is. The secret never leaves this node. Both proofs cover
/// the address dialed, and the answer must carry the name Tailscale gives that address, so a host
/// that relays a real member's answer is rejected
pub async fn probe(addr: &str, secret: &str, expect_name: Option<&str>) -> Option<Hello> {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let res = CLIENT
        .get(format!("http://{addr}/cluster/hello"))
        .header(NONCE_HEADER, &nonce)
        .header(PROOF_HEADER, mac(secret, &["probe", &nonce, addr]))
        .timeout(Duration::from_secs(1))
        .send()
        .await
        .ok()?;
    let hello: Hello = res.error_for_status().ok()?.json().await.ok()?;
    if expect_name.is_some_and(|name| name != hello.name) {
        return None;
    }

    let expected = mac(secret, &["hello", &nonce, &hello.name, addr]);
    let proven: bool = hello.proof.as_bytes().ct_eq(expected.as_bytes()).into();
    proven.then_some(hello)
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

    if let Err(e) = req.cmd.check_replicable() {
        return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }

    match a.raft.client_write(req).await {
        Ok(r) => Json(r.data).into_response(),
        // Refused before anything was proposed, so the sender may safely try another node
        Err(e @ RaftError::APIError(openraft::error::ClientWriteError::ForwardToLeader(_))) => {
            (StatusCode::MISDIRECTED_REQUEST, e.to_string()).into_response()
        }
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    }
}

/// Why a forwarded write failed. Only `NotApplied` is safe to send again
#[derive(Debug)]
pub(crate) enum ForwardError {
    /// The connection was refused, or the node said it is not the leader
    NotApplied(String),
    /// It may or may not have been applied, so it must not be sent again
    Unknown(anyhow::Error),
}

pub(crate) async fn forward(
    addr: &str,
    secret: &str,
    req: &Request,
) -> Result<Reply, ForwardError> {
    let res = CLIENT
        .post(format!("http://{addr}/cluster/write"))
        .header(SECRET_HEADER, secret)
        .json(req)
        .send()
        .await
        .map_err(|e| {
            if e.is_connect() {
                ForwardError::NotApplied(e.to_string())
            } else {
                ForwardError::Unknown(e.into())
            }
        })?;
    let status = res.status();
    if status == StatusCode::MISDIRECTED_REQUEST {
        let body = res.text().await.unwrap_or_default();
        return Err(ForwardError::NotApplied(format!(
            "{addr} is not the leader: {body}"
        )));
    }

    if !status.is_success() {
        let body = res.text().await.unwrap_or_default();
        return Err(ForwardError::Unknown(anyhow::anyhow!(
            "leader at {addr} refused the write ({status}): {body}"
        )));
    }

    res.json()
        .await
        .map_err(|e| ForwardError::Unknown(e.into()))
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
            secret: std::sync::Arc::clone(&self.secret),
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
