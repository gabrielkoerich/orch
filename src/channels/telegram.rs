//! Telegram channel — receives commands and streams agent output.
//!
//! Uses the Telegram Bot API to receive commands and stream agent output.

use super::{Channel, IncomingMessage, OutgoingMessage, OutputChunk};
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use tokio::sync::broadcast;

pub struct TelegramChannel {
    token: String,
    client: Client,
    chat_id: Option<String>,
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

impl TelegramChannel {
    pub fn new(token: String, chat_id: Option<String>) -> Self {
        Self {
            token,
            client: Client::new(),
            chat_id,
        }
    }
}

#[async_trait]
impl Channel for TelegramChannel {
    fn name(&self) -> &str {
        "telegram"
    }

    async fn start(&self) -> anyhow::Result<tokio::sync::mpsc::Receiver<IncomingMessage>> {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        tracing::info!(token_prefix = %self.token.chars().take(8).collect::<String>(), "telegram channel started");
        
        let token = self.token.clone();
        tokio::spawn(async move {
            let _tx = tx;
            let _token = token;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }
        });
        Ok(rx)
    }

    async fn send(&self, msg: &OutgoingMessage) -> anyhow::Result<()> {
        let chat_id = self.chat_id.as_ref().ok_or_else(|| anyhow::anyhow!("telegram chat_id not configured"))?;
        
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.token);
        let params = serde_json::json!({
            "chat_id": chat_id,
            "text": msg.body,
            "parse_mode": "Markdown"
        });

        let response = self.client
            .post(&url)
            .json(&params)
            .send()
            .await?;

        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("telegram API error: {}", body);
        }

        Ok(())
    }

    async fn stream_output(
        &self,
        _thread_id: &str,
        _rx: broadcast::Receiver<OutputChunk>,
    ) -> anyhow::Result<()> {
        anyhow::bail!("telegram streaming not yet implemented")
    }

    async fn health_check(&self) -> anyhow::Result<()> {
        let url = format!("https://api.telegram.org/bot{}/getMe", self.token);
        
        let response = self.client
            .get(&url)
            .send()
            .await?;

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
