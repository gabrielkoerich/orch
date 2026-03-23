//! Reacts to all status transitions — pushes notifications to channels.

use crate::channels::notification::TaskNotification;
use crate::channels::transport::Transport;
use crate::engine::events::TaskEvent;
use std::sync::Arc;
use tokio::sync::broadcast;

/// Spawn a task that listens for all events and pushes to transport.
///
/// This subscriber is spawned ONCE (not per-project) since the transport
/// handles all repos. It converts every `TaskEvent` into a `TaskNotification`
/// and calls `transport.push_notification()`, so notifications fire immediately
/// on status change instead of being scattered through tick/sync code.
pub fn spawn(mut rx: broadcast::Receiver<TaskEvent>, transport: Arc<Transport>) {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let notification = TaskNotification {
                        task_id: event.task_id.clone(),
                        title: String::new(),
                        status: event.new_status.clone(),
                        agent: event.agent.unwrap_or_default(),
                        duration_seconds: 0.0,
                        summary: String::new(),
                        repo: Some(event.repo.clone()),
                    };
                    transport.push_notification(notification);
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!(n, "notify subscriber lagged");
                }
                Err(_) => break, // Channel closed
            }
        }
    });
}
