//! Discord channel — future implementation.
//!
//! Will use the Discord Gateway/REST API to receive commands and stream output.
//! For now, this is a placeholder module.

use super::{Channel, IncomingMessage, OutgoingMessage, OutputChunk};
use async_trait::async_trait;
use tokio::sync::broadcast;

pub struct DiscordChannel;

#[async_trait]
impl Channel for DiscordChannel {
    fn name(&self) -> &str {
        "discord"
    }

    async fn start(&self) -> anyhow::Result<tokio::sync::mpsc::Receiver<IncomingMessage>> {
        anyhow::bail!("discord channel not yet implemented")
    }

    async fn send(&self, _msg: &OutgoingMessage) -> anyhow::Result<()> {
        anyhow::bail!("discord channel not yet implemented")
    }

    async fn stream_output(
        &self,
        _thread_id: &str,
        _rx: broadcast::Receiver<OutputChunk>,
    ) -> anyhow::Result<()> {
        anyhow::bail!("discord channel not yet implemented")
    }

    async fn health_check(&self) -> anyhow::Result<()> {
        anyhow::bail!("discord channel not yet implemented")
    }

    async fn shutdown(&self) -> anyhow::Result<()> {
        Ok(())
    }
}
