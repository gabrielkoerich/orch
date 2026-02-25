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
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::time::{Duration, Interval};

pub struct TmuxChannel {
    /// Shared transport for pushing output
    transport: Option<Arc<crate::channels::transport::Transport>>,
}

impl TmuxChannel {
    pub fn new() -> Self {
        Self { transport: None }
    }

    pub fn with_transport(transport: Arc<crate::channels::transport::Transport>) -> Self {
        Self {
            transport: Some(transport),
        }
    }
}

/// State for the capture loop
struct CaptureState {
    /// Last captured content per session
    last_content: HashMap<String, String>,
}

impl CaptureState {
    fn new() -> Self {
        Self {
            last_content: HashMap::new(),
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

        // Spawn background capture loop if transport is configured
        if let Some(transport) = &self.transport {
            let transport = transport.clone();
            tokio::spawn(async move {
                if let Err(e) = capture_loop(transport).await {
                    tracing::error!(?e, "tmux capture loop failed");
                }
            });
        } else {
            tracing::warn!("tmux channel started without transport - output streaming disabled");
        }

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
        tracing::debug!(session = thread_id, "tmux channel handles its own output capture");
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
pub async fn send_keys(session: &str, text: &str) -> anyhow::Result<()> {
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

/// Background loop that captures tmux output every 2 seconds for active sessions.
///
/// Runs continuously:
/// 1. Lists all orch-* tmux sessions
/// 2. Captures pane content for each session
/// 3. Diffs against last capture to find new content
/// 4. Pushes new content to transport for broadcasting
async fn capture_loop(transport: Arc<crate::channels::transport::Transport>) -> anyhow::Result<()> {
    let capture_interval = Duration::from_secs(2);
    let mut ticker: Interval = tokio::time::interval(capture_interval);

    // Track last captured content per session
    let mut last_content: HashMap<String, String> = HashMap::new();

    tracing::info!("tmux capture loop started");

    loop {
        ticker.tick().await;

        // Get active sessions from transport (or fall back to listing all orch-* sessions)
        let active_bindings = transport.active_sessions().await;
        let sessions_to_capture: Vec<String> = if active_bindings.is_empty() {
            // No active bindings - fall back to listing all orch sessions
            match list_orch_sessions().await {
                Ok(sessions) => sessions,
                Err(e) => {
                    tracing::debug!(?e, "failed to list tmux sessions");
                    continue;
                }
            }
        } else {
            // Capture only sessions with active bindings
            active_bindings
                .iter()
                .map(|b| b.tmux_session.clone())
                .collect()
        };

        for session_name in sessions_to_capture {
            // Extract task_id from session name (e.g., "orch-42" -> "42")
            let task_id = session_name
                .strip_prefix("orch-")
                .unwrap_or(&session_name)
                .to_string();

            // Capture current pane content
            match capture_pane(&session_name).await {
                Ok(content) => {
                    let last = last_content.get(&session_name);

                    // Check if content changed
                    if last.map(|s| s.as_str()) != Some(&content) {
                        // Find the diff (new content since last capture)
                        let _new_content = if let Some(last_content) = last {
                            // Simple diff: content that appears after the last known content
                            if let Some(pos) = content.find(last_content) {
                                // Found previous content, take everything after it
                                let _remaining = &content[pos + last_content.len()..];
                                // Handle case where previous content appears multiple times
                                // We want the content AFTER the last occurrence for accurate diffing
                                content.len() - (pos + last_content.len())
                            } else {
                                // Content changed completely - just send the new content
                                content.len()
                            }
                        } else {
                            // First capture - send all content
                            content.len()
                        };

                        // Extract new content properly
                        let new_chunk = if let Some(last_content) = last {
                            if let Some(pos) = content.rfind(last_content) {
                                // Use rfind to find the LAST occurrence of last_content
                                // This handles cases where content is repeated
                                content[pos + last_content.len()..].to_string()
                            } else {
                                content.clone()
                            }
                        } else {
                            // First capture - truncate to avoid sending too much
                            let lines: Vec<&str> = content.lines().rev().take(50).collect();
                            lines.into_iter().rev().collect::<Vec<_>>().join("\n")
                        };

                        if !new_chunk.trim().is_empty() {
                            // Push to transport for broadcasting
                            transport
                                .push_output(
                                    &task_id,
                                    OutputChunk {
                                        task_id: task_id.clone(),
                                        content: new_chunk,
                                        timestamp: chrono::Utc::now(),
                                        is_final: false,
                                    },
                                )
                                .await;
                        }

                        // Update last content
                        last_content.insert(session_name.clone(), content);
                    }
                }
                Err(e) => {
                    // Session might have ended - remove from tracking
                    tracing::debug!(session = %session_name, ?e, "capture failed");
                    last_content.remove(&session_name);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capture_state_new() {
        let state = CaptureState::new();
        assert!(state.last_content.is_empty());
    }
}
