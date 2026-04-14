//! Transport layer — connects channels to tmux agent sessions.
//!
//! The transport is the glue between external channels and local tmux sessions.
//! When a user sends a message on Telegram, Discord, or GitHub, the transport:
//! 1. Routes it to the correct tmux session (or creates a task)
//! 2. Captures agent output from tmux and broadcasts to all connected channels
//!
//! Architecture:
//!
//!   User (Telegram) ─────┐
//!   User (Discord) ──────┤
//!   User (GitHub Issue) ─┤
//!                        ▼
//!                  ┌───────────┐
//!                  │ Transport │  ← routes messages, manages sessions
//!                  └─────┬─────┘
//!                        │
//!          ┌─────────────┼─────────────┐
//!          ▼             ▼             ▼
//!   tmux:orch-42   tmux:orch-43   tmux:main
//!   (task agent)   (task agent)   (chat session)
//!          │             │             │
//!          └─────────────┼─────────────┘
//!                        ▼
//!                  ┌───────────┐
//!                  │ Broadcast │  ← fans out output to all connected channels
//!                  └───────────┘

use super::notification::TaskNotification;
use super::{IncomingMessage, OutputChunk};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

const MAX_OUTPUT_CHUNK_BYTES: usize = 64 * 1024;

/// Build a globally unique session key from `(repo, task_id)`.
///
/// External task IDs (e.g. `"42"` for GitHub issue #42) are only unique
/// within a repo. Internal tasks (`"internal:<id>"`) are globally unique.
/// This function prefixes external task IDs with the repo slug to avoid
/// collisions across repos while keeping internal task keys unchanged.
pub fn session_key(repo: &str, task_id: &str) -> String {
    if task_id.starts_with("internal:") {
        task_id.to_string()
    } else {
        format!("{repo}:{task_id}")
    }
}

/// Parse a session key created by [`session_key`] back into `(repo, task_id)`.
///
/// Returns `None` if the key is malformed.
pub fn parse_session_key(key: &str) -> Option<(&str, &str)> {
    if key.starts_with("internal:") {
        // Internal tasks don't have a repo component in the key.
        // Callers that need repo must track it separately.
        None
    } else {
        // Format is "repo:task_id" — repo itself may contain "/" but not ":"
        // The first ":" separates repo from task_id.
        let (repo, task_id) = key.split_once(':')?;
        Some((repo, task_id))
    }
}

/// A live connection between a channel thread and a tmux session.
#[derive(Debug, Clone)]
pub struct SessionBinding {
    /// tmux session name (e.g. "orch-myproject-42")
    pub tmux_session: String,
    /// Channel threads connected to this session.
    ///
    /// Each entry is a canonical conversation key produced by
    /// [`conversation_key`].  The format is either:
    ///   - `"channel:thread_id"` (no topic), or
    ///   - `"channel:thread_id|topic_id"` (with topic/thread)
    ///
    /// Use [`parse_conversation_key`] to decompose a key back into its parts.
    pub connected_threads: Vec<String>,
    /// Broadcast sender for output streaming
    pub output_tx: broadcast::Sender<OutputChunk>,
}

/// Build a canonical conversation key that is unique per topic/thread when
/// `topic_id` is present, and falls back to `channel:thread_id` otherwise.
///
/// Telegram and Discord encode the forum-topic / thread identity in `topic_id`
/// while `thread_id` carries the parent chat / channel id.  Two different topics
/// inside the same parent would share the same `"channel:thread_id"` key —
/// causing bindings to collide.  Including `topic_id` in the key avoids that.
///
/// `|` is used as the topic separator because it cannot appear in Telegram or
/// Discord snowflake IDs (they are numeric / alphanumeric).
pub fn conversation_key(channel: &str, thread_id: &str, topic_id: Option<&str>) -> String {
    match topic_id {
        Some(tid) if !tid.is_empty() => format!("{channel}:{thread_id}|{tid}"),
        _ => format!("{channel}:{thread_id}"),
    }
}

/// Decompose a conversation key created by [`conversation_key`] back into
/// `(channel, thread_id, topic_id)`.
///
/// Returns `None` if the key is malformed (no `':'` separator).
pub fn parse_conversation_key(key: &str) -> Option<(&str, &str, Option<&str>)> {
    let (channel, rest) = key.split_once(':')?;
    let (thread_id, topic_id) = match rest.split_once('|') {
        Some((tid, topic)) => (tid, Some(topic)),
        None => (rest, None),
    };
    Some((channel, thread_id, topic_id))
}

