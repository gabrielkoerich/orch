//! Slack channel — receives commands and streams agent output.
//!
//! Uses the Slack Web API with HTTP polling (conversations.history) to receive
//! messages and the chat.postMessage API to send responses.
//!
//! Configuration in `~/.orch/config.yml`:
//! ```yaml
//! channels:
//!   slack:
//!     bot_token: "xoxb-..."       # Bot User OAuth Token
//!     channel_id: "C1234567890"   # Channel to monitor and post to
//! ```
//!
//! The bot token requires the following OAuth scopes:
//! - `channels:history` / `groups:history` — read messages
//! - `chat:write` — post messages
//! - `channels:read` — health check

use super::{Channel, IncomingMessage, OutgoingMessage};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::Deserialize;

pub struct SlackChannel {
    pub bot_token: String,
    pub client: Client,
    pub channel_id: Option<String>,
    /// Cursor tracking the latest message timestamp seen (Slack uses `ts` as cursor).
    pub last_ts: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

#[derive(Deserialize)]
struct SlackMessage {
    #[serde(rename = "type")]
    msg_type: Option<String>,
    ts: String,
    /// Absent for bot-posted messages; present for human users.
    user: Option<String>,
    /// Present for bot messages.
    bot_id: Option<String>,
    text: Option<String>,
}

#[derive(Deserialize)]
struct ConversationsHistoryResponse {
    ok: bool,
    messages: Option<Vec<SlackMessage>>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct PostMessageResponse {
    ok: bool,
    error: Option<String>,
}

#[derive(Deserialize)]
struct AuthTestResponse {
    ok: bool,
    error: Option<String>,
    #[allow(dead_code)]
    user_id: Option<String>,
    #[allow(dead_code)]
    bot_id: Option<String>,
}

impl SlackChannel {
    pub fn new(bot_token: String, channel_id: Option<String>) -> Self {
        Self {
            bot_token,
            client: Client::new(),
            channel_id,
            last_ts: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    fn api_url(&self, method: &str) -> String {
        format!("https://slack.com/api/{}", method)
    }

    /// Fetch recent messages from a channel.
    ///
    /// Uses `oldest` to retrieve only messages newer than `last_ts`.
    async fn get_messages(&self, channel_id: &str) -> anyhow::Result<Vec<SlackMessage>> {
        let mut params = vec![("channel", channel_id.to_string()), ("limit", "50".to_string())];

        {
            let last = self.last_ts.lock().unwrap();
            if let Some(ref ts) = *last {
                // `oldest` is exclusive — add a tiny epsilon to avoid re-fetching the last msg
                params.push(("oldest", ts.clone()));
            }
        }

        let response = self
            .client
            .get(self.api_url("conversations.history"))
            .bearer_auth(&self.bot_token)
            .query(&params)
            .send()
            .await?;

        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("slack API HTTP error: {}", body);
        }

        let result: ConversationsHistoryResponse = response.json().await?;

        if !result.ok {
            let err = result.error.as_deref().unwrap_or("unknown");
            anyhow::bail!("slack conversations.history error: {}", err);
        }

        Ok(result.messages.unwrap_or_default())
    }

    async fn send_message(&self, channel_id: &str, text: &str) -> anyhow::Result<()> {
        let response = self
            .client
            .post(self.api_url("chat.postMessage"))
            .bearer_auth(&self.bot_token)
            .json(&serde_json::json!({
                "channel": channel_id,
                "text": text,
                "mrkdwn": true
            }))
            .send()
            .await?;

        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("slack API HTTP error: {}", body);
        }

        let result: PostMessageResponse = response.json().await?;

        if !result.ok {
            let err = result.error.as_deref().unwrap_or("unknown");
            anyhow::bail!("slack chat.postMessage error: {}", err);
        }

        Ok(())
    }
}

#[async_trait]
impl Channel for SlackChannel {
    fn name(&self) -> &str {
        "slack"
    }

