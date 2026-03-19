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
                topic_id: None,
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
        // Wait for either a new chunk or the next scheduled flush
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
                    // Flush all remaining buffers immediately regardless of rate limits
                    if let Some(_binding) = transport.get_binding(&task_id).await {
                        // Drain buffers map and send everything
                        for (thread_key, buffered) in buffers.drain() {
                            if buffered.is_empty() {
                                continue;
                            }
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

        // If there are buffered messages pending due to rate limiting, compute
        // the soonest time we can send and wait (non-busy) so future loop
        // iterations can flush them even if no new chunks arrive.
        if !buffers.is_empty() {
            let now = Instant::now();
            let mut earliest = None::<Instant>;
            for thread_key in buffers.keys() {
                if let Some((ch_name, _)) = thread_key.split_once(':') {
                    let rate = platform_rate(ch_name);
                    if let Some(last) = last_send.get(ch_name) {
                        let when = *last + rate;
                        if when > now {
                            earliest = Some(match earliest {
                                Some(e) if e <= when => e,
                                _ => when,
                            });
                        } else {
                            earliest = Some(now);
                        }
                    } else {
                        earliest = Some(now);
                    }
                }
            }

            if let Some(wake_at) = earliest {
                // Sleep until earliest send time or 50ms minimum to avoid busy-looping
                let sleep_dur = if wake_at > now {
                    wake_at - now
                } else {
                    Duration::from_millis(50)
                };
                tokio::time::sleep(sleep_dur).await;

                // After waiting, attempt to flush any buffers whose rate limit expired
                if let Some(_binding) = transport.get_binding(&task_id).await {
                    let now = Instant::now();
                    // Collect keys to flush to avoid borrowing issues
                    let keys: Vec<String> = buffers.keys().cloned().collect();
                    for thread_key in keys {
                        if let Some(buffered) = buffers.remove(&thread_key) {
                            let (ch_name, thread_id) = match thread_key.split_once(':') {
                                Some(p) => p,
                                None => continue,
                            };
                            let rate = platform_rate(ch_name);
                            let can_send = last_send
                                .get(ch_name)
                                .map(|t| now.duration_since(*t) >= rate)
                                .unwrap_or(true);
                            if can_send {
                                let parts = split_for_platform(&buffered, ch_name);
                                for part in parts {
                                    send_to_thread(&channels, ch_name, thread_id, part).await;
                                }
                                last_send.insert(ch_name.to_string(), now);
                            } else {
                                // Put it back for next round
                                buffers.insert(thread_key, buffered);
                            }
                        }
                    }
                }
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

    // A lightweight test channel implementation used to assert that
    // fanout_output sends OutgoingMessage bodies to registered channels.
    struct TestChannel {
        name: String,
        tx: tokio::sync::mpsc::Sender<crate::channels::OutgoingMessage>,
    }

    #[async_trait::async_trait]
    impl crate::channels::Channel for TestChannel {
        fn name(&self) -> &str {
            &self.name
        }

        async fn start(
            &self,
        ) -> anyhow::Result<tokio::sync::mpsc::Receiver<crate::channels::IncomingMessage>> {
            // Not used in this test — return a dummy receiver.
            let (_tx, rx) = tokio::sync::mpsc::channel(1);
            Ok(rx)
        }

        async fn send(&self, msg: &crate::channels::OutgoingMessage) -> anyhow::Result<()> {
            // Forward the message to the test harness channel so assertions can observe it.
            let _ = self.tx.send(msg.clone()).await;
            Ok(())
        }

        async fn health_check(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn fanout_sends_to_registered_channel() {
        use crate::channels::transport::Transport;

        let transport = std::sync::Arc::new(Transport::new());

        // Prepare a test channel that will collect OutgoingMessage values
        let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::channels::OutgoingMessage>(16);
        let test_ch = TestChannel {
            name: "telegram".to_string(),
            tx,
        };

        // Register channel in a ChannelRegistry
        let mut registry = crate::channels::ChannelRegistry::new();
        registry.register(Box::new(test_ch));
        let registry = std::sync::Arc::new(registry);

        // Bind a task to the channel:thread so fanout knows where to send
        let task_id = "fanout-test-task";
        transport
            .bind(task_id, "orch-fanout-test", "telegram", "thread-1")
            .await;

        // Spawn the fanout_output task
        let transport_clone = transport.clone();
        let registry_clone = registry.clone();
        let task_id_str = task_id.to_string();
        tokio::spawn(
            async move { fanout_output(task_id_str, transport_clone, registry_clone).await },
        );

        // Give fanout time to subscribe to the broadcast
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Push some output into the transport; the fanout should forward it
        transport
            .push_output(
                task_id,
                crate::channels::OutputChunk {
                    content: "hello from agent".to_string(),
                    is_final: false,
                },
            )
            .await;

        // Push a final chunk to force flush
        transport
            .push_output(
                task_id,
                crate::channels::OutputChunk {
                    content: "".to_string(),
                    is_final: true,
                },
            )
            .await;

        // Expect at least one outgoing message to be sent to the test channel
        let mut got = false;
        for _ in 0..10 {
            if let Ok(Some(msg)) =
                tokio::time::timeout(std::time::Duration::from_millis(200), async {
                    rx.recv().await
                })
                .await
            {
                if msg.body.contains("hello from agent") {
                    got = true;
                    break;
                }
            }
        }

        assert!(
            got,
            "fanout did not send expected outgoing message to test channel"
        );
    }
}