/// The transport layer manages all session bindings and routes messages.
pub struct Transport {
    /// Active session bindings, keyed by session_key(repo, task_id)
    bindings: Arc<RwLock<HashMap<String, SessionBinding>>>,
    /// Reverse lookup: conversation_key → session_key(repo, task_id)
    thread_to_task: Arc<RwLock<HashMap<String, String>>>,
    /// Broadcast sender for task completion notifications
    notification_tx: broadcast::Sender<TaskNotification>,
}

impl Transport {
    pub fn new() -> Self {
        let (notification_tx, _) = broadcast::channel(64);
        Self {
            bindings: Arc::new(RwLock::new(HashMap::new())),
            thread_to_task: Arc::new(RwLock::new(HashMap::new())),
            notification_tx,
        }
    }

    /// Bind a channel thread to a task's tmux session.
    ///
    /// `repo` is required to build a globally unique session key, preventing
    /// collisions when multiple repos share the same external task ID.
    ///
    /// `topic_id` should be set for topic-aware channels (Telegram forum topics,
    /// Discord threads) so that bindings for different topics inside the same
    /// parent chat / channel do not collide.
    pub async fn bind(
        &self,
        repo: &str,
        task_id: &str,
        tmux_session: &str,
        channel: &str,
        thread_id: &str,
        topic_id: Option<&str>,
    ) {
        let key = conversation_key(channel, thread_id, topic_id);
        let skey = session_key(repo, task_id);
        // Update bindings synchronously (no await while lock held)
        {
            let mut bindings = self.bindings.write().await;
            let binding = bindings.entry(skey.clone()).or_insert_with(|| {
                let (tx, _) = broadcast::channel(256);
                SessionBinding {
                    tmux_session: tmux_session.to_string(),
                    connected_threads: Vec::new(),
                    output_tx: tx,
                }
            });
            // Always update tmux_session — task retries get a new session
            binding.tmux_session = tmux_session.to_string();
            if !binding.connected_threads.contains(&key) {
                binding.connected_threads.push(key.clone());
            }
        } // bindings lock released here
          // Update thread_to_task after releasing bindings lock
        self.thread_to_task.write().await.insert(key, skey);
    }

    /// Get the broadcast receiver for a task's output stream.
    pub async fn subscribe(
        &self,
        repo: &str,
        task_id: &str,
    ) -> Option<broadcast::Receiver<OutputChunk>> {
        let skey = session_key(repo, task_id);
        let bindings = self.bindings.read().await;
        bindings.get(&skey).map(|b| b.output_tx.subscribe())
    }

    /// Push an output chunk to all subscribers of a task.
    pub async fn push_output(&self, repo: &str, task_id: &str, chunk: OutputChunk) {
        let skey = session_key(repo, task_id);
        if chunk.content.is_empty() {
            let bindings = self.bindings.read().await;
            if let Some(binding) = bindings.get(&skey) {
                let _ = binding.output_tx.send(chunk.clone());
            }
            return;
        }

        let parts = split_chunks(&chunk.content, MAX_OUTPUT_CHUNK_BYTES);
        let last_index = parts.len().saturating_sub(1);

        for (idx, part) in parts.into_iter().enumerate() {
            let part_chunk = OutputChunk {
                content: part,
                is_final: chunk.is_final && idx == last_index,
            };
            let bindings = self.bindings.read().await;
            if let Some(binding) = bindings.get(&skey) {
                let _ = binding.output_tx.send(part_chunk.clone());
            }
        }
    }

    /// Get the session binding for a specific task, if any.
    pub async fn get_binding(&self, repo: &str, task_id: &str) -> Option<SessionBinding> {
        let skey = session_key(repo, task_id);
        self.bindings.read().await.get(&skey).cloned()
    }