    async fn start(&self) -> anyhow::Result<tokio::sync::mpsc::Receiver<IncomingMessage>> {
        let (tx, rx) = tokio::sync::mpsc::channel(64);

        // If no channel_id configured we can't poll
        if self.channel_id.is_none() {
            tracing::warn!("slack channel_id not configured, skipping message polling");
            return Ok(rx);
        }

        let bot_token = self.bot_token.clone();
        let client = self.client.clone();
        let channel_id = self.channel_id.clone().unwrap();
        let last_ts = self.last_ts.clone();

        tracing::info!(
            token_prefix = %bot_token.chars().take(12).collect::<String>(),
            channel_id = %channel_id,
            "slack channel started"
        );

        tokio::spawn(async move {
            let polling_interval = std::time::Duration::from_secs(5);

            loop {
                tokio::time::sleep(polling_interval).await;

                let channel = SlackChannel {
                    bot_token: bot_token.clone(),
                    client: client.clone(),
                    channel_id: Some(channel_id.clone()),
                    last_ts: last_ts.clone(),
                };

                let messages = match channel.get_messages(&channel_id).await {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(?e, "failed to get slack messages");
                        continue;
                    }
                };

                // Slack returns messages newest-first; reverse for chronological order
                let mut messages = messages;
                messages.reverse();

                for msg in messages {
                    // Skip non-message types (e.g. channel_join)
                    if msg.msg_type.as_deref() != Some("message") {
                        continue;
                    }

                    // Skip bot messages (including our own)
                    if msg.bot_id.is_some() {
                        continue;
                    }

                    let text = msg.text.clone().unwrap_or_default();
                    if text.is_empty() {
                        continue;
                    }

                    // Update last_ts to the maximum ts seen
                    {
                        let mut last = last_ts.lock().unwrap();
                        if last.as_ref().is_none_or(|prev: &String| msg.ts > *prev) {
                            *last = Some(msg.ts.clone());
                        }
                    }

                    let author = msg.user.clone().unwrap_or_else(|| "unknown".to_string());

                    // Parse Slack ts (Unix seconds with decimal sub-second)
                    let timestamp = msg
                        .ts
                        .split('.')
                        .next()
                        .and_then(|s| s.parse::<i64>().ok())
                        .and_then(|secs| DateTime::from_timestamp(secs, 0))
                        .unwrap_or_else(Utc::now);

                    let incoming = IncomingMessage {
                        channel: "slack".to_string(),
                        id: msg.ts.clone(),
                        thread_id: channel_id.clone(),
                        author,
                        body: text,
                        timestamp,
                        metadata: serde_json::json!({ "channel_id": channel_id }),
                    };

                    if tx.send(incoming).await.is_err() {
                        tracing::debug!("slack channel receiver dropped");
                        return;
                    }
                }
            }
        });

        Ok(rx)
    }

    async fn send(&self, msg: &OutgoingMessage) -> anyhow::Result<()> {
        let channel_id = self
            .channel_id
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("slack channel_id not configured"))?;

        self.send_message(channel_id, &msg.body).await
    }

    async fn health_check(&self) -> anyhow::Result<()> {
        let response = self
            .client
            .get(self.api_url("auth.test"))
            .bearer_auth(&self.bot_token)
            .send()
            .await?;

        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("slack health check HTTP error: {}", body);
        }

        let result: AuthTestResponse = response.json().await?;

        if !result.ok {
            let err = result.error.as_deref().unwrap_or("unknown");
            anyhow::bail!("slack auth.test failed: {}", err);
        }

        tracing::info!("slack bot health check passed");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slack_channel_name() {
        let ch = SlackChannel::new("xoxb-test".to_string(), None);
        assert_eq!(ch.name(), "slack");
    }

    #[test]
    fn slack_api_url() {
        let ch = SlackChannel::new("xoxb-test".to_string(), None);
        assert_eq!(
            ch.api_url("auth.test"),
            "https://slack.com/api/auth.test"
        );
        assert_eq!(
            ch.api_url("chat.postMessage"),
            "https://slack.com/api/chat.postMessage"
        );
    }

    #[test]
    fn slack_ts_comparison_is_lexicographic() {
        // Slack ts values are strings like "1700000000.123456"
        // Lexicographic comparison works because the integer part is the same width
        let older = "1700000000.000100".to_string();
        let newer = "1700000001.000000".to_string();
        assert!(newer > older);
    }
}
