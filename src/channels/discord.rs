//! Discord channel — receives commands and streams agent output.
//!
//! Uses the Discord API to receive commands and stream output via HTTP polling.

use super::{Channel, IncomingMessage, OutgoingMessage, OutputChunk};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::Deserialize;
use tokio::sync::broadcast;

pub struct DiscordChannel {
    token: String,
    client: Client,
    channel_id: Option<String>,
    last_message_id: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

#[derive(Deserialize)]
struct DiscordUser {
    id: String,
    username: String,
}

#[derive(Deserialize)]
struct DiscordUserResponse {
    user: DiscordUser,
}

#[derive(Deserialize)]
struct DiscordMessage {
    id: String,
    channel_id: String,
    author: DiscordMessageAuthor,
    content: String,
    timestamp: String,
}

#[derive(Deserialize)]
struct DiscordMessageAuthor {
    id: String,
    username: String,
    bot: Option<bool>,
}

#[derive(Deserialize)]
struct DiscordMessagesResponse {
    #[serde(default)]
    messages: Vec<DiscordMessage>,
}

impl DiscordChannel {
    pub fn new(token: String, channel_id: Option<String>) -> Self {
        Self {
            token,
            client: Client::new(),
            channel_id,
            last_message_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    fn api_url(&self, endpoint: &str) -> String {
        format!("https://discord.com/api/v10{}", endpoint)
    }

    async fn get_messages(&self, channel_id: &str, before: Option<&str>) -> anyhow::Result<Vec<DiscordMessage>> {
        let mut url = self.api_url(&format!("/channels/{}/messages", channel_id));
        
        if let Some(before_id) = before {
            url = format!("{}?before={}&limit=50", url, before_id);
        }

        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bot {}", self.token))
            .send()
            .await?;

        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("discord API error: {}", body);
        }

        let messages: Vec<DiscordMessage> = response.json().await?;
        Ok(messages)
    }

    async fn send_message(&self, channel_id: &str, content: &str) -> anyhow::Result<()> {
        let url = self.api_url(&format!("/channels/{}/messages", channel_id));

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bot {}", self.token))
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "content": content
            }))
            .send()
            .await?;

        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("discord API error: {}", body);
        }

        Ok(())
    }
}

#[async_trait]
impl Channel for DiscordChannel {
    fn name(&self) -> &str {
        "discord"
    }

    async fn start(&self) -> anyhow::Result<tokio::sync::mpsc::Receiver<IncomingMessage>> {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let token = self.token.clone();
        let _client = self.client.clone();
        let channel_id = self.channel_id.clone();
        let last_message_id = self.last_message_id.clone();

        tracing::info!(token_prefix = %token.chars().take(8).collect::<String>(), "discord channel started");

        // If no channel_id is configured, we can't poll
        if channel_id.is_none() {
            tracing::warn!("discord channel_id not configured, skipping message polling");
            return Ok(rx);
        }

        let channel_id = channel_id.unwrap();

        tokio::spawn(async move {
            let polling_interval = std::time::Duration::from_secs(5);

            loop {
                tokio::time::sleep(polling_interval).await;

                let before = {
                    let last = last_message_id.lock().unwrap();
                    last.clone()
                };

                let messages = match DiscordChannel::new(token.clone(), Some(channel_id.clone()))
                    .get_messages(&channel_id, before.as_deref())
                    .await
                {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(?e, "failed to get discord messages");
                        continue;
                    }
                };

                // Messages come in reverse chronological order
                for msg in messages.into_iter().rev() {
                    // Skip bot messages (we don't want to react to ourselves)
                    if msg.author.bot.unwrap_or(false) {
                        continue;
                    }

                    // Update last message id
                    {
                        let mut last = last_message_id.lock().unwrap();
                        if last.as_ref().map(|s| s.as_str()) != Some(&msg.id) {
                            *last = Some(msg.id.clone());
                        }
                    }

                    // Parse timestamp
                    let timestamp = match DateTime::parse_from_rfc3339(&msg.timestamp) {
                        Ok(dt) => dt.with_timezone(&Utc),
                        Err(_) => Utc::now(),
                    };

                    let incoming = IncomingMessage {
                        channel: "discord".to_string(),
                        id: msg.id,
                        thread_id: msg.channel_id,
                        author: msg.author.username,
                        body: msg.content,
                        timestamp,
                        metadata: serde_json::json!({}),
                    };

                    if tx.send(incoming).await.is_err() {
                        tracing::debug!("discord channel receiver dropped");
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
            .ok_or_else(|| anyhow::anyhow!("discord channel_id not configured"))?;

        self.send_message(channel_id, &msg.body).await
    }

    async fn stream_output(
        &self,
        thread_id: &str,
        mut rx: broadcast::Receiver<OutputChunk>,
    ) -> anyhow::Result<()> {
        let channel_id = thread_id;

        let mut buffer = String::new();
        let mut last_post = std::time::Instant::now();
        let post_interval = std::time::Duration::from_secs(10);

        loop {
            tokio::select! {
                chunk = rx.recv() => {
                    match chunk {
                        Ok(chunk) => {
                            if chunk.is_final {
                                if !buffer.is_empty() {
                                    let _ = self.send_message(channel_id, &buffer).await;
                                    buffer.clear();
                                }
                                let _ = self.send_message(channel_id, "---").await;
                                let _ = self.send_message(channel_id, "Session complete.").await;
                                break;
                            }

                            buffer.push_str(&chunk.content);

                            // Truncate if buffer gets too large (Discord has 2000 char limit)
                            if buffer.len() > 1800 {
                                let _ = self.send_message(channel_id, &buffer).await;
                                buffer.clear();
                                last_post = std::time::Instant::now();
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => {
                            break;
                        }
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
            }

            if last_post.elapsed() >= post_interval && !buffer.is_empty() {
                let _ = self.send_message(channel_id, &buffer).await;
                buffer.clear();
                last_post = std::time::Instant::now();
            }
        }

        Ok(())
    }

    async fn health_check(&self) -> anyhow::Result<()> {
        let url = self.api_url("/users/@me");

        let response = self
            .client
            .get(url)
            .header("Authorization", format!("Bot {}", self.token))
            .send()
            .await?;

        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("discord health check failed: {}", body);
        }

        let _me: DiscordUserResponse = response.json().await?;
        tracing::info!("discord bot health check passed");
        Ok(())
    }

    async fn shutdown(&self) -> anyhow::Result<()> {
        Ok(())
    }
}
