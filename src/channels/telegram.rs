//! Telegram channel — future implementation.
//!
//! Will use the Telegram Bot API to receive commands and stream agent output.
//! For now, this is a placeholder module.

use super::{Channel, IncomingMessage, OutgoingMessage, OutputChunk};
use async_trait::async_trait;
use tokio::sync::broadcast;

pub struct TelegramChannel;

#[async_trait]
impl Channel for TelegramChannel {
    fn name(&self) -> &str {
        "telegram"
    }

    async fn start(&self) -> anyhow::Result<tokio::sync::mpsc::Receiver<IncomingMessage>> {
        anyhow::bail!("telegram channel not yet implemented")
    }

    async fn send(&self, _msg: &OutgoingMessage) -> anyhow::Result<()> {
        anyhow::bail!("telegram channel not yet implemented")
    }

    async fn stream_output(
        &self,
        _thread_id: &str,
        _rx: broadcast::Receiver<OutputChunk>,
    ) -> anyhow::Result<()> {
        anyhow::bail!("telegram channel not yet implemented")
    }

    async fn health_check(&self) -> anyhow::Result<()> {
        anyhow::bail!("telegram channel not yet implemented")
    }

    async fn shutdown(&self) -> anyhow::Result<()> {
        Ok(())
    }
}
