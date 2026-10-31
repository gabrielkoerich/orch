//! Channel abstraction — the core trait for all I/O surfaces.
//!
//! Every external interface (GitHub, Telegram, Discord, tmux) implements
//! the `Channel` trait. This gives the engine a uniform way to:
//! - Receive commands/messages
//! - Send task updates and agent output
//!
//! Channels are bidirectional and async. The engine doesn't care whether
//! a message came from a Telegram DM, a GitHub issue comment, or a
//! Discord thread — it processes the same `IncomingMessage` type.

pub mod capture;
pub mod discord;
pub mod discord_ws;
pub mod github;
pub mod notification;
pub mod routing;
pub mod slack;
pub mod stream;
pub mod telegram;
pub mod tmux;
pub mod transport;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Escape `&`, `<` and `>` for Telegram's HTML parse_mode, which rejects bare ones
pub(crate) fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Split content into chunks of at most `max_bytes`, never inside a UTF-8 character
pub(crate) fn split_utf8(content: &str, max_bytes: usize) -> Vec<String> {
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
            // A single char over the limit still goes out whole
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
    /// Optional forum topic / thread ID (e.g. Telegram forum topic)
    #[serde(default)]
    pub topic_id: Option<String>,
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
    /// Optional forum topic / thread ID (e.g. Telegram forum topic)
    #[serde(default)]
    pub topic_id: Option<String>,
}

/// An agent output chunk for streaming.
#[derive(Debug, Clone)]
pub struct OutputChunk {
    pub content: String,
    pub is_final: bool,
}

/// The core channel trait. All external interfaces implement this.
#[expect(clippy::double_must_use)] // async_trait marks its boxed futures must_use twice
#[async_trait]
pub trait Channel: Send + Sync {
    /// Human-readable channel name.
    fn name(&self) -> &str;

    /// Start listening for incoming messages.
    /// Returns a receiver that the engine polls.
    async fn start(&self) -> anyhow::Result<tokio::sync::mpsc::Receiver<IncomingMessage>>;

    /// Send a message to a thread/conversation.
    async fn send(&self, msg: &OutgoingMessage) -> anyhow::Result<()>;

    /// Acknowledge an interactive callback to dismiss the loading indicator.
    ///
    /// Telegram requires `answerCallbackQuery` to dismiss the spinner after a button press.
    /// Discord buttons are already acknowledged in the websocket handler — no-op here.
    /// Default implementation is a no-op so non-interactive channels need not implement it.
    async fn ack_interaction(&self, _callback_query_id: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Check if this channel is healthy/connected.
    async fn health_check(&self) -> anyhow::Result<()>;
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
        self.channels.iter().map(std::convert::AsRef::as_ref)
    }
}
