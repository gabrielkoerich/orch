//! Capture service — polls tmux sessions for output and broadcasts to transport.
//!
//! This service:
//! - Runs a background loop every 2 seconds
//! - Captures pane output for registered sessions
//! - Diffs against previous capture to find new content
//! - Pushes new content as OutputChunk to transport
//!
//! Sessions are registered when tasks are dispatched and unregistered
//! when they complete.

use crate::channels::tmux;
use crate::channels::transport::Transport;
use crate::channels::OutputChunk;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

pub(crate) const MAX_OUTPUT_BUFFER_BYTES: usize = 1024 * 1024;

/// Buffer for tracking session output state.
#[derive(Debug, Clone)]
pub struct OutputBuffer {
    /// The tmux session name (e.g., "orch-myproject-42")
    pub session: String,
    /// The task ID this session belongs to
    pub task_id: String,
    /// Content from the last capture
    pub last_content: String,
    /// Byte length of the last capture
    pub last_len: usize,
    /// Hash of the last capture (for dedup)
    pub last_hash: Option<[u8; 32]>,
    /// When the last capture occurred
    pub last_capture: DateTime<Utc>,
    /// Whether the session has been seen alive at least once.
    /// Prevents firing "session ended" for sessions that were registered
    /// before the tmux session was actually created.
    pub seen_alive: bool,
}

/// Service that captures tmux pane output and broadcasts to transport.
pub struct CaptureService {
    /// Session buffers keyed by task_id
    buffers: Arc<RwLock<HashMap<String, OutputBuffer>>>,
    /// Transport layer for broadcasting output
    transport: Arc<Transport>,
    /// Polling interval
    interval: std::time::Duration,
}

impl CaptureService {
    /// Create a new CaptureService.
    pub fn new(transport: Arc<Transport>) -> Self {
        Self {
            buffers: Arc::new(RwLock::new(HashMap::new())),
            transport,
            interval: std::time::Duration::from_secs(2),
        }
    }

    /// Register a session to be tracked.
    pub async fn register_session(&self, task_id: &str, session: &str) {
        let buffer = OutputBuffer {
            session: session.to_string(),
            task_id: task_id.to_string(),
            last_content: String::new(),
            last_len: 0,
            last_hash: None,
            last_capture: Utc::now(),
            seen_alive: false,
        };
        self.buffers
            .write()
            .await
            .insert(task_id.to_string(), buffer);
        tracing::debug!(task_id, session, "session registered for capture");
    }

    /// Unregister a session (stop tracking).
    pub async fn unregister_session(&self, task_id: &str) {
        if let Some(buffer) = self.buffers.write().await.remove(task_id) {
            tracing::debug!(
                task_id = buffer.task_id,
                session = buffer.session,
                "session unregistered"
            );
        }
    }

    /// Start the capture loop.
    ///
    /// This runs indefinitely, polling registered sessions for new output.
    pub async fn start(&self) {
        tracing::info!(
            interval_secs = self.interval.as_secs(),
            "capture service started"
        );
        let mut interval = tokio::time::interval(self.interval);

        loop {
            interval.tick().await;
            self.tick().await;
        }
    }

    /// Run the capture loop while there are registered sessions.
    ///
    /// This returns when no more sessions are registered, making it suitable
    /// for CLI streaming where the capture should stop when the session ends.
    pub async fn run(&self) {
        let mut interval = tokio::time::interval(self.interval);

        loop {
            interval.tick().await;

            // Check if there are any sessions to capture
            let has_sessions = !self.buffers.read().await.is_empty();
            if !has_sessions {
                tracing::debug!("no sessions registered, capture loop exiting");
                break;
            }

            self.tick().await;
        }
    }

    /// Run one tick of the capture loop.
    async fn tick(&self) {
        let buffers = self.buffers.read().await;
        let task_ids: Vec<String> = buffers.keys().cloned().collect();
        drop(buffers);

        for task_id in task_ids {
            // Get buffer (reborrow for each iteration)
            let buffer = {
                let buffers = self.buffers.read().await;
                buffers.get(&task_id).cloned()
            };

            if let Some(buffer) = buffer {
                // Prefer transport-backed PTY output if available (engine-managed PTYs)
                let current_content = match self.transport.get_session_output(&buffer.task_id).await
                {
                    Some(s) => s,
                    None => match tmux::capture_pane(&buffer.session).await {
                        Ok(s) => s,
                        Err(e) => {
                            // Only fire "session ended" if the session was seen alive before.
                            // Prevents false positives when the session is registered before
                            // the tmux session is actually created (race between registration
                            // and session creation).
                            if buffer.seen_alive && tmux::is_session_dead(&buffer.session).await {
                                tracing::info!(
                                    task_id,
                                    session = buffer.session,
                                    "session ended, sending final chunk"
                                );
                                let chunk = OutputChunk {
                                    content: String::new(),
                                    is_final: true,
                                };
                                self.transport.push_output(&task_id, chunk).await;
                                self.unregister_session(&task_id).await;
                            } else {
                                tracing::trace!(
                                    task_id,
                                    session = buffer.session,
                                    ?e,
                                    "capture failed (transient)"
                                );
                            }
                            continue;
                        }
                    },
                };

                let new_content = {
                    let mut buffers = self.buffers.write().await;
                    if let Some(buf) = buffers.get_mut(&task_id) {
                        buf.seen_alive = true;
                        buf.diff_and_update(&current_content)
                    } else {
                        None
                    }
                };

                if let Some(new_content) = new_content {
                    let chunk = OutputChunk {
                        content: new_content,
                        is_final: false,
                    };
                    self.transport.push_output(&task_id, chunk).await;
                }
            }
        }
    }
}

