//! GitHub channel — receives commands from issue comments, sends updates.
//!
//! Polls for new comments on issues with `status:in_progress` labels.
//! Posts task updates, agent output summaries, and status changes as comments.

use super::{Channel, IncomingMessage, OutgoingMessage, OutputChunk};
use async_trait::async_trait;
use tokio::sync::broadcast;

pub struct GitHubChannel {
    repo: String,
}

impl GitHubChannel {
    pub fn new(repo: String) -> Self {
        Self { repo }
    }
}

#[async_trait]
impl Channel for GitHubChannel {
    fn name(&self) -> &str {
        "github"
    }

    async fn start(&self) -> anyhow::Result<tokio::sync::mpsc::Receiver<IncomingMessage>> {
        let (_tx, rx) = tokio::sync::mpsc::channel(64);
        tracing::info!(repo = %self.repo, "github channel started (polling)");
        // TODO: spawn polling loop for new issue comments
        Ok(rx)
    }

    async fn send(&self, _msg: &OutgoingMessage) -> anyhow::Result<()> {
        // TODO: post comment via gh api
        Ok(())
    }

    async fn stream_output(
        &self,
        _thread_id: &str,
        _rx: broadcast::Receiver<OutputChunk>,
    ) -> anyhow::Result<()> {
        // GitHub doesn't support real-time streaming —
        // we post periodic summaries instead.
        Ok(())
    }

    async fn health_check(&self) -> anyhow::Result<()> {
        // Delegate to backend health check
        Ok(())
    }

    async fn shutdown(&self) -> anyhow::Result<()> {
        Ok(())
    }
}