    /// Route an incoming message to the appropriate handler.
    /// Returns the session_key if this message maps to an existing session.
    pub async fn route(&self, msg: &IncomingMessage) -> MessageRoute {
        let key = conversation_key(&msg.channel, &msg.thread_id, msg.topic_id.as_deref());

        // Check if this thread is bound to a task
        if let Some(session_key) = self.thread_to_task.read().await.get(&key) {
            return MessageRoute::TaskSession {
                session_key: session_key.clone(),
            };
        }

        // Check if this looks like a command
        let body = msg.body.trim();
        if body.starts_with('/') || body.starts_with("orch ") {
            return MessageRoute::Command {
                raw: body.to_string(),
            };
        }

        // New conversation — could become a task
        MessageRoute::NewTask
    }

    /// Unbind a task session, removing all associated entries.
    ///
    /// Removes the entry from `bindings`, all reverse-lookup entries from
    /// `thread_to_task` whose value matches the session key, and clears any
    /// cached output for the session.  Call this when a task session ends so
    /// memory does not grow indefinitely.
    pub async fn unbind(&self, repo: &str, task_id: &str) {
        let skey = session_key(repo, task_id);

        // Remove the binding and collect its connected threads.
        let connected_threads = {
            let mut bindings = self.bindings.write().await;
            bindings
                .remove(&skey)
                .map(|b| b.connected_threads)
                .unwrap_or_default()
        };

        // Remove reverse-lookup entries for all connected threads.
        if !connected_threads.is_empty() {
            let mut t2t = self.thread_to_task.write().await;
            for key in &connected_threads {
                t2t.remove(key);
            }
        }

        // Clear any cached output.
        self.clear_output(&skey).await;
    }

    /// List all active bindings.
    pub async fn active_sessions(&self) -> Vec<SessionBinding> {
        self.bindings.read().await.values().cloned().collect()
    }

    /// Push a task completion notification to all subscribers.
    pub fn push_notification(&self, notification: TaskNotification) {
        // Ignore send errors (no active receivers)
        let _ = self.notification_tx.send(notification);
    }

    /// Subscribe to task completion notifications.
    pub fn subscribe_notifications(&self) -> broadcast::Receiver<TaskNotification> {
        self.notification_tx.subscribe()
    }
}

fn split_chunks(content: &str, max_bytes: usize) -> Vec<String> {
    if content.len() <= max_bytes {
        return vec![content.to_string()];
    }

    let mut chunks = Vec::new();
    let mut start = 0;
    let total = content.len();

    while start < total {
        let mut end = (start + max_bytes).min(total);
        while end > start && !content.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            // Single char exceeds limit — advance past the full character
            if let Some(ch) = content[start..].chars().next() {
                end = start + ch.len_utf8();
            } else {
                break;
            }
        }
        chunks.push(content[start..end].to_string());
        start = end;
    }

    chunks
}

/// How an incoming message should be handled.
#[derive(Debug)]
pub enum MessageRoute {
    /// Message belongs to an existing task session — forward to tmux.
    /// `session_key` is the globally unique key (repo:task_id or internal:id).
    TaskSession { session_key: String },
    /// Message is a command (e.g. "/status", "orch task list")
    Command { raw: String },
    /// Message is in the configured control session channel — route to control agent
    ControlSession,
    /// Message is new — should create a task
    NewTask,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn push_notification_reaches_subscriber() {
        let transport = Transport::new();
        let mut rx = transport.subscribe_notifications();

        let notification = TaskNotification {
            task_id: "42".to_string(),
            title: "Test task".to_string(),
            status: "done".to_string(),
            agent: "claude".to_string(),
            duration_seconds: 60.0,
            summary: "Completed successfully".to_string(),
            repo: None,
            notify_target: None,
        };

        transport.push_notification(notification.clone());

        let received = rx.recv().await.unwrap();
        assert_eq!(received.task_id, "42");
        assert_eq!(received.status, "done");
        assert_eq!(received.agent, "claude");
        assert_eq!(received.summary, "Completed successfully");
    }

    #[tokio::test]
    async fn push_notification_multiple_subscribers() {
        let transport = Transport::new();
        let mut rx1 = transport.subscribe_notifications();
        let mut rx2 = transport.subscribe_notifications();

        transport.push_notification(TaskNotification {
            task_id: "1".to_string(),
            title: "Task".to_string(),
            status: "done".to_string(),
            agent: "codex".to_string(),
            duration_seconds: 10.0,
            summary: "Done".to_string(),
            repo: None,
            notify_target: None,
        });

        let n1 = rx1.recv().await.unwrap();
        let n2 = rx2.recv().await.unwrap();
        assert_eq!(n1.task_id, "1");
        assert_eq!(n2.task_id, "1");
    }

