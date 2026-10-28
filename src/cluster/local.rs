//! The localhost endpoint the CLI sends replicated writes to, so they go through the daemon's log

use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use subtle::ConstantTimeEq;

use super::{Command, Reply};
use crate::store::TaskStore;

static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_default()
});

/// Where the CLI reaches the daemon, and the token that proves the caller can read `~/.orch`
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub url: String,
    pub token: String,
}

fn port_path() -> anyhow::Result<PathBuf> {
    if cfg!(test) {
        Ok(std::env::temp_dir()
            .join("orch-test-state")
            .join("cluster.port"))
    } else {
        Ok(crate::home::state_dir()?.join("cluster.port"))
    }
}

#[derive(Clone)]
struct App {
    store: TaskStore,
    token: Arc<String>,
    host: Arc<String>,
}

/// Serve `POST /write` on 127.0.0.1 and record the port, a fresh token and the pid for the CLI
pub async fn serve(store: TaskStore) -> anyhow::Result<Endpoint> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    write_private(
        &port_path()?,
        &format!("{port} {token} {}", std::process::id()),
    )?;
    let app = Router::new()
        .route("/write", post(write))
        .route("/progress", post(progress))
        .route("/nodes", get(nodes))
        .with_state(App {
            store,
            token: Arc::new(token.clone()),
            host: Arc::new(format!("127.0.0.1:{port}")),
        });
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::warn!(err = %e, "cluster write endpoint stopped");
        }
    });
    tracing::info!(port, "cluster write endpoint started on 127.0.0.1");
    Ok(Endpoint {
        url: format!("http://127.0.0.1:{port}/write"),
        token,
    })
}

/// Owner-only file, written to a temp name and renamed so a reader never sees half of it
fn write_private(path: &std::path::Path, body: &str) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(body.as_bytes())?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn authorized(app: &App, headers: &HeaderMap) -> bool {
    // A rebound DNS name sends its own Host, so only the literal address passes
    let host_ok = headers
        .get(header::HOST)
        .is_some_and(|h| h.as_bytes() == app.host.as_bytes());
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    let token_ok: bool = given.as_bytes().ct_eq(app.token.as_bytes()).into();
    host_ok && token_ok
}

async fn write(State(app): State<App>, headers: HeaderMap, Json(cmd): Json<Command>) -> Response {
    if !authorized(&app, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    Json(match app.store.write(cmd).await {
        Ok(reply) => reply,
        Err(e) => Reply::Err(format!("{e:#}")),
    })
    .into_response()
}

async fn progress(
    State(app): State<App>,
    headers: HeaderMap,
    Json(report): Json<super::progress::Report>,
) -> Response {
    if !authorized(&app, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let node = crate::cluster::discovery::status()
        .await
        .map(|t| t.me.name)
        .unwrap_or_else(|_| "local".to_string());
    match super::progress::handle(&app.store, &node, report).await {
        Ok(record) => Json(record).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}

async fn nodes(State(app): State<App>, headers: HeaderMap) -> Response {
    if !authorized(&app, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let members = app.store.node().map(super::members).unwrap_or_default();
    Json(members).into_response()
}

pub async fn fetch_nodes(endpoint: &Endpoint) -> anyhow::Result<Vec<super::Member>> {
    let url = endpoint.url.replace("/write", "/nodes");
    let res = CLIENT.get(url).bearer_auth(&endpoint.token).send().await?;
    Ok(res.error_for_status()?.json().await?)
}

/// Send an `orch progress` report to the daemon, which commits, pushes and records it
pub async fn send_progress(
    endpoint: &Endpoint,
    report: &super::progress::Report,
) -> anyhow::Result<super::progress::Record> {
    let url = endpoint.url.replace("/write", "/progress");
    let res = CLIENT
        .post(url)
        .bearer_auth(&endpoint.token)
        .json(report)
        .send()
        .await?;
    let status = res.status();
    if !status.is_success() {
        anyhow::bail!(
            "orch service refused the report ({status}): {}",
            res.text().await?
        );
    }

    Ok(res.json().await?)
}

pub async fn cleanup() {
    if let Ok(path) = port_path() {
        let _ = tokio::fs::remove_file(path).await;
    }
}

/// The running daemon's endpoint. A file left by a daemon that is gone names nothing
pub async fn daemon_endpoint() -> Option<Endpoint> {
    let body = tokio::fs::read_to_string(port_path().ok()?).await.ok()?;
    let mut parts = body.split_whitespace();
    let port: u16 = parts.next()?.parse().ok()?;
    let token = parts.next()?.to_string();
    let pid: u32 = parts.next()?.parse().ok()?;
    let alive = tokio::task::spawn_blocking(move || crate::home::pid_is_orch_serve(pid))
        .await
        .ok()?;
    alive.then(|| Endpoint {
        url: format!("http://127.0.0.1:{port}/write"),
        token,
    })
}

pub(crate) async fn send(endpoint: &Endpoint, cmd: &Command) -> reqwest::Result<Reply> {
    CLIENT
        .post(&endpoint.url)
        .bearer_auth(&endpoint.token)
        .json(cmd)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
}
