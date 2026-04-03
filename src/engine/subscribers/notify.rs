//! Reacts to status transitions — pushes notifications to channels for meaningful states.
//!
//! Only terminal/meaningful transitions are forwarded by default (`level: all`):
//! `done`, `needs_review`, `blocked`, `failed`. Intermediate transitions
//! (`new`, `routed`, `in_progress`, `in_review`) are suppressed unless
//! `notifications.level: verbose` is set in config.

use crate::channels::notification::{NotificationLevel, TaskNotification};
use crate::channels::transport::Transport;
use crate::engine::events::TaskEvent;
use std::sync::Arc;
use tokio::sync::broadcast;

/// Spawn a task that listens for events and pushes to transport.
///
/// This subscriber is spawned ONCE (not per-project) since the transport
/// handles all repos. It filters events using `NotificationLevel::should_notify()`
/// so only meaningful status transitions reach the channels.
pub fn spawn(mut rx: broadcast::Receiver<TaskEvent>, transport: Arc<Transport>) {
    let level = NotificationLevel::from_config();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if !level.should_notify(&event.new_status) {
                        continue;
                    }
                    let notification = TaskNotification {
                        task_id: event.task_id.clone(),
                        title: event.title.unwrap_or_default(),
                        status: event.new_status.clone(),
                        agent: event.agent.unwrap_or_default(),
                        duration_seconds: event.duration_seconds.unwrap_or(0.0),
                        summary: event.summary.unwrap_or_default(),
                        repo: Some(event.repo.clone()),
                        notify_target: None,
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
