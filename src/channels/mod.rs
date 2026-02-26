//! Channel abstraction — the core trait for all I/O surfaces.
//!
//! Every external interface (GitHub, Telegram, Discord, tmux) implements
//! the `Channel` trait. This gives the engine a uniform way to:
//! - Receive commands/messages
//! - Send task updates and agent output
//! - Stream real-time output from agent sessions
//!
//! Channels are bidirectional and async. The engine doesn't care whether
//! a message came from a Telegram DM, a GitHub issue comment, or a
//! Discord thread — it processes the same `IncomingMessage` type.

pub mod capture;
pub mod discord;
pub mod github;
pub mod notification;
pub mod telegram;
pub mod tmux;
pub mod transport;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// A message received from any channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncomingMessage {
    /// Channel that produced this message (e.g. "github", "telegram", "discord")
    pub channel: String,
    /// Unique ID within the channel (comment ID, message ID, etc.)
    pub id: String,
    /// Thread/conversation context (issue number, chat ID, channel ID)
    pub thread_id: String,
    /// Author identifier
    pub author: String,
    /// Raw text content
    pub body: String,
    /// When the message was created
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Optional metadata (labels, attachments, etc.)
    #[serde(default)]
    pub metadata: serde_json::Value,
}

/// A message to send to a channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutgoingMessage {
    /// Target thread/conversation
    pub thread_id: String,
    /// Message body (markdown)
    pub body: String,
    /// Optional: reply to a specific message ID
    pub reply_to: Option<String>,
    /// Optional metadata for channel-specific features
    #[serde(default)]
    pub metadata: serde_json::Value,
}

/// An agent output chunk for streaming.
#[derive(Debug, Clone)]
pub struct OutputChunk {
    pub task_id: String,
    pub content: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub is_final: bool,
}

/// The core channel trait. All external interfaces implement this.
#[async_trait]
pub trait Channel: Send + Sync {
    /// Human-readable channel name.
    fn name(&self) -> &str;

    /// Start listening for incoming messages.
    /// Returns a receiver that the engine polls.
    async fn start(&self) -> anyhow::Result<tokio::sync::mpsc::Receiver<IncomingMessage>>;

    /// Send a message to a thread/conversation.
    async fn send(&self, msg: &OutgoingMessage) -> anyhow::Result<()>;

    /// Stream real-time agent output to a thread.
    /// The channel reads from the broadcast receiver and forwards to the thread.
    async fn stream_output(
        &self,
        thread_id: &str,
        rx: broadcast::Receiver<OutputChunk>,
    ) -> anyhow::Result<()>;

    /// Check if this channel is healthy/connected.
    async fn health_check(&self) -> anyhow::Result<()>;

    /// Graceful shutdown.
    async fn shutdown(&self) -> anyhow::Result<()>;
}

/// Registry of active channels.
pub struct ChannelRegistry {
    channels: Vec<Box<dyn Channel>>,
}

impl ChannelRegistry {
    pub fn new() -> Self {
        Self {
            channels: Vec::new(),
        }
    }

    pub fn register(&mut self, channel: Box<dyn Channel>) {
        tracing::info!(channel = channel.name(), "registered channel");
        self.channels.push(channel);
    }

    pub fn iter(&self) -> impl Iterator<Item = &dyn Channel> {
        self.channels.iter().map(|c| c.as_ref())
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Box<dyn Channel>> {
        self.channels.iter_mut()
    }
}
