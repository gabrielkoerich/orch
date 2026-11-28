//! Dispatch of routed tasks to agents

use super::display_status_for;
use crate::backends::github::GitHubBackend;
use crate::backends::{ExternalBackend, ExternalId, ExternalTask, Status};
use crate::channels::capture::CaptureService;
use crate::config;
use crate::cooldown::{is_agent_in_cooldown, is_model_in_cooldown};
use crate::engine;
use crate::engine::router::config as router_config;
use crate::engine::router::{RouteResultError, Router, get_route_result};
use crate::engine::runner::{NoProjectDir, TaskRunner, WeightSignal};
use crate::engine::tasks;
use crate::engine::tasks::dependencies;
use crate::engine::tasks::dispatch_guard::DispatchGuard;
use crate::engine::tasks::priority;
use crate::engine::tasks::priority::Priority;
use crate::engine::tasks::{TaskManager, is_internal_id};
use crate::repo_context::REPO_CONTEXT;
use crate::store::Task;
use crate::store::TaskStatus;
use crate::store::TaskStatus as DbStatus;
use crate::store::TaskStore;
use crate::tmux::TmuxManager;
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::{Semaphore, mpsc};
use tokio::time;

/// Phase 3b of tick: spawn agents for all status:routed tasks up to the parallel limit.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DispatchMode {
    pub(crate) is_degraded: bool,
    pub(crate) healthy_agents: usize,
    pub(crate) threshold: usize,
}

pub(crate) fn dispatch_mode_from_router(router: &Router) -> DispatchMode {
    let threshold = router_config::min_healthy_agents_threshold();
    let healthy_agents = router.healthy_agent_count("simple");
    let is_degraded = router.is_degraded(threshold);
    DispatchMode {
        is_degraded,
        healthy_agents,
        threshold,
    }
}

/// Phase 3b of tick: spawn agents for all status:routed tasks up to the parallel limit.
#[expect(
    clippy::too_many_arguments,
    reason = "each argument is engine state the tick passes down"
)]
#[tracing::instrument(skip_all, name = "engine.tick.phase3b.dispatch")]
pub(crate) async fn tick_dispatch_tasks(
    backend: &Arc<dyn ExternalBackend>,
    tmux: &Arc<TmuxManager>,
    repo: &str,
    runner: &Arc<TaskRunner>,
    capture: &Arc<CaptureService>,
    semaphore: &Arc<Semaphore>,
    task_manager: &Arc<TaskManager>,
    weight_tx: &mpsc::Sender<WeightSignal>,
    dispatch_mode: DispatchMode,
    dispatching: &Arc<DashMap<String, String>>,
    store: &Arc<TaskStore>,
    session_map: &HashMap<String, bool>,
) -> anyhow::Result<()> {
    // Phase 3a already drops no-agent and owner:human, this filter stays as defense in depth
    let mut routed_tasks = task_manager.list_external_by_status(Status::Routed).await?;

    // Also include internal tasks in Routed status.
    let internal_routed = task_manager
        .list_internal_by_status(DbStatus::Routed)
        .await?;
    routed_tasks.extend(internal_routed);

    // Filter and collect owned tasks to avoid lifetime issues
    let dispatchable: Vec<ExternalTask> = routed_tasks
        .into_iter()
        .filter(|t| !skips_agent(&t.labels))
        .collect();

    dispatch_tasks_for_repo(
        backend,
        tmux,
        repo,
        runner,
        capture,
        semaphore,
        task_manager,
        weight_tx,
        dispatch_mode,
        dispatching,
        store,
        session_map,
        dispatchable,
    )
    .await
}

