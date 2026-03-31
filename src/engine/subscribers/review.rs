//! Reacts to NeedsReview events — spawns review agent immediately.

use crate::backends::{ExternalBackend, ExternalId, Status};
use crate::engine::dispatch_guard::DispatchGuard;
use crate::engine::events::TaskEvent;
use crate::engine::review::{review_and_merge, ReviewDecision, MAX_REVIEW_AGENT_FAILURES};
use crate::engine::router::Router;
use crate::engine::tasks::TaskManager;
use crate::github::http::GhHttp;
use crate::repo_context::REPO_CONTEXT;
use crate::store::{opt_store_get_task, store_set, TaskStatus, TaskStore};
use crate::tmux::TmuxManager;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::{RwLock, Semaphore};

/// Spawn a task that listens for NeedsReview events and triggers the review agent.
///
/// This is the sole trigger for NeedsReview → InReview transitions. The sync tick
/// no longer has a competing NeedsReview loop (removed in fix for issue #857 — both
/// paths firing simultaneously caused double failure-counter increments).
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
                        if crate::engine::tasks::is_internal_id(task_id)
                            || store.has_external_tasks(&repo).await
                        {
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

                    // If every known agent is currently in cooldown, wait until the
                    // earliest cooldown expires before attempting to dispatch the
                    // review. This prevents burning review attempts against
                    // rate-limited agents.
                    {
                        let agents = router.read().await.available_agents.clone();
                        let all_cooled = agents
                            .iter()
                            .all(|a| crate::engine::cooldown::is_agent_in_cooldown(a));
                        if all_cooled && !agents.is_empty() {
                            let now = chrono::Utc::now().timestamp();
                            let wait_secs = agents
                                .iter()
                                .filter_map(|a| crate::engine::cooldown::cooldown_until(a))
                                .min()
                                .map(|until| ((until - now).max(1)) as u64)
                                .unwrap_or(crate::engine::cooldown::AGENT_COOLDOWN_SECS as u64);
                            tracing::info!(
                                task_id,
                                wait_secs,
                                "all agents cooled — delaying review until cooldown expires"
                            );
                            tokio::time::sleep(std::time::Duration::from_secs(wait_secs)).await;
                        }
                    }

                    // Check model-level cooldowns before selecting an agent.
                    // This prevents dispatching multiple reviews to the same
                    // rate-limited model when events arrive concurrently.
                    // The review agent selection (review.rs) also checks this,
                    // but we check here to fail fast and avoid acquiring the
                    // semaphore if the model is already cooled.
                    {
                        let router_guard = router.read().await;
                        let config = &router_guard.config;
                        let mut all_models_cooled = true;
                        for agent in &router_guard.available_agents {
                            if config
                                .model_for_complexity(agent, "review", task_id)
                                .is_some()
                            {
                                all_models_cooled = false;
                                break;
                            }
                        }
                        if all_models_cooled && !router_guard.available_agents.is_empty() {
                            let now = chrono::Utc::now().timestamp();
                            let wait_secs = router_guard
                                .available_agents
                                .iter()
                                .filter_map(|a| crate::engine::cooldown::cooldown_until(a))
                                .min()
                                .map(|until| ((until - now).max(1)) as u64)
                                .unwrap_or(crate::engine::cooldown::MODEL_COOLDOWN_SECS as u64);
                            tracing::info!(
                                task_id,
                                wait_secs,
                                "all review models cooled — delaying review until cooldown expires"
                            );
                            tokio::time::sleep(std::time::Duration::from_secs(wait_secs)).await;
                        }
                    }

                    // Try to acquire a semaphore permit.
                    let permit = match semaphore.clone().try_acquire_owned() {
                        Ok(p) => p,
                        Err(_) => {
                            tracing::debug!(task_id, "all parallel slots busy; sync catch-up will re-fire when a slot opens");
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

                    crate::store::set_review_session_expected(&store, &repo, &task.id.0, true)
                        .await;

                    // Insert into dispatching set.
                    {
                        let mut guard = dispatching.lock().unwrap_or_else(|e| e.into_inner());
                        guard.insert(dispatch_key.clone());
                    }
                    // RAII guard — removes dispatch_key on drop even if the spawned task panics.
                    let dispatch_guard =
                        DispatchGuard::new(dispatching.clone(), dispatch_key.clone());

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
                            RateLimited,
                            Block(String),
                            Ok,
                        }

                        // Re-check agent cooldowns inside the spawned task. A concurrent
                        // review that ran between the outer dispatch check and now may have
                        // set a cooldown — bail early rather than burning a review run
                        // against a rate-limited agent.
                        let pre_check_all_cooled = {
                            let agents = router_c.read().await.available_agents.clone();
                            !agents.is_empty()
                                && agents
                                    .iter()
                                    .all(|a| crate::engine::cooldown::is_agent_in_cooldown(a))
                        };
                        if pre_check_all_cooled {
                            tracing::info!(
                                task_id = tid,
                                "all agents cooled at review spawn time — deferring without running agent"
                            );
                            // Fall through to RateLimited outcome handling below, which
                            // waits for the cooldown to expire and resets to NeedsReview.
                            crate::store::set_review_session_expected(
                                &store_c,
                                &repo_s,
                                &tid,
                                false,
                            )
                            .await;
                            let wait_secs = {
                                let agents = router_c.read().await.available_agents.clone();
                                let now = chrono::Utc::now().timestamp();
                                agents
                                    .iter()
                                    .filter_map(|a| crate::engine::cooldown::cooldown_until(a))
                                    .min()
                                    .map(|until| ((until - now).max(1)) as u64)
                                    .unwrap_or(crate::engine::cooldown::AGENT_COOLDOWN_SECS as u64)
                            };
                            tracing::info!(
                                task_id = tid,
                                wait_secs,
                                "pre-check: deferring review retry due to agent rate limit cooldown"
                            );
                            tokio::time::sleep(std::time::Duration::from_secs(wait_secs)).await;
                            if let Err(e) = task_manager_c
                                .update_task_status(
                                    &ExternalId(tid.clone()),
                                    Status::NeedsReview,
                                )
                                .await
                            {
                                tracing::error!(task_id = %tid, err = %e, "pre-check: update_task_status(NeedsReview) failed — task may be stuck in InReview");
                            }
                            drop(permit);
                            return;
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
                                ReviewOutcome::Block(format!(
                                    "review gate blocked after repeated failures: {reason}"
                                ))
                            }
                            Ok(ReviewDecision::Failed(reason)) => {
                                // Rate-limit failures are not real review failures — don't count
                                // them toward MAX_REVIEW_AGENT_FAILURES. The cooldown system will
                                // steer the next attempt to an available agent.
                                if reason.to_lowercase().contains("rate limit") {
                                    tracing::warn!(
                                        task_id = tid,
                                        reason,
                                        "review agent hit rate limit — deferring retry until cooldown expires"
                                    );
                                    ReviewOutcome::RateLimited
                                } else {
                                    let failures = crate::store::store_increment(
                                        &Some(store_c.clone()),
                                        &repo_s,
                                        &tid,
                                        "review_agent_failures",
                                    )
                                    .await;
                                    let blocking = failures >= MAX_REVIEW_AGENT_FAILURES;
                                    // Post failure comment to the PR so the history is visible.
                                    post_review_failure_comment(
                                        &store_c, &repo_s, &tid, &reason, failures, blocking,
                                    ).await;
                                    if blocking {
                                        tracing::error!(
                                            task_id = tid,
                                            reason,
                                            failures,
                                            "review agent failed too many times — blocking task"
                                        );
                                        ReviewOutcome::Block(format!(
                                            "review agent failed {failures} times: {reason}"
                                        ))
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
                            }
                            Err(e) => {
                                let reason = format!("{e:#}");
                                // Rate-limit errors don't count against the failure threshold.
                                if reason.to_lowercase().contains("rate limit") {
                                    tracing::warn!(
                                        task_id = tid,
                                        reason,
                                        "review_and_merge hit rate limit — deferring retry until cooldown expires"
                                    );
                                    ReviewOutcome::RateLimited
                                } else {
                                    let failures = crate::store::store_increment(
                                        &Some(store_c.clone()),
                                        &repo_s,
                                        &tid,
                                        "review_agent_failures",
                                    )
                                    .await;
                                    let blocking = failures >= MAX_REVIEW_AGENT_FAILURES;
                                    post_review_failure_comment(
                                        &store_c, &repo_s, &tid, &reason, failures, blocking,
                                    ).await;
                                    if blocking {
                                        tracing::error!(
                                            task_id = tid,
                                            error = %e,
                                            failures,
                                            "review_and_merge failed too many times — blocking task"
                                        );
                                        ReviewOutcome::Block(format!(
                                            "review_and_merge failed {failures} times: {reason}"
                                        ))
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
                            }
                            Ok(ReviewDecision::Approve) | Ok(ReviewDecision::Skipped) => {
                                // On approval or skip we want to clear transient failure
                                // counters (per-attempt noise) but preserve the
                                // `review_cycles` counter which tracks how many
                                // times the PR has requested changes and is used
                                // as a circuit-breaker. Resetting all counters
                                // here (including `review_cycles`) enabled an
                                // approval loop when `auto_close_task_on_approval`
                                // is disabled — keep only the failure counters.
                                crate::store::store_reset_failure_counters(
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
                                crate::store::store_reset_failure_counters(
                                    &Some(store_c.clone()),
                                    &repo_s,
                                    &tid,
                                )
                                .await;
                                ReviewOutcome::Ok
                            }
                        };
                        crate::store::set_review_session_expected(
                            &store_c,
                            &repo_s,
                            &tid,
                            false,
                        )
                        .await;
                        match outcome {
                            ReviewOutcome::Reset => {
                                // Kill any stale tmux review session before resetting — the
                                // session may still be alive if the agent hit a rate limit,
                                // if two concurrent reviewers raced and the loser's spawn
                                // failed with "duplicate session", or if review_and_merge
                                // returned an error before reaching its own cleanup.
                                // Killing a non-existent session is a harmless no-op.
                                let stale_session =
                                    tmux_c.session_name(&repo_s, &format!("{}-review", tid));
                                tmux_c.kill_session(&stale_session).await.ok();
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
                            ReviewOutcome::RateLimited => {
                                // Kill stale tmux session — same as Reset.
                                let stale_session =
                                    tmux_c.session_name(&repo_s, &format!("{}-review", tid));
                                tmux_c.kill_session(&stale_session).await.ok();

                                // Compute wait duration: if any agent is available right now,
                                // use a short backoff (30 s). If every agent is cooled, sleep
                                // until the earliest cooldown expires so we don't spin.
                                let wait_secs = {
                                    let agents = router_c.read().await.available_agents.clone();
                                    let any_available = agents
                                        .iter()
                                        .any(|a| !crate::engine::cooldown::is_agent_in_cooldown(a));
                                    if any_available {
                                        30u64
                                    } else {
                                        let now = chrono::Utc::now().timestamp();
                                        agents
                                            .iter()
                                            .filter_map(|a| crate::engine::cooldown::cooldown_until(a))
                                            .min()
                                            .map(|until| ((until - now).max(1)) as u64)
                                            .unwrap_or(crate::engine::cooldown::AGENT_COOLDOWN_SECS as u64)
                                    }
                                };
                                tracing::info!(
                                    task_id = tid,
                                    wait_secs,
                                    "deferring review retry due to agent rate limit cooldown"
                                );
                                tokio::time::sleep(std::time::Duration::from_secs(wait_secs)).await;
                                if let Err(e) = task_manager_c
                                    .update_task_status(
                                        &ExternalId(tid.clone()),
                                        Status::NeedsReview,
                                    )
                                    .await
                                {
                                    tracing::error!(task_id = %tid, err = %e, "update_task_status(NeedsReview) failed after rate limit backoff — task may be stuck in InReview");
                                }
                            }
                            ReviewOutcome::Block(reason) => {
                                store_set(
                                    &Some(store_c.clone()),
                                    &repo_s,
                                    &tid,
                                    &[
                                        (
                                            "block_reason",
                                            serde_json::json!("review agent blocked — exceeded failure threshold"),
                                        ),
                                        ("last_error", serde_json::json!(reason)),
                                    ],
                                )
                                .await;
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

                    // Small delay between dispatches to allow cooldowns from
                    // early failures to propagate before subsequent reviews are
                    // dispatched. This prevents batch dispatch of N reviews to
                    // the same rate-limited agent when events arrive concurrently.
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
                Ok(_) => {} // Not a needs_review event or different repo
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!(n, "review subscriber lagged; sync catch-up will re-fire missed NeedsReview tasks");
                }
                Err(_) => break, // Channel closed
            }
        }
    });
}

/// Post a comment on the PR explaining why the review agent failed.
/// Best-effort — if the PR number is missing or the API call fails, we just log.
async fn post_review_failure_comment(
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
    reason: &str,
    failures: u64,
    blocking: bool,
) {
    let pr_number = match opt_store_get_task(&Some(store.clone()), repo, task_id).await {
        Some(t) if t.pr_number.is_some() => t.pr_number.unwrap(),
        _ => return, // No PR to comment on
    };

    let status = if blocking {
        "Task blocked for human review."
    } else {
        "Retrying with a different agent."
    };

    let comment = format!(
        "⚠️ Review agent failed (attempt {}/{})\n\n**Reason:** {}\n\n{}",
        failures, MAX_REVIEW_AGENT_FAILURES, reason, status,
    );

    let gh = match GhHttp::new() {
        Ok(gh) => gh,
        Err(_) => return,
    };
    if let Err(e) = gh.add_comment(repo, &pr_number.to_string(), &comment).await {
        tracing::debug!(
            task_id,
            pr_number,
            error = %e,
            "failed to post review failure comment"
        );
    }
}
