//! Reacts to Routed events — dispatches agent immediately.

use crate::backends::ExternalBackend;
use crate::channels::capture::CaptureService;
use crate::engine::events::TaskEvent;
use crate::engine::router::Router;
use crate::engine::runner::{TaskRunner, WeightSignal};
use crate::engine::tasks::TaskManager;
use crate::store::TaskStore;
use crate::tmux::TmuxManager;
use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::{mpsc, RwLock, Semaphore};

type SessionMap = std::collections::HashMap<String, bool>;

async fn prepare_dispatch_context<F>(
    router_arc: &Arc<RwLock<Router>>,
    session_map_fut: F,
) -> (crate::engine::tick::DispatchMode, SessionMap)
where
    F: std::future::Future<Output = SessionMap>,
{
    // Keep lock scope minimal: snapshot router state, then drop the guard
    // before awaiting session/dispatch work.
    let dispatch_mode = {
        let router_guard = router_arc.read().await;
        crate::engine::tick::dispatch_mode_from_router(&router_guard)
    };
    let session_map = session_map_fut.await;
    (dispatch_mode, session_map)
}

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
    dispatching: Arc<DashMap<String, String>>,
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
                    let (dispatch_mode, session_map) =
                        prepare_dispatch_context(&router_arc, tmux.batch_session_active()).await;
                    if let Err(e) = crate::engine::tick::tick_dispatch_tasks(
                        &backend,
                        &tmux,
                        &repo,
                        &runner,
                        &capture,
                        &semaphore,
                        &task_manager,
                        &weight_tx,
                        dispatch_mode,
                        &dispatching,
                        &store,
                        &session_map,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::router::{Router, RouterConfig};
    use std::time::Duration;
    use tokio::sync::{oneshot, Notify};

    #[tokio::test]
    async fn prepare_dispatch_context_drops_read_lock_before_awaiting_session_map() {
        let router_arc = Arc::new(RwLock::new(Router::new(RouterConfig::default())));
        let (tx, rx) = oneshot::channel::<SessionMap>();
        let waiting = Arc::new(Notify::new());
        let waiting_for_task = Arc::clone(&waiting);
        let router_for_task = Arc::clone(&router_arc);

        let task = tokio::spawn(async move {
            prepare_dispatch_context(&router_for_task, async move {
                waiting_for_task.notify_one();
                rx.await.unwrap_or_default()
            })
            .await
        });

        // Wait until the function has entered its awaited session-map future.
        waiting.notified().await;

        // If the read guard leaked across the await above, this write lock would block.
        let write_guard = tokio::time::timeout(Duration::from_secs(2), router_arc.write())
            .await
            .expect("router write lock blocked while dispatch subscriber awaited session map");
        drop(write_guard);

        let _ = tx.send(SessionMap::new());
        let _ = task.await.expect("prepare_dispatch_context task panicked");
    }
}