/// Dispatch a pre-fetched list of routed tasks for a single repo.
///
/// Shared by `tick_dispatch_tasks` (active-repo, per-tick dispatch) and
/// `dispatch_routed_tasks_global` (inactive-repo sweep) so both paths spawn
/// agents through the exact same worktree/tmux/runner logic.
#[expect(
    clippy::too_many_arguments,
    reason = "each argument is engine state the tick passes down"
)]
async fn dispatch_tasks_for_repo(
    backend: &Arc<dyn ExternalBackend>,
    tmux: &Arc<TmuxManager>,
    repo: &str,
    runner: &Arc<TaskRunner>,
    capture: &Arc<CaptureService>,
    semaphore: &Arc<Semaphore>,
    task_manager: &Arc<TaskManager>,
    weight_tx: &mpsc::Sender<WeightSignal>,
    dispatch_mode: DispatchMode,
    dispatching: &Arc<DashMap<String, String>>,
    store: &Arc<TaskStore>,
    session_map: &HashMap<String, bool>,
    dispatchable: Vec<ExternalTask>,
) -> anyhow::Result<()> {
    if dispatchable.is_empty() {
        tracing::debug!(count = 0, "dispatchable tasks found");
        return Ok(());
    }

    tracing::info!(count = dispatchable.len(), "dispatchable tasks found");
    if dispatch_mode.is_degraded {
        tracing::warn!(
            healthy_agents = dispatch_mode.healthy_agents,
            threshold = dispatch_mode.threshold,
            "degraded mode: using sequential dispatch"
        );
    }

    let sequential_delay = if dispatch_mode.is_degraded {
        router_config::sequential_dispatch_delay_ms()
    } else {
        0
    };

    let mut ranked: Vec<(Priority, ExternalTask)> = Vec::with_capacity(dispatchable.len());
    for task in dispatchable {
        let has_pr = match store.resolve_id_by_external(repo, &task.id.0).await {
            Ok(Some(id)) => store.get(id).await.is_ok_and(|t| t.pr_number.is_some()),
            _ => false,
        };
        ranked.push((Priority::from_labels(&task.labels).with_pr(has_pr), task));
    }
    priority::sort_by_priority(&mut ranked, |t| t.0);

    for (idx, (priority, task)) in ranked.into_iter().enumerate() {
        if dependencies::dispatch_is_waiting(store, repo, &task.id.0).await {
            continue;
        }
        // In degraded mode, add delay between dispatches to pace the system
        if idx > 0 && dispatch_mode.is_degraded {
            let delay_ms = sequential_delay;
            tracing::debug!(
                task_id = task.id.0,
                delay_ms = delay_ms,
                "sequential dispatch: waiting before dispatch"
            );
            time::sleep(Duration::from_millis(delay_ms)).await;
        }
        // In-memory guard: prevents double-dispatch due to GitHub API eventual consistency.
        // After update_status(InProgress), the label removal fires a webhook that can
        // trigger an immediate tick. GitHub's search index may not yet reflect the label
        // change, so list_by_status(Routed) can still return this task. The tmux session
        // does not exist until the runner completes worktree setup (~10s later), so the
        // session_exists check alone is insufficient.
        let dispatch_key = format!("{}/{}", repo, task.id.0);
        // Atomically claim the dispatch slot.  DashMap lets us distinguish:
        //   - same task already in-flight (expected): log debug and skip.
        //   - different task holds the same key (should never happen): log warning and skip.
        // A plain DashSet::insert would return false for both cases without distinction.
        {
            match dispatching.entry(dispatch_key.clone()) {
                Entry::Occupied(existing) => {
                    let existing_id = existing.get().clone();
                    drop(existing); // release shard lock before logging
                    if existing_id == task.id.0 {
                        tracing::debug!(
                            task_id = task.id.0,
                            "task already dispatching, skipping duplicate"
                        );
                    } else {
                        tracing::warn!(
                            task_id = task.id.0,
                            existing_task_id = existing_id,
                            dispatch_key,
                            "dispatch key collision: unexpected task already holds this key"
                        );
                    }
                    continue;
                }
                Entry::Vacant(slot) => {
                    slot.insert(task.id.0.clone());
                    // Promoted from debug! to info! (issue #3526): paired with the
                    // "dispatch guard released" line in dispatch_guard.rs, this is the
                    // only direct evidence of how long a dispatch guard was actually held
                    // for a given key. Both were debug!, silently dropped under the
                    // default "orch=info" filter (main.rs) — see the reclaim-skip log
                    // promotions above for the full rationale.
                    tracing::info!(task_id = task.id.0, dispatch_key, "dispatch guard claimed");
                }
            }
        }
        // RAII guard — removes dispatch_key on drop even if the spawned task panics.
        let dispatch_guard = DispatchGuard::new(Arc::clone(dispatching), dispatch_key.clone());
        let task_id = task.id.0.clone();

        // Load routing result from store (stored during Phase 3a). Loaded before the
        // tmux/semaphore/in_progress steps so a stale cooldown can bail out cheaply.
        let route_result = match get_route_result(store, repo, &task_id).await {
            Ok(r) => Some(r),
            Err(RouteResultError::NoAgent { .. }) => {
                tracing::warn!(
                    task_id,
                    "routed task missing agent — resetting to new for re-routing (#1604)"
                );
                if let Err(e2) = task_manager.update_task_status(&task.id, Status::New).await {
                    tracing::error!(task_id, error = %e2, "failed to reset task to new");
                }
                continue; // dispatch_guard drops here, removing the key
            }
            Err(e) => {
                tracing::warn!(task_id, error = %e, "get_route_result failed — dispatching without route info");
                None
            }
        };

        // Re-check the assigned agent/model against the cooldown store: routing and
        // dispatch are decoupled by ticks, and a failure recorded after this task was
        // routed but before it dispatched would otherwise burn a full run on an
        // already-cooled agent/model. Leave the task in Routed so the next tick's
        // dispatch pass re-evaluates it — no new per-task defer state (see #3243).
        if let Some(route) = route_result.as_ref() {
            let agent_cooled = is_agent_in_cooldown(&route.agent);
            let model_cooled = route
                .model
                .as_deref()
                .is_some_and(|model| is_model_in_cooldown(&route.agent, model));
            if agent_cooled || model_cooled {
                tracing::warn!(
                    task_id,
                    agent = route.agent,
                    model = route.model.as_deref().unwrap_or(""),
                    "routed agent/model entered cooldown after routing — deferring dispatch to next tick"
                );
                continue; // dispatch_guard drops here, removing the key
            }
        }

        // Check if already running (has active session)
        let session_name = tmux.session_name(repo, &task_id);
        if tmux
            .session_blocks_dispatch_from_map(&session_name, session_map)
            .await
        {
            tracing::warn!(
                task_id,
                session_name,
                "task has existing tmux session, skipping dispatch"
            );
            continue; // dispatch_guard drops here, removing the key
        }

        // Try to acquire a slot
        let Some(permit) = priority::acquire_dispatch_slot(semaphore, priority) else {
            tracing::debug!("all parallel slots busy, skipping remaining tasks");
            break; // dispatch_guard drops here, removing the key
        };

        // Mark in_progress BEFORE spawning to prevent double dispatch.
        let set_in_progress_result = task_manager
            .update_task_status(&task.id, Status::InProgress)
            .await;
        if let Err(e) = set_in_progress_result {
            tracing::error!(task_id, ?e, "failed to set in_progress, skipping dispatch");
            drop(permit);
            continue; // dispatch_guard drops here, removing the key
        }
        tracing::info!(task_id, "dispatching task");

        // Register session for capture
        capture
            .register_session(repo, &task_id, &session_name)
            .await;

        // Dispatch task
        let runner = Arc::clone(runner);
        let backend = Arc::clone(backend);
        let tmux = Arc::clone(tmux);
        let capture = Arc::clone(capture);
        let task_id_for_cleanup = task_id.clone();
        let task_owned = task;
        let weight_tx = weight_tx.clone();
        let repo_owned = repo.to_string();
        let task_manager_for_spawn = Arc::clone(task_manager);
        let store_for_spawn = Arc::clone(store);

        let repo_ctx = repo_owned.clone();
        tokio::spawn(REPO_CONTEXT.scope(repo_ctx, async move {
            let _dispatch_guard = dispatch_guard; // released on drop (normal or panic)
            let dispatch_start = Instant::now();
            match runner
                .run_with_context(&task_owned, &backend, &tmux, route_result.as_ref())
                .await
            {
                Ok(signal) => {
                    tracing::info!(task_id, "task runner completed");

                    let duration = dispatch_start.elapsed().as_secs_f64();

                    // Derive a display status from the weight signal.
                    // When review agent is enabled, successful tasks go through
                    // review before being marked done — UNLESS the response
                    // handler already marked the task done (no-op tasks with
                    // no PR and no commits should not enter the review cycle).
                    let enable_review = config::get("workflow.enable_review_agent")
                        .map_or(true, |v| v != "false");
                    // Only a successful run can reach the no-PR shortcut, so only then read the PR.
                    let has_pr = if enable_review && matches!(signal, WeightSignal::Success { .. })
                    {
                        match store_for_spawn.resolve_task_id(&repo_owned, &task_id).await {
                            Ok(Some(store_id)) => match store_for_spawn.get(store_id).await {
                                Ok(t) => t.pr_number.is_some(),
                                Err(e) => {
                                    tracing::warn!(task_id, err = %e, "store.get() failed — defaulting to needs_review");
                                    true
                                }
                            },
                            Ok(None) => false,
                            Err(e) => {
                                tracing::warn!(task_id, err = %e, "failed to check PR status — defaulting to needs_review");
                                true
                            }
                        }
                    } else {
                        false
                    };
                    let display_status = display_status_for(&signal, enable_review, has_pr);

                    if display_status == "done" {
                        tracing::info!(
                            task_id,
                            "marking done: the run succeeded with no PR to review"
                        );
                    }
                    tracing::info!(task_id, display_status, "task run finished, applying status");

                    // Send weight signal back to the router
                    let _ = weight_tx.send(signal).await;

                    // All tasks: if needs_review, trigger the review agent.
                    // Status updates go through task_manager so internal tasks
                    // hit SQLite while external tasks hit GitHub labels.
                    if display_status == "needs_review" {
                        // Transition to NeedsReview — emits event so the review subscriber
                        // fires with the correct duration before the review agent starts.
                        if let Err(e) = task_manager_for_spawn
                            .update_task_status_with_duration(
                                &ExternalId(task_id.clone()),
                                Status::NeedsReview,
                                Some(duration),
                            )
                            .await
                        {
                            tracing::error!(task_id, err = %e, "update_task_status(NeedsReview) failed — task may be stuck");
                        }
                        // Review agent dispatch is handled by the event-driven subscriber.
                    } else {
                        // done, blocked, or new (rate-limited): update status directly.
                        let final_status = match display_status {
                            "done" => Status::Done,
                            "blocked" => Status::Blocked,
                            _ => Status::New,
                        };
                        if let Err(e) = task_manager_for_spawn
                            .update_task_status_with_duration(
                                &ExternalId(task_id.clone()),
                                final_status,
                                Some(duration),
                            )
                            .await
                        {
                            tracing::warn!(task_id, ?e, "failed to update task status after completion");
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(task_id, ?e, "task runner failed");
                    let duration = dispatch_start.elapsed().as_secs_f64();
                    if e.downcast_ref::<NoProjectDir>().is_some() {
                        // The run failed before any agent started, so nothing exists to review
                        block_task_without_project(
                            &task_manager_for_spawn,
                            &store_for_spawn,
                            &repo_owned,
                            &task_id,
                        )
                        .await;
                    } else if is_internal_id(&task_id) {
                        // Internal tasks have no GitHub issue to comment on.
                        if let Err(ue) = task_manager_for_spawn
                            .update_task_status_with_duration(
                                &ExternalId(task_id.clone()),
                                Status::NeedsReview,
                                Some(duration),
                            )
                            .await
                        {
                            tracing::error!(task_id, err = %ue, "update_task_status(NeedsReview) failed — task may be stuck");
                        }
                    } else {
                        // Post comment (best-effort)
                        if let Err(comment_err) = backend
                            .post_comment(
                                &ExternalId(task_id.clone()),
                                &format!(
                                    "[{}] error: task runner failed: {e}{}",
                                    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
                                    engine::orch_footer()
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
                        // The notify subscriber will send the channel notification with duration.
                        if let Err(ue) = task_manager_for_spawn
                            .update_task_status_with_duration(
                                &ExternalId(task_id.clone()),
                                Status::NeedsReview,
                                Some(duration),
                            )
                            .await
                        {
                            tracing::error!(task_id, err = %ue, "update_task_status(NeedsReview) failed — task may be stuck");
                        }
                    }
                }
            }

            // Unregister session from capture
            capture.unregister_session(&repo_owned, &task_id_for_cleanup).await;

            // Release the semaphore permit
            drop(permit);
            // _dispatch_guard drops here, removing the key from the dispatching set.
        }));
    }
    Ok(())
}

/// Block a task whose repo has no project directory. Nothing exists to review, so
/// `NeedsReview` would only feed the refire and rebroadcast loop (#4024)
pub(super) async fn block_task_without_project(
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
) {
    let store_id = match store.resolve_task_id(repo, task_id).await {
        Ok(Some(id)) => id,
        Ok(None) => {
            tracing::error!(
                task_id,
                repo,
                "cannot block task without project: not in the store"
            );
            return;
        }
        Err(e) => {
            tracing::error!(task_id, repo, err = %e, "cannot block task without project: store lookup failed");
            return;
        }
    };
    let fields = [
        (
            "block_reason",
            serde_json::json!(NoProjectDir::BLOCK_REASON),
        ),
        (
            "last_error",
            serde_json::json!(format!("no project directory for repo {repo}")),
        ),
    ];
    if let Err(e) = task_manager
        .update_task_status_and_result_by_store_id(
            store_id,
            repo,
            &ExternalId(task_id.to_string()),
            Status::Blocked,
            &fields,
        )
        .await
    {
        tracing::error!(task_id, repo, err = %e, "failed to block task without project");
    }
}

pub(super) fn skips_agent(labels: &[String]) -> bool {
    labels
        .iter()
        .any(|label| label == "no-agent" || label == "owner:human")
}

/// Filter routed tasks down to those belonging to a repo outside `active_repos`,
/// drop `no-agent` and `owner:human` tasks, and group the rest by repo.
///
/// Pure/side-effect-free so it can be unit tested without touching real tmux
/// sessions or spawning a runner — see `dispatch_routed_tasks_global`, which is
/// the only caller and does the actual (side-effectful) dispatch.
pub(super) fn group_orphaned_routed_tasks(
    all_routed: Vec<Task>,
    active_repos: &HashSet<String>,
) -> HashMap<String, Vec<Task>> {
    let mut by_repo: HashMap<String, Vec<Task>> = HashMap::new();
    for task in all_routed
        .into_iter()
        .filter(|t| !active_repos.contains(&t.repo))
        .filter(|t| !skips_agent(&t.labels))
    {
        by_repo.entry(task.repo.clone()).or_default().push(task);
    }
    by_repo
}

/// Global dispatch sweep: dispatch status:routed tasks from inactive or removed repos.
///
/// `tick_dispatch_tasks` is scoped to the active repo per-tick (its `task_manager`
/// only queries that repo's tasks) and never processes tasks whose `repo` field
/// differs from the current project. When a project is removed from the orch
/// config, tasks that reach `routed` for it — including ones routed by the
/// `route_new_tasks_global` sweep — have no code path left to dispatch them, so
/// they stay `routed` forever. This sweep covers all repos so orphaned routed
/// tasks are eventually dispatched.
///
/// `active_repos` must be the full set of currently-configured project repos,
/// not just one project's repo; passing only that project's own repo would make
/// every other active repo's routed tasks look "orphaned" too and dispatch them
/// a second time through this fallback path, racing the real per-repo dispatch.
///
/// `TaskRunner::resolve_project_dir` already falls back to the deterministic
/// `~/.orch/projects/<owner>/<repo>.git` bare-clone convention when a repo is
/// not listed in the active config, so dispatch works without a `ProjectEngine`.
#[expect(
    clippy::too_many_arguments,
    reason = "each argument is engine state the tick passes down"
)]
pub(crate) async fn dispatch_routed_tasks_global(
    tmux: &Arc<TmuxManager>,
    capture: &Arc<CaptureService>,
    semaphore: &Arc<Semaphore>,
    weight_tx: &mpsc::Sender<WeightSignal>,
    dispatch_mode: DispatchMode,
    dispatching: &Arc<DashMap<String, String>>,
    store: &Arc<TaskStore>,
    session_map: &HashMap<String, bool>,
    active_repos: &HashSet<String>,
) -> anyhow::Result<()> {
    let all_routed = store.list_all_by_status_global(TaskStatus::Routed).await?;
    let by_repo = group_orphaned_routed_tasks(all_routed, active_repos);

    if by_repo.is_empty() {
        return Ok(());
    }

    tracing::info!(
        repos = by_repo.len(),
        "global dispatch sweep: found routed tasks from inactive repos"
    );

    for (repo, tasks) in by_repo {
        let backend: Arc<dyn ExternalBackend> = match GitHubBackend::new(repo.clone()) {
            Ok(b) => Arc::new(b),
            Err(e) => {
                tracing::warn!(repo = %repo, err = %e, "global sweep: failed to construct backend, skipping repo");
                continue;
            }
        };
        let task_manager = Arc::new(TaskManager::with_store(
            Arc::clone(&backend),
            Arc::clone(store),
            repo.clone(),
        ));
        let runner = Arc::new(TaskRunner::new(repo.clone()).with_store(Arc::clone(store)));

        // Without a project directory the runner bails before any agent starts, so
        // block now instead of taking a dispatch slot first
        if let Err(e) = runner.resolve_project_dir().await
            && e.downcast_ref::<NoProjectDir>().is_some()
        {
            tracing::warn!(repo = %repo, count = tasks.len(), "global sweep: repo has no project directory, blocking its routed tasks");
            for task in &tasks {
                let ext = task
                    .external_id
                    .clone()
                    .unwrap_or_else(|| format!("internal:{}", task.id));
                block_task_without_project(&task_manager, store, &repo, &ext).await;
            }
            continue;
        }

        let dispatchable: Vec<ExternalTask> =
            tasks.iter().map(tasks::store_task_to_external).collect();

        if let Err(e) = dispatch_tasks_for_repo(
            &backend,
            tmux,
            &repo,
            &runner,
            capture,
            semaphore,
            &task_manager,
            weight_tx,
            dispatch_mode,
            dispatching,
            store,
            session_map,
            dispatchable,
        )
        .await
        {
            tracing::warn!(repo = %repo, err = %e, "global sweep: dispatch failed for repo");
        }
    }

    Ok(())
}