    #[test]
    fn split_chunks_multibyte_at_boundary_does_not_panic() {
        // 4-byte emoji at the chunk boundary: the old code set end = start + 1
        // and then sliced content[start..end], which panics on multi-byte chars.
        let content = "x".repeat(97) + "😀"; // 101 bytes; with max=100 the cut is at byte 100 (mid-emoji)
        let chunks = split_chunks(&content, 100);
        assert_eq!(chunks.join(""), content);
        for c in &chunks {
            assert!(std::str::from_utf8(c.as_bytes()).is_ok());
        }
    }

    #[test]
    fn split_chunks_only_emojis() {
        // Every boundary falls inside a 4-byte character.
        let content = "😀".repeat(50); // 200 bytes
        let chunks = split_chunks(&content, 10); // 10 bytes per chunk, emoji is 4 bytes
        assert_eq!(chunks.join(""), content);
        for c in &chunks {
            assert!(std::str::from_utf8(c.as_bytes()).is_ok());
        }
    }

    #[test]
    fn push_notification_no_subscribers_does_not_panic() {
        let transport = Transport::new();
        // No subscribers — should not panic
        transport.push_notification(TaskNotification {
            task_id: "1".to_string(),
            title: "Task".to_string(),
            status: "done".to_string(),
            agent: "claude".to_string(),
            duration_seconds: 0.0,
            summary: "Done".to_string(),
            repo: None,
            notify_target: None,
        });
    }

    #[tokio::test]
    async fn bind_and_route_to_session() {
        let transport = Transport::new();
        transport
            .bind(
                "owner/repo",
                "42",
                "orch-myproject-42",
                "telegram",
                "12345",
                None,
            )
            .await;

        let msg = IncomingMessage {
            channel: "telegram".to_string(),
            id: "msg1".to_string(),
            thread_id: "12345".to_string(),
            author: "user".to_string(),
            body: "hello".to_string(),
            timestamp: chrono::Utc::now(),
            metadata: serde_json::json!({}),
            topic_id: None,
        };

        match transport.route(&msg).await {
            MessageRoute::TaskSession { session_key } => {
                assert_eq!(session_key, "owner/repo:42");
            }
            _ => panic!("expected TaskSession"),
        }
    }

    #[tokio::test]
    async fn route_command() {
        let transport = Transport::new();

        let msg = IncomingMessage {
            channel: "telegram".to_string(),
            id: "msg1".to_string(),
            thread_id: "99".to_string(),
            author: "user".to_string(),
            body: "/status".to_string(),
            timestamp: chrono::Utc::now(),
            metadata: serde_json::json!({}),
            topic_id: None,
        };

        match transport.route(&msg).await {
            MessageRoute::Command { raw } => assert_eq!(raw, "/status"),
            _ => panic!("expected Command"),
        }
    }

    // ── session_key ──────────────────────────────────────────────────────────

    #[test]
    fn session_key_external_is_prefixed_with_repo() {
        assert_eq!(session_key("owner/repo", "42"), "owner/repo:42");
    }

    #[test]
    fn session_key_internal_is_unchanged() {
        assert_eq!(session_key("owner/repo", "internal:99"), "internal:99");
    }

    #[test]
    fn session_key_same_id_different_repos_are_unique() {
        let k1 = session_key("owner/repo-a", "42");
        let k2 = session_key("owner/repo-b", "42");
        assert_ne!(k1, k2);
        assert_eq!(k1, "owner/repo-a:42");
        assert_eq!(k2, "owner/repo-b:42");
    }

    // ── cross-repo collision regression ──────────────────────────────────────

