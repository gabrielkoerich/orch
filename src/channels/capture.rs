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
use crate::channels::transport::{session_key, Transport};
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
    /// Repo slug (owner/repo) that owns this session.
    pub repo: String,
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
    /// Whether the session has been seen alive at least once (i.e., `capture_pane`
    /// succeeded at least once). Prevents firing "session ended" for sessions that
    /// were registered before the tmux session was actually created.
    /// Also used by silence detection: a seen-alive session with no output is only
    /// treated as silent if the tmux session is currently dead (silent exit-0).
    pub seen_alive: bool,
    /// When this session was registered for capture.
    pub registered_at: DateTime<Utc>,
    /// Whether the agent has produced any meaningful output since session start.
    /// Used for silence detection: only agents that never produced output are
    /// considered silent. Agents that produced output then go quiet are NOT
    /// killed (they may be running long tool calls with sparse output).
    pub has_output: bool,
    /// Generation counter incremented on each registration for this session_key.
    /// Used to detect when a session has been re-registered (e.g., after retry)
    /// so that stale snapshots from previous generations don't incorrectly
    /// trigger final/unregister side effects.
    pub generation: u64,
}

/// Service that captures tmux pane output and broadcasts to transport.
pub struct CaptureService {
    /// Session buffers keyed by session_key(repo, task_id)
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
    pub async fn register_session(&self, repo: &str, task_id: &str, session: &str) {
        let now = Utc::now();
        let skey = session_key(repo, task_id);

        // Atomically read the current generation and insert the new buffer under
        // a single write lock to prevent a TOCTOU race where two concurrent
        // re-registrations both observe the same stale generation value.
        let generation = {
            let mut buffers = self.buffers.write().await;
            let generation = buffers.get(&skey).map(|b| b.generation + 1).unwrap_or(0);
            let buffer = OutputBuffer {
                repo: repo.to_string(),
                session: session.to_string(),
                task_id: task_id.to_string(),
                last_content: String::new(),
                last_len: 0,
                last_hash: None,
                last_capture: now,
                seen_alive: false,
                registered_at: now,
                has_output: false,
                generation,
            };
            buffers.insert(skey, buffer);
            generation
        };
        tracing::debug!(
            repo,
            task_id,
            session,
            generation,
            "session registered for capture"
        );
    }

