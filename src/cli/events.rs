//! CLI handler for `orch events` and `orch task watch` — connects to the
//! event bus websocket server and prints task status transitions in real-time.

use anyhow::Context;
use futures::StreamExt;
use tokio_tungstenite::connect_async;

/// Read the websocket port from `~/.orch/state/ws.port`.
fn read_ws_port() -> anyhow::Result<u16> {
    let state_dir = crate::home::state_dir()?;
    let port_str = std::fs::read_to_string(state_dir.join("ws.port"))
        .context("event bus not running — is the service started? (ws.port not found)")?;
    port_str
        .trim()
        .parse()
        .context("invalid port number in ws.port")
}

/// Stream events from the event bus, optionally filtered by repo and/or task ID.
///
/// Output format: `HH:MM:SS task_id old_status → new_status agent PR: #N`
pub async fn stream(repo: Option<&str>, task: Option<&str>) -> anyhow::Result<()> {
    let port = read_ws_port()?;
    let url = format!("ws://127.0.0.1:{port}/events");

    let (ws, _) = connect_async(&url)
        .await
        .context("failed to connect to event bus — is the service running?")?;

    let (_write, mut read) = ws.split();

    println!("Connected to event bus (port {port}). Streaming events...\n");

    while let Some(msg) = read.next().await {
        let msg = msg?;
        if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
            let Ok(event) = serde_json::from_str::<crate::engine::events::TaskEvent>(&text) else {
                continue;
            };

            // Apply filters
            if let Some(repo_filter) = repo {
                if !event.repo.contains(repo_filter) {
                    continue;
                }
            }
            if let Some(task_filter) = task {
                if event.task_id != task_filter {
                    continue;
                }
            }

            // Format output
            let time = if event.timestamp.len() >= 19 {
                &event.timestamp[11..19]
            } else {
                &event.timestamp
            };
            let agent_str = event.agent.as_deref().unwrap_or("");
            let pr_str = event
                .pr_number
                .as_ref()
                .map(|p| format!(" PR: #{p}"))
                .unwrap_or_default();

            println!(
                "{time} {} {} \u{2192} {} {agent_str}{pr_str}",
                event.task_id, event.old_status, event.new_status,
            );
        }
    }

    Ok(())
}
