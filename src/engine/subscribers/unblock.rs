//! Reacts to Done events — unblocks parent tasks immediately.

use crate::backends::ExternalBackend;
use crate::engine::events::TaskEvent;
use crate::engine::tasks::TaskManager;
use crate::store::TaskStore;
use std::sync::Arc;
use tokio::sync::broadcast;

/// Spawn a task that listens for Done events and unblocks parent tasks.
///
/// This mirrors the logic in `tick_unblock_parents` but triggers instantly
/// instead of waiting for the next tick. The tick-loop call remains as a
/// fallback for any events missed due to subscriber lag.
pub fn spawn(
    mut rx: broadcast::Receiver<TaskEvent>,
    backend: Arc<dyn ExternalBackend>,
    task_manager: Arc<TaskManager>,
    store: Arc<TaskStore>,
    repo: String,
) {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) if event.new_status == "done" && event.repo == repo => {
                    tracing::info!(
                        task_id = event.task_id,
                        "event-driven parent unblock triggered"
                    );
                    if let Err(e) = crate::engine::tick::tick_unblock_parents(
                        &backend,
                        &task_manager,
                        &store,
                        &repo,
                    )
                    .await
                    {
                        tracing::warn!(?e, "event-driven unblock failed (tick will retry)");
                    }
                }
                Ok(_) => {} // Not a done event or different repo
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!(n, "unblock subscriber lagged, tick will catch up");
                }
                Err(_) => break, // Channel closed
            }
        }
    });
}
