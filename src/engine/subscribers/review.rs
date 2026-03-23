//! Reacts to NeedsReview events — spawns review agent immediately.

use crate::backends::{ExternalBackend, ExternalId, Status};
use crate::engine::dispatch_guard::DispatchGuard;
use crate::engine::events::TaskEvent;
use crate::engine::review::{review_and_merge, ReviewDecision, MAX_REVIEW_AGENT_FAILURES};
use crate::engine::router::Router;
use crate::engine::tasks::TaskManager;
use crate::repo_context::REPO_CONTEXT;
use crate::store::{TaskStatus, TaskStore};
use crate::tmux::TmuxManager;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::{RwLock, Semaphore};

/// Spawn a task that listens for NeedsReview events and triggers the review agent.
///
/// This mirrors the catch-up logic in `sync_tick` (step 5) but triggers instantly
/// instead of waiting for the next sync interval. The `needs_review → in_review`
/// label transition is the atomic guard against duplicate review agents.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    mut rx: broadcast::Receiver<TaskEvent>,
    backend: Arc<dyn ExternalBackend>,
    tmux: Arc<TmuxManager>,
    semaphore: Arc<Semaphore>,
    task_manager: Arc<TaskManager>,
    router: Arc<RwLock<Router>>,
    dispatching: Arc<std::sync::Mutex<HashSet<String>>>,
    store: Arc<TaskStore>,
    repo: String,
) {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) if event.new_status == "needs_review" && event.repo == repo => {
                    // Check if review agent is enabled (read fresh each time).
                    let enable_review = crate::config::get("workflow.enable_review_agent")
                        .map(|v| v != "false")
                        .unwrap_or(true);
                    if !enable_review {
                        continue;
                    }

                    let task_id = &event.task_id;
                    let dispatch_key = format!("{}/{}", repo, task_id);

                    // Guard: skip if already being processed.
                    {
                        let guard = dispatching.lock().unwrap_or_else(|e| e.into_inner());
                        if guard.contains(&dispatch_key) {
                            tracing::debug!(
                                task_id,
                                "task locked by dispatch flow, skipping event-driven review"
                            );
                            continue;
                        }
                    }

                    tracing::info!(task_id, "event-driven review triggered");

                    // Look up the task from the store to get an ExternalTask.
                    let task = {
                        if store.has_tasks(&repo).await {
                            match store.list_by_status(&repo, TaskStatus::NeedsReview).await {
                                Ok(tasks) => tasks
                                    .iter()
                                    .find(|t| {
                                        let ext_id = t
                                            .external_id
                                            .clone()
                                            .unwrap_or_else(|| format!("internal:{}", t.id));
                                        ext_id == *task_id
                                    })
                                    .map(crate::engine::tasks::store_task_to_external),
                                Err(e) => {
                                    tracing::warn!(task_id, err = %e, "store lookup failed for review");
                                    None
                                }
                            }
                        } else {
                            // Fall back to backend
                            match backend.list_by_status(Status::NeedsReview).await {
                                Ok(tasks) => tasks.into_iter().find(|t| t.id.0 == *task_id),
                                Err(e) => {
                                    tracing::warn!(task_id, err = %e, "backend lookup failed for review");
                                    None
                                }
                            }
                        }
                    };

                    let Some(task) = task else {
                        tracing::debug!(
                            task_id,
                            "task not found in needs_review list (may have already transitioned)"
                        );
                        continue;
                    };

                    // Try to acquire a semaphore permit.
                    let permit = match semaphore.clone().try_acquire_owned() {
                        Ok(p) => p,
                        Err(_) => {
                            tracing::debug!(task_id, "all parallel slots busy, sync will catch up");
                            continue;
                        }
                    };

                    // Transition to InReview — this IS the atomic guard against duplicates.
                    if let Err(e) = task_manager
                        .update_task_status(&task.id, Status::InReview)
                        .await
                    {
                        tracing::warn!(task_id, err = %e, "failed to transition to InReview");
                        drop(permit);
                        continue;
                    }

                    // Insert into dispatching set.
                    {
                        let mut guard = dispatching.lock().unwrap_or_else(|e| e.into_inner());
                        guard.insert(dispatch_key.clone());
                    }
                    // RAII guard — removes dispatch_key on drop even if the spawned task panics.
                    let dispatch_guard = DispatchGuard::new(dispatching.clone(), dispatch_key.clone());

                    let backend_c = backend.clone();
                    let task_manager_c = task_manager.clone();
                    let tmux_c = tmux.clone();
                    let router_c = router.clone();
                    let store_c = store.clone();
                    let repo_s = repo.clone();
                    tokio::spawn(REPO_CONTEXT.scope(repo_s.clone(), async move {
                        let _dispatch_guard = dispatch_guard; // released on drop (normal or panic)
                        let tid = task.id.0.clone();
                        enum ReviewOutcome {
                            Reset,
                            Block,
                            Ok,
                        }
                        let outcome = match review_and_merge(
                            &task,
                            &backend_c,
                            &tmux_c,
                            &repo_s,
                            &router_c,
                            &task_manager_c,
                            &store_c,
                        )
                        .await
                        {
                            Ok(ReviewDecision::Blocked(reason)) => {
                                tracing::error!(
                                    task_id = tid,
                                    reason,
                                    "review gate blocked after repeated failures — marking task blocked"
                                );
                                ReviewOutcome::Block
                            }
                            Ok(ReviewDecision::Failed(reason)) => {
                                let failures = super::super::cleanup::store_increment(
                                    &Some(store_c.clone()),
                                    &repo_s,
                                    &tid,
                                    "review_agent_failures",
                                )
                                .await;
                                if failures >= MAX_REVIEW_AGENT_FAILURES {
                                    tracing::error!(
                                        task_id = tid,
                                        reason,
                                        failures,
                                        "review agent failed too many times — blocking task"
                                    );
                                    ReviewOutcome::Block
                                } else {
                                    tracing::error!(
                                        task_id = tid,
                                        reason,
                                        failures,
                                        "review agent failed — resetting to NeedsReview for retry"
                                    );
                                    ReviewOutcome::Reset
                                }
                            }
                            Err(e) => {
                                let failures = super::super::cleanup::store_increment(
                                    &Some(store_c.clone()),
                                    &repo_s,
                                    &tid,
                                    "review_agent_failures",
                                )
                                .await;
                                if failures >= MAX_REVIEW_AGENT_FAILURES {
                                    tracing::error!(
                                        task_id = tid,
                                        error = %e,
                                        failures,
                                        "review_and_merge failed too many times — blocking task"
                                    );
                                    ReviewOutcome::Block
                                } else {
                                    tracing::error!(
                                        task_id = tid,
                                        error = %e,
                                        failures,
                                        "review_and_merge failed — resetting to NeedsReview for retry"
                                    );
                                    ReviewOutcome::Reset
                                }
                            }
                            Ok(ReviewDecision::Approve) | Ok(ReviewDecision::Skipped) => {
                                super::super::cleanup::store_reset_counters(
                                    &Some(store_c.clone()),
                                    &repo_s,
                                    &tid,
                                )
                                .await;
                                ReviewOutcome::Ok
                            }
                            Ok(ReviewDecision::RequestChanges { .. }) => {
                                // handle_review_changes already incremented review_cycles —
                                // only reset transient per-attempt counters.
                                super::super::cleanup::store_reset_failure_counters(
                                    &Some(store_c.clone()),
                                    &repo_s,
                                    &tid,
                                )
                                .await;
                                ReviewOutcome::Ok
                            }
                        };
                        match outcome {
                            ReviewOutcome::Reset => {
                                // Backoff before re-queuing to prevent rapid spin
                                // if the review agent keeps failing instantly.
                                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                                if let Err(e) = task_manager_c
                                    .update_task_status(
                                        &ExternalId(tid.clone()),
                                        Status::NeedsReview,
                                    )
                                    .await
                                {
                                    tracing::error!(task_id = %tid, err = %e, "update_task_status(NeedsReview) failed — task may be stuck in InReview");
                                }
                            }
                            ReviewOutcome::Block => {
                                if let Err(e) = task_manager_c
                                    .update_task_status(
                                        &ExternalId(tid.clone()),
                                        Status::Blocked,
                                    )
                                    .await
                                {
                                    tracing::error!(task_id = %tid, err = %e, "update_task_status(Blocked) failed — task may be stuck in InReview");
                                }
                            }
                            ReviewOutcome::Ok => {}
                        }

                        drop(permit);
                        // _dispatch_guard dropped here — releases the per-task lock.
                    }));
                }
                Ok(_) => {} // Not a needs_review event or different repo
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!(n, "review subscriber lagged, sync will catch up");
                }
                Err(_) => break, // Channel closed
            }
        }
    });
}