impl OutputBuffer {
    pub(crate) fn diff_and_update(&mut self, current_content: &str) -> Option<String> {
        let current_bytes = current_content.as_bytes();
        let current_len = current_bytes.len();
        let current_hash = hash_bytes(current_bytes);

        if self.last_len == current_len && self.last_hash == Some(current_hash) {
            self.last_capture = Utc::now();
            return None;
        }

        let new_content = if self.last_len > current_len {
            // Content shrank (terminal cleared) — no incremental diff available.
            // Emitting full current_content would duplicate already-broadcast output.
            String::new()
        } else if self.last_len == current_len {
            // Same-length overwrite (e.g. spinner/progress refresh) — resync with full content.
            current_content.to_string()
        } else {
            let mut offset = self.last_len.min(current_len);
            while offset < current_len && !current_content.is_char_boundary(offset) {
                offset += 1;
            }
            String::from_utf8_lossy(&current_bytes[offset..]).to_string()
        };

        self.last_len = current_len;
        self.last_hash = Some(current_hash);
        self.last_content = cap_content(current_content);
        self.last_capture = Utc::now();

        if new_content.trim().is_empty() {
            None
        } else {
            Some(new_content)
        }
    }
}

fn hash_bytes(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

fn cap_content(content: &str) -> String {
    if content.len() <= MAX_OUTPUT_BUFFER_BYTES {
        return content.to_string();
    }

    let mut start = content.len() - MAX_OUTPUT_BUFFER_BYTES;
    while start < content.len() && !content.is_char_boundary(start) {
        start += 1;
    }
    content[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_buffer() -> OutputBuffer {
        OutputBuffer {
            session: "test-session".to_string(),
            task_id: "task-1".to_string(),
            last_content: String::new(),
            last_len: 0,
            last_hash: None,
            last_capture: Utc::now(),
            seen_alive: false,
        }
    }

    #[test]
    fn normal_append_returns_new_suffix() {
        let mut buf = make_buffer();
        // First capture
        let result = buf.diff_and_update("hello");
        assert_eq!(result.as_deref(), Some("hello"));
        // Append more
        let result = buf.diff_and_update("hello world");
        assert_eq!(result.as_deref(), Some(" world"));
    }

    #[test]
    fn no_change_returns_none() {
        let mut buf = make_buffer();
        buf.diff_and_update("hello");
        // Same content → no diff
        let result = buf.diff_and_update("hello");
        assert_eq!(result, None);
    }

    #[test]
    fn terminal_clear_shrink_returns_none_not_full_content() {
        let mut buf = make_buffer();
        // Simulate some prior output
        buf.diff_and_update("line1\nline2\nline3\n");
        // Terminal clears: pane now contains less content (e.g. after \033[2J)
        let result = buf.diff_and_update("prompt$ ");
        // Must NOT return the full current content as "new" — that would duplicate output.
        assert_eq!(
            result, None,
            "shrinking pane content should produce no output, not a duplicate of visible screen"
        );
    }

    #[test]
    fn terminal_clear_to_empty_returns_none() {
        let mut buf = make_buffer();
        buf.diff_and_update("lots of output here\nmore output\n");
        // Pane cleared to empty
        let result = buf.diff_and_update("");
        assert_eq!(result, None);
    }

    #[test]
    fn same_length_content_change_is_broadcast() {
        let mut buf = make_buffer();
        buf.diff_and_update("aaaa");

        let result = buf.diff_and_update("bbbb");

        assert_eq!(result.as_deref(), Some("bbbb"));
    }

    #[test]
    fn empty_initial_capture_returns_none() {
        let mut buf = make_buffer();
        let result = buf.diff_and_update("");
        assert_eq!(result, None);
    }

    #[test]
    fn whitespace_only_new_content_returns_none() {
        let mut buf = make_buffer();
        buf.diff_and_update("hello");
        // New suffix is only whitespace
        let result = buf.diff_and_update("hello   \n");
        assert_eq!(result, None);
    }
}
