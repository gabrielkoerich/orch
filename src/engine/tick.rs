//! Core tick loop phases.
//!
//! The engine ticks every ~10 seconds. Each tick runs six sequential phases:
//! 1. Poll tmux for finished sessions
//! 2. Recover stuck in-progress tasks
//! 3. Route and dispatch:
//!    - 3a. Route new tasks to agents
//!    - 3b. Dispatch routed tasks (spawn agents)
//! 4. Unblock parents whose children are all done
//! 5. Run cron job scheduler

use crate::backends::{ExternalBackend, ExternalId, ExternalTask, Status};
use crate::channels::capture::CaptureService;
use crate::channels::notification::TaskNotification;
use crate::channels::transport::Transport;
use crate::config;
use crate::db::Db;
use crate::engine::jobs;
use crate::engine::router::{get_route_result, Router};
use crate::engine::runner::{TaskRunner, WeightSignal};
use crate::engine::tasks::{is_internal_id, TaskManager};
use crate::sidecar::{self, REPO_CONTEXT};
use crate::store::TaskStore;
use crate::tmux::TmuxManager;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock, Semaphore};

use super::review::{review_and_merge, ReviewDecision, MAX_REVIEW_AGENT_FAILURES};
use super::EngineConfig;

/// Phase 1 of tick: poll tmux for finished sessions and clean them up.
pub(crate) async fn tick_check_session_completions(
    tmux: &Arc<TmuxManager>,
    repo: &str,
    capture: &Arc<CaptureService>,
) -> anyhow::Result<()> {
    let _span = tracing::info_span!("engine.tick.phase1.sessions").entered();
    let session_snapshot = tmux.snapshot().await;
    for (task_id, active) in &session_snapshot {
        if !active {
            tracing::info!(task_id, "session completed, collecting results");
            // Unregister from capture service
            capture.unregister_session(task_id).await;
            // The runner handles status updates and GitHub comment posting.
            // We just clean up the session.
            let session_name = tmux.session_name(repo, task_id);
            if let Err(e) = tmux.kill_session(&session_name).await {
                tracing::debug!(
                    task_id,
                    ?e,
                    "kill_session failed (session may already be gone)"
                );
            }
        }
    }
    Ok(())
}

/// Phase 2 of tick: detect tasks stuck in_progress without an active tmux session and reset them.
pub(crate) async fn tick_recover_stuck_tasks(
    backend: &Arc<dyn ExternalBackend>,
    tmux: &Arc<TmuxManager>,
    repo: &str,
    task_manager: &Arc<TaskManager>,
    config: &EngineConfig,
) -> anyhow::Result<()> {
    let _span = tracing::info_span!("engine.tick.phase2.stuck_tasks").entered();
    let in_progress = task_manager
        .list_external_by_status(Status::InProgress)
        .await?;
    for task in &in_progress {
        let session_name = tmux.session_name(repo, &task.id.0);
        let has_session = tmux.session_exists(&session_name).await;

        let threshold = if has_session {
            config.stuck_timeout
        } else {
            config.no_session_stuck_timeout
        };

        let updated = match chrono::DateTime::parse_from_rfc3339(&task.updated_at) {
            Ok(dt) => dt.with_timezone(&chrono::Utc),
            Err(e) => {
                tracing::warn!(
                    task_id = task.id.0,
                    updated_at = task.updated_at,
                    ?e,
                    "cannot parse updated_at, skipping stuck-task check"
                );
                continue;
            }
        };
        let age = chrono::Utc::now() - updated;

        if age.num_seconds() > threshold as i64 {
            if has_session {
                tracing::warn!(
                    task_id = task.id.0,
                    age_mins = age.num_minutes(),
                    threshold_mins = threshold / 60,
                    "recovering stuck task: timed out with active session → new"
                );
            } else {
                tracing::warn!(
                    task_id = task.id.0,
                    age_mins = age.num_minutes(),
                    threshold_mins = threshold / 60,
                    "recovering stuck task: no session found — reclaiming early → new"
                );
            }
            // Remove stale agent label so the LLM router re-routes properly
            for label in &task.labels {
                if label.starts_with("agent:") {
                    backend.remove_label(&task.id, label).await.ok();
                }
            }
            if let Err(e) = sidecar::set(
                &task.id.0,
                &[
                    "agent=".to_string(),
                    "model=".to_string(),
                    "route_attempts=0".to_string(),
                ],
            ) {
                tracing::warn!(
                    task_id = task.id.0,
                    ?e,
                    "failed to reset sidecar for stuck task"
                );
                continue;
            }
            if let Err(e) = backend.update_status(&task.id, Status::New).await {
                tracing::warn!(task_id = task.id.0, ?e, "failed to reset stuck task status");
                continue;
            }
            let reason = if has_session {
                format!(
                    "timed out after {}m with active session (cleared agent for re-routing)",
                    age.num_minutes()
                )
            } else {
                format!(
                    "no session found — reclaiming early after {}m (cleared agent for re-routing)",
                    age.num_minutes()
                )
            };
            if let Err(e) = backend
                .post_comment(
                    &task.id,
                    &format!(
                        "[{}] recovered: stuck in_progress — {}{}",
                        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
                        reason,
                        crate::engine::orch_footer()
                    ),
                )
                .await
            {
                tracing::warn!(
                    task_id = task.id.0,
                    ?e,
                    "failed to post stuck-task recovery comment"
                );
                continue;
            }
        }
    }

    // Recover internal (SQLite) tasks stuck in in_progress.
    // These have no GitHub labels or comments — just reset the DB status to New.
    use crate::db::TaskStatus as DbStatus;
    let internal_in_progress = task_manager
        .db_list_internal_by_status(DbStatus::InProgress)
        .await?;
    for task in &internal_in_progress {
        let task_id = format!("internal:{}", task.id);
        let session_name = tmux.session_name(repo, &task_id);
        let has_session = tmux.session_exists(&session_name).await;

        let threshold = if has_session {
            config.stuck_timeout
        } else {
            config.no_session_stuck_timeout
        };

        let age = chrono::Utc::now() - task.updated_at;

        if age.num_seconds() > threshold as i64 {
            if has_session {
                tracing::warn!(
                    task_id,
                    age_mins = age.num_minutes(),
                    threshold_mins = threshold / 60,
                    "recovering stuck internal task: timed out with active session → new"
                );
            } else {
                tracing::warn!(
                    task_id,
                    age_mins = age.num_minutes(),
                    threshold_mins = threshold / 60,
                    "recovering stuck internal task: no session found — reclaiming early → new"
                );
            }
            if let Err(e) = task_manager
                .update_task_status(&ExternalId(task_id.clone()), Status::New)
                .await
            {
                tracing::warn!(task_id, ?e, "failed to reset stuck internal task status");
            }
        }
    }

    Ok(())
}

