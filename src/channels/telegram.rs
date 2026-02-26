//! Telegram channel — receives commands and streams agent output.
//!
//! Uses the Telegram Bot API to receive commands and stream agent output.

use super::{Channel, IncomingMessage, OutgoingMessage, OutputChunk};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::Deserialize;
use tokio::sync::broadcast;

pub struct TelegramChannel {
    token: String,
    client: Client,
    chat_id: Option<String>,
    offset: std::sync::Arc<std::sync::Mutex<i64>>,
}

#[derive(Deserialize)]
struct TelegramUser {
    id: i64,
    first_name: String,
    #[serde(default)]
    username: Option<String>,
}

#[derive(Deserialize)]
struct GetMeResponse {
    result: TelegramUser,
}

#[derive(Deserialize)]
struct TelegramMessage {
    message_id: i64,
    from: Option<TelegramUser>,
    chat: TelegramChat,
    text: Option<String>,
    date: i64,
}

#[derive(Deserialize)]
struct TelegramChat {
    id: i64,
}

#[derive(Deserialize)]
struct Update {
    update_id: i64,
    message: Option<TelegramMessage>,
}

#[derive(Deserialize)]
struct GetUpdatesResponse {
    ok: bool,
    result: Vec<Update>,
}

impl TelegramChannel {
    pub fn new(token: String, chat_id: Option<String>) -> Self {
        Self {
            token,
            client: Client::new(),
            chat_id,
            offset: std::sync::Arc::new(std::sync::Mutex::new(0)),
        }
    }

    fn api_url(&self, method: &str) -> String {
        format!("https://api.telegram.org/bot{}/{}", self.token, method)
    }

    async fn get_updates(&self, offset: i64) -> anyhow::Result<Vec<Update>> {
        let url = self.api_url("getUpdates");
        
        let params = serde_json::json!({
            "offset": offset,
            "timeout": 30,
            "allowed_updates": ["message"]
        });

        let response = self.client.post(&url).json(&params).send().await?;

        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("telegram API error: {}", body);
        }

        let updates: GetUpdatesResponse = response.json().await?;

        if !updates.ok {
            anyhow::bail!("telegram API returned ok=false");
        }

        Ok(updates.result)
    }

    async fn send_message(&self, chat_id: i64, text: &str) -> anyhow::Result<()> {
        let url = self.api_url("sendMessage");
        
        let params = serde_json::json!({
            "chat_id": chat_id,
            "text": text,
            "parse_mode": "Markdown"
        });

        let response = self.client.post(&url).json(&params).send().await?;

        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("telegram API error: {}", body);
        }

        Ok(())
    }
}

#[async_trait]
impl Channel for TelegramChannel {
    fn name(&self) -> &str {
        "telegram"
    }

    async fn start(&self) -> anyhow::Result<tokio::sync::mpsc::Receiver<IncomingMessage>> {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let token = self.token.clone();
        let _client = self.client.clone();
        let chat_id = self.chat_id.clone();
        let offset = self.offset.clone();

        tracing::info!(token_prefix = %token.chars().take(8).collect::<String>(), "telegram channel started");

        tokio::spawn(async move {
            loop {
                let current_offset = {
                    let off = offset.lock().unwrap();
                    *off
                };

                let updates = match TelegramChannel::new(token.clone(), chat_id.clone())
                    .get_updates(current_offset)
                    .await
                {
                    Ok(u) => u,
                    Err(e) => {
                        tracing::warn!(?e, "failed to get telegram updates");
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        continue;
                    }
                };

                let has_updates = !updates.is_empty();

                for update in updates {
                    let msg = match update.message {
                        Some(m) => m,
                        None => continue,
                    };

                    // Update offset
                    {
                        let mut off = offset.lock().unwrap();
                        if update.update_id + 1 > *off {
                            *off = update.update_id + 1;
                        }
                    }

                    let author = msg
                        .from
                        .as_ref()
                        .map(|u| u.username.clone().unwrap_or(u.first_name.clone()))
                        .unwrap_or_else(|| "unknown".to_string());

                    let body = msg.text.unwrap_or_default();

                    // Skip empty messages or non-command messages unless we have a specific chat_id
                    if body.is_empty() {
                        continue;
                    }

                    let incoming = IncomingMessage {
                        channel: "telegram".to_string(),
                        id: msg.message_id.to_string(),
                        thread_id: msg.chat.id.to_string(),
                        author,
                        body,
                        timestamp: DateTime::from_timestamp(msg.date, 0).unwrap_or_else(Utc::now),
                        metadata: serde_json::json!({ "chat_id": msg.chat.id }),
                    };

                    if tx.send(incoming).await.is_err() {
                        tracing::debug!("telegram channel receiver dropped");
                        return;
                    }
                }

                // If no updates, sleep briefly to avoid busy looping
                if !has_updates {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        });

        Ok(rx)
    }

    async fn send(&self, msg: &OutgoingMessage) -> anyhow::Result<()> {
        let chat_id = self
            .chat_id
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("telegram chat_id not configured"))?
            .parse::<i64>()
            .map_err(|_| anyhow::anyhow!("invalid chat_id"))?;

        self.send_message(chat_id, &msg.body).await
    }

    async fn stream_output(
        &self,
        thread_id: &str,
        mut rx: broadcast::Receiver<OutputChunk>,
    ) -> anyhow::Result<()> {
        let chat_id: i64 = thread_id
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid chat_id: {}", thread_id))?;

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
                                    let _ = self.send_message(chat_id, &buffer).await;
                                    buffer.clear();
                                }
                                let _ = self.send_message(chat_id, "---").await;
                                let _ = self.send_message(chat_id, "Session complete.").await;
                                break;
                            }

                            buffer.push_str(&chunk.content);

                            // Truncate if buffer gets too large
                            if buffer.len() > 3000 {
                                let _ = self.send_message(chat_id, &buffer).await;
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
                let _ = self.send_message(chat_id, &buffer).await;
                buffer.clear();
                last_post = std::time::Instant::now();
            }
        }

        Ok(())
    }

    async fn health_check(&self) -> anyhow::Result<()> {
        let url = self.api_url("getMe");

        let response = self.client.get(&url).send().await?;

        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("telegram health check failed: {}", body);
        }

        let _me: GetMeResponse = response.json().await?;
        tracing::info!("telegram bot health check passed");
        Ok(())
    }

    async fn shutdown(&self) -> anyhow::Result<()> {
        Ok(())
    }
}
