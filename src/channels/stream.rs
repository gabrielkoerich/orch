//! Output streaming — fans out session output to bound channel threads.
//!
//! When an agent session is bound to one or more channel threads (Telegram,
//! Discord, Slack), this module streams `OutputChunk` broadcasts to those
//! threads in real-time.
//!
//! Platform limits and rate limiting:
//! - Telegram: 4 KB per message, 100 ms between sends
//! - Discord:  2 KB per message, 500 ms between sends
//! - Slack:    4 KB per message, 200 ms between sends

use crate::channels::transport::Transport;
use crate::channels::{ChannelRegistry, OutgoingMessage};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::time::{Duration, Instant};

const TELEGRAM_MAX_BYTES: usize = 4096;
const DISCORD_MAX_BYTES: usize = 2000;
const SLACK_MAX_BYTES: usize = 4000;

fn platform_max_bytes(channel_name: &str) -> usize {
    match channel_name {
        "telegram" => TELEGRAM_MAX_BYTES,
        "discord" => DISCORD_MAX_BYTES,
        "slack" => SLACK_MAX_BYTES,
        _ => TELEGRAM_MAX_BYTES,
    }
}

fn platform_rate(channel_name: &str) -> Duration {
    match channel_name {
        "telegram" => Duration::from_millis(100),
        "discord" => Duration::from_millis(500),
        "slack" => Duration::from_millis(200),
        _ => Duration::from_millis(500),
    }
}

/// Split content into chunks that fit within the platform's message size limit.
fn split_for_platform(content: &str, channel_name: &str) -> Vec<String> {
    let max = platform_max_bytes(channel_name);
    if content.len() <= max {
        return vec![content.to_string()];
    }

    let mut parts = Vec::new();
    let mut start = 0;
    while start < content.len() {
        let mut end = (start + max).min(content.len());
        // Walk back to a UTF-8 char boundary
        while end > start && !content.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            // Single char exceeds limit — skip one byte (shouldn't happen with valid UTF-8)
            end = start + 1;
        }
        parts.push(content[start..end].to_string());
        start = end;
    }
    parts
}

/// Send a message to a specific channel thread, finding the channel by name.
async fn send_to_thread(
    channels: &Arc<ChannelRegistry>,
    channel_name: &str,
    thread_id: &str,
    body: String,
) {
    for ch in channels.iter() {
        if ch.name() == channel_name {
            let msg = OutgoingMessage {
                thread_id: thread_id.to_string(),
                body,
                reply_to: None,
                metadata: serde_json::json!({}),
            };
            if let Err(e) = ch.send(&msg).await {
                tracing::warn!(channel = channel_name, thread_id, ?e, "stream: send failed");
            }
            return;
        }
    }
    tracing::debug!(
        channel = channel_name,
        "stream: channel not found in registry"
    );
}

/// Fan out output chunks from a task to all bound channel threads.
///
/// Subscribes to the task's output broadcast in `transport`, accumulates
/// content per channel thread, applies rate limiting, and terminates when
/// the stream receives a final chunk or is closed.
///
/// This function should be spawned as a background task when a channel
/// thread is bound to an agent session.
pub async fn fanout_output(
    task_id: String,
    transport: Arc<Transport>,
    channels: Arc<ChannelRegistry>,
) {
    let mut rx = match transport.subscribe(&task_id).await {
        Some(r) => r,
        None => {
            tracing::debug!(task_id, "fanout: no binding for task, cannot subscribe");
            return;
        }
    };

    tracing::debug!(task_id, "fanout: started output streaming");

    // Per channel-name: last send time (for rate limiting)
    let mut last_send: HashMap<String, Instant> = HashMap::new();
    // Per thread-key ("channel:thread_id"): buffered content pending send
    let mut buffers: HashMap<String, String> = HashMap::new();

    loop {
        match rx.recv().await {
            Ok(chunk) => {
                let is_final = chunk.is_final;

                if !chunk.content.is_empty() {
                    // Look up current binding to find connected threads
                    if let Some(binding) = transport.get_binding(&task_id).await {
                        let now = Instant::now();

                        for thread_key in &binding.connected_threads {
                            let (ch_name, thread_id) = match thread_key.split_once(':') {
                                Some(p) => p,
                                None => continue,
                            };

                            // Append new content to this thread's buffer
                            buffers
                                .entry(thread_key.clone())
                                .or_default()
                                .push_str(&chunk.content);

                            // Check if enough time has passed since last send
                            let rate = platform_rate(ch_name);
                            let can_send = last_send
                                .get(ch_name)
                                .map(|t| now.duration_since(*t) >= rate)
                                .unwrap_or(true);

                            if can_send {
                                if let Some(buffered) = buffers.remove(thread_key) {
                                    let parts = split_for_platform(&buffered, ch_name);
                                    for part in parts {
                                        send_to_thread(&channels, ch_name, thread_id, part).await;
                                    }
                                    last_send.insert(ch_name.to_string(), now);
                                }
                            }
                        }
                    }
                }

                if is_final {
                    // Flush all remaining buffers
                    if let Some(binding) = transport.get_binding(&task_id).await {
                        for thread_key in &binding.connected_threads {
                            if let Some(buffered) = buffers.remove(thread_key) {
                                if !buffered.is_empty() {
                                    let (ch_name, thread_id) = match thread_key.split_once(':') {
                                        Some(p) => p,
                                        None => continue,
                                    };
                                    let parts = split_for_platform(&buffered, ch_name);
                                    for part in parts {
                                        send_to_thread(&channels, ch_name, thread_id, part).await;
                                    }
                                }
                            }
                        }
                    }
                    tracing::debug!(task_id, "fanout: final chunk received, stream complete");
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(
                    task_id,
                    missed = n,
                    "fanout: output receiver lagged, some output dropped"
                );
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                tracing::debug!(task_id, "fanout: output broadcast closed");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_short_content_unchanged() {
        let parts = split_for_platform("hello", "telegram");
        assert_eq!(parts, vec!["hello"]);
    }

    #[test]
    fn split_long_content_telegram() {
        let content = "a".repeat(5000);
        let parts = split_for_platform(&content, "telegram");
        assert!(parts.len() >= 2);
        for p in &parts {
            assert!(p.len() <= TELEGRAM_MAX_BYTES);
        }
        // Reassembled content equals original
        assert_eq!(parts.join(""), content);
    }

    #[test]
    fn split_long_content_discord() {
        let content = "x".repeat(3000);
        let parts = split_for_platform(&content, "discord");
        assert!(parts.len() >= 2);
        for p in &parts {
            assert!(p.len() <= DISCORD_MAX_BYTES);
        }
        assert_eq!(parts.join(""), content);
    }

    #[test]
    fn platform_max_bytes_known_channels() {
        assert_eq!(platform_max_bytes("telegram"), 4096);
        assert_eq!(platform_max_bytes("discord"), 2000);
        assert_eq!(platform_max_bytes("slack"), 4000);
        assert_eq!(platform_max_bytes("unknown"), 4096);
    }
}
