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
use tokio::sync::{broadcast, Mutex, RwLock};

/// A live connection between a channel thread and a tmux session.
#[derive(Debug, Clone)]
pub struct SessionBinding {
    /// The task ID (issue number)
    pub task_id: String,
    /// tmux session name (e.g. "orch-42")
    pub tmux_session: String,
    /// Channel threads connected to this session.
    /// Key: "channel:thread_id" (e.g. "telegram:12345", "github:42")
    pub connected_threads: Vec<String>,
    /// Broadcast sender for output streaming
    pub output_tx: broadcast::Sender<OutputChunk>,
}

/// The transport layer manages all session bindings and routes messages.
pub struct Transport {
    /// Active session bindings, keyed by task_id
    bindings: Arc<RwLock<HashMap<String, SessionBinding>>>,
    /// Reverse lookup: "channel:thread_id" → task_id
    thread_to_task: Arc<RwLock<HashMap<String, String>>>,
    /// The main chat session (for direct orchestrator commands)
    main_session: Arc<Mutex<Option<String>>>,
    /// Broadcast sender for task completion notifications
    notification_tx: broadcast::Sender<TaskNotification>,
}

impl Transport {
    pub fn new() -> Self {
        let (notification_tx, _) = broadcast::channel(64);
        Self {
            bindings: Arc::new(RwLock::new(HashMap::new())),
            thread_to_task: Arc::new(RwLock::new(HashMap::new())),
            main_session: Arc::new(Mutex::new(None)),
            notification_tx,
        }
    }

    /// Bind a channel thread to a task's tmux session.
    pub async fn bind(&self, task_id: &str, tmux_session: &str, channel: &str, thread_id: &str) {
        let key = format!("{channel}:{thread_id}");
        let mut bindings = self.bindings.write().await;
        let binding = bindings.entry(task_id.to_string()).or_insert_with(|| {
            let (tx, _) = broadcast::channel(256);
            SessionBinding {
                task_id: task_id.to_string(),
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
        self.thread_to_task
            .write()
            .await
            .insert(key, task_id.to_string());
    }

    /// Unbind a channel thread from its task session.
    pub async fn unbind(&self, channel: &str, thread_id: &str) {
        let key = format!("{channel}:{thread_id}");
        let task_id = self.thread_to_task.write().await.remove(&key);
        if let Some(task_id) = task_id {
            let mut bindings = self.bindings.write().await;
            if let Some(binding) = bindings.get_mut(&task_id) {
                binding.connected_threads.retain(|t| t != &key);
                if binding.connected_threads.is_empty() {
                    bindings.remove(&task_id);
                }
            }
        }
    }

    /// Get the broadcast receiver for a task's output stream.
    pub async fn subscribe(&self, task_id: &str) -> Option<broadcast::Receiver<OutputChunk>> {
        let bindings = self.bindings.read().await;
        bindings.get(task_id).map(|b| b.output_tx.subscribe())
    }

    /// Push an output chunk to all subscribers of a task.
    pub async fn push_output(&self, task_id: &str, chunk: OutputChunk) {
        let bindings = self.bindings.read().await;
        if let Some(binding) = bindings.get(task_id) {
            // Ignore send errors (no active receivers)
            let _ = binding.output_tx.send(chunk);
        }
    }

    /// Route an incoming message to the appropriate handler.
    /// Returns the task_id if this message maps to an existing session.
    pub async fn route(&self, msg: &IncomingMessage) -> MessageRoute {
        let key = format!("{}:{}", msg.channel, msg.thread_id);

        // Check if this thread is bound to a task
        if let Some(task_id) = self.thread_to_task.read().await.get(&key) {
            return MessageRoute::TaskSession {
                task_id: task_id.clone(),
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

    /// Get the tmux session name for a task.
    pub async fn tmux_session_for(&self, task_id: &str) -> Option<String> {
        let bindings = self.bindings.read().await;
        bindings.get(task_id).map(|b| b.tmux_session.clone())
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

/// How an incoming message should be handled.
#[derive(Debug)]
pub enum MessageRoute {
    /// Message belongs to an existing task session — forward to tmux
    TaskSession { task_id: String },
    /// Message is a command (e.g. "/status", "orch task list")
    Command { raw: String },
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
        });

        let n1 = rx1.recv().await.unwrap();
        let n2 = rx2.recv().await.unwrap();
        assert_eq!(n1.task_id, "1");
        assert_eq!(n2.task_id, "1");
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
        });
    }

    #[tokio::test]
    async fn bind_and_route_to_session() {
        let transport = Transport::new();
        transport.bind("42", "orch-42", "telegram", "12345").await;

        let msg = IncomingMessage {
            channel: "telegram".to_string(),
            id: "msg1".to_string(),
            thread_id: "12345".to_string(),
            author: "user".to_string(),
            body: "hello".to_string(),
            timestamp: chrono::Utc::now(),
            metadata: serde_json::json!({}),
        };

        match transport.route(&msg).await {
            MessageRoute::TaskSession { task_id } => assert_eq!(task_id, "42"),
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
        };

        match transport.route(&msg).await {
            MessageRoute::Command { raw } => assert_eq!(raw, "/status"),
            _ => panic!("expected Command"),
        }
    }
}
