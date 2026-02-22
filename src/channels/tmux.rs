//! tmux channel — bridges agent sessions to the transport layer.
//!
//! This is the key channel for live session streaming:
//! - Captures pane output via `tmux capture-pane`
//! - Streams output chunks through the transport broadcast
//! - Accepts input via `tmux send-keys`
//!
//! Users can watch agent sessions in real-time from any connected channel,
//! and even join/intervene by sending input through the transport.

use super::{Channel, IncomingMessage, OutgoingMessage, OutputChunk};
use async_trait::async_trait;
use tokio::sync::broadcast;

pub struct TmuxChannel;

impl TmuxChannel {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Channel for TmuxChannel {
    fn name(&self) -> &str {
        "tmux"
    }

    async fn start(&self) -> anyhow::Result<tokio::sync::mpsc::Receiver<IncomingMessage>> {
        let (_tx, rx) = tokio::sync::mpsc::channel(64);
        tracing::info!("tmux channel started");
        // TODO: poll active tmux sessions for output
        Ok(rx)
    }

    async fn send(&self, msg: &OutgoingMessage) -> anyhow::Result<()> {
        // Send input to a tmux session via send-keys
        let session = &msg.thread_id; // thread_id = tmux session name
        send_keys(session, &msg.body).await
    }

    async fn stream_output(
        &self,
        thread_id: &str,
        _rx: broadcast::Receiver<OutputChunk>,
    ) -> anyhow::Result<()> {
        // tmux IS the output source — this captures and broadcasts
        tracing::debug!(session = thread_id, "starting tmux capture loop");
        // TODO: spawn capture-pane loop, push to transport
        Ok(())
    }

    async fn health_check(&self) -> anyhow::Result<()> {
        // Check if tmux server is running
        let output = tokio::process::Command::new("tmux")
            .args(["list-sessions"])
            .output()
            .await?;
        if !output.status.success() {
            anyhow::bail!("tmux server not running");
        }
        Ok(())
    }

    async fn shutdown(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Send keystrokes to a tmux session.
async fn send_keys(session: &str, text: &str) -> anyhow::Result<()> {
    let output = tokio::process::Command::new("tmux")
        .args(["send-keys", "-t", session, text, "Enter"])
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("tmux send-keys failed: {stderr}");
    }
    Ok(())
}

/// Capture the current content of a tmux pane.
pub async fn capture_pane(session: &str) -> anyhow::Result<String> {
    let output = tokio::process::Command::new("tmux")
        .args(["capture-pane", "-t", session, "-p", "-S", "-100"])
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("tmux capture-pane failed: {stderr}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// List active tmux sessions matching the orch- prefix.
pub async fn list_orch_sessions() -> anyhow::Result<Vec<String>> {
    let output = tokio::process::Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
        .await?;
    if !output.status.success() {
        return Ok(vec![]);
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|s| s.starts_with("orch-"))
        .map(String::from)
        .collect())
}
