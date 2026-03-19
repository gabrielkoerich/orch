//! Telegram channel — receives commands and streams agent output.
//!
//! Uses the Telegram Bot API to receive commands and stream agent output.

use super::{Channel, IncomingMessage, OutgoingMessage};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::Deserialize;

pub struct TelegramChannel {
    pub token: String,
    pub client: Client,
    pub chat_id: Option<String>,
    pub offset: std::sync::Arc<std::sync::Mutex<i64>>,
}

#[derive(Deserialize)]
struct TelegramUser {
    first_name: String,
    #[serde(default)]
    username: Option<String>,
}

#[derive(Deserialize)]
struct TelegramMessage {
    message_id: i64,
    from: Option<TelegramUser>,
    chat: TelegramChat,
    text: Option<String>,
    date: i64,
    #[serde(default)]
    message_thread_id: Option<i64>,
}

#[derive(Deserialize)]
struct TelegramChat {
    id: i64,
}

#[derive(Deserialize)]
struct CallbackQuery {
    id: String,
    from: TelegramUser,
    message: Option<TelegramMessage>,
    data: Option<String>,
}

#[derive(Deserialize)]
struct Update {
    update_id: i64,
    message: Option<TelegramMessage>,
    callback_query: Option<CallbackQuery>,
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
            "allowed_updates": ["message", "callback_query"]
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

    async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        topic_id: Option<i64>,
    ) -> anyhow::Result<()> {
        let url = self.api_url("sendMessage");

        let mut params = serde_json::json!({
            "chat_id": chat_id,
            "text": text,
            "parse_mode": "Markdown"
        });
        if let Some(tid) = topic_id {
            params["message_thread_id"] = serde_json::json!(tid);
        }

        let response = self.client.post(&url).json(&params).send().await?;

        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("telegram API error: {}", body);
        }

        Ok(())
    }

    #[allow(dead_code)]
    pub async fn send_inline_keyboard(
        &self,
        chat_id: i64,
        topic_id: Option<i64>,
        text: &str,
        buttons: &[(String, String)], // (label, callback_data)
    ) -> anyhow::Result<i64> {
        let keyboard: Vec<Vec<serde_json::Value>> = buttons
            .iter()
            .map(|(label, data)| {
                vec![serde_json::json!({
                    "text": label,
                    "callback_data": data
                })]
            })
            .collect();

        let mut params = serde_json::json!({
            "chat_id": chat_id,
            "text": text,
            "reply_markup": { "inline_keyboard": keyboard }
        });
        if let Some(tid) = topic_id {
            params["message_thread_id"] = serde_json::json!(tid);
        }

        let url = self.api_url("sendMessage");
        let response = self.client.post(&url).json(&params).send().await?;
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("telegram API error: {}", body);
        }
        let result: serde_json::Value = response.json().await?;
        let message_id = result["result"]["message_id"].as_i64().unwrap_or(0);
        Ok(message_id)
    }

    #[allow(dead_code)]
    pub async fn answer_callback_query(
        &self,
        callback_query_id: &str,
        text: &str,
    ) -> anyhow::Result<()> {
        let url = self.api_url("answerCallbackQuery");
        let params = serde_json::json!({
            "callback_query_id": callback_query_id,
            "text": text
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
        let client = self.client.clone();
        let chat_id = self.chat_id.clone();
        let offset = self.offset.clone();

        tracing::info!(token_prefix = %token.chars().take(8).collect::<String>(), "telegram channel started");

        // Create a single TelegramChannel instance to reuse for all API calls
        let channel = TelegramChannel {
            token: token.clone(),
            client: client.clone(),
            chat_id: chat_id.clone(),
            offset: offset.clone(),
        };

        tokio::spawn(async move {
            loop {
                let current_offset = {
                    let off = offset.lock().unwrap_or_else(|e| e.into_inner());
                    *off
                };

                let updates = match channel.get_updates(current_offset).await {
                    Ok(u) => u,
                    Err(e) => {
                        tracing::warn!(?e, "failed to get telegram updates");
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        continue;
                    }
                };

                let has_updates = !updates.is_empty();

                for update in updates {
                    // Update offset
                    {
                        let mut off = offset.lock().unwrap_or_else(|e| e.into_inner());
                        if update.update_id + 1 > *off {
                            *off = update.update_id + 1;
                        }
                    }

                    // Handle callback queries
                    if let Some(cb) = update.callback_query {
                        let chat_id_val = cb
                            .message
                            .as_ref()
                            .map(|m| m.chat.id.to_string())
                            .unwrap_or_default();
                        let topic_id = cb
                            .message
                            .as_ref()
                            .and_then(|m| m.message_thread_id)
                            .map(|id| id.to_string());
                        let author = cb
                            .from
                            .username
                            .clone()
                            .unwrap_or(cb.from.first_name.clone());
                        let body = cb.data.clone().unwrap_or_default();
                        let msg_id = cb
                            .message
                            .as_ref()
                            .map(|m| m.message_id.to_string())
                            .unwrap_or_default();
                        let msg_date = cb.message.as_ref().map(|m| m.date).unwrap_or(0);

                        let incoming = IncomingMessage {
                            channel: "telegram".to_string(),
                            id: msg_id,
                            thread_id: chat_id_val,
                            author,
                            body,
                            timestamp: DateTime::from_timestamp(msg_date, 0)
                                .unwrap_or_else(Utc::now),
                            metadata: serde_json::json!({
                                "callback_query_id": cb.id,
                                "callback_data": cb.data
                            }),
                            topic_id,
                        };

                        if tx.send(incoming).await.is_err() {
                            tracing::debug!("telegram channel receiver dropped");
                            return;
                        }
                        continue;
                    }

                    // Handle regular messages
                    let msg = match update.message {
                        Some(m) => m,
                        None => continue,
                    };

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

                    let topic_id = msg.message_thread_id.map(|id| id.to_string());

                    let incoming = IncomingMessage {
                        channel: "telegram".to_string(),
                        id: msg.message_id.to_string(),
                        thread_id: msg.chat.id.to_string(),
                        author,
                        body,
                        timestamp: DateTime::from_timestamp(msg.date, 0).unwrap_or_else(Utc::now),
                        metadata: serde_json::json!({ "chat_id": msg.chat.id }),
                        topic_id,
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

        let topic_id = msg.topic_id.as_ref().and_then(|t| t.parse::<i64>().ok());
        self.send_message(chat_id, &msg.body, topic_id).await
    }

    async fn health_check(&self) -> anyhow::Result<()> {
        let url = self.api_url("getMe");

        let response = self.client.get(&url).send().await?;

        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("telegram health check failed: {}", body);
        }

        tracing::info!("telegram bot health check passed");
        Ok(())
    }
}