    /// Two repos with the same external task ID must not collide in bindings.
    #[tokio::test]
    async fn same_external_id_different_repos_do_not_collide() {
        let transport = Transport::new();

        // Repo A binds task "42"
        transport
            .bind(
                "owner/repo-a",
                "42",
                "orch-repo-a-42",
                "telegram",
                "111",
                None,
            )
            .await;
        // Repo B binds task "42" (same external ID, different repo)
        transport
            .bind(
                "owner/repo-b",
                "42",
                "orch-repo-b-42",
                "telegram",
                "222",
                None,
            )
            .await;

        // Each binding has its own session
        let binding_a = transport.get_binding("owner/repo-a", "42").await.unwrap();
        let binding_b = transport.get_binding("owner/repo-b", "42").await.unwrap();
        assert_eq!(binding_a.tmux_session, "orch-repo-a-42");
        assert_eq!(binding_b.tmux_session, "orch-repo-b-42");
        assert_ne!(binding_a.tmux_session, binding_b.tmux_session);
    }

    /// Same external task ID in two repos: pushing output to one must not
    /// reach the other's subscribers.
    #[tokio::test]
    async fn push_output_isolated_across_repos_with_same_task_id() {
        let transport = Transport::new();

        // Subscribe to repo-a's task 42
        transport
            .bind("owner/repo-a", "42", "orch-a-42", "cli", "stream-a", None)
            .await;
        let mut rx_a = transport.subscribe("owner/repo-a", "42").await.unwrap();

        // Subscribe to repo-b's task 42
        transport
            .bind("owner/repo-b", "42", "orch-b-42", "cli", "stream-b", None)
            .await;
        let mut rx_b = transport.subscribe("owner/repo-b", "42").await.unwrap();

        // Push output to repo-a's task 42
        transport
            .push_output(
                "owner/repo-a",
                "42",
                OutputChunk {
                    content: "hello from repo-a".to_string(),
                    is_final: false,
                },
            )
            .await;

        // repo-a receives it
        let chunk_a = tokio::time::timeout(std::time::Duration::from_millis(100), rx_a.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(chunk_a.content, "hello from repo-a");

        // repo-b should NOT receive anything (timeout)
        let result_b =
            tokio::time::timeout(std::time::Duration::from_millis(100), rx_b.recv()).await;
        assert!(
            result_b.is_err(),
            "repo-b should not receive repo-a's output"
        );
    }

    // ── conversation_key ──────────────────────────────────────────────────────

    #[test]
    fn conversation_key_without_topic() {
        let key = conversation_key("telegram", "12345", None);
        assert_eq!(key, "telegram:12345");
    }

    #[test]
    fn conversation_key_with_topic() {
        let key = conversation_key("telegram", "12345", Some("678"));
        assert_eq!(key, "telegram:12345|678");
    }

    #[test]
    fn conversation_key_empty_topic_treated_as_none() {
        // An empty topic_id should produce the same key as None.
        let key_empty = conversation_key("discord", "abc", Some(""));
        let key_none = conversation_key("discord", "abc", None);
        assert_eq!(key_empty, key_none);
    }

    // ── parse_conversation_key ────────────────────────────────────────────────

    #[test]
    fn parse_key_without_topic() {
        let (channel, thread_id, topic_id) =
            parse_conversation_key("telegram:12345").expect("valid key");
        assert_eq!(channel, "telegram");
        assert_eq!(thread_id, "12345");
        assert!(topic_id.is_none());
    }

    #[test]
    fn parse_key_with_topic() {
        let (channel, thread_id, topic_id) =
            parse_conversation_key("telegram:12345|678").expect("valid key");
        assert_eq!(channel, "telegram");
        assert_eq!(thread_id, "12345");
        assert_eq!(topic_id, Some("678"));
    }

    #[test]
    fn parse_key_malformed_returns_none() {
        assert!(parse_conversation_key("no-colon-here").is_none());
    }

    #[test]
    fn conversation_key_round_trips() {
        // Build a key and parse it back; values must match what went in.
        for (ch, tid, topic) in [
            ("telegram", "111", Some("222")),
            ("discord", "abc", None),
            ("slack", "x", Some("y")),
        ] {
            let key = conversation_key(ch, tid, topic);
            let (ch2, tid2, topic2) = parse_conversation_key(&key).expect("round-trip valid");
            assert_eq!(ch2, ch);
            assert_eq!(tid2, tid);
            assert_eq!(topic2, topic);
        }
    }

    // ── topic collision prevention ────────────────────────────────────────────

    /// Two different Telegram forum topics inside the same chat must bind to
    /// different keys and not collide.
    #[tokio::test]
    async fn different_topics_do_not_collide() {
        let transport = Transport::new();

        // Task 10 is in topic 100 of chat 999
        transport
            .bind(
                "owner/repo",
                "10",
                "orch-proj-10",
                "telegram",
                "999",
                Some("100"),
            )
            .await;
        // Task 20 is in topic 200 of the same chat 999
        transport
            .bind(
                "owner/repo",
                "20",
                "orch-proj-20",
                "telegram",
                "999",
                Some("200"),
            )
            .await;

        let msg_topic_100 = IncomingMessage {
            channel: "telegram".to_string(),
            id: "m1".to_string(),
            thread_id: "999".to_string(),
            author: "user".to_string(),
            body: "reply for task 10".to_string(),
            timestamp: chrono::Utc::now(),
            metadata: serde_json::json!({}),
            topic_id: Some("100".to_string()),
        };

        let msg_topic_200 = IncomingMessage {
            channel: "telegram".to_string(),
            id: "m2".to_string(),
            thread_id: "999".to_string(),
            author: "user".to_string(),
            body: "reply for task 20".to_string(),
            timestamp: chrono::Utc::now(),
            metadata: serde_json::json!({}),
            topic_id: Some("200".to_string()),
        };

        match transport.route(&msg_topic_100).await {
            MessageRoute::TaskSession { session_key } => {
                assert_eq!(session_key, "owner/repo:10");
            }
            other => panic!("expected TaskSession for task 10, got {other:?}"),
        }

        match transport.route(&msg_topic_200).await {
            MessageRoute::TaskSession { session_key } => {
                assert_eq!(session_key, "owner/repo:20");
            }
            other => panic!("expected TaskSession for task 20, got {other:?}"),
        }
    }

    // ── unbind ────────────────────────────────────────────────────────────────

    /// After unbinding, the binding and reverse-lookup entries are removed.
    #[tokio::test]
    async fn unbind_removes_binding_and_reverse_lookup() {
        let transport = Transport::new();
        transport
            .bind("owner/repo", "99", "orch-proj-99", "telegram", "555", None)
            .await;

        // Verify bound
        assert!(transport.get_binding("owner/repo", "99").await.is_some());

        // Unbind
        transport.unbind("owner/repo", "99").await;

        // Binding gone
        assert!(transport.get_binding("owner/repo", "99").await.is_none());

        // Reverse lookup gone — message should not route to the old session
        let msg = IncomingMessage {
            channel: "telegram".to_string(),
            id: "m1".to_string(),
            thread_id: "555".to_string(),
            author: "user".to_string(),
            body: "hello".to_string(),
            timestamp: chrono::Utc::now(),
            metadata: serde_json::json!({}),
            topic_id: None,
        };
        match transport.route(&msg).await {
            MessageRoute::NewTask => {} // correct
            other => panic!("expected NewTask after unbind, got {other:?}"),
        }
    }

    /// Unbinding a session that was never bound must not panic.
    #[tokio::test]
    async fn unbind_nonexistent_does_not_panic() {
        let transport = Transport::new();
        // Should complete without error
        transport.unbind("owner/repo", "nonexistent").await;
    }

    /// A message arriving in a *different* topic must not be routed to a task
    /// that was bound to another topic in the same chat.
    #[tokio::test]
    async fn wrong_topic_does_not_route_to_bound_task() {
        let transport = Transport::new();

        // Task 5 bound to topic 10 of chat 42
        transport
            .bind(
                "owner/repo",
                "5",
                "orch-proj-5",
                "telegram",
                "42",
                Some("10"),
            )
            .await;

        // Message arrives in topic 99 of the same chat — should NOT route to task 5
        let msg = IncomingMessage {
            channel: "telegram".to_string(),
            id: "m1".to_string(),
            thread_id: "42".to_string(),
            author: "user".to_string(),
            body: "hello from a different topic".to_string(),
            timestamp: chrono::Utc::now(),
            metadata: serde_json::json!({}),
            topic_id: Some("99".to_string()),
        };

        match transport.route(&msg).await {
            MessageRoute::NewTask => {} // correct — not routed to task 5
            MessageRoute::TaskSession { session_key } => {
                panic!("should not have routed to session {session_key}")
            }
            other => panic!("unexpected route result: {other:?}"),
        }
    }
}
