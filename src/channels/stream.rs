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

use crate::channels::transport::{parse_conversation_key, Transport};
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
            // Single char exceeds limit — advance past the full character
            if let Some(ch) = content[start..].chars().next() {
                end = start + ch.len_utf8();
            } else {
                break;
            }
        }
        parts.push(content[start..end].to_string());
        start = end;
    }
    parts
}

/// Escape text for Telegram HTML parse_mode.
///
/// Telegram's HTML parser is strict: bare `<`, `>`, or `&` cause
/// "can't parse entities" errors. This function escapes those characters.
fn telegram_html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Send a message to a specific channel thread, finding the channel by name.
///
/// `topic_id` is forwarded to the channel so that topic-aware channels
/// (Telegram forum topics, Discord threads) deliver the message to the
/// correct sub-conversation rather than the parent chat/channel.
///
/// `body` is assumed to already be in the correct format for the channel:
/// - For Telegram: already HTML-escaped (caller escapes before splitting)
/// - For other channels: raw text
async fn send_to_thread(
    channels: &Arc<ChannelRegistry>,
    channel_name: &str,
    thread_id: &str,
    topic_id: Option<&str>,
    body: String,
) {
    let metadata = if channel_name == "telegram" {
        serde_json::json!({ "preformatted_html": true })
    } else {
        serde_json::json!({})
    };

    for ch in channels.iter() {
        if ch.name() == channel_name {
            let msg = OutgoingMessage {
                thread_id: thread_id.to_string(),
                body,
                reply_to: None,
                metadata,
                topic_id: topic_id.map(String::from),
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
    repo: String,
    task_id: String,
    transport: Arc<Transport>,
    channels: Arc<ChannelRegistry>,
) {
    let mut rx = match transport.subscribe(&repo, &task_id).await {
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
                    if let Some(binding) = transport.get_binding(&repo, &task_id).await {
                        let now = Instant::now();

                        for thread_key in &binding.connected_threads {
                            let (ch_name, thread_id, topic_id) =
                                match parse_conversation_key(thread_key) {
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
                                    // Escape HTML for Telegram before splitting so chunk
                                    // boundaries are computed on the actual API payload size.
                                    let to_send = if ch_name == "telegram" {
                                        telegram_html_escape(&buffered)
                                    } else {
                                        buffered
                                    };
                                    let parts = split_for_platform(&to_send, ch_name);
                                    for part in parts {
                                        send_to_thread(
                                            &channels, ch_name, thread_id, topic_id, part,
                                        )
                                        .await;
                                    }
                                    last_send.insert(ch_name.to_string(), now);
                                }
                            }
                        }
                    }
                }

                if is_final {
                    // Flush all remaining buffers immediately regardless of rate limits
                    if let Some(_binding) = transport.get_binding(&repo, &task_id).await {
                        // Drain buffers map and send everything
                        for (thread_key, buffered) in buffers.drain() {
                            if buffered.is_empty() {
                                continue;
                            }
                            let (ch_name, thread_id, topic_id) =
                                match parse_conversation_key(&thread_key) {
                                    Some(p) => p,
                                    None => continue,
                                };
                            // Escape HTML for Telegram before splitting (same as above).
                            let to_send = if ch_name == "telegram" {
                                telegram_html_escape(&buffered)
                            } else {
                                buffered
                            };
                            let parts = split_for_platform(&to_send, ch_name);
                            for part in parts {
                                send_to_thread(&channels, ch_name, thread_id, topic_id, part).await;
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
                if let Some((ch_name, _, _)) = parse_conversation_key(thread_key) {
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
                if let Some(_binding) = transport.get_binding(&repo, &task_id).await {
                    let now = Instant::now();
                    // Collect keys to flush to avoid borrowing issues
                    let keys: Vec<String> = buffers.keys().cloned().collect();
                    for thread_key in keys {
                        if let Some(buffered) = buffers.remove(&thread_key) {
                            let (ch_name, thread_id, topic_id) =
                                match parse_conversation_key(&thread_key) {
                                    Some(p) => p,
                                    None => continue,
                                };
                            let rate = platform_rate(ch_name);
                            let can_send = last_send
                                .get(ch_name)
                                .map(|t| now.duration_since(*t) >= rate)
                                .unwrap_or(true);
                            if can_send {
                                // Escape HTML for Telegram before splitting (same as above).
                                let to_send = if ch_name == "telegram" {
                                    telegram_html_escape(&buffered)
                                } else {
                                    buffered
                                };
                                let parts = split_for_platform(&to_send, ch_name);
                                for part in parts {
                                    send_to_thread(&channels, ch_name, thread_id, topic_id, part)
                                        .await;
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

    #[test]
    fn split_multibyte_char_at_boundary_does_not_panic() {
        // 4-byte emoji: each "😀" is 4 bytes. With max=3, the first chunk boundary
        // falls in the middle of the character — the old code would panic here.
        // Build content so the cut at byte 2000 falls inside the trailing 4-byte emoji.
        let content = "x".repeat(1997) + "😀"; // 2001 bytes, discord limit is 2000
        let parts = split_for_platform(&content, "discord");
        // Must not panic and reassembled content must equal original
        assert_eq!(parts.join(""), content);
        for p in &parts {
            // Each part must be valid UTF-8 (Rust strings always are, but slice must be sound)
            assert!(std::str::from_utf8(p.as_bytes()).is_ok());
        }
    }

    #[test]
    fn split_single_multibyte_char_exceeding_limit() {
        // Directly exercise split_for_platform where the only content is a single
        // character whose byte length exceeds the chunk limit.
        // We can't change platform limits, but we can test that a string of emojis
        // where every boundary falls mid-char is handled correctly.
        let content = "😀".repeat(600); // 2400 bytes, exceeds discord limit of 2000
        let parts = split_for_platform(&content, "discord");
        assert_eq!(parts.join(""), content);
        for p in &parts {
            assert!(std::str::from_utf8(p.as_bytes()).is_ok());
        }
    }

    // Regression test for #1897: content must be HTML-escaped BEFORE splitting so
    // that chunk boundaries are computed on the actual payload that reaches the
    // Telegram API. The old code split on raw content, then escaped each chunk —
    // if a chunk boundary fell inside an HTML tag, the escaped result could exceed
    // 4096 bytes and be rejected by Telegram.
    #[test]
    fn split_telegram_html_content_escaped_before_split() {
        // Build content where the 4096-byte boundary falls inside an HTML tag.
        // After escaping, each chunk (computed on escaped content) must be ≤ 4096.
        let raw = "a".repeat(4096) + "<code>123</code>"; // 4096 + 16 = 4112 bytes raw
        assert_eq!(raw.len(), 4112);
        let escaped = telegram_html_escape(&raw);
        // 2 `<` + 2 `>` each gain 3 bytes when escaped (4 vs 1)
        assert_eq!(escaped.len(), 4112 + 12); // 4124 bytes
        assert!(escaped.len() > raw.len());

        // Split the ESCAPED content — this is what fanout_output now does.
        let parts = split_for_platform(&escaped, "telegram");
        assert!(
            parts.len() >= 2,
            "escaped content ({}) must split into ≥2 parts",
            escaped.len()
        );
        for (i, p) in parts.iter().enumerate() {
            assert!(
                p.len() <= TELEGRAM_MAX_BYTES,
                "part {} ({} bytes) exceeds Telegram limit {}",
                i,
                p.len(),
                TELEGRAM_MAX_BYTES
            );
        }
        // Reassembled parts must equal fully-escaped content
        assert_eq!(parts.join(""), escaped);
    }

    // Documents the old buggy behavior that caused #1897.
    // Splitting on raw content then escaping each chunk causes the escaped
    // chunk to exceed 4096 bytes when a boundary falls inside an HTML tag.
    #[test]
    fn split_then_escape_exceeds_limit_when_boundary_inside_html_tag() {
        // `<code>123</code>` is 16 bytes: 2×`<>`, 3 digits, 5 letters, 1×`/`
        let html_tag = "<code>123</code>";
        assert_eq!(html_tag.len(), 16);

        // Build content where raw boundary lands mid-HTML-tag.
        // The escaped version of `<code>123</code>` is 28 bytes (each `<` and `>`
        // becomes `&lt;` / `&gt;`, a +3 byte expansion).
        // If the boundary splits in the middle of `</code>`, the raw chunk end
        // (containing `>`) expands by 3 bytes when escaped — enough to overflow.
        let raw = "a".repeat(4093) + html_tag; // 4093 + 16 = 4109 bytes
        assert_eq!(raw.len(), 4109);

        // Old buggy approach: split raw, then escape each
        let buggy_parts: Vec<String> = split_for_platform(&raw, "telegram")
            .into_iter()
            .map(|p| telegram_html_escape(&p))
            .collect();

        // At least one chunk exceeds the limit after escaping
        let has_overflow = buggy_parts.iter().any(|p| p.len() > TELEGRAM_MAX_BYTES);
        assert!(
            has_overflow,
            "expected old buggy approach to produce an oversized chunk"
        );

        // Fixed approach: escape first, then split on escaped content
        let escaped = telegram_html_escape(&raw);
        let fixed_parts = split_for_platform(&escaped, "telegram");
        for (i, p) in fixed_parts.iter().enumerate() {
            assert!(
                p.len() <= TELEGRAM_MAX_BYTES,
                "fixed part {} ({} bytes) must not exceed limit",
                i,
                p.len()
            );
        }
        // Fixed approach preserves all content
        assert_eq!(fixed_parts.join(""), escaped);
    }

    #[test]
    fn telegram_html_escape_produces_valid_html_entities() {
        assert_eq!(telegram_html_escape("<test>"), "&lt;test&gt;");
        assert_eq!(telegram_html_escape("a & b"), "a &amp; b");
        assert_eq!(telegram_html_escape("<>&"), "&lt;&gt;&amp;");
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
        let repo = "owner/test-repo";
        transport
            .bind(
                repo,
                task_id,
                "orch-fanout-test",
                "telegram",
                "thread-1",
                None,
            )
            .await;

        // Spawn the fanout_output task
        let transport_clone = transport.clone();
        let registry_clone = registry.clone();
        let task_id_str = task_id.to_string();
        let repo_str = repo.to_string();
        tokio::spawn(async move {
            fanout_output(repo_str, task_id_str, transport_clone, registry_clone).await
        });

        // Give fanout time to subscribe to the broadcast
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Push some output into the transport; the fanout should forward it
        transport
            .push_output(
                repo,
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
                repo,
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

    /// When a task is bound with a `topic_id`, the outgoing `OutgoingMessage`
    /// forwarded by fanout must carry the same `topic_id` so that the channel
    /// delivers the reply to the correct forum topic / thread.
    #[tokio::test]
    async fn fanout_forwards_topic_id() {
        let transport = std::sync::Arc::new(Transport::new());

        let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::channels::OutgoingMessage>(16);
        let test_ch = TestChannel {
            name: "telegram".to_string(),
            tx,
        };

        let mut registry = crate::channels::ChannelRegistry::new();
        registry.register(Box::new(test_ch));
        let registry = std::sync::Arc::new(registry);

        let task_id = "topic-forward-task";
        let repo = "owner/test-repo";
        // Bind with a specific topic_id
        transport
            .bind(
                repo,
                task_id,
                "orch-topic-test",
                "telegram",
                "chat-111",
                Some("topic-42"),
            )
            .await;

        let transport_clone = transport.clone();
        let registry_clone = registry.clone();
        let task_id_str = task_id.to_string();
        let repo_str = repo.to_string();
        tokio::spawn(async move {
            fanout_output(repo_str, task_id_str, transport_clone, registry_clone).await
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        transport
            .push_output(
                repo,
                task_id,
                crate::channels::OutputChunk {
                    content: "topic reply".to_string(),
                    is_final: true,
                },
            )
            .await;

        let mut got_topic = false;
        for _ in 0..10 {
            if let Ok(Some(msg)) =
                tokio::time::timeout(std::time::Duration::from_millis(200), async {
                    rx.recv().await
                })
                .await
            {
                if msg.body.contains("topic reply") {
                    assert_eq!(
                        msg.topic_id.as_deref(),
                        Some("topic-42"),
                        "outgoing message must carry the bound topic_id"
                    );
                    got_topic = true;
                    break;
                }
            }
        }

        assert!(
            got_topic,
            "fanout did not forward topic_id in outgoing message"
        );
    }
}
