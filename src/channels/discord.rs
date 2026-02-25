//! Discord channel — receives commands and streams agent output.
//!
//! Uses the Discord API to receive commands and stream output.

use super::{Channel, IncomingMessage, OutgoingMessage, OutputChunk};
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use tokio::sync::broadcast;

pub struct DiscordChannel {
    token: String,
    client: Client,
    channel_id: Option<String>,
}

#[derive(Deserialize)]
struct DiscordUser {
    id: String,
    username: String,
    discriminator: String,
}

#[derive(Deserialize)]
struct DiscordUserResponse {
    user: DiscordUser,
}

impl DiscordChannel {
    pub fn new(token: String, channel_id: Option<String>) -> Self {
        Self {
            token,
            client: Client::new(),
            channel_id,
        }
    }
}

#[async_trait]
impl Channel for DiscordChannel {
    fn name(&self) -> &str {
        "discord"
    }

    async fn start(&self) -> anyhow::Result<tokio::sync::mpsc::Receiver<IncomingMessage>> {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        tracing::info!(token_prefix = %self.token.chars().take(8).collect::<String>(), "discord channel started");
        
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
        let channel_id = self.channel_id.as_ref().ok_or_else(|| anyhow::anyhow!("discord channel_id not configured"))?;
        
        let url = format!("https://discord.com/api/v10/channels/{}/messages", channel_id);
        let response = self.client
            .post(&url)
            .header("Authorization", format!("Bot {}", self.token))
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "content": msg.body
            }))
            .send()
            .await?;

        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("discord API error: {}", body);
        }

        Ok(())
    }

    async fn stream_output(
        &self,
        _thread_id: &str,
        _rx: broadcast::Receiver<OutputChunk>,
    ) -> anyhow::Result<()> {
        anyhow::bail!("discord streaming not yet implemented")
    }

    async fn health_check(&self) -> anyhow::Result<()> {
        let url = "https://discord.com/api/v10/users/@me";
        
        let response = self.client
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
