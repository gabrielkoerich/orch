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

use crate::channels::transport::Transport;
use crate::channels::OutputChunk;
use crate::channels::tmux;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

/// Buffer for tracking session output state.
#[derive(Debug, Clone)]
pub struct OutputBuffer {
    /// The tmux session name (e.g., "orch-42")
    pub session: String,
    /// The task ID this session belongs to
    pub task_id: String,
    /// Content from the last capture
    pub last_content: String,
    /// When the last capture occurred
    pub last_capture: DateTime<Utc>,
}

/// Service that captures tmux pane output and broadcasts to transport.
pub struct CaptureService {
    /// Session buffers keyed by task_id
    buffers: Arc<RwLock<HashMap<String, OutputBuffer>>>,
    /// Transport layer for broadcasting output
    transport: Arc<Transport>,
    /// Polling interval
    interval: Duration,
}

impl CaptureService {
    /// Create a new CaptureService.
    pub fn new(transport: Arc<Transport>) -> Self {
        Self {
            buffers: Arc::new(RwLock::new(HashMap::new())),
            transport,
            interval: Duration::from_secs(2),
        }
    }

    /// Set the polling interval.
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Register a session to be tracked.
    pub async fn register_session(&self, task_id: &str, session: &str) {
        let buffer = OutputBuffer {
            session: session.to_string(),
            task_id: task_id.to_string(),
            last_content: String::new(),
            last_capture: Utc::now(),
        };
        self.buffers.write().await.insert(task_id.to_string(), buffer);
        tracing::debug!(task_id, session, "session registered for capture");
    }

    /// Unregister a session (stop tracking).
    pub async fn unregister_session(&self, task_id: &str) {
        if let Some(buffer) = self.buffers.write().await.remove(task_id) {
            tracing::debug!(task_id = buffer.task_id, session = buffer.session, "session unregistered");
        }
    }

    /// Start the capture loop.
    ///
    /// This runs indefinitely, polling registered sessions for new output.
    pub async fn start(&self) {
        tracing::info!(interval_secs = self.interval.as_secs(), "capture service started");
        let mut interval = tokio::time::interval(self.interval);

        loop {
            interval.tick().await;
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
                match tmux::capture_pane(&buffer.session).await {
                    Ok(current_content) => {
                        // Diff against previous content
                        if current_content != buffer.last_content {
                            // Find the new content (everything after the last known content)
                            let new_content = if buffer.last_content.is_empty() {
                                current_content.clone()
                            } else {
                                // Find where the old content ends in the new content
                                match current_content.find(&buffer.last_content) {
                                    Some(_pos) => {
                                        // New content is everything after the old content
                                        // But we need to handle the case where the old content
                                        // appears multiple times - use the last occurrence
                                        let last_pos = current_content.rfind(&buffer.last_content);
                                        if let Some(last) = last_pos {
                                            let offset = last + buffer.last_content.len();
                                            if offset < current_content.len() {
                                                current_content[offset..].to_string()
                                            } else {
                                                String::new()
                                            }
                                        } else {
                                            current_content.clone()
                                        }
                                    }
                                    None => current_content.clone(),
                                }
                            };

                            if !new_content.is_empty() {
                                let chunk = OutputChunk {
                                    task_id: task_id.clone(),
                                    content: new_content,
                                    timestamp: Utc::now(),
                                    is_final: false,
                                };
                                self.transport.push_output(&task_id, chunk).await;
                            }

                            // Update buffer
                            let mut buffers = self.buffers.write().await;
                            if let Some(buf) = buffers.get_mut(&task_id) {
                                buf.last_content = current_content;
                                buf.last_capture = Utc::now();
                            }
                        }
                    }
                    Err(e) => {
                        // Session might have ended - don't spam logs
                        tracing::trace!(task_id, session = buffer.session, ?e, "capture failed");
                    }
                }
            }
        }
    }

    /// Check if a session is being tracked.
    pub async fn is_tracking(&self, task_id: &str) -> bool {
        self.buffers.read().await.contains_key(task_id)
    }

    /// Get the list of tracked task IDs.
    pub async fn tracked_sessions(&self) -> Vec<String> {
        self.buffers.read().await.keys().cloned().collect()
    }
}
