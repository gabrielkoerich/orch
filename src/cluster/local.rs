//! The localhost endpoint the CLI sends replicated writes to, so they go through the daemon's log

use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};

use super::{Command, Reply};
use crate::store::TaskStore;

static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_default()
});

fn port_path() -> anyhow::Result<PathBuf> {
    if cfg!(test) {
        Ok(std::env::temp_dir()
            .join("orch-test-state")
            .join("cluster.port"))
    } else {
        Ok(crate::home::state_dir()?.join("cluster.port"))
    }
}

/// Serve `POST /write` on 127.0.0.1 and record the port for the CLI. Returns the port
pub async fn serve(store: TaskStore) -> anyhow::Result<u16> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let path = port_path()?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    tokio::fs::write(&path, port.to_string()).await?;
    let app = Router::new().route("/write", post(write)).with_state(store);
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::warn!(err = %e, "cluster write endpoint stopped");
        }
    });
    tracing::info!(port, "cluster write endpoint started on 127.0.0.1");
    Ok(port)
}

async fn write(State(store): State<TaskStore>, Json(cmd): Json<Command>) -> Json<Reply> {
    Json(match store.write(cmd).await {
        Ok(reply) => reply,
        Err(e) => Reply::Err(format!("{e:#}")),
    })
}

pub async fn cleanup() {
    if let Ok(path) = port_path() {
        let _ = tokio::fs::remove_file(path).await;
    }
}

/// The running daemon's write URL, when its port file names one
pub async fn daemon_url() -> Option<String> {
    let port = tokio::fs::read_to_string(port_path().ok()?).await.ok()?;
    let port: u16 = port.trim().parse().ok()?;
    Some(format!("http://127.0.0.1:{port}/write"))
}

pub(crate) async fn send(url: &str, cmd: &Command) -> reqwest::Result<Reply> {
    CLIENT
        .post(url)
        .json(cmd)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
}
