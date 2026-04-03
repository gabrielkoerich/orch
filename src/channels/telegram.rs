//! Telegram channel — receives commands and streams agent output.
//!
//! Uses the Telegram Bot API to receive commands and stream agent output.

use super::{Channel, IncomingMessage, OutgoingMessage};
use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::time::Duration;

const TELEGRAM_LONG_POLL_TIMEOUT_SECS: u64 = 30;
const TELEGRAM_HTTP_TIMEOUT_SECS: u64 = TELEGRAM_LONG_POLL_TIMEOUT_SECS + 15;
const TELEGRAM_MAX_RETRIES: u32 = 3;
const _: () = assert!(TELEGRAM_HTTP_TIMEOUT_SECS > TELEGRAM_LONG_POLL_TIMEOUT_SECS);

/// Escape text for Telegram HTML parse_mode.
///
/// Telegram's HTML parser is strict: bare `<`, `>`, or `&` cause
/// "can't parse entities" errors. This function escapes those characters.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn is_preformatted_html(msg: &OutgoingMessage) -> bool {
    msg.metadata
        .get("preformatted_html")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

pub struct TelegramChannel {
    pub token: String,
    pub client: Client,
    pub chat_id: Option<String>,
    pub offset: std::sync::Arc<tokio::sync::Mutex<i64>>,
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
    pub fn new(token: String, chat_id: Option<String>) -> anyhow::Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(TELEGRAM_HTTP_TIMEOUT_SECS))
            .build()
            .context("failed to build HTTP client")?;

        Ok(Self {
            token,
            client,
            chat_id,
            offset: std::sync::Arc::new(tokio::sync::Mutex::new(0)),
        })
    }

    fn api_url(&self, method: &str) -> String {
        format!("https://api.telegram.org/bot{}/{}", self.token, method)
    }

    async fn get_updates(&self, offset: i64) -> anyhow::Result<Vec<Update>> {
        let url = self.api_url("getUpdates");

        let params = serde_json::json!({
            "offset": offset,
            "timeout": TELEGRAM_LONG_POLL_TIMEOUT_SECS,
            "allowed_updates": ["message", "callback_query"]
        });

        for attempt in 0..TELEGRAM_MAX_RETRIES {
            let response = match self.client.post(&url).json(&params).send().await {
                Ok(r) => r,
                Err(e) => {
                    if attempt + 1 < TELEGRAM_MAX_RETRIES {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                    anyhow::bail!(
                        "telegram getUpdates failed after {} attempts: {e}",
                        TELEGRAM_MAX_RETRIES,
                    );
                }
            };

            if !response.status().is_success() {
                let body = response.text().await.unwrap_or_default();
                anyhow::bail!("telegram API error: {}", body);
            }

            let updates: GetUpdatesResponse = response.json().await?;

            if !updates.ok {
                anyhow::bail!("telegram API returned ok=false");
            }

            return Ok(updates.result);
        }

        anyhow::bail!(
            "telegram getUpdates failed after {} attempts",
            TELEGRAM_MAX_RETRIES,
        );
    }

    async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        topic_id: Option<i64>,
    ) -> anyhow::Result<()> {
        let escaped = html_escape(text);
        self.send_formatted_message(chat_id, &escaped, topic_id)
            .await
    }

    async fn send_formatted_message(
        &self,
        chat_id: i64,
        text: &str,
        topic_id: Option<i64>,
    ) -> anyhow::Result<()> {
        let url = self.api_url("sendMessage");

        // Use HTML parse mode. Callers must ensure text is properly escaped
        // (send_message escapes automatically; send_formatted_message expects
        // pre-escaped/pre-formatted input from TaskNotification::format_telegram).
        let mut params = serde_json::json!({
            "chat_id": chat_id,
            "text": text,
            "parse_mode": "HTML"
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

        let token_fingerprint = {
            let mut hasher = Sha256::new();
            hasher.update(token.as_bytes());
            let hash = hasher.finalize();
            let hex: String = hash[..8].iter().map(|b| format!("{:02x}", b)).collect();
            format!("sha256:{hex}")
        };

        tracing::info!(token_fingerprint = %token_fingerprint, "telegram channel started");

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
                    let off = offset.lock().await;
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
                        let mut off = offset.lock().await;
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
        // Resolve chat_id: metadata override → configured default.
        let chat_id = if let Some(override_val) = msg.metadata.get("chat_id_override") {
            override_val
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("chat_id_override is not a string"))?
                .parse::<i64>()
                .map_err(|_| anyhow::anyhow!("invalid chat_id_override"))?
        } else {
            self.chat_id
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("telegram chat_id not configured"))?
                .parse::<i64>()
                .map_err(|_| anyhow::anyhow!("invalid chat_id"))?
        };

        let topic_id = msg.topic_id.as_ref().and_then(|t| t.parse::<i64>().ok());

        // If metadata carries inline keyboard buttons, send as interactive message.
        if let Some(buttons_val) = msg.metadata.get("buttons") {
            if let Some(buttons_arr) = buttons_val.as_array() {
                let buttons: Vec<(String, String)> = buttons_arr
                    .iter()
                    .filter_map(|b| {
                        let text = b["text"].as_str()?.to_string();
                        let callback_data = b["callback_data"].as_str()?.to_string();
                        Some((text, callback_data))
                    })
                    .collect();
                if !buttons.is_empty() {
                    self.send_inline_keyboard(chat_id, topic_id, &msg.body, &buttons)
                        .await?;
                    return Ok(());
                }
            }
        }

        // Check if body is already pre-formatted HTML (e.g. TaskNotification::format_telegram)
        // or raw text that needs escaping (e.g. streamed agent output).
        let preformatted = is_preformatted_html(msg);

        if preformatted {
            self.send_formatted_message(chat_id, &msg.body, topic_id)
                .await
        } else {
            self.send_message(chat_id, &msg.body, topic_id).await
        }
    }

    async fn ack_interaction(&self, callback_query_id: &str) -> anyhow::Result<()> {
        self.answer_callback_query(callback_query_id, "").await
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn html_escape_escapes_telegram_html_entities() {
        assert_eq!(
            html_escape("a < b && c > d"),
            "a &lt; b &amp;&amp; c &gt; d"
        );
    }

    #[test]
    fn preformatted_html_defaults_to_false() {
        let msg = OutgoingMessage {
            thread_id: "thread".to_string(),
            body: "raw <text>".to_string(),
            reply_to: None,
            metadata: serde_json::Value::Null,
            topic_id: None,
        };

        assert!(!is_preformatted_html(&msg));
    }

    #[test]
    fn preformatted_html_honors_metadata_flag() {
        let msg = OutgoingMessage {
            thread_id: "thread".to_string(),
            body: "<b>formatted</b>".to_string(),
            reply_to: None,
            metadata: json!({ "preformatted_html": true }),
            topic_id: None,
        };

        assert!(is_preformatted_html(&msg));
    }
}
