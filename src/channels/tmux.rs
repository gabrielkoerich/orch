//! tmux channel — bridges agent sessions to the transport layer.
//!
//! This is the key channel for live session streaming:
//! - Captures pane output via `tmux capture-pane`
//! - Streams output chunks through the transport broadcast
//! - Accepts input via `tmux send-keys`
//!
//! Users can watch agent sessions in real-time from any connected channel,
//! and even join/intervene by sending input through the transport.

use super::{Channel, IncomingMessage, OutgoingMessage};
use crate::cmd::CommandErrorContext;
use async_trait::async_trait;
use std::sync::Arc;

pub struct TmuxChannel {
    /// Shared transport for pushing output
    transport: Option<Arc<crate::channels::transport::Transport>>,
}

impl TmuxChannel {
    pub fn with_transport(transport: Arc<crate::channels::transport::Transport>) -> Self {
        Self {
            transport: Some(transport),
        }
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

        // Output streaming is handled by CaptureService now.
        // The transport-backed capture loop has been removed to avoid duplicate output.
        if self.transport.is_none() {
            tracing::warn!("tmux channel started without transport - output streaming disabled");
        }

        Ok(rx)
    }

    async fn send(&self, msg: &OutgoingMessage) -> anyhow::Result<()> {
        // Send input to a tmux session via send-keys
        let session = &msg.thread_id; // thread_id = tmux session name
        send_keys(session, &msg.body).await
    }

    async fn health_check(&self) -> anyhow::Result<()> {
        // Check if tmux server is running
        let output = tokio::process::Command::new("tmux")
            .args(["list-sessions"])
            .output_with_context()
            .await?;
        if !output.status.success() {
            anyhow::bail!("tmux server not running");
        }
        Ok(())
    }
}

/// Send keystrokes to a tmux session.
pub async fn send_keys(session: &str, text: &str) -> anyhow::Result<()> {
    let output = tokio::process::Command::new("tmux")
        .args(["send-keys", "-t", session, text, "Enter"])
        .output_with_context()
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
        .args(["capture-pane", "-t", session, "-p", "-S", "-"])
        .output_with_context()
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
        .output_with_context()
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

/// Check if a tmux session is dead (no longer exists).
///
/// Best-effort check — spawns `tmux has-session` to verify.
/// Used by the capture service to detect ended sessions and send
/// final output chunks.
pub async fn is_session_dead(session: &str) -> bool {
    let output = tokio::process::Command::new("tmux")
        .args(["has-session", "-t", session])
        .output_with_context()
        .await;

    match output {
        Ok(output) => !output.status.success(),
        Err(_) => true,
    }
}

// Capture loop removed — output streaming handled by CaptureService.
