//! Reacts to Routed events — dispatches agent immediately.

use crate::backends::ExternalBackend;
use crate::channels::capture::CaptureService;
use crate::engine::events::TaskEvent;
use crate::engine::router::Router;
use crate::engine::runner::{TaskRunner, WeightSignal};
use crate::engine::tasks::TaskManager;
use crate::store::TaskStore;
use crate::tmux::TmuxManager;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::{mpsc, RwLock, Semaphore};

/// Spawn a task that listens for Routed events and dispatches agents.
///
/// This mirrors the logic in `tick_dispatch_tasks` but triggers instantly
/// instead of waiting for the next tick. The dispatching set prevents
/// double-dispatch if the tick loop picks up the same task.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    mut rx: broadcast::Receiver<TaskEvent>,
    backend: Arc<dyn ExternalBackend>,
    tmux: Arc<TmuxManager>,
    runner: Arc<TaskRunner>,
    capture: Arc<CaptureService>,
    semaphore: Arc<Semaphore>,
    task_manager: Arc<TaskManager>,
    weight_tx: mpsc::Sender<WeightSignal>,
    router_arc: Arc<RwLock<Router>>,
    dispatching: Arc<std::sync::Mutex<HashSet<String>>>,
    store: Arc<TaskStore>,
    repo: String,
) {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) if event.new_status == "routed" && event.repo == repo => {
                    tracing::info!(task_id = event.task_id, "event-driven dispatch triggered");
                    // Delegate to existing dispatch logic.
                    // The dispatching set guard inside tick_dispatch_tasks
                    // prevents double-dispatch if the tick loop also picks this up.
                    if let Err(e) = crate::engine::tick::tick_dispatch_tasks(
                        &backend,
                        &tmux,
                        &repo,
                        &runner,
                        &capture,
                        &semaphore,
                        &task_manager,
                        &weight_tx,
                        &router_arc,
                        &dispatching,
                        &store,
                    )
                    .await
                    {
                        tracing::warn!(?e, "event-driven dispatch failed (tick will retry)");
                    }
                }
                Ok(_) => {} // Not a routed event or different repo
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!(n, "dispatch subscriber lagged, tick will catch up");
                }
                Err(_) => break, // Channel closed
            }
        }
    });
}