    /// Unregister a session (stop tracking).
    pub async fn unregister_session(&self, repo: &str, task_id: &str) {
        let skey = session_key(repo, task_id);
        if let Some(buffer) = self.buffers.write().await.remove(&skey) {
            tracing::debug!(
                repo = buffer.repo,
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

    /// Return task IDs of sessions that have been silent since registration.
    ///
    /// A session is "silent" when:
    /// 1. It has been registered for longer than `grace_period`
    /// 2. It has NEVER produced any meaningful output (`has_output == false`)
    /// 3. Either it was NEVER confirmed alive, OR it was seen alive but the tmux
    ///    session is now dead (silent exit-0 case)
    ///
    /// Condition (3) balances two cases:
    /// - Session alive and working (tmux session exists, agent running, sparse output)
    ///   → should NOT be killed; hard timeout will catch it if it runs too long (#2318)
    /// - Session was alive but exited silently with no output (tmux session dead)
    ///   → SHOULD trigger silence fallback within grace_period, not wait 30 min (#2573)
    ///
    /// Returns (task_id, session_name, age_secs) for silent sessions.
    pub async fn get_silent_sessions_for_repo(
        &self,
        repo: &str,
        grace_period: std::time::Duration,
    ) -> Vec<(String, String, i64)> {
        let now = Utc::now();
        let buffers = self.buffers.read().await;
        let mut candidates: Vec<OutputBuffer> = buffers
            .values()
            .filter(|buf| buf.repo == repo && !buf.has_output)
            .cloned()
            .collect();
        drop(buffers);

        let mut silent = Vec::new();
        for buf in candidates.drain(..) {
            if buf.seen_alive {
                // Session was confirmed alive at least once. If the tmux session is
                // still running, it's doing real work with sparse output — skip it
                // (the hard stuck_timeout will catch runaway tasks). If the session
                // is dead and produced no output, it's a silent exit — fall through
                // to the grace period check so silence detection fires within 120s
                // instead of waiting the full 30-minute stuck_timeout.
                if !tmux::is_session_dead(&buf.session).await {
                    continue;
                }
                tracing::debug!(
                    task_id = buf.task_id,
                    session = buf.session,
                    "session seen-alive but now dead with no output, treating as silent exit"
                );
            }
            let age = now.signed_duration_since(buf.registered_at);
            if age.num_seconds() > grace_period.as_secs() as i64 {
                silent.push((buf.task_id.clone(), buf.session.clone(), age.num_seconds()));
            }
        }
        silent
    }

    /// Run one tick of the capture loop.
    ///
    /// Acquires the read lock once and iterates over all buffers to avoid
    /// the race condition of collecting keys, dropping the lock, then
    /// re-acquiring to look up each buffer (which could result in missing
    /// or stale entries).
    async fn tick(&self) {
        let buffers = self.buffers.read().await;
        let sessions: Vec<(String, OutputBuffer)> = buffers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        drop(buffers);

        for (skey, buffer) in sessions {
            // Capture pane content directly via tmux
            let current_content = match tmux::capture_pane(&buffer.session).await {
                Ok(s) => s,
                Err(e) => {
                    // Validate current buffer still represents same session instance
                    // before triggering final/unregister side effects. This prevents
                    // stale snapshots from previous registration generations
                    // (e.g., after retry/re-dispatch) from incorrectly terminating
                    // a newly registered session.
                    let current = {
                        let buffers = self.buffers.read().await;
                        buffers.get(&skey).cloned()
                    };

                    // Only fire "session ended" if:
                    // 1. The session still exists in the map
                    // 2. It has the same generation (not re-registered)
                    // 3. It was seen alive before
                    // 4. The tmux session is actually dead
                    let should_finalize = current
                        .as_ref()
                        .map(|c| {
                            c.generation == buffer.generation
                                && c.seen_alive
                                && c.session == buffer.session
                        })
                        .unwrap_or(false);

                    if should_finalize && tmux::is_session_dead(&buffer.session).await {
                        tracing::info!(
                            task_id = buffer.task_id,
                            session = buffer.session,
                            generation = buffer.generation,
                            "session ended, sending final chunk"
                        );
                        let chunk = OutputChunk {
                            content: String::new(),
                            is_final: true,
                        };
                        self.transport
                            .push_output(&buffer.repo, &buffer.task_id, chunk)
                            .await;
                        self.unregister_session(&buffer.repo, &buffer.task_id).await;
                    } else if current.is_none() {
                        tracing::trace!(
                            task_id = buffer.task_id,
                            session = buffer.session,
                            "session already unregistered"
                        );
                    } else {
                        tracing::trace!(
                            task_id = buffer.task_id,
                            session = buffer.session,
                            ?e,
                            "capture failed (transient)"
                        );
                    }
                    continue;
                }
            };

            let new_content = {
                let mut buffers = self.buffers.write().await;
                if let Some(buf) = buffers.get_mut(&skey) {
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
                self.transport
                    .push_output(&buffer.repo, &buffer.task_id, chunk)
                    .await;
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
            while offset > 0 && !current_content.is_char_boundary(offset) {
                offset -= 1;
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
            self.has_output = true;
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

impl CaptureService {
    /// Test helper: mutate buffer fields for a registered session by task_id.
    /// Panics if the session is not registered.
    #[cfg(test)]
    pub(crate) async fn set_buffer_state_for_test(
        &self,
        repo: &str,
        task_id: &str,
        seen_alive: bool,
        has_output: bool,
        registered_at: chrono::DateTime<Utc>,
    ) {
        let skey = crate::channels::transport::session_key(repo, task_id);
        let mut buffers = self.buffers.write().await;
        let buf = buffers
            .get_mut(&skey)
            .unwrap_or_else(|| panic!("no buffer for {repo}/{task_id}"));
        buf.seen_alive = seen_alive;
        buf.has_output = has_output;
        buf.registered_at = registered_at;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::transport::Transport;

    fn make_buffer() -> OutputBuffer {
        OutputBuffer {
            repo: "owner/repo".to_string(),
            session: "test-session".to_string(),
            task_id: "task-1".to_string(),
            last_content: String::new(),
            last_len: 0,
            last_hash: None,
            last_capture: Utc::now(),
            seen_alive: false,
            registered_at: Utc::now(),
            has_output: false,
            generation: 0,
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
    fn has_output_set_on_meaningful_content() {
        let mut buf = make_buffer();
        assert!(!buf.has_output);
        buf.diff_and_update("hello");
        assert!(buf.has_output);
    }

    #[test]
    fn has_output_not_set_on_empty_content() {
        let mut buf = make_buffer();
        buf.diff_and_update("");
        assert!(!buf.has_output, "empty content should not set has_output");
    }

    #[test]
    fn has_output_stays_true_after_silence() {
        let mut buf = make_buffer();
        buf.diff_and_update("hello");
        assert!(buf.has_output);
        // Same content (no new output) — has_output should remain true
        buf.diff_and_update("hello");
        assert!(buf.has_output, "has_output should not revert once set");
    }

    #[tokio::test]
    async fn get_silent_sessions_returns_only_no_output_past_grace() {
        let transport = Arc::new(Transport::new());
        let svc = CaptureService::new(transport);
        let repo = "owner/repo";

        // Register a session and backdate it past the grace period.
        // seen_alive=false (default) + has_output=false → IS silent.
        svc.register_session(repo, "silent-task", "orch-test-silent")
            .await;
        svc.set_buffer_state_for_test(
            repo,
            "silent-task",
            false,
            false,
            Utc::now() - chrono::Duration::seconds(200),
        )
        .await;

        // Register a session that HAS produced output (should not be returned).
        svc.register_session(repo, "active-task", "orch-test-active")
            .await;
        svc.set_buffer_state_for_test(
            repo,
            "active-task",
            false,
            true, // has_output
            Utc::now() - chrono::Duration::seconds(200),
        )
        .await;

        // Register a fresh session within grace period (should not be returned).
        svc.register_session(repo, "new-task", "orch-test-new")
            .await;

        let grace = std::time::Duration::from_secs(120);
        let silent = svc.get_silent_sessions_for_repo(repo, grace).await;
        assert_eq!(silent.len(), 1);
        assert_eq!(silent[0].0, "silent-task");
    }

    /// Regression test for issue #2318 and fix for issue #2573.
    ///
    /// A session confirmed alive (capture_pane succeeded at least once) with no
    /// terminal output falls into two cases:
    ///
    /// - **Tmux session dead, no output** (silent exit-0, issue #2573): IS silent.
    ///   The agent exited immediately without printing anything. Should be detected
    ///   within the grace period, not wait 30 minutes for stuck_timeout.
    ///
    /// - **Tmux session alive, no output** (issue #2318): NOT silent.
    ///   Claude agents doing complex refactoring can run 15+ minutes with sparse
    ///   terminal output (writing files via Edit/Write tools). Hard timeout handles them.
    ///
    /// In tests, tmux sessions don't exist, so `is_session_dead` returns `true` for
    /// both sessions below — both "never-seen" and "seen-but-dead" are correctly
    /// classified as silent exits.
    #[tokio::test]
    async fn get_silent_sessions_seen_alive_dead_session_is_silent() {
        let transport = Arc::new(Transport::new());
        let svc = CaptureService::new(transport);
        let repo = "owner/repo";

        // Session past grace period, never seen alive → IS silent.
        svc.register_session(repo, "never-seen-task", "orch-never-seen")
            .await;
        svc.set_buffer_state_for_test(
            repo,
            "never-seen-task",
            false, // seen_alive = false (default)
            false, // has_output = false
            Utc::now() - chrono::Duration::seconds(500),
        )
        .await;

        // Session past grace period, seen alive, no output, tmux session dead
        // (simulates silent exit-0 from issue #2573) → IS silent.
        // The session name ends with "-dead" to work with both the integration test
        // mock (which returns `is_session_dead=true` for "-dead" names) and the unit
        // test context (where no real tmux session by that name exists → dead).
        svc.register_session(repo, "seen-but-dead-task", "orch-seen-dead")
            .await;
        svc.set_buffer_state_for_test(
            repo,
            "seen-but-dead-task",
            true,  // seen_alive: capture_pane succeeded at least once
            false, // has_output = false, tmux session no longer exists → silent exit
            Utc::now() - chrono::Duration::seconds(500),
        )
        .await;

        let grace = std::time::Duration::from_secs(120);
        let silent = svc.get_silent_sessions_for_repo(repo, grace).await;
        // Both sessions should be returned: "never-seen-task" was never alive, and
        // "seen-but-dead-task" was alive but the tmux session is now dead with no output.
        assert_eq!(
            silent.len(),
            2,
            "both never-alive and dead-with-no-output sessions must be silenced"
        );
        let ids: std::collections::HashSet<&str> =
            silent.iter().map(|(id, _, _)| id.as_str()).collect();
        assert!(ids.contains("never-seen-task"));
        assert!(ids.contains("seen-but-dead-task"));
    }

    #[tokio::test]
    async fn get_silent_sessions_filters_by_repo() {
        let transport = Arc::new(Transport::new());
        let svc = CaptureService::new(transport);

        svc.register_session("owner/repo-a", "task-a", "orch-a")
            .await;
        svc.register_session("owner/repo-b", "task-b", "orch-b")
            .await;
        svc.set_buffer_state_for_test(
            "owner/repo-a",
            "task-a",
            false,
            false,
            Utc::now() - chrono::Duration::seconds(200),
        )
        .await;
        svc.set_buffer_state_for_test(
            "owner/repo-b",
            "task-b",
            false,
            false,
            Utc::now() - chrono::Duration::seconds(200),
        )
        .await;

        let grace = std::time::Duration::from_secs(120);
        let silent_a = svc
            .get_silent_sessions_for_repo("owner/repo-a", grace)
            .await;
        assert_eq!(silent_a.len(), 1);
        assert_eq!(silent_a[0].0, "task-a");

        let silent_b = svc
            .get_silent_sessions_for_repo("owner/repo-b", grace)
            .await;
        assert_eq!(silent_b.len(), 1);
        assert_eq!(silent_b[0].0, "task-b");
    }

    /// Same external task ID in two repos must not collide in capture buffers.
    #[tokio::test]
    async fn same_task_id_different_repos_capture_buffers_isolated() {
        let transport = Arc::new(Transport::new());
        let svc = CaptureService::new(transport);

        svc.register_session("owner/repo-a", "42", "orch-a-42")
            .await;
        svc.register_session("owner/repo-b", "42", "orch-b-42")
            .await;

        let skey_a = session_key("owner/repo-a", "42");
        let skey_b = session_key("owner/repo-b", "42");

        let buffers = svc.buffers.read().await;
        let buf_a = buffers
            .get(&skey_a)
            .expect("buffer should exist for repo-a session");
        let buf_b = buffers
            .get(&skey_b)
            .expect("buffer should exist for repo-b session");

        assert_eq!(buf_a.session, "orch-a-42");
        assert_eq!(buf_b.session, "orch-b-42");
        assert_ne!(buf_a.session, buf_b.session);
        assert_eq!(buf_a.repo, "owner/repo-a");
        assert_eq!(buf_b.repo, "owner/repo-b");
    }

    #[test]
    fn multibyte_boundary_no_bytes_dropped() {
        // "Hello " is 6 bytes. Append a 3-byte CJK character (世, U+4E16).
        // Simulate last_len landing in the middle of the 3-byte sequence.
        let mut buf = make_buffer();
        buf.diff_and_update("Hello ");
        // Full string with CJK appended
        let full = "Hello 世界";
        let result = buf.diff_and_update(full);
        // The new suffix must contain both CJK characters intact.
        let s = result.expect("should return new content");
        assert!(s.contains('世'), "first CJK char must not be dropped");
        assert!(s.contains('界'), "second CJK char must not be dropped");
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
