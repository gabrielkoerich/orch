//! Reacts to NeedsReview events — spawns review agent immediately.

use crate::backends::{ExternalBackend, ExternalId, ExternalTask, Status};
use crate::engine::cooldown::AGENT_COOLDOWN_SECS;
use crate::engine::dispatch_guard::DispatchGuard;
use crate::engine::events::TaskEvent;
use crate::engine::review::{review_and_merge, ReviewDecision, MAX_REVIEW_AGENT_FAILURES};
use crate::engine::review_gate::CiGate;
use crate::engine::review_shipcheck::Prerun;
use crate::engine::router::Router;
use crate::engine::tasks::TaskManager;
use crate::github::http::GhHttp;
use crate::repo_context::REPO_CONTEXT;
use crate::store::{opt_store_get_task, TaskStore};
use crate::tmux::TmuxManager;
use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::{RwLock, Semaphore};

enum ReviewOutcome {
    Reset,
    RateLimited,
    /// The PR was approved but the merge did not land for a reason outside the
    /// review (mergeability not computed yet, rate limit, transient GitHub error).
    /// The task moves to Approved so `review_open_prs` retries the merge, instead of
    /// resetting to NeedsReview (which spawns a redundant review agent)
    RetryMerge,
    /// The review approved the PR. The task moves to Approved, where orch waits for CI and
    /// merges with no agent running
    Approved,
    /// Re-route to the task agent (e.g. merge conflicts that need the agent to fix).
    Reroute,
    Block(String),
    Ok,
}

