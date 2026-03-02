//! Core tick loop phases.
//!
//! The engine ticks every ~10 seconds. Each tick runs six sequential phases:
//! 1. Poll tmux for finished sessions
//! 2. Recover stuck in-progress tasks
//! 3a. Route new tasks to agents
//! 3b. Dispatch routed tasks (spawn agents)
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
use crate::engine::tasks::TaskManager;
use crate::sidecar::{self, REPO_CONTEXT};
use crate::tmux::TmuxManager;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock, Semaphore};

use super::review::{review_and_merge, ReviewDecision};
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
                        "[{}] recovered: stuck in_progress — {}",
                        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
                        reason
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
    Ok(())
}

/// Phase 3a of tick: route status:new tasks to an agent and transition them to status:routed.
pub(crate) async fn tick_route_tasks(
    backend: &Arc<dyn ExternalBackend>,
    task_manager: &Arc<TaskManager>,
    router: &Router,
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
) -> anyhow::Result<()> {
    let _span = tracing::info_span!("engine.tick.phase3b.dispatch").entered();
    // Note: Routed tasks should never have no-agent (filtered during Phase 3a routing),
    // but we keep this filter as defense-in-depth.
    let routed_tasks = task_manager.list_external_by_status(Status::Routed).await?;
    let dispatchable: Vec<&ExternalTask> = routed_tasks
        .iter()
        .filter(|t| !t.labels.iter().any(|l| l == "no-agent"))
        .collect();

    if !dispatchable.is_empty() {
        tracing::info!(count = dispatchable.len(), "dispatchable tasks found");
    }

    for task in dispatchable {
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
        if let Err(e) = backend.update_status(&task.id, Status::InProgress).await {
            tracing::error!(task_id, ?e, "failed to set in_progress, skipping dispatch");
            drop(permit);
            continue;
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
                    // Send weight signal back to the router
                    let _ = weight_tx.send(signal).await;

                    // Send task completion notification
                    let status = sidecar::get(&task_id, "status").unwrap_or_default();
                    let summary = sidecar::get(&task_id, "summary").unwrap_or_default();
                    let duration = dispatch_start.elapsed().as_secs_f64();

                    transport.push_notification(TaskNotification {
                        task_id: task_id.clone(),
                        title: task_owned.title.clone(),
                        status: status.clone(),
                        agent: agent_name.clone(),
                        duration_seconds: duration,
                        summary: summary.clone(),
                    });

                    // Trigger review agent for needs_review tasks (PR exists, queued for review)
                    tracing::debug!(task_id, %status, "checking review trigger");
                    if status == "needs_review" {
                        let enable_review = config::get("workflow.enable_review_agent")
                            .map(|v| v != "false")
                            .unwrap_or(true);
                        tracing::info!(task_id, enable_review, "review gate check");
                        if enable_review {
                            // Transition to InReview — this IS the guard against duplicates
                            if let Err(e) = backend
                                .update_status(
                                    &ExternalId(task_id.clone()),
                                    Status::InReview,
                                )
                                .await
                            {
                                tracing::warn!(task_id, err = %e, "failed to transition to InReview");
                            } else {
                                let backend_clone = backend.clone();
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
                                    )
                                    .await
                                    {
                                        Ok(ReviewDecision::Failed(reason)) => {
                                            tracing::error!(
                                                task_id = task_id_for_review,
                                                reason,
                                                "review agent failed — resetting to NeedsReview for retry"
                                            );
                                            let _ = backend_clone
                                                .update_status(
                                                    &ExternalId(
                                                        task_id_for_review,
                                                    ),
                                                    Status::NeedsReview,
                                                )
                                                .await;
                                        }
                                        Err(e) => {
                                            tracing::error!(
                                                task_id = task_id_for_review,
                                                error = %e,
                                                "review_and_merge failed — resetting to NeedsReview for retry"
                                            );
                                            let _ = backend_clone
                                                .update_status(
                                                    &ExternalId(
                                                        task_id_for_review,
                                                    ),
                                                    Status::NeedsReview,
                                                )
                                                .await;
                                        }
                                        Ok(_) => {} // Approve or RequestChanges handled inside
                                    }
                                }));
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(task_id, ?e, "task runner failed");
                    if let Err(comment_err) = backend
                        .post_comment(
                            &ExternalId(task_id.clone()),
                            &format!(
                                "[{}] error: task runner failed: {e}",
                                chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
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
) -> anyhow::Result<()> {
    let _tick_span = tracing::info_span!("engine.tick").entered();
    tick_check_session_completions(tmux, repo, capture).await?;
    tick_recover_stuck_tasks(backend, tmux, repo, task_manager, config).await?;
    tick_route_tasks(backend, task_manager, router).await?;
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
    )
    .await?;
    tick_unblock_parents(backend, task_manager).await?;
    if let Err(e) = tick_job_scheduler(jobs_path, backend, db).await {
        tracing::error!(?e, "job scheduler tick failed");
    }
    Ok(())
}