/// Phase 3a of tick: route status:new tasks to an agent and transition them to status:routed.
pub(crate) async fn tick_route_tasks(
    backend: &Arc<dyn ExternalBackend>,
    task_manager: &Arc<TaskManager>,
    router: &Router,
    store: &Arc<TaskStore>,
    repo: &str,
) -> anyhow::Result<()> {
    let _span = tracing::info_span!("engine.tick.phase3a.route").entered();
    let new_tasks = task_manager.list_routable().await?;
    let routable: Vec<&ExternalTask> = new_tasks
        .iter()
        .filter(|t| !t.labels.iter().any(|l| l == "no-agent"))
        .collect();

    for task in routable {
        let _task_span = tracing::info_span!("engine.route", task_id = %task.id.0).entered();
        match router.route(task).await {
            Ok(result) => {
                // Store route result in sidecar
                if let Err(e) = router.store_route_result(&task.id.0, &result) {
                    tracing::warn!(task_id = task.id.0, ?e, "failed to store route result");
                }

                if is_internal_id(&task.id.0) {
                    // Internal tasks: update DB status, skip GitHub label ops.
                    if let Err(e) = task_manager
                        .update_task_status(&task.id, Status::Routed)
                        .await
                    {
                        tracing::warn!(
                            task_id = task.id.0,
                            ?e,
                            "failed to set internal status:routed"
                        );
                    }
                } else {
                    // Add agent and complexity labels (additive — does not remove existing labels)
                    let labels = vec![
                        format!("agent:{}", result.agent),
                        format!("complexity:{}", result.complexity),
                    ];
                    if let Err(e) = backend.set_labels(&task.id, &labels).await {
                        tracing::warn!(task_id = task.id.0, ?e, "failed to set routing labels");
                    }

                    // Transition to routed
                    if let Err(e) = backend.update_status(&task.id, Status::Routed).await {
                        tracing::warn!(task_id = task.id.0, ?e, "failed to set status:routed");
                    }
                }

                // Dual-write: upsert task + store route result in SQLite
                match store.ensure_external_task(repo, task).await {
                    Ok(store_id) => {
                        let profile_json =
                            serde_json::to_string(&result.profile).unwrap_or_default();
                        let skills_json =
                            serde_json::to_string(&result.selected_skills).unwrap_or_default();
                        if let Err(e) = store
                            .store_route(&crate::store::StoreRoute {
                                id: store_id,
                                agent: &result.agent,
                                model: result.model.as_deref(),
                                complexity: &result.complexity,
                                reason: &result.reason,
                                profile: &profile_json,
                                skills: &skills_json,
                            })
                            .await
                        {
                            tracing::debug!(
                                task_id = task.id.0,
                                ?e,
                                "dual-write: store_route failed"
                            );
                        }
                        if let Err(e) = store
                            .update_status(store_id, crate::db::TaskStatus::Routed)
                            .await
                        {
                            tracing::debug!(
                                task_id = task.id.0,
                                ?e,
                                "dual-write: update_status failed"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::debug!(
                            task_id = task.id.0,
                            ?e,
                            "dual-write: ensure_external_task failed"
                        );
                    }
                }

                if let Some(ref warning) = result.warning {
                    tracing::warn!(task_id = task.id.0, warning, "routing sanity warning");
                }

                tracing::info!(
                    task_id = task.id.0,
                    agent = %result.agent,
                    complexity = %result.complexity,
                    reason = %result.reason,
                    "task routed"
                );
            }
            Err(e) => {
                tracing::error!(task_id = task.id.0, ?e, "routing failed, skipping task");
            }
        }
    }
    Ok(())
}

/// Phase 3b of tick: spawn agents for all status:routed tasks up to the parallel limit.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn tick_dispatch_tasks(
    backend: &Arc<dyn ExternalBackend>,
    tmux: &Arc<TmuxManager>,
    repo: &str,
    runner: &Arc<TaskRunner>,
    capture: &Arc<CaptureService>,
    semaphore: &Arc<Semaphore>,
    task_manager: &Arc<TaskManager>,
    weight_tx: &mpsc::Sender<WeightSignal>,
    transport: &Arc<Transport>,
    router_arc: &Arc<RwLock<Router>>,
    dispatching: &Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    store: &Arc<TaskStore>,
) -> anyhow::Result<()> {
    let _span = tracing::info_span!("engine.tick.phase3b.dispatch").entered();
    // Note: Routed tasks should never have no-agent (filtered during Phase 3a routing),
    // but we keep this filter as defense-in-depth.
    let mut routed_tasks = task_manager.list_external_by_status(Status::Routed).await?;

    // Also include internal tasks in Routed status.
    use crate::db::TaskStatus as DbStatus;
    let internal_routed = task_manager
        .db_list_internal_by_status(DbStatus::Routed)
        .await?;
    for t in internal_routed {
        routed_tasks.push(ExternalTask {
            id: ExternalId(format!("internal:{}", t.id)),
            title: t.title,
            body: t.body,
            state: "open".to_string(),
            labels: vec!["status:routed".to_string()],
            author: t.source,
            created_at: t.created_at.to_rfc3339(),
            updated_at: t.updated_at.to_rfc3339(),
            url: String::new(),
        });
    }

    let dispatchable: Vec<&ExternalTask> = routed_tasks
        .iter()
        .filter(|t| !t.labels.iter().any(|l| l == "no-agent"))
        .collect();

    if !dispatchable.is_empty() {
        tracing::info!(count = dispatchable.len(), "dispatchable tasks found");
    }

    for task in dispatchable {
        // In-memory guard: prevents double-dispatch due to GitHub API eventual consistency.
        // After update_status(InProgress), the label removal fires a webhook that can
        // trigger an immediate tick. GitHub's search index may not yet reflect the label
        // change, so list_by_status(Routed) can still return this task. The tmux session
        // does not exist until the runner completes worktree setup (~10s later), so the
        // session_exists check alone is insufficient.
        let dispatch_key = format!("{}/{}", repo, task.id.0);
        {
            let guard = dispatching.lock().unwrap();
            if guard.contains(&dispatch_key) {
                tracing::debug!(
                    task_id = task.id.0,
                    "task already dispatching, skipping duplicate"
                );
                continue;
            }
        }

        // Check if already running (has active session)
        let session_name = tmux.session_name(repo, &task.id.0);
        if tmux.session_exists(&session_name).await {
            continue;
        }

        // Try to acquire a slot
        let permit = match semaphore.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                tracing::debug!("all parallel slots busy, skipping remaining tasks");
                break;
            }
        };

        // Mark in_progress BEFORE spawning to prevent double dispatch.
        let task_id = task.id.0.clone();
        let set_in_progress_result = if is_internal_id(&task_id) {
            task_manager
                .update_task_status(&task.id, Status::InProgress)
                .await
        } else {
            backend.update_status(&task.id, Status::InProgress).await
        };
        if let Err(e) = set_in_progress_result {
            tracing::error!(task_id, ?e, "failed to set in_progress, skipping dispatch");
            drop(permit);
            continue;
        }

        // Insert into dispatching set after successful status update.
        // This prevents the webhook-triggered tick (fired by label removal during
        // update_status) from re-dispatching the same task.
        {
            let mut guard = dispatching.lock().unwrap();
            guard.insert(dispatch_key.clone());
        }

        // Register session for capture
        let session_name = tmux.session_name(repo, &task_id);
        capture.register_session(&task_id, &session_name).await;

        // Dispatch task
        let runner = runner.clone();
        let backend = backend.clone();
        let tmux = tmux.clone();
        let capture = capture.clone();
        let transport = transport.clone();
        let router_clone = router_arc.clone();
        let task_id_for_cleanup = task_id.clone();
        let task_owned = task.clone();
        let weight_tx = weight_tx.clone();
        let repo_owned = repo.to_string();
        let dispatching_for_cleanup = dispatching.clone();
        let dispatch_key_for_cleanup = dispatch_key.clone();
        let task_manager_for_spawn = task_manager.clone();
        let store_for_spawn = store.clone();

        // Load routing result from sidecar (stored during Phase 3a)
        let route_result = get_route_result(&task_id).ok();
        let agent_name = route_result
            .as_ref()
            .map(|r| r.agent.clone())
            .unwrap_or_else(|| "claude".to_string());

        let repo_ctx = repo_owned.clone();
        tokio::spawn(REPO_CONTEXT.scope(repo_ctx, async move {
            // Note: Using tracing::info_span directly without holding across await
            // to avoid Send issues with EnteredSpan
            tracing::info!(task_id, "dispatching task");

            let dispatch_start = std::time::Instant::now();

            match runner
                .run_with_context(&task_owned, &backend, &tmux, route_result.as_ref())
                .await
            {
                Ok(signal) => {
                    tracing::info!(task_id, "task runner completed");

                    // Dual-write: sync sidecar fields to SQLite store
                    if let Ok(Some(store_id)) = store_for_spawn.resolve_task_id(&repo_owned, &task_id).await {
                        store_for_spawn.sync_sidecar_to_store(store_id, &task_id).await;
                    }

                    // Send task completion notification
                    let summary = sidecar::get(&task_id, "summary").unwrap_or_default();
                    let duration = dispatch_start.elapsed().as_secs_f64();

                    // Derive a display status from the weight signal for the notification.
                    let display_status = match &signal {
                        WeightSignal::Success { .. } => "done",
                        WeightSignal::RateLimited { .. } => "new",
                        WeightSignal::Blocked => "blocked",
                        WeightSignal::None => "needs_review",
                    };

                    transport.push_notification(TaskNotification {
                        task_id: task_id.clone(),
                        title: task_owned.title.clone(),
                        status: display_status.to_string(),
                        agent: agent_name.clone(),
                        duration_seconds: duration,
                        summary: summary.clone(),
                    });

                    // Send weight signal back to the router
                    let _ = weight_tx.send(signal).await;

                    // All tasks: if needs_review, trigger the review agent.
                    // Status updates go through task_manager so internal tasks
                    // hit SQLite while external tasks hit GitHub labels.
                    if display_status == "needs_review" {
                        let enable_review = config::get("workflow.enable_review_agent")
                            .map(|v| v != "false")
                            .unwrap_or(true);
                        tracing::info!(task_id, enable_review, "review gate check");
                        if enable_review {
                            // Transition to InReview — atomic guard against duplicate reviews.
                            match task_manager_for_spawn
                                .update_task_status(
                                    &ExternalId(task_id.clone()),
                                    Status::InReview,
                                )
                                .await
                            {
                                Err(e) => {
                                    tracing::warn!(task_id, err = %e, "failed to transition to InReview");
                                }
                                Ok(_) => {
                                    let backend_clone = backend.clone();
                                    let task_manager_for_review = task_manager_for_spawn.clone();
                                    let tmux_clone = tmux.clone();
                                    let task_owned_clone = task_owned.clone();
                                    let router_for_review = router_clone.clone();
                                    let task_id_for_review = task_id.clone();
                                    let repo_ctx = repo_owned.clone();
                                    tokio::spawn(REPO_CONTEXT.scope(repo_ctx, async move {
                                        match review_and_merge(
                                            &task_owned_clone,
                                            &backend_clone,
                                            &tmux_clone,
                                            &repo_owned,
                                            &router_for_review,
                                            &task_manager_for_review,
                                        )
                                        .await
                                        {
                                            Ok(ReviewDecision::Blocked(reason)) => {
                                                tracing::error!(
                                                    task_id = task_id_for_review,
                                                    reason,
                                                    "review gate blocked after repeated failures — marking task blocked"
                                                );
                                                let _ = task_manager_for_review
                                                    .update_task_status(
                                                        &ExternalId(task_id_for_review.clone()),
                                                        Status::Blocked,
                                                    )
                                                    .await;
                                            }
                                            Ok(ReviewDecision::Failed(reason)) => {
                                                let failures = sidecar::get_u64(
                                                    &task_id_for_review,
                                                    "review_agent_failures",
                                                )
                                                .saturating_add(1);
                                                let _ = sidecar::set(
                                                    &task_id_for_review,
                                                    &[format!(
                                                        "review_agent_failures={failures}"
                                                    )],
                                                );
                                                if failures >= MAX_REVIEW_AGENT_FAILURES {
                                                    tracing::error!(
                                                        task_id = task_id_for_review,
                                                        reason,
                                                        failures,
                                                        "review agent failed too many times — blocking task"
                                                    );
                                                    let _ = task_manager_for_review
                                                        .update_task_status(
                                                            &ExternalId(
                                                                task_id_for_review.clone(),
                                                            ),
                                                            Status::Blocked,
                                                        )
                                                        .await;
                                                } else {
                                                    tracing::error!(
                                                        task_id = task_id_for_review,
                                                        reason,
                                                        failures,
                                                        "review agent failed — resetting to NeedsReview for retry"
                                                    );
                                                    let _ = task_manager_for_review
                                                        .update_task_status(
                                                            &ExternalId(
                                                                task_id_for_review.clone(),
                                                            ),
                                                            Status::NeedsReview,
                                                        )
                                                        .await;
                                                }
                                            }
                                            Err(e) => {
                                                let failures = sidecar::get_u64(
                                                    &task_id_for_review,
                                                    "review_agent_failures",
                                                )
                                                .saturating_add(1);
                                                let _ = sidecar::set(
                                                    &task_id_for_review,
                                                    &[format!(
                                                        "review_agent_failures={failures}"
                                                    )],
                                                );
                                                if failures >= MAX_REVIEW_AGENT_FAILURES {
                                                    tracing::error!(
                                                        task_id = task_id_for_review,
                                                        error = %e,
                                                        failures,
                                                        "review_and_merge failed too many times — blocking task"
                                                    );
                                                    let _ = task_manager_for_review
                                                        .update_task_status(
                                                            &ExternalId(
                                                                task_id_for_review.clone(),
                                                            ),
                                                            Status::Blocked,
                                                        )
                                                        .await;
                                                } else {
                                                    tracing::error!(
                                                        task_id = task_id_for_review,
                                                        error = %e,
                                                        failures,
                                                        "review_and_merge failed — resetting to NeedsReview for retry"
                                                    );
                                                    let _ = task_manager_for_review
                                                        .update_task_status(
                                                            &ExternalId(
                                                                task_id_for_review.clone(),
                                                            ),
                                                            Status::NeedsReview,
                                                        )
                                                        .await;
                                                }
                                            }
                                            Ok(_) => {
                                                // Reset all review-cycle failure counters on success
                                                // so a subsequent retry doesn't inherit stale values.
                                                let _ = sidecar::set(
                                                    &task_id_for_review,
                                                    &[
                                                        "review_agent_failures=0".to_string(),
                                                        "merge_conflict_retries=0".to_string(),
                                                        "pr_create_failures=0".to_string(),
                                                        "ci_merge_failures=0".to_string(),
                                                    ],
                                                );
                                            } // Approve or RequestChanges handled inside
                                        }
                                    }));
                                }
                            }
                        } else {
                            // Review disabled: persist needs_review status.
                            let _ = task_manager_for_spawn
                                .update_task_status(
                                    &ExternalId(task_id.clone()),
                                    Status::NeedsReview,
                                )
                                .await;
                        }
                    } else {
                        // done, blocked, or new (rate-limited): update status directly.
                        let final_status = match display_status {
                            "done" => Status::Done,
                            "blocked" => Status::Blocked,
                            _ => Status::New,
                        };
                        if let Err(e) = task_manager_for_spawn
                            .update_task_status(&ExternalId(task_id.clone()), final_status)
                            .await
                        {
                            tracing::warn!(task_id, ?e, "failed to update task status after completion");
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(task_id, ?e, "task runner failed");
                    if is_internal_id(&task_id) {
                        // Internal tasks have no GitHub issue to comment on.
                        let _ = task_manager_for_spawn
                            .update_task_status(&ExternalId(task_id.clone()), Status::NeedsReview)
                            .await;
                    } else {
                        // Post comment (best-effort)
                        if let Err(comment_err) = backend
                            .post_comment(
                                &ExternalId(task_id.clone()),
                                &format!(
                                    "[{}] error: task runner failed: {e}{}",
                                    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
                                    crate::engine::orch_footer()
                                ),
                            )
                            .await
                        {
                            tracing::warn!(
                                task_id,
                                ?comment_err,
                                "failed to post error comment to GitHub"
                            );
                        }
                        // Update status immediately to NeedsReview so external tasks
                        // don't remain stuck in InProgress until the no-session timeout.
                        let _ = task_manager_for_spawn
                            .update_task_status(&ExternalId(task_id.clone()), Status::NeedsReview)
                            .await;
                    }

                    // Send error notification
                    let duration = dispatch_start.elapsed().as_secs_f64();
                    transport.push_notification(TaskNotification {
                        task_id: task_id.clone(),
                        title: task_owned.title.clone(),
                        status: "failed".to_string(),
                        agent: agent_name.clone(),
                        duration_seconds: duration,
                        summary: format!("Task runner failed: {e}"),
                    });
                }
            }

            // Unregister session from capture
            capture.unregister_session(&task_id_for_cleanup).await;

            // Remove from dispatching set so the task can be re-dispatched if needed
            {
                let mut guard = dispatching_for_cleanup.lock().unwrap();
                guard.remove(&dispatch_key_for_cleanup);
            }

            // Release the semaphore permit
            drop(permit);
        }));
    }
    Ok(())
}

/// Phase 4 of tick: unblock parent tasks whose sub-issues are all done.
pub(crate) async fn tick_unblock_parents(
    backend: &Arc<dyn ExternalBackend>,
    task_manager: &Arc<TaskManager>,
) -> anyhow::Result<()> {
    let blocked = task_manager
        .list_external_by_status(Status::Blocked)
        .await?;
    for task in &blocked {
        let children = match backend.get_sub_issues(&task.id).await {
            Ok(ids) => ids,
            Err(e) => {
                tracing::debug!(task_id = task.id.0, ?e, "failed to get sub-issues");
                continue;
            }
        };

        // No children means nothing to wait on — skip (may be blocked for other reasons)
        if children.is_empty() {
            continue;
        }

        // Check if every child is done
        let mut all_done = true;
        for child_id in &children {
            match backend.get_task(child_id).await {
                Ok(child) => {
                    if !child.labels.iter().any(|l| l == Status::Done.as_label()) {
                        all_done = false;
                        break;
                    }
                }
                Err(e) => {
                    tracing::debug!(
                        parent = task.id.0,
                        child = child_id.0,
                        ?e,
                        "failed to fetch child task"
                    );
                    all_done = false;
                    break;
                }
            }
        }

        if all_done {
            tracing::info!(
                task_id = task.id.0,
                children = children.len(),
                "all children done, unblocking parent"
            );
            if let Err(e) = backend.update_status(&task.id, Status::New).await {
                tracing::warn!(task_id = task.id.0, ?e, "failed to unblock parent");
            }
        }
    }
    Ok(())
}

/// Phase 5 of tick: run cron job matching and fire any due jobs.
pub(crate) async fn tick_job_scheduler(
    jobs_path: &std::path::PathBuf,
    backend: &Arc<dyn ExternalBackend>,
    db: &Arc<Db>,
) -> anyhow::Result<()> {
    jobs::tick(jobs_path, backend, db).await
}

/// Core tick — runs every 10s.
///
/// Delegates to named phase functions in order:
/// 1. `tick_check_session_completions` — poll tmux for finished sessions
/// 2. `tick_recover_stuck_tasks`       — reset in_progress tasks with no active session
/// 3. `tick_route_tasks`               — route status:new tasks to an agent
/// 4. `tick_dispatch_tasks`            — spawn agents for status:routed tasks
/// 5. `tick_unblock_parents`           — unblock parents whose sub-issues are all done
/// 6. `tick_job_scheduler`             — fire due cron jobs
#[allow(clippy::too_many_arguments)]
pub(crate) async fn tick(
    backend: &Arc<dyn ExternalBackend>,
    tmux: &Arc<TmuxManager>,
    repo: &str,
    runner: &Arc<TaskRunner>,
    capture: &Arc<CaptureService>,
    semaphore: &Arc<Semaphore>,
    config: &EngineConfig,
    jobs_path: &std::path::PathBuf,
    db: &Arc<Db>,
    router: &Router,
    router_arc: &Arc<RwLock<Router>>,
    task_manager: &Arc<TaskManager>,
    weight_tx: &mpsc::Sender<WeightSignal>,
    transport: &Arc<Transport>,
    dispatching: &Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    store: &Arc<TaskStore>,
) -> anyhow::Result<()> {
    let _tick_span = tracing::info_span!("engine.tick").entered();
    tick_check_session_completions(tmux, repo, capture).await?;
    tick_recover_stuck_tasks(backend, tmux, repo, task_manager, config).await?;
    tick_route_tasks(backend, task_manager, router, store, repo).await?;
    tick_dispatch_tasks(
        backend,
        tmux,
        repo,
        runner,
        capture,
        semaphore,
        task_manager,
        weight_tx,
        transport,
        router_arc,
        dispatching,
        store,
    )
    .await?;
    tick_unblock_parents(backend, task_manager).await?;
    if let Err(e) = tick_job_scheduler(jobs_path, backend, db).await {
        tracing::error!(?e, "job scheduler tick failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::{ExternalId, ExternalTask, Mention, Status};
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    // ── minimal mock backend ─────────────────────────────────────────────────

    /// Configurable mock backend for tick tests.
    struct MockBackend {
        /// Tasks returned by `list_by_status(Blocked)`.
        blocked_tasks: Vec<ExternalTask>,
        /// Sub-issues map: task_id → list of child ExternalIds.
        sub_issues: std::collections::HashMap<String, Vec<ExternalId>>,
        /// Tasks returned by `get_task`. Keyed by id.0.
        tasks_by_id: std::collections::HashMap<String, ExternalTask>,
        /// Recorded `update_status` calls.
        status_updates: Arc<Mutex<Vec<(String, Status)>>>,
    }

    impl MockBackend {
        fn new() -> Self {
            Self {
                blocked_tasks: vec![],
                sub_issues: Default::default(),
                tasks_by_id: Default::default(),
                status_updates: Arc::new(Mutex::new(vec![])),
            }
        }
    }

    fn make_task(id: &str, labels: &[&str]) -> ExternalTask {
        ExternalTask {
            id: ExternalId(id.to_string()),
            title: format!("Task {id}"),
            body: "".to_string(),
            state: "open".to_string(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            author: "bot".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: format!("https://github.com/test/test/issues/{id}"),
        }
    }

    #[async_trait]
    impl ExternalBackend for MockBackend {
        fn name(&self) -> &str {
            "mock"
        }
        async fn create_task(
            &self,
            _title: &str,
            _body: &str,
            _labels: &[String],
        ) -> anyhow::Result<ExternalId> {
            Ok(ExternalId("new".to_string()))
        }
        async fn get_task(&self, id: &ExternalId) -> anyhow::Result<ExternalTask> {
            self.tasks_by_id
                .get(&id.0)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("task not found: {}", id.0))
        }
        async fn list_by_status(&self, status: Status) -> anyhow::Result<Vec<ExternalTask>> {
            if status == Status::Blocked {
                Ok(self.blocked_tasks.clone())
            } else {
                Ok(vec![])
            }
        }
        async fn list_routable(&self) -> anyhow::Result<Vec<ExternalTask>> {
            Ok(vec![])
        }
        async fn post_comment(&self, _id: &ExternalId, _body: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn set_labels(&self, _id: &ExternalId, _labels: &[String]) -> anyhow::Result<()> {
            Ok(())
        }
        async fn remove_label(&self, _id: &ExternalId, _label: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn get_sub_issues(&self, id: &ExternalId) -> anyhow::Result<Vec<ExternalId>> {
            Ok(self.sub_issues.get(&id.0).cloned().unwrap_or_default())
        }
        async fn create_sub_task(
            &self,
            _parent: &ExternalId,
            _title: &str,
            _body: &str,
            _labels: &[String],
        ) -> anyhow::Result<ExternalId> {
            Ok(ExternalId("child".to_string()))
        }
        async fn ensure_status_label(&self, _label: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn has_open_issue_with_title(
            &self,
            _title: &str,
            _label: &str,
        ) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn health_check(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn is_pr_merged(&self, _branch: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn get_authenticated_user(&self) -> anyhow::Result<Option<String>> {
            Ok(Some("testbot".to_string()))
        }
        async fn get_mentions(&self, _since: &str) -> anyhow::Result<Vec<Mention>> {
            Ok(vec![])
        }
        async fn update_status(&self, id: &ExternalId, status: Status) -> anyhow::Result<()> {
            self.status_updates
                .lock()
                .unwrap()
                .push((id.0.clone(), status));
            Ok(())
        }
    }

    fn make_task_manager(backend: Arc<dyn ExternalBackend>) -> Arc<TaskManager> {
        let db = Arc::new(crate::db::Db::open_memory().unwrap());
        Arc::new(TaskManager::new(db, backend))
    }

    // ── tick_unblock_parents ─────────────────────────────────────────────────

    #[tokio::test]
    async fn unblock_parents_unblocks_when_all_children_done() {
        let mut mock = MockBackend::new();

        // Blocked parent with two done children
        let parent = make_task("10", &["status:blocked"]);
        mock.blocked_tasks.push(parent.clone());
        mock.sub_issues.insert(
            "10".to_string(),
            vec![ExternalId("11".to_string()), ExternalId("12".to_string())],
        );
        mock.tasks_by_id
            .insert("11".to_string(), make_task("11", &["status:done"]));
        mock.tasks_by_id
            .insert("12".to_string(), make_task("12", &["status:done"]));

        let status_updates = mock.status_updates.clone();
        let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
        let task_manager = make_task_manager(backend.clone());

        tick_unblock_parents(&backend, &task_manager).await.unwrap();

        let updates = status_updates.lock().unwrap();
        assert_eq!(updates.len(), 1, "parent should be unblocked");
        assert_eq!(updates[0], ("10".to_string(), Status::New));
    }

    #[tokio::test]
    async fn unblock_parents_skips_when_child_not_done() {
        let mut mock = MockBackend::new();

        let parent = make_task("20", &["status:blocked"]);
        mock.blocked_tasks.push(parent.clone());
        mock.sub_issues.insert(
            "20".to_string(),
            vec![ExternalId("21".to_string()), ExternalId("22".to_string())],
        );
        // Child 21 is done, child 22 is still in_progress
        mock.tasks_by_id
            .insert("21".to_string(), make_task("21", &["status:done"]));
        mock.tasks_by_id
            .insert("22".to_string(), make_task("22", &["status:in_progress"]));

        let status_updates = mock.status_updates.clone();
        let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
        let task_manager = make_task_manager(backend.clone());

        tick_unblock_parents(&backend, &task_manager).await.unwrap();

        let updates = status_updates.lock().unwrap();
        assert!(
            updates.is_empty(),
            "should not unblock when a child is still running"
        );
    }

    #[tokio::test]
    async fn unblock_parents_skips_task_with_no_children() {
        let mut mock = MockBackend::new();

        // Blocked task with no sub-issues (blocked for a different reason)
        let parent = make_task("30", &["status:blocked"]);
        mock.blocked_tasks.push(parent.clone());
        // No entry in sub_issues → get_sub_issues returns []

        let status_updates = mock.status_updates.clone();
        let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
        let task_manager = make_task_manager(backend.clone());

        tick_unblock_parents(&backend, &task_manager).await.unwrap();

        let updates = status_updates.lock().unwrap();
        assert!(
            updates.is_empty(),
            "should not unblock tasks with no sub-issues"
        );
    }

    #[tokio::test]
    async fn unblock_parents_no_blocked_tasks() {
        let mock = MockBackend::new(); // no blocked tasks
        let status_updates = mock.status_updates.clone();
        let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
        let task_manager = make_task_manager(backend.clone());

        tick_unblock_parents(&backend, &task_manager).await.unwrap();

        assert!(status_updates.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unblock_parents_handles_multiple_blocked_tasks() {
        let mut mock = MockBackend::new();

        // Task 40: all children done → should unblock
        let t40 = make_task("40", &["status:blocked"]);
        mock.blocked_tasks.push(t40);
        mock.sub_issues
            .insert("40".to_string(), vec![ExternalId("41".to_string())]);
        mock.tasks_by_id
            .insert("41".to_string(), make_task("41", &["status:done"]));

        // Task 50: child not done → should stay blocked
        let t50 = make_task("50", &["status:blocked"]);
        mock.blocked_tasks.push(t50);
        mock.sub_issues
            .insert("50".to_string(), vec![ExternalId("51".to_string())]);
        mock.tasks_by_id
            .insert("51".to_string(), make_task("51", &["status:in_progress"]));

        let status_updates = mock.status_updates.clone();
        let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
        let task_manager = make_task_manager(backend.clone());

        tick_unblock_parents(&backend, &task_manager).await.unwrap();

        let updates = status_updates.lock().unwrap();
        assert_eq!(updates.len(), 1, "only one parent should be unblocked");
        assert_eq!(updates[0].0, "40", "task 40 should be unblocked");
        assert_eq!(updates[0].1, Status::New);
    }
}