/// Spawn a task that listens for NeedsReview events and triggers the review agent.
///
/// This is the sole trigger for NeedsReview → InReview transitions. The sync tick
/// no longer has a competing NeedsReview loop (removed in fix for issue #857 — both
/// paths firing simultaneously caused double failure-counter increments).
#[expect(
    clippy::too_many_arguments,
    reason = "the arguments have no natural grouping"
)]
pub fn spawn(
    mut rx: broadcast::Receiver<TaskEvent>,
    backend: Arc<dyn ExternalBackend>,
    tmux: Arc<TmuxManager>,
    semaphore: Arc<Semaphore>,
    task_manager: Arc<TaskManager>,
    router: Arc<RwLock<Router>>,
    dispatching: Arc<DashMap<String, String>>,
    store: Arc<TaskStore>,
    repo: String,
) {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) if event.new_status == "needs_review" && event.repo == repo => {
                    // Check if review agent is enabled (read fresh each time).
                    let enable_review = crate::config::get("workflow.enable_review_agent")
                        .map_or(true, |v| v != "false");
                    if !enable_review {
                        continue;
                    }

                    let task_id = &event.task_id;
                    let dispatch_key = format!("{repo}/{task_id}");

                    // Atomically claim the task before any async work so concurrent
                    // review events cannot double-spawn the same review flow.
                    // Using DashMap lets us distinguish same-task duplicates (expected) from
                    // key collisions involving a different task (should never happen).
                    {
                        use dashmap::mapref::entry::Entry;
                        match dispatching.entry(dispatch_key.clone()) {
                            Entry::Occupied(existing) => {
                                let existing_id = existing.get().clone();
                                drop(existing);
                                if existing_id == *task_id {
                                    tracing::debug!(
                                        task_id,
                                        "task locked by dispatch flow, skipping event-driven review"
                                    );
                                } else {
                                    tracing::warn!(
                                        task_id,
                                        existing_task_id = existing_id,
                                        dispatch_key,
                                        "dispatch key collision: unexpected task already holds this key"
                                    );
                                }
                                continue;
                            }
                            Entry::Vacant(slot) => {
                                slot.insert(task_id.clone());
                            }
                        }
                    }

                    // Create guard IMMEDIATELY after inserting the key — owns removal for all subsequent exit paths
                    let dispatch_guard = DispatchGuard::new(
                        std::sync::Arc::clone(&dispatching),
                        dispatch_key.clone(),
                    );

                    tracing::info!(task_id, "event-driven review triggered");

                    // Look up the task from the store to get an ExternalTask.
                    let task = {
                        if crate::engine::tasks::is_internal_id(task_id)
                            || match store.has_external_tasks(&repo).await {
                                Ok(has_external) => has_external,
                                Err(e) => {
                                    tracing::warn!(
                                        repo = %repo,
                                        task_id,
                                        error = %e,
                                        "failed to check external task sentinel; keeping store-first review lookup"
                                    );
                                    true
                                }
                            }
                        {
                            // O(1) indexed lookup — resolve task_id to its SQLite row id,
                            // then fetch directly instead of scanning the whole needs_review set.
                            let result = if let Some(numeric_id) =
                                crate::engine::tasks::parse_internal_id(task_id)
                            {
                                store.get(numeric_id).await.map(Some)
                            } else {
                                store.get_by_external_id(&repo, task_id).await
                            };
                            match result {
                                Ok(Some(t))
                                    if t.status == crate::store::TaskStatus::NeedsReview =>
                                {
                                    Some(crate::engine::tasks::store_task_to_external(&t))
                                }
                                Ok(Some(t)) => {
                                    tracing::debug!(
                                        task_id,
                                        status = t.status.as_str(),
                                        "task found but not in needs_review status, skipping"
                                    );
                                    None
                                }
                                Ok(None) => None,
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

                    if let Some(stored) =
                        opt_store_get_task(&Some(std::sync::Arc::clone(&store)), &repo, &task.id.0)
                            .await
                    {
                        let no_code_review = stored.worktree.trim().is_empty()
                            && stored.branch.trim().is_empty()
                            && stored.pr_number.is_none();
                        if no_code_review {
                            tracing::info!(
                                task_id,
                                "no worktree/branch/pr for needs_review task — skipping review dispatch and marking done"
                            );
                            crate::store::store_reset_failure_counters(
                                &Some(std::sync::Arc::clone(&store)),
                                &repo,
                                &task.id.0,
                            )
                            .await;
                            if let Err(e) = crate::store::store_set_result(
                                &Some(std::sync::Arc::clone(&store)),
                                &repo,
                                &task.id.0,
                                &[("needs_review_refires", serde_json::json!(0))],
                            )
                            .await
                            {
                                tracing::warn!(
                                    task_id,
                                    err = %e,
                                    "failed to reset needs_review_refires while skipping no-code review"
                                );
                            }
                            if let Err(e) = task_manager
                                .update_task_status(&task.id, Status::Done)
                                .await
                            {
                                tracing::error!(
                                    task_id,
                                    err = %e,
                                    "failed to mark no-code needs_review task as done"
                                );
                            }
                            continue;
                        }
                    }

                    // If no agents are currently routable (all cooled, degraded, or without
                    // an available review model), skip this event and let sync re-fire when
                    // an agent is available again. Sleeping inline here would block every
                    // subsequent NeedsReview event for this repo for the duration of the
                    // cooldown (potentially hours/days), causing sync's rebroadcast loop to
                    // escalate piled-up tasks to Blocked.
                    {
                        let router_guard = router.read().await;
                        let no_routable = !router_guard.available_agents.is_empty()
                            && router_guard.healthy_agent_count("review") == 0;
                        drop(router_guard);
                        if no_routable {
                            tracing::info!(
                                task_id,
                                "no routable review agents — leaving task in needs_review for sync to re-fire"
                            );
                            continue;
                        }
                    }

                    if !ci_allows_review(&store, &task_manager, &repo, &task).await {
                        continue;
                    }

                    // Try to acquire a semaphore permit.
                    let Ok(permit) = std::sync::Arc::clone(&semaphore).try_acquire_owned() else {
                        tracing::debug!(
                            task_id,
                            "all parallel slots busy; sync catch-up will re-fire when a slot opens"
                        );
                        continue;
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

                    let backend_c = std::sync::Arc::clone(&backend);
                    let task_manager_c = std::sync::Arc::clone(&task_manager);
                    let tmux_c = std::sync::Arc::clone(&tmux);
                    let router_c = std::sync::Arc::clone(&router);
                    let store_c = std::sync::Arc::clone(&store);
                    let repo_s = repo.clone();
                    tokio::spawn(REPO_CONTEXT.scope(repo_s.clone(), async move {
                        let _dispatch_guard = dispatch_guard; // released on drop (normal or panic)
                        let tid = task.id.0.clone();

                        // Re-check agent routability inside the spawned task. A concurrent
                        // review that ran between the outer dispatch check and now may have
                        // set a cooldown — bail early rather than burning a review run
                        // against a rate-limited agent.
                        let pre_check_all_cooled = {
                            let r = router_c.read().await;
                            !r.available_agents.is_empty() && r.healthy_agent_count("review") == 0
                        };
                        if pre_check_all_cooled {
                            tracing::info!(
                                task_id = tid,
                                "all agents cooled at review spawn time — resetting to needs_review for sync to re-fire"
                            );
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
                                classify_review_failure(
                                    &store_c, &repo_s, &tid, &reason, "review agent",
                                )
                                .await
                            }
                            Err(e) => {
                                let reason = format!("{e:#}");
                                classify_review_failure(
                                    &store_c, &repo_s, &tid, &reason, "review_and_merge",
                                )
                                .await
                            }
                            Ok(ReviewDecision::Approve { .. }) => {
                                // On approval we clear transient failure counters
                                // but preserve the `review_cycles` counter which
                                // tracks how many times the PR has requested
                                // changes and is used as a circuit-breaker.
                                crate::store::store_reset_failure_counters(
                                    &Some(std::sync::Arc::clone(&store_c)),
                                    &repo_s,
                                    &tid,
                                )
                                .await;
                                // Also reset needs_review_refires so the sync backoff doesn't persist across a successful review
                                if let Err(e) = crate::store::store_set_result(
                                    &Some(std::sync::Arc::clone(&store_c)),
                                    &repo_s,
                                    &tid,
                                    &[("needs_review_refires", serde_json::json!(0))],
                                )
                                .await
                                {
                                    tracing::warn!(task_id = %tid, err = %e, "failed to write needs_review_refires to store");
                                }

                                // Approval only means the code looks good, the merge is the
                                // completion signal. review_open_prs merges Approved tasks and
                                // moves them to Done
                                ReviewOutcome::Approved
                            }
                            Ok(ReviewDecision::Rerouted) => {
                                // Task was re-routed because there was no PR and no commits.
                                // Do NOT reset failure counters — no_code_reroutes must accumulate
                                // so the max_reroute_attempts safety limit can be reached.
                                ReviewOutcome::Ok
                            }
                            Ok(ReviewDecision::Skipped) => {
                                // Skipped indicates there's nothing to review (no PR etc.).
                                // Clear transient failure counters but preserve review_cycles.
                                crate::store::store_reset_failure_counters(
                                    &Some(std::sync::Arc::clone(&store_c)),
                                    &repo_s,
                                    &tid,
                                )
                                .await;
                                if let Err(e) = crate::store::store_set_result(
                                    &Some(std::sync::Arc::clone(&store_c)),
                                    &repo_s,
                                    &tid,
                                    &[("needs_review_refires", serde_json::json!(0))],
                                )
                                .await
                                {
                                    tracing::warn!(task_id = %tid, err = %e, "failed to write needs_review_refires to store");
                                }
                                ReviewOutcome::Ok
                            }
                            Ok(ReviewDecision::RequestChanges { .. }) => {
                                // handle_review_changes already incremented review_cycles —
                                // only reset transient per-attempt counters.
                                crate::store::store_reset_failure_counters(
                                    &Some(std::sync::Arc::clone(&store_c)),
                                    &repo_s,
                                    &tid,
                                )
                                .await;
                                if let Err(e) = crate::store::store_set_result(
                                    &Some(std::sync::Arc::clone(&store_c)),
                                    &repo_s,
                                    &tid,
                                    &[("needs_review_refires", serde_json::json!(0))],
                                )
                                .await
                                {
                                    tracing::warn!(task_id = %tid, err = %e, "failed to write needs_review_refires to store");
                                }
                                ReviewOutcome::Ok
                            }
                        };
                        // Freshness guard: re-fetch the task's current status before applying
                        // any outcome transitions. A concurrent path (e.g. PR merge, manual
                        // close, or another review cycle) may have already moved the task out
                        // of `in_review`. If so, discard the stale outcome rather than
                        // overwriting the newer state.
                        //
                        // IMPORTANT: Distinguish between DB errors and legitimate status changes.
                        // A DB lookup failure should NOT be treated as "task moved out of in_review"
                        // — we should retain the review outcome rather than discarding it on
                        // transient errors.
                        let fetched: Result<Option<(crate::store::TaskStatus, Option<String>)>, anyhow::Error> = async {
                            let id = match store_c.resolve_task_id(&repo_s, &tid).await {
                                Ok(Some(id)) => id,
                                Ok(None) => return Ok(None), // Task doesn't exist — treat as missing
                                Err(e) => return Err(e),      // DB error — propagate, don't discard
                            };
                            let task = store_c.get(id).await?;
                            Ok(Some((task.status, task.block_reason)))
                        }
                        .await;

                        let (current_status, current_block_reason) = match fetched {
                            Ok(Some((status, block_reason))) => (Some(status), block_reason),
                            Ok(None) => {
                                // Task doesn't exist in store — treat as not in_review
                                (None, None)
                            }
                            Err(e) => {
                                // DB error — log and keep the review outcome instead of discarding
                                tracing::error!(
                                    task_id = tid,
                                    error = %e,
                                    "review outcome retained — DB lookup failed, not treating as stale"
                                );
                                // Skip the stale outcome check and proceed with the outcome
                                // by treating it as if the task is still InReview
                                (Some(crate::store::TaskStatus::InReview), None)
                            }
                        };

                        if !matches!(current_status, Some(crate::store::TaskStatus::InReview)) {
                            let ci_blocked = matches!(
                                current_status,
                                Some(crate::store::TaskStatus::Blocked)
                            ) && current_block_reason
                                .as_deref()
                                .is_some_and(|r| r.contains("CI failure limit"));
                            let expected = ci_blocked
                                || matches!(
                                    current_status,
                                    Some(crate::store::TaskStatus::Done | crate::store::TaskStatus::Approved |
crate::store::TaskStatus::Routed | crate::store::TaskStatus::New)
                                );
                            if expected {
                                tracing::debug!(
                                    task_id = tid,
                                    current_status = current_status.as_ref().map_or("unknown", crate::store::tasks::TaskStatus::as_str),
                                    "review outcome discarded — task transitioned away from in_review (expected)"
                                );
                            } else {
                                tracing::warn!(
                                    task_id = tid,
                                    current_status = current_status.as_ref().map_or("unknown", crate::store::tasks::TaskStatus::as_str),
                                    "review outcome discarded — task is no longer in_review (stale review agent result)"
                                );
                            }

                            // Kill the orphaned review session
                            let stale_session = tmux_c.session_name(&repo_s, &format!("{tid}-review"));
                            if let Err(e) = tmux_c.kill_session(&stale_session).await {
                                tracing::debug!(task_id = tid, error = %e, "failed to kill stale review session");
                            }

                            drop(permit);
                            return;
                        }

                        match outcome {
                            ReviewOutcome::Reset => {
                                // Kill any stale tmux review session before resetting — the
                                // session may still be alive if the agent hit a rate limit,
                                // if two concurrent reviewers raced and the loser's spawn
                                // failed with "duplicate session", or if review_and_merge
                                // returned an error before reaching its own cleanup.
                                // Killing a non-existent session is a harmless no-op.
                                let stale_session =
                                    tmux_c.session_name(&repo_s, &format!("{tid}-review"));
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
                            ReviewOutcome::Reroute => {
                                // Merge conflict or similar — route back to the
                                // task agent so it can fix the issue.
                                let stale_session =
                                    tmux_c.session_name(&repo_s, &format!("{tid}-review"));
                                tmux_c.kill_session(&stale_session).await.ok();
                                tracing::info!(
                                    task_id = tid,
                                    "re-routing to task agent to resolve merge conflict"
                                );
                                if let Err(e) = task_manager_c
                                    .update_task_status(
                                        &ExternalId(tid.clone()),
                                        Status::Routed,
                                    )
                                    .await
                                {
                                    tracing::error!(task_id = %tid, err = %e, "update_task_status(Routed) failed");
                                }
                            }
                            ReviewOutcome::Approved | ReviewOutcome::RetryMerge => {
                                // The review is over, so its session goes. The merge may have
                                // landed already, or CI sent the task back, so only in_review moves
                                let stale_session =
                                    tmux_c.session_name(&repo_s, &format!("{tid}-review"));
                                tmux_c.kill_session(&stale_session).await.ok();
                                match task_manager_c
                                    .update_task_status_if(
                                        &ExternalId(tid.clone()),
                                        Status::Approved,
                                        Status::InReview,
                                    )
                                    .await
                                {
                                    Ok(true) => tracing::info!(
                                        task_id = tid,
                                        "approval received, task approved until the PR is merged"
                                    ),
                                    Ok(false) => tracing::debug!(
                                        task_id = tid,
                                        "approval received after the task left in_review"
                                    ),
                                    Err(e) => tracing::error!(
                                        task_id = %tid,
                                        err = %e,
                                        "update_task_status_if(Approved) failed, task may be stuck in in_review"
                                    ),
                                }
                            }
                            ReviewOutcome::RateLimited => {
                                // Kill stale tmux session — same as Reset.
                                let stale_session =
                                    tmux_c.session_name(&repo_s, &format!("{tid}-review"));
                                tmux_c.kill_session(&stale_session).await.ok();

                                // Compute wait duration: if any agent is available right now,
                                // use a short backoff (30 s). If every agent is cooled, sleep
                                // until the earliest cooldown expires so we don't spin.
                                let wait_secs = {
                                    let r = router_c.read().await;
                                    if r.healthy_agent_count("review") > 0 {
                                        30u64
                                    } else {
                                        let now = chrono::Utc::now().timestamp();
                                        r.earliest_cooldown_until(Some("review"))
                                            .map_or(AGENT_COOLDOWN_SECS as u64, |until| u64::try_from((until - now).max(1)).unwrap_or(1))
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
                                 // Persist block_reason BEFORE transitioning to Blocked to avoid
                                 // a race where auto_unblock sees a blocked task without a reason
                                 // and immediately unblocks it.
                                 let fields = [
                                     (
                                         "block_reason",
                                         serde_json::json!("review agent blocked — exceeded failure threshold"),
                                     ),
                                     ("last_error", serde_json::json!(reason)),
                                 ];
                                 if let Err(e) = task_manager_c
                                     .update_task_status_and_result(
                                         &ExternalId(tid.clone()),
                                         Status::Blocked,
                                         &fields,
                                     )
                                     .await
                                 {
                                     tracing::error!(task_id = %tid, err = %e, "update_task_status_and_result(Blocked) failed — task may be stuck in InReview");
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

/**
 * Record a failed review attempt on the task's activity timeline and hand the outcome
 * back to `classify_review_failure`, which returns it unchanged. Without this the only
 * trace of a failed attempt was the final `last_error` written at escalation, so
 * `orch task inspect` showed that a task gave up but never why each attempt failed.
 *
 * `external` marks a cause outside the review itself (agent rate limit, GitHub API
 * limit, no review model). Those also clear `needs_review_refires`: the refire quota
 * exists to catch a task the subscriber never picks up, and spending it on a cause the
 * review agent cannot control escalated healthy tasks to Blocked during a rate-limit
 * window. The retry itself is already handled by the `RateLimited` / `Reset` arms
 **/
/// The CI gate (#3904): true when the review may start. A task held for CI stays in needs_review
/// and its refire count goes back to 0, so the sync sweep checks it again and never blocks it
async fn ci_allows_review(
    store: &Arc<TaskStore>,
    task_manager: &Arc<TaskManager>,
    repo: &str,
    task: &ExternalTask,
) -> bool {
    let task_id = task.id.0.as_str();
    let stored = opt_store_get_task(&Some(std::sync::Arc::clone(store)), repo, task_id).await;
    let Some(pr_number) = stored
        .as_ref()
        .and_then(|t| t.pr_number)
        .and_then(|n| u64::try_from(n).ok())
    else {
        return true;
    };
    let worktree = stored
        .map(|t| std::path::PathBuf::from(t.worktree))
        .filter(|p| !p.as_os_str().is_empty() && p.exists());
    let gate = match GhHttp::new() {
        Ok(gh) => {
            crate::engine::review_gate::review_ci_gate(&gh, store, repo, task_id, pr_number).await
        }
        Err(e) => Err(e),
    };

    let hold = match gate {
        Ok(CiGate::Start) => return true,
        // The merge still waits for green CI, so a lookup error does not hold the review
        Err(e) => {
            tracing::warn!(task_id, pr_number, err = %e, "CI gate lookup failed, starting the review");
            return true;
        }
        Ok(CiGate::Red(failed)) => {
            route_back_on_red_ci(store, task_manager, repo, task, pr_number, &failed).await;
            return false;
        }
        Ok(CiGate::Wait) => "CI still running on the PR head, review waits",
        // The reviewer runs shipcheck when no check ran, so orch runs it first, outside the review's limit
        Ok(CiGate::NotStarted { waited_out }) => match local_ci_worktree(repo, worktree).await {
            Some(wt) => match crate::engine::review_shipcheck::prerun(&wt).await {
                Prerun::Ready => return true,
                Prerun::Running => "shipcheck running on the PR head, review waits",
            },
            None if waited_out => {
                tracing::warn!(
                    task_id,
                    pr_number,
                    "no CI ran on the PR head, starting the review anyway"
                );
                return true;
            }
            None => "no CI started on the PR head yet, review waits",
        },
    };

    tracing::info!(task_id, pr_number, "{hold}");
    if let Err(e) = crate::store::store_set_result(
        &Some(std::sync::Arc::clone(store)),
        repo,
        task_id,
        &[("needs_review_refires", serde_json::json!(0))],
    )
    .await
    {
        tracing::warn!(task_id, err = %e, "failed to reset needs_review_refires for a task held by the CI gate");
    }

    false
}

/// The task worktree, when the reviewer there would run `shipcheck run`
async fn local_ci_worktree(
    repo: &str,
    worktree: Option<std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    let wt = worktree?;
    let sandboxed = crate::engine::review::review_sandboxed(repo);
    let shipcheck = crate::engine::runner::agent::Shipcheck::detect(&wt)
        .await
        .for_prompt(sandboxed);
    (shipcheck == crate::engine::runner::agent::Shipcheck::Available).then_some(wt)
}

/// CI failed before the review, so the task agent fixes it without a review round
async fn route_back_on_red_ci(
    store: &Arc<TaskStore>,
    task_manager: &Arc<TaskManager>,
    repo: &str,
    task: &ExternalTask,
    pr_number: u64,
    failed: &[String],
) {
    let task_id = task.id.0.as_str();
    let failures = match crate::store::store_increment(
        &Some(std::sync::Arc::clone(store)),
        repo,
        task_id,
        "ci_merge_failures",
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(task_id, err = %e, "failed to increment ci_merge_failures, review waits");
            return;
        }
    };
    let checks = failed.join(", ");
    let max = crate::engine::auto_merge::MAX_CI_MERGE_FAILURES;
    let (status, fields) = if failures >= max {
        tracing::error!(
            task_id,
            pr_number,
            failures,
            checks,
            "CI failed before review, CI failure limit reached"
        );
        (
            Status::Blocked,
            vec![(
                "block_reason",
                serde_json::json!(format!("CI failure limit ({max}) reached before review")),
            )],
        )
    } else {
        tracing::warn!(
            task_id,
            pr_number,
            failures,
            checks,
            "CI failed before review, routing back to the task agent"
        );
        (
            Status::Routed,
            vec![
                (
                    "last_error",
                    serde_json::json!(format!(
                        "CI failed on PR #{pr_number} before review: {checks}. Fix the failing checks"
                    )),
                ),
                ("route_reason", serde_json::json!("re-dispatch after CI failed before review")),
            ],
        )
    };
    if let Err(e) = task_manager
        .update_task_status_and_result(&task.id, status, &fields)
        .await
    {
        tracing::error!(task_id, err = %e, "failed to move a task whose CI failed before review");
    }
}

async fn recorded(
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
    reason: &str,
    cause: &str,
    external: bool,
    outcome: ReviewOutcome,
) -> ReviewOutcome {
    let store_id = match store.resolve_task_id(repo, task_id).await {
        Ok(Some(id)) => id,
        Ok(None) => {
            tracing::warn!(
                task_id,
                "review attempt not recorded — task not found in store"
            );
            return outcome;
        }
        Err(e) => {
            tracing::warn!(task_id, err = %e, "review attempt not recorded — task id lookup failed");
            return outcome;
        }
    };

    if let Err(e) = store
        .append_activity(
            store_id,
            "review_attempt_failed",
            None,
            None,
            None,
            None,
            Some(&serde_json::json!({
                "cause": cause,
                "external": external,
                "reason": reason,
            })),
        )
        .await
    {
        tracing::warn!(task_id, err = %e, "failed to record review attempt failure");
    }

    if external {
        if let Err(e) = store
            .set_fields(store_id, &[("needs_review_refires", serde_json::json!(0))])
            .await
        {
            tracing::warn!(task_id, err = %e, "failed to clear needs_review_refires after external review failure");
        }
    }

    outcome
}

/// Resets that end with no decision and do not count as a failure, before the task blocks
pub(crate) const MAX_REVIEW_RESETS: u64 = 6;

/// A reset that does not count toward `MAX_REVIEW_AGENT_FAILURES` still raises `review_resets`,
/// so a review that never produces a decision stops bouncing between needs_review and in_review
async fn reset_or_block(
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
    reason: &str,
    cause: &str,
    external: bool,
    context: &str,
) -> ReviewOutcome {
    let resets = match crate::store::store_increment(
        &Some(std::sync::Arc::clone(store)),
        repo,
        task_id,
        "review_resets",
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(task_id, err = %e, "failed to increment review_resets");
            0
        }
    };
    let outcome = if resets >= MAX_REVIEW_RESETS {
        tracing::error!(
            task_id,
            reason,
            resets,
            "{context} ended {resets} times without a decision, blocking task"
        );
        ReviewOutcome::Block(format!(
            "{context} ended {resets} times without a decision: {reason}"
        ))
    } else {
        ReviewOutcome::Reset
    };
    recorded(store, repo, task_id, reason, cause, external, outcome).await
}

/// Classify a review failure and return the appropriate outcome.
///
/// Both the `Ok(ReviewDecision::Failed)` and `Err(e)` arms share the same
/// logic; callers convert the error to a string before calling this helper.
/// `context` is used in log messages to distinguish the two call sites
/// ("review agent" vs "review_and_merge").
async fn classify_review_failure(
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
    reason: &str,
    context: &str,
) -> ReviewOutcome {
    // Rate-limit and auth failures are not real review failures — don't count them
    // toward MAX_REVIEW_AGENT_FAILURES. The cooldown system steers the next
    // attempt to an available agent.
    let lower_reason = reason.to_lowercase();

    // Parse errors are orch's fault, not review feedback. Re-route for retry
    // without counting toward the failure threshold, so parse errors don't
    // incorrectly block tasks that have no actual review feedback.
    if lower_reason.contains("parse error") {
        tracing::warn!(
            task_id,
            reason,
            "{context} hit parse error — re-routing for retry without counting as failure"
        );
        return reset_or_block(store, repo, task_id, reason, "parse_error", false, context).await;
    }

    // Session exited mid-thought with no result envelope (issue #3734). Same
    // treatment as a parse error: retry without counting toward the failure cap
    if lower_reason.contains("incomplete:") {
        tracing::warn!(
            task_id,
            reason,
            "{context} session incomplete — re-routing for retry without counting as failure"
        );
        return reset_or_block(
            store,
            repo,
            task_id,
            reason,
            "incomplete_session",
            false,
            context,
        )
        .await;
    }

    // The review output was cut off by the model's output/reasoning token
    // budget, not a genuine parse/format failure. Same treatment as a parse
    // error: re-route without counting toward MAX_REVIEW_AGENT_FAILURES.
    if lower_reason.contains("truncated") {
        tracing::warn!(
            task_id,
            reason,
            "{context} output truncated by token budget — re-routing for retry without counting as failure"
        );
        return reset_or_block(
            store,
            repo,
            task_id,
            reason,
            "truncated_output",
            false,
            context,
        )
        .await;
    }

    // Transient mergeability check: GitHub hasn't finished computing yet.
    // This is a normal, expected transient state for freshly-pushed PRs.
    // Return RetryMerge (not Reset) so the task stays in InReview — avoids
    // spawning a redundant review agent that would just re-approve anyway.
    if lower_reason.contains("not yet computed") {
        tracing::warn!(
            task_id,
            reason,
            "{context} PR mergeability check still pending — will retry merge without re-reviewing"
        );
        return recorded(
            store,
            repo,
            task_id,
            reason,
            "mergeability_pending",
            true,
            ReviewOutcome::RetryMerge,
        )
        .await;
    }

    // Merge conflicts should be handled by handle_merge_conflict() in
    // auto_merge.rs (which routes to the task agent). If a conflict error
    // leaks here, route to the task agent instead of resetting to NeedsReview
    // (which would re-spawn the review agent in a loop).
    if lower_reason.contains("merge conflict") || lower_reason.contains("not mergeable") {
        tracing::warn!(
            task_id,
            reason,
            "{context} hit merge conflict — routing to task agent to resolve"
        );
        return recorded(
            store,
            repo,
            task_id,
            reason,
            "merge_conflict",
            false,
            ReviewOutcome::Reroute,
        )
        .await;
    }

    // Any other merge failure on approved work (rate limit, 5xx, stale lease, a check
    // still pending) is orch's problem, not the agent's. Keep the task in InReview so
    // review_open_prs retries the merge once the cause clears, and never re-dispatch the
    // agent — that burned attempts until the task blocked on work that was already done
    if lower_reason.contains("merge failed") {
        tracing::warn!(
            task_id,
            reason,
            "{context} merge failed without a conflict — retrying the merge, not re-dispatching the agent"
        );
        return recorded(
            store,
            repo,
            task_id,
            reason,
            "merge_retry",
            true,
            ReviewOutcome::RetryMerge,
        )
        .await;
    }

    // No configured review model for any available agent, or the selected agent
    // has no review model. This is a configuration/cooldown gap, not a review
    // failure. Defer without incrementing the failure counter so the task retries
    // once a review model is available.
    if lower_reason.contains("no review agent") || lower_reason.contains("review model missing") {
        tracing::warn!(
            task_id,
            reason,
            "{context}: no review model available — deferring retry until one is available"
        );
        return recorded(
            store,
            repo,
            task_id,
            reason,
            "no_review_model",
            true,
            ReviewOutcome::RateLimited,
        )
        .await;
    }

    // GhHttp's own throttle says "rate-limited", and GitHub answers 429 or abuse detection
    // without the words "rate limit"
    if lower_reason.contains("rate limit")
        || lower_reason.contains("rate-limited")
        || lower_reason.contains("rate limited")
        || lower_reason.contains("too many requests")
        || lower_reason.contains("abuse detection")
        || lower_reason.contains("failed (429)")
        || lower_reason.contains("auth error")
    {
        tracing::warn!(
            task_id,
            reason,
            "{context} hit rate limit or auth error — deferring retry until cooldown expires"
        );
        return recorded(
            store,
            repo,
            task_id,
            reason,
            "rate_limit",
            true,
            ReviewOutcome::RateLimited,
        )
        .await;
    }

    if crate::engine::runner::git_ops::is_transient_github_api_error(reason) {
        tracing::warn!(
            task_id,
            reason,
            "{context} hit transient GitHub error — deferring retry without counting as failure"
        );
        return reset_or_block(
            store,
            repo,
            task_id,
            reason,
            "github_api_error",
            true,
            context,
        )
        .await;
    }

    // Provider-side network errors (ConnectionRefused, timeouts, etc.) are
    // transient infrastructure issues, not review feedback. Re-route without
    // counting toward MAX_REVIEW_AGENT_FAILURES.
    if crate::engine::runner::agents::patterns::detect_network_error(reason).is_some() {
        tracing::warn!(
            task_id,
            reason,
            "{context} hit transient provider network error — re-routing for retry without counting as failure"
        );
        return reset_or_block(
            store,
            repo,
            task_id,
            reason,
            "provider_network_error",
            true,
            context,
        )
        .await;
    }

    let failures = match crate::store::store_increment(
        &Some(std::sync::Arc::clone(store)),
        repo,
        task_id,
        "review_agent_failures",
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(task_id, err = %e, "failed to increment review_agent_failures — skipping failure escalation this tick");
            // Treat as 0 for now (no escalation)
            0
        }
    };
    let blocking = failures >= MAX_REVIEW_AGENT_FAILURES;
    // Post failure comment to the PR so the history is visible.
    post_review_failure_comment(store, repo, task_id, reason, failures, blocking).await;
    if blocking {
        tracing::error!(
            task_id,
            reason,
            failures,
            "{context} failed too many times — blocking task"
        );
        recorded(
            store,
            repo,
            task_id,
            reason,
            "review_failure",
            false,
            ReviewOutcome::Block(format!("{context} failed {failures} times: {reason}")),
        )
        .await
    } else {
        tracing::error!(
            task_id,
            reason,
            failures,
            "{context} failed — resetting to NeedsReview for retry"
        );
        recorded(
            store,
            repo,
            task_id,
            reason,
            "review_failure",
            false,
            ReviewOutcome::Reset,
        )
        .await
    }
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
    let pr_number =
        match opt_store_get_task(&Some(std::sync::Arc::clone(store)), repo, task_id).await {
            Some(t) => match t.pr_number {
                Some(n) => n,
                None => return, // No PR to comment on
            },
            _ => return, // No PR to comment on
        };

    let status = if blocking {
        "Task blocked for human review."
    } else {
        "Retrying with a different agent."
    };

    let comment = format!(
        "⚠️ Review agent failed (attempt {}/{})\n\n**Reason:** {}\n\n{}{}",
        failures,
        MAX_REVIEW_AGENT_FAILURES,
        reason,
        status,
        crate::engine::orch_footer()
    );

    let Ok(gh) = GhHttp::new() else { return };
    if let Err(e) = gh.add_comment(repo, &pr_number.to_string(), &comment).await {
        tracing::debug!(
            task_id,
            pr_number,
            error = %e,
            "failed to post review failure comment"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An incomplete mid-thought review (no result envelope) must reset for
    /// retry without counting toward MAX_REVIEW_AGENT_FAILURES (issue #3734).
    #[tokio::test]
    async fn classify_review_failure_incomplete_resets_without_counting() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let repo = "owner/repo";
        let store_id = store
            .create_internal(
                repo,
                "Incomplete review test",
                "",
                "review",
                "review-3734",
                None,
            )
            .await
            .unwrap();
        let task_id = format!("internal:{store_id}");

        for _ in 0..=MAX_REVIEW_AGENT_FAILURES {
            let outcome = classify_review_failure(
                &store,
                repo,
                &task_id,
                "incomplete: review session exited before producing a decision",
                "review agent",
            )
            .await;
            assert!(
                matches!(outcome, ReviewOutcome::Reset),
                "incomplete review failure must reset for retry, not block"
            );
        }

        let task = opt_store_get_task(&Some(std::sync::Arc::clone(&store)), repo, &task_id)
            .await
            .expect("task should exist");
        assert_eq!(
            task.review_agent_failures, 0,
            "incomplete failures must not count toward review_agent_failures"
        );
    }

    /// A truncated review output (opencode step_finish reason=length) must be
    /// treated like a parse error: reset for retry without counting toward
    /// MAX_REVIEW_AGENT_FAILURES, so a run of token-budget flukes never
    /// permanently blocks the task (issue #3473).
    #[tokio::test]
    async fn classify_review_failure_truncated_resets_without_counting() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let repo = "owner/repo";
        let store_id = store
            .create_internal(
                repo,
                "Truncated review test",
                "",
                "review",
                "review-1",
                None,
            )
            .await
            .unwrap();
        let task_id = format!("internal:{store_id}");

        for _ in 0..=MAX_REVIEW_AGENT_FAILURES {
            let outcome = classify_review_failure(
                &store,
                repo,
                &task_id,
                "truncated: review output was cut off before completion \
                 (output/reasoning token budget exceeded)",
                "review agent",
            )
            .await;
            assert!(
                matches!(outcome, ReviewOutcome::Reset),
                "truncated review failure must reset for retry, not block"
            );
        }

        let task = opt_store_get_task(&Some(std::sync::Arc::clone(&store)), repo, &task_id)
            .await
            .expect("task should exist");
        assert_eq!(
            task.review_agent_failures, 0,
            "truncated failures must not count toward review_agent_failures"
        );
    }

    /**
     * After an approved review, a merge that fails for anything other than a content
     * conflict must retry the merge with the task still in InReview. Re-dispatching the
     * agent re-ran finished work until the task hit the attempt limit (#3783)
     **/
    #[tokio::test]
    async fn classify_review_failure_non_conflict_merge_failure_retries_merge() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let repo = "owner/repo";
        let store_id = store
            .create_internal(repo, "Merge retry", "", "review", "review-3783", None)
            .await
            .unwrap();
        let task_id = format!("internal:{store_id}");

        let error_messages = [
            "merge failed: GitHub REST API rate-limited, skipping call for 187s",
            "merge failed: GitHub PR merge failed (502 Bad Gateway)",
            "merge failed: required status check still pending",
        ];

        for _ in 0..=MAX_REVIEW_AGENT_FAILURES {
            for msg in &error_messages {
                let outcome =
                    classify_review_failure(&store, repo, &task_id, msg, "review agent").await;
                assert!(
                    matches!(outcome, ReviewOutcome::RetryMerge),
                    "non-conflict merge failure '{msg}' must retry the merge, not re-dispatch the agent"
                );
            }
        }

        let task = opt_store_get_task(&Some(std::sync::Arc::clone(&store)), repo, &task_id)
            .await
            .expect("task should exist");
        assert_eq!(
            task.review_agent_failures, 0,
            "a stuck merge is not a review failure"
        );
    }

    /// A genuine content conflict is the one merge failure the agent has to fix
    #[tokio::test]
    async fn classify_review_failure_merge_conflict_still_reroutes() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let repo = "owner/repo";
        let store_id = store
            .create_internal(repo, "Merge conflict", "", "review", "review-3783b", None)
            .await
            .unwrap();
        let task_id = format!("internal:{store_id}");

        for msg in [
            "merge failed: PR is not mergeable",
            "merge conflict detected on PR #12",
        ] {
            let outcome =
                classify_review_failure(&store, repo, &task_id, msg, "review agent").await;
            assert!(
                matches!(outcome, ReviewOutcome::Reroute),
                "content conflict '{msg}' must go back to the task agent"
            );
        }
    }

    /// Transient provider network errors (ConnectionRefused, timeouts) must be
    /// treated like parse errors: reset for retry without counting toward
    /// MAX_REVIEW_AGENT_FAILURES (issue #3482).
    #[tokio::test]
    async fn classify_review_failure_network_error_resets_without_counting() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let repo = "owner/repo";
        let store_id = store
            .create_internal(
                repo,
                "Network error review test",
                "",
                "review",
                "review-2",
                None,
            )
            .await
            .unwrap();
        let task_id = format!("internal:{store_id}");

        let error_messages = [
            "API Error: Unable to connect to API (ConnectionRefused)",
            "connection refused",
            "ECONNREFUSED",
            "Upstream idle timeout exceeded",
            "connect ETIMEDOUT",
            "network request failed",
        ];

        for msg in &error_messages {
            // Each message on its own, the reset cap has its own test
            store
                .set_fields(store_id, &[("review_resets", serde_json::json!(0))])
                .await
                .unwrap();
            let outcome =
                classify_review_failure(&store, repo, &task_id, msg, "review agent").await;
            assert!(
                matches!(outcome, ReviewOutcome::Reset),
                "network error '{msg}' must reset for retry, not block"
            );
        }

        let task = opt_store_get_task(&Some(std::sync::Arc::clone(&store)), repo, &task_id)
            .await
            .expect("task should exist");
        assert_eq!(
            task.review_agent_failures, 0,
            "network errors must not count toward review_agent_failures"
        );
    }

    /// When no agent has a configured review model (or the fallback agent has none),
    /// the review must be deferred, not counted as a review failure. Counting it
    /// would eventually block tasks for a configuration/cooldown gap (issue #3584).
    #[tokio::test]
    async fn classify_review_failure_missing_review_model_is_rate_limited() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let repo = "owner/repo";
        let store_id = store
            .create_internal(
                repo,
                "Missing review model test",
                "",
                "review",
                "review-3",
                None,
            )
            .await
            .unwrap();
        let task_id = format!("internal:{store_id}");

        let error_messages = [
            "no review agent with a configured review model is available for task internal:123",
            "review model missing for task internal:123 after agent selection — refusing to dispatch",
        ];

        for _ in 0..=MAX_REVIEW_AGENT_FAILURES {
            for msg in &error_messages {
                let outcome =
                    classify_review_failure(&store, repo, &task_id, msg, "review_and_merge").await;
                assert!(
                    matches!(outcome, ReviewOutcome::RateLimited),
                    "missing review model error '{msg}' must defer, not block"
                );
            }
        }

        let task = opt_store_get_task(&Some(std::sync::Arc::clone(&store)), repo, &task_id)
            .await
            .expect("task should exist");
        assert_eq!(
            task.review_agent_failures, 0,
            "missing review model errors must not count toward review_agent_failures"
        );
    }

    /// Resets that do not count as failures still stop at `MAX_REVIEW_RESETS`
    #[tokio::test]
    async fn classify_review_failure_blocks_after_max_resets() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let repo = "owner/repo";
        let store_id = store
            .create_internal(repo, "Looping review", "", "review", "review-resets", None)
            .await
            .unwrap();
        let task_id = format!("internal:{store_id}");
        let msg = "parse error: failed to parse review response";

        for attempt in 1..MAX_REVIEW_RESETS {
            let outcome =
                classify_review_failure(&store, repo, &task_id, msg, "review agent").await;
            assert!(
                matches!(outcome, ReviewOutcome::Reset),
                "reset {attempt} must retry the review"
            );
        }
        let outcome = classify_review_failure(&store, repo, &task_id, msg, "review agent").await;
        assert!(
            matches!(outcome, ReviewOutcome::Block(ref r) if r.contains("without a decision")),
            "reset {MAX_REVIEW_RESETS} must block the task"
        );

        let task = opt_store_get_task(&Some(std::sync::Arc::clone(&store)), repo, &task_id)
            .await
            .expect("task should exist");
        assert_eq!(
            u64::try_from(task.review_resets).unwrap(),
            MAX_REVIEW_RESETS
        );
        assert_eq!(
            task.review_agent_failures, 0,
            "a reset must not count as a review failure"
        );
    }

    /**
     * A review that fails on a rate limit is retried once the cooldown clears, and must
     * never spend the rebroadcast quota: `needs_review_refires` is cleared so the sync
     * sweep cannot escalate the task to Blocked for a cause outside the review (#3767)
     **/
    #[tokio::test]
    async fn classify_review_failure_rate_limit_clears_refire_quota() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let repo = "owner/repo";
        let store_id = store
            .create_internal(
                repo,
                "Rate limited review",
                "",
                "review",
                "review-3767",
                None,
            )
            .await
            .unwrap();
        let task_id = format!("internal:{store_id}");

        let error_messages = [
            "claude failed: rate limit reached for model sonnet",
            "GitHub API GET https://api.github.com/repos/owner/repo/pulls/65 failed (403): API rate limit exceeded",
            "GitHub core API rate-limited, skipping call for 1800s",
            "GitHub API GET https://api.github.com/repos/owner/repo/pulls/65 failed (429): Too Many Requests",
        ];

        // The sweep has already burned most of the quota before the rate-limit window
        store
            .set_fields(store_id, &[("needs_review_refires", serde_json::json!(5))])
            .await
            .unwrap();

        for _ in 0..=MAX_REVIEW_AGENT_FAILURES {
            for msg in &error_messages {
                let outcome =
                    classify_review_failure(&store, repo, &task_id, msg, "review agent").await;
                assert!(
                    matches!(outcome, ReviewOutcome::RateLimited | ReviewOutcome::Reset),
                    "rate limit '{msg}' must defer for retry, not block"
                );
            }
        }

        let task = opt_store_get_task(&Some(std::sync::Arc::clone(&store)), repo, &task_id)
            .await
            .expect("task should exist");
        assert_eq!(
            task.needs_review_refires, 0,
            "a rate limit must not consume the rebroadcast quota"
        );
        assert_eq!(
            task.review_agent_failures, 0,
            "rate limits must not count toward review_agent_failures"
        );
        assert_ne!(
            task.status,
            crate::store::TaskStatus::Blocked,
            "a rate-limited review must leave the task retryable"
        );

        let attempts = store
            .get_activity(store_id, None)
            .await
            .unwrap()
            .into_iter()
            .filter(|a| a.event_type == "review_attempt_failed")
            .collect::<Vec<_>>();
        assert_eq!(
            attempts.len(),
            (usize::try_from(MAX_REVIEW_AGENT_FAILURES).unwrap() + 1) * error_messages.len(),
            "every failed attempt must be recorded for `orch task inspect`"
        );
        assert!(
            attempts.iter().all(|a| a
                .details
                .get("external")
                .and_then(serde_json::Value::as_bool)
                == Some(true)),
            "rate limits must be recorded as an external cause"
        );
        assert!(
            attempts.iter().any(
                |a| a.details.get("reason").and_then(|v| v.as_str()) == Some(error_messages[0])
            ),
            "the recorded attempt must carry that attempt's error"
        );
    }
}
