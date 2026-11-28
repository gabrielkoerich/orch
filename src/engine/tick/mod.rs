//! Core tick loop phases.
//!
//! The engine ticks every ~10 seconds. Each tick runs six phases:
//! 1. Poll tmux for finished sessions
//! 2. Recover stuck in-progress tasks
//! 3. Route and dispatch:
//!    - 3a. Route new tasks to agents
//!    - 3b. Dispatch routed tasks (spawn agents)
//! 4. Unblock parents whose children are all done
//! 5. Run cron job scheduler
//!
//! [`tick_all`] runs them for every project. Phases 1-2 and 3b-5 run for all projects at
//! once; only routing is serial, because it holds the router write lock. Store-wide work
//! (`tick_global_sweeps`, `reconcile_dependencies`) runs once per tick, not once per project.

mod dispatch;
mod recovery;

use super::{EngineConfig, ProjectEngine};
use crate::backends::{ExternalBackend, ExternalId, ExternalTask, Status};
use crate::channels::capture::CaptureService;
use crate::channels::transport::Transport;
use crate::cooldown::{github_circuit_remaining_secs, is_github_circuit_open};
use crate::engine;
use crate::engine::router::config as router_config;
use crate::engine::router::{AllAgentsCooledError, Router, RouterConfig};
use crate::engine::runner::{TaskRunner, WeightSignal};
use crate::engine::sync;
use crate::engine::tasks;
use crate::engine::tasks::dependencies;
use crate::engine::tasks::jobs;
use crate::engine::tasks::{TaskManager, is_internal_id};
use crate::github::http;
use crate::repo_context::REPO_CONTEXT;
use crate::store;
use crate::store::StoreRoute;
use crate::store::Task;
use crate::store::TaskStatus;
use crate::store::TaskStore;
use crate::store::WaitingOnDependency;
use crate::store::{store_set_by_id, store_touch_updated_at};
use crate::tmux::TmuxManager;
use dashmap::{DashMap, DashSet};
use dispatch::skips_agent;
pub(crate) use dispatch::{
    DispatchMode, dispatch_mode_from_router, dispatch_routed_tasks_global, tick_dispatch_tasks,
};
use futures::future;
use recovery::{StuckTaskTiming, finalize_open_runs_for_recovery};
pub(crate) use recovery::{
    recover_untracked_repo_stuck_tasks, tick_detect_silent_agents, tick_recover_stuck_tasks,
};
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Instant, SystemTime};
use tokio::sync::RwLock;
use tokio::sync::{Semaphore, mpsc};

/// Per outer engine-loop quota for expensive routing calls.
///
/// Routing runs once per active project. Without a quota shared by all of them,
/// `router.max_tasks_per_tick = 1` still permits one slow LLM routing call per
/// project, multiplying 45s router timeouts into multi-minute watchdog stalls.
pub(crate) struct RoutingTickQuota {
    remaining: usize,
}

impl RoutingTickQuota {
    pub(crate) fn new(max: usize) -> Self {
        Self { remaining: max }
    }

    fn is_exhausted(&self) -> bool {
        self.remaining == 0
    }

    fn remaining(&self) -> usize {
        self.remaining
    }

    fn consume_one(&mut self) -> bool {
        if self.remaining == 0 {
            return false;
        }
        self.remaining -= 1;
        true
    }
}

/// Cleanup stale tmux sessions from previous engine runs.
/// Called once from `serve()` before the main loop, never from a tick
pub(crate) async fn startup_cleanup(tmux: &TmuxManager) {
    let start_time = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    tracing::info!(
        start_time,
        "running startup cleanup for stale tmux sessions"
    );

    match tmux.kill_stale_sessions(start_time).await {
        Ok(killed) => {
            if killed > 0 {
                tracing::info!(killed, "startup cleanup killed stale sessions");
            } else {
                tracing::debug!("no stale tmux sessions found on startup");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to run startup cleanup");
        }
    }
}

async fn recover_routed_blocked_dispatch(
    backend: &Arc<dyn ExternalBackend>,
    store: &Arc<TaskStore>,
    task_manager: &Arc<TaskManager>,
    repo: &str,
    task: &ExternalTask,
    timing: &StuckTaskTiming,
) {
    tracing::warn!(
        task_id = task.id.0,
        age_mins = timing.age.num_minutes(),
        threshold_mins = timing.threshold / 60,
        "recovering routed task: dispatch blocked by stale tmux session → new"
    );

    let resolved_store_id = match store.resolve_task_id(repo, &task.id.0).await {
        Ok(Some(id)) => Some(id),
        Ok(None) => {
            tracing::warn!(
                task_id = %task.id.0,
                repo,
                "resolve_task_id returned None during routed-task recovery"
            );
            None
        }
        Err(e) => {
            tracing::error!(
                task_id = %task.id.0,
                repo,
                error = %e,
                "resolve_task_id failed during routed-task recovery"
            );
            None
        }
    };

    if let Some(store_id) = resolved_store_id {
        if let Err(e) = store_set_by_id(
            Some(store),
            store_id,
            &[
                ("agent", serde_json::Value::Null),
                ("model", serde_json::Value::Null),
                ("route_attempts", serde_json::json!(0)),
            ],
        )
        .await
        {
            tracing::warn!(
                task_id = task.id.0,
                ?e,
                "failed to clear routing fields for routed stuck task — will retry next tick"
            );
            return;
        }
        finalize_open_runs_for_recovery(
            store,
            store_id,
            "stuck-task recovery: stale session blocking dispatch killed",
        )
        .await;
    }

    if let Err(e) = task_manager.update_task_status(&task.id, Status::New).await {
        tracing::warn!(
            task_id = task.id.0,
            ?e,
            "failed to reset routed stuck task status"
        );
        return;
    }

    store::store_log_activity(
        &Some(Arc::clone(store)),
        repo,
        &task.id.0,
        "rerouted",
        Some("routed"),
        Some("new"),
        None::<&str>,
        None::<&str>,
        Some(&serde_json::json!({
            "failure_reason": "routed_blocked_by_existing_session",
            "age_minutes": timing.age.num_minutes(),
            "had_session": true,
        })),
    )
    .await;

    if !is_internal_id(&task.id.0) {
        let backend_clone = Arc::clone(backend);
        let task_id_clone = task.id.clone();
        let body = format!(
            "[{}] recovered: stuck routed task — existing tmux session blocked dispatch for {}m; cleared route for retry{}",
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
            timing.age.num_minutes(),
            engine::orch_footer()
        );
        tokio::spawn(async move {
            if let Err(e) = backend_clone.post_comment(&task_id_clone, &body).await {
                tracing::warn!(
                    task_id = task_id_clone.0,
                    ?e,
                    "failed to post routed-task recovery comment"
                );
            }
        });
    }
}

/// Phase 1 of tick: poll tmux for finished sessions and clean them up.
pub(crate) async fn tick_check_session_completions(
    tmux: &Arc<TmuxManager>,
    repo: &str,
    capture: &Arc<CaptureService>,
    store: &Arc<TaskStore>,
) -> anyhow::Result<()> {
    let _span = tracing::info_span!("engine.tick.phase1.sessions").entered();
    let session_snapshot = tmux.snapshot().await;
    // Derive the short project name from repo (owner/repo -> repo), matching
    // TmuxManager::session_name's derivation exactly (including the ".git" strip)
    // so a repo configured with a ".git" suffix doesn't silently fail to match
    // its own live sessions here — leaving stuck-task recovery as the only
    // (later, coarser-grained) safety net for this repo's session completions.
    let repo_name = repo
        .rsplit('/')
        .next()
        .unwrap_or(repo)
        .trim_end_matches(".git");
    for (session, active) in session_snapshot {
        // Only handle sessions that belong to this repo/project.
        if session.project != repo_name {
            continue;
        }

        if !active {
            tracing::info!(
                session = %session.name,
                task_id = %session.task_id,
                "session completed, collecting results"
            );
            // Touch the store.updated_at immediately when we observe a completed
            // session so the stuck-task recovery phase doesn't incorrectly
            // reclaim this task while the runner is still finishing
            // post-processing (race: session observed dead → tick reclaim).
            // Best-effort: no-ops if the task isn't present in the store.
            //
            // session.task_id uses hyphens (e.g. "internal-63714") because the
            // tmux session name sanitizes colons to hyphens. resolve_task_id only
            // handles "internal:" prefix, so we must convert back before touching.
            // Review sessions carry a "-review" suffix tracked under the main task.
            let store_task_id = {
                let base = session
                    .task_id
                    .strip_suffix("-review")
                    .unwrap_or(&session.task_id);
                if let Some(n) = base.strip_prefix("internal-") {
                    format!("internal:{n}")
                } else {
                    base.to_string()
                }
            };
            store_touch_updated_at(&Some(Arc::clone(store)), repo, &store_task_id).await;
            // Unregister from capture service using the task id the capture service
            // was registered under (task id without project prefix).
            capture.unregister_session(repo, &session.task_id).await;
            // Kill the actual tmux session name we discovered (do not reconstruct)
            if let Err(e) = tmux.kill_session(&session.name).await {
                tracing::debug!(
                    session = %session.name,
                    ?e,
                    "kill_session failed (session may already be gone)"
                );
            }
        }
    }
    Ok(())
}

/// The tier whose run timeout the task runs with: the tier of its stored model, which differs from
/// its own complexity when the model came from an adjacent tier
fn run_tier(router: &RouterConfig, task: &Task) -> String {
    match (task.agent.as_deref(), task.model.as_deref()) {
        (Some(a), Some(m)) if !m.is_empty() => router.tier_of_model(a, m, &task.complexity),
        _ => task.complexity.clone(),
    }
}

/// The router config for this tick, loaded on first use. A full load re-reads the config and
/// repeats its startup warnings, so one tick shares one load across every live run
fn tick_router(
    cell: &OnceLock<RouterConfig>,
    load: impl FnOnce() -> RouterConfig,
) -> &RouterConfig {
    cell.get_or_init(load)
}

fn should_use_backend(task_id: &str) -> bool {
    !is_internal_id(task_id)
}

/**
 * The three store-wide recovery sweeps, run once per tick rather than once per project.
 *
 * Each one reads every repo, so a per-project call duplicates the work. The NeedsReview
 * sweep also reads `needs_review_refires` and then increments it, so concurrent per-project
 * passes all see the same count and four projects burn four of the five refire attempts in
 * one tick (#3864)
 **/
pub(crate) async fn tick_global_sweeps(
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
    router: &Router,
    dispatching: &Arc<DashMap<String, String>>,
    auto_merge_in_flight: &Arc<DashSet<String>>,
    active_repos: &HashSet<String>,
) {
    // Global sweep for CI-failure blocked tasks from inactive or removed projects.
    // auto_unblock_blocked_tasks runs inside sync_tick and is scoped to the active repo.
    // Tasks blocked for projects no longer in orch config are never processed there.
    // This sweep covers all repos so stale CI-failure blocks are eventually resolved.
    if let Err(e) = sync::auto_unblock_ci_failure_blocked_tasks_global(
        task_manager,
        store,
        auto_merge_in_flight,
    )
    .await
    {
        tracing::warn!(err = %e, "global CI-failure blocked sweep failed");
    }

    // Re-fire stale NeedsReview tasks and escalate persistently-stuck ones to
    // Blocked, across all repos
    if let Err(e) = sync::refire_and_escalate_stale_needs_review_global(
        task_manager,
        store,
        router,
        dispatching,
        active_repos,
    )
    .await
    {
        tracing::warn!(err = %e, "global NeedsReview refire/escalation sweep failed");
    }

    // Recover rebroadcast-blocked tasks once a review agent is routable, across
    // all repos
    if let Err(e) = sync::auto_recover_rebroadcast_blocked_tasks_global(
        router,
        task_manager,
        store,
        active_repos,
    )
    .await
    {
        tracing::warn!(err = %e, "global rebroadcast-blocked recovery sweep failed");
    }
}

/// Phase 3a of tick: route status:new tasks to an agent and transition them to status:routed.
pub(crate) async fn tick_route_tasks(
    backend: &Arc<dyn ExternalBackend>,
    task_manager: &Arc<TaskManager>,
    router: &mut Router,
    store: &Arc<TaskStore>,
    repo: &str,
    routing_quota: &mut RoutingTickQuota,
) -> anyhow::Result<()> {
    let _span = tracing::info_span!("engine.tick.phase3a.route").entered();
    if let Err(err) = dependencies::apply_claim_gates(store).await {
        tracing::warn!(err = %err, "dependency claim gate failed");
    }
    if routing_quota.is_exhausted() {
        tracing::debug!("routing quota exhausted for this tick — skipping repo routing phase");
        return Ok(());
    }

    // Global GitHub 5xx circuit breaker — skip routing during sustained GitHub outages
    // to avoid routing-heavy retry storms. Tasks will remain in 'new' status and be
    // retried when the circuit closes.
    if is_github_circuit_open() {
        let remaining = github_circuit_remaining_secs();
        tracing::info!(
            remaining_secs = remaining,
            "GitHub 5xx circuit breaker open — skipping routing phase"
        );
        return Ok(());
    }
    let new_tasks = task_manager.list_routable().await?;
    let routable: Vec<&ExternalTask> = new_tasks
        .iter()
        .filter(|t| !skips_agent(&t.labels))
        .collect();

    // Pre-emptive health check: refresh degraded-agent flags once per tick,
    // not per-task (previously this ran inside route() causing N redundant
    // DB queries when routing N tasks in the same tick).
    if !routable.is_empty() {
        let start = Instant::now();
        router.refresh_health(store).await;
        tracing::debug!(
            duration_ms = start.elapsed().as_millis(),
            "router.refresh_health completed"
        );
    }

    // Limit routing to the remaining global quota for this outer engine tick.
    for task in routable.into_iter().take(routing_quota.remaining()) {
        if !routing_quota.consume_one() {
            break;
        }
        let _task_span = tracing::info_span!("engine.route", task_id = %task.id.0).entered();

        let task_start = Instant::now();
        match router.route(task, store, repo).await {
            Ok(result) => {
                tracing::debug!(task_id = %task.id.0, duration_ms = task_start.elapsed().as_millis(), "route completed");
                // Store route result in store
                if let Err(e) = router
                    .store_route_result(&task.id.0, &result, store, repo)
                    .await
                {
                    tracing::warn!(
                        task_id = task.id.0,
                        ?e,
                        "failed to store route result; skipping Routed transition"
                    );
                    continue;
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
                        continue;
                    }
                } else {
                    // Fire-and-forget: label operations are cosmetic (routing already
                    // persisted in DB). Spawning them prevents blocking the tick loop
                    // on 6-9 sequential HTTP calls (~3-9s overhead per routed task).
                    let backend_clone = Arc::clone(backend);
                    let task_id_clone = task.id.clone();
                    let task_labels: Vec<String> = task.labels.clone();
                    let new_labels = {
                        let mut labels = vec![
                            format!("agent:{}", result.agent),
                            format!("complexity:{}", result.complexity),
                        ];
                        if let Some(ref model) = result.model {
                            labels.push(format!("model:{model}"));
                        }
                        labels
                    };
                    tokio::spawn(async move {
                        // Remove old agent/complexity/model labels
                        for label in &task_labels {
                            if (label.starts_with("agent:")
                                || label.starts_with("complexity:")
                                || label.starts_with("model:"))
                                && let Err(e) =
                                    backend_clone.remove_label(&task_id_clone, label).await
                            {
                                tracing::warn!(task_id = task_id_clone.0, label, error = %e, "failed to remove stale routing label during re-route");
                            }
                        }
                        // Add new labels
                        if let Err(e) = backend_clone.set_labels(&task_id_clone, &new_labels).await
                        {
                            tracing::warn!(
                                task_id = task_id_clone.0,
                                ?e,
                                "failed to set routing labels"
                            );
                        }
                    });

                    // Transition to routed
                    if let Err(e) = task_manager
                        .update_task_status(&task.id, Status::Routed)
                        .await
                    {
                        tracing::warn!(task_id = task.id.0, ?e, "failed to set status:routed");
                        continue;
                    }
                }

                // Dual-write: upsert task + store route result in SQLite
                match store.upsert_external(&task.upsert_row(repo)).await {
                    Ok(store_id) => {
                        let profile_json =
                            serde_json::to_string(&result.profile).unwrap_or_default();
                        if let Err(e) = store
                            .store_route(&crate::store::StoreRoute {
                                id: store_id,
                                agent: &result.agent,
                                model: result.model.as_deref(),
                                complexity: &result.complexity,
                                estimate: result.estimate,
                                reason: &result.reason,
                                profile: &profile_json,
                            })
                            .await
                        {
                            tracing::debug!(
                                task_id = task.id.0,
                                ?e,
                                "dual-write: store_route failed"
                            );
                        }
                        if let Err(e) = store.update_status(store_id, TaskStatus::Routed).await {
                            tracing::debug!(
                                task_id = task.id.0,
                                ?e,
                                "dual-write: update_status failed"
                            );
                        }
                        // Sync the estimate to GitHub Projects if the router assigned one.
                        // Fire-and-forget: do NOT await inline — GraphQL mutations can take
                        // up to 30 s and would block the entire tick loop on every routed task.
                        if result.estimate > 0 {
                            let backend_clone = Arc::clone(backend);
                            let task_id = task.id.clone();
                            let estimate = result.estimate;
                            tokio::spawn(async move {
                                if let Err(e) = backend_clone
                                    .sync_estimate_to_project(&task_id, estimate)
                                    .await
                                {
                                    tracing::debug!(
                                        task_id = task_id.0,
                                        estimate,
                                        err = %e,
                                        "dual-write: estimate project sync failed"
                                    );
                                }
                            });
                        }
                    }
                    Err(e) => {
                        tracing::debug!(
                            task_id = task.id.0,
                            ?e,
                            "dual-write: upsert_external failed"
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
                if let Some(cooled) = e.downcast_ref::<AllAgentsCooledError>() {
                    tracing::error!(
                        task_id = %task.id.0,
                        scope = %cooled.scope(),
                        remaining_secs = ?cooled.remaining_secs(),
                        degraded = ?cooled.degraded(),
                        "ALL AGENTS COOLED — no agent available to route this task. \
                         Task stays in 'new' and will be retried on every tick. \
                         Check `orch cooldown list` to see what is blocking dispatch."
                    );
                    continue;
                }
                if let Some(waiting) = e.downcast_ref::<WaitingOnDependency>() {
                    tracing::info!(
                        task_id = %task.id.0,
                        blockers = %waiting.blockers.join(", "),
                        "task is waiting on a dependency"
                    );
                    continue;
                }
                tracing::error!(task_id = task.id.0, ?e, "routing failed, skipping task");
            }
        }
    }
    Ok(())
}

/// Global routing sweep: route status:new tasks from inactive or removed repos.
///
/// `tick_route_tasks` is scoped to the active repo per-tick and never processes
/// tasks whose `repo` field differs from the current project. When a project is
/// removed from the orch config, its tasks accumulate in `new` indefinitely
/// because no tick ever runs for that repo. This sweep covers all repos so
/// orphaned new tasks are eventually routed.
///
/// `active_repos` must be the full set of currently-configured project repos,
/// not just the repo of the tick that called this — mirroring
/// `dispatch_routed_tasks_global` (see its doc comment for the "racing the real
/// per-repo dispatch" failure mode this guards against). Filtering on
/// `t.repo != current_repo` alone (the previous behavior) made every *other*
/// active repo's new tasks look "orphaned" too and routed them a tick early,
/// through the wrong repo's tick.
pub(crate) async fn route_new_tasks_global(
    router: &mut Router,
    store: &Arc<TaskStore>,
    active_repos: &HashSet<String>,
    routing_quota: &mut RoutingTickQuota,
) -> anyhow::Result<()> {
    if routing_quota.is_exhausted() {
        tracing::debug!("routing quota exhausted for this tick — skipping global routing sweep");
        return Ok(());
    }

    let all_new = store.list_all_by_status_global(TaskStatus::New).await?;
    let orphaned: Vec<&Task> = all_new
        .iter()
        .filter(|t| !active_repos.contains(&t.repo))
        .filter(|t| !skips_agent(&t.labels))
        .collect();

    if orphaned.is_empty() {
        return Ok(());
    }

    tracing::info!(
        count = orphaned.len(),
        "global routing sweep: found new tasks from inactive repos"
    );

    router.refresh_health(store).await;

    for task in orphaned.into_iter().take(routing_quota.remaining()) {
        if !routing_quota.consume_one() {
            break;
        }
        let ext_task = tasks::store_task_to_external(task);
        let task_start = Instant::now();
        match router.route(&ext_task, store, &task.repo).await {
            Ok(result) => {
                tracing::debug!(
                    task_id = %ext_task.id.0,
                    repo = %task.repo,
                    duration_ms = task_start.elapsed().as_millis(),
                    "global sweep: route completed"
                );
                let profile_json = serde_json::to_string(&result.profile).unwrap_or_default();
                if let Err(e) = store
                    .store_route(&StoreRoute {
                        id: task.id,
                        agent: &result.agent,
                        model: result.model.as_deref(),
                        complexity: &result.complexity,
                        estimate: result.estimate,
                        reason: &result.reason,
                        profile: &profile_json,
                    })
                    .await
                {
                    tracing::warn!(
                        task_id = %ext_task.id.0,
                        repo = %task.repo,
                        err = %e,
                        "global sweep: failed to store route result; skipping status update"
                    );
                    continue;
                }
                match store.update_status(task.id, TaskStatus::Routed).await {
                    Err(e) => {
                        tracing::warn!(
                            task_id = %ext_task.id.0,
                            repo = %task.repo,
                            err = %e,
                            "global sweep: failed to set status:routed"
                        );
                    }
                    _ => {
                        tracing::info!(
                            task_id = %ext_task.id.0,
                            repo = %task.repo,
                            agent = %result.agent,
                            complexity = %result.complexity,
                            "global sweep: task routed"
                        );
                    }
                }
                if let Some(ref warning) = result.warning {
                    tracing::warn!(task_id = %ext_task.id.0, warning, "global sweep: routing sanity warning");
                }
            }
            Err(e) => {
                if let Some(cooled) = e.downcast_ref::<AllAgentsCooledError>() {
                    tracing::error!(
                        task_id = %ext_task.id.0,
                        repo = %task.repo,
                        scope = %cooled.scope(),
                        remaining_secs = ?cooled.remaining_secs(),
                        degraded = ?cooled.degraded(),
                        "global sweep: ALL AGENTS COOLED — task stays in 'new'"
                    );
                    continue;
                }
                if let Some(waiting) = e.downcast_ref::<WaitingOnDependency>() {
                    tracing::info!(
                        task_id = %ext_task.id.0,
                        repo = %task.repo,
                        blockers = %waiting.blockers.join(", "),
                        "global sweep: task is waiting on a dependency"
                    );
                    continue;
                }
                tracing::error!(
                    task_id = %ext_task.id.0,
                    repo = %task.repo,
                    err = ?e,
                    "global sweep: routing failed, skipping task"
                );
            }
        }
    }

    Ok(())
}

/// Phase 4 of tick: unblock parent tasks whose sub-issues are all done.
pub(crate) async fn tick_unblock_parents(
    backend: &Arc<dyn ExternalBackend>,
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
    repo: &str,
) -> anyhow::Result<()> {
    let blocked = task_manager.list_all_by_status(Status::Blocked).await?;

    // Pre-filter: skip tasks that are blocked for a known reason (e.g. CI failure,
    // escalation). These are not waiting on children, so there is no point making
    // a GraphQL call for them.
    let store_blocked = store.list_by_status(repo, TaskStatus::Blocked).await?;
    let has_block_reason: HashSet<String> = store_blocked
        .iter()
        .filter(|t| t.block_reason.is_some())
        .filter_map(|t| t.external_id.clone())
        .collect();

    // Spawn a concurrent task for each candidate blocked task.
    // Previously these ran sequentially; get_sub_issues is a GraphQL call taking 1-3 s each,
    // and with 4 blocked tasks that was 4-12 s of sequential latency every tick.
    let handles: Vec<_> = blocked
        .into_iter()
        .filter(|task| {
            if has_block_reason.contains(&task.id.0) {
                tracing::debug!(
                    task_id = task.id.0,
                    "skipping blocked task with known block_reason"
                );
                false
            } else {
                true
            }
        })
        .map(|task| {
            let backend_clone = Arc::clone(backend);
            let task_manager_clone = Arc::clone(task_manager);
            let store_clone = Arc::clone(store);
            let repo_owned = repo.to_string();
            tokio::spawn(async move {
                let task_id = &task.id;

                let mut children: Vec<ExternalId> = Vec::new();
                let mut child_statuses: HashMap<
                    String,
                    TaskStatus,
                > = HashMap::new();

                match store_clone.resolve_task_id(&repo_owned, &task_id.0).await {
                    Ok(Some(parent_store_id)) => {
                        match store_clone.list_children(&repo_owned, parent_store_id).await {
                            Ok(store_children) => {
                                for child in store_children {
                                    let Some(child_external_id) = child.external_id else {
                                        continue;
                                    };
                                    child_statuses.insert(child_external_id.clone(), child.status);
                                    children.push(ExternalId(child_external_id));
                                }
                            }
                            Err(e) => {
                                tracing::debug!(
                                    task_id = task_id.0,
                                    ?e,
                                    "failed to list store-linked children"
                                );
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::debug!(
                            task_id = task_id.0,
                            ?e,
                            "failed to resolve blocked task in store for child lookup"
                        );
                    }
                }

                if !is_internal_id(&task_id.0) {
                    match backend_clone.get_sub_issues(task_id).await {
                        Ok(ids) => {
                            let mut seen: HashSet<String> =
                                children.iter().map(|child| child.0.clone()).collect();
                            for id in ids {
                                if seen.insert(id.0.clone()) {
                                    children.push(id);
                                }
                            }
                        }
                        Err(e) => {
                            tracing::debug!(task_id = task_id.0, ?e, "failed to get sub-issues");
                        }
                    }
                }

                // No children means nothing to wait on — skip (may be blocked for other reasons)
                if children.is_empty() {
                    return;
                }

                // Check if every child is done using the local store (batched) and
                // fall back to the backend only for children not present in the store.
                let mut all_done = true;
                // Build owned strings for the batch lookup (needed for &str slices below)
                let child_ext_strs: Vec<String> =
                    children.iter().map(|c| c.0.clone()).collect();
                let child_exts: Vec<&str> =
                    child_ext_strs.iter().map(String::as_str).collect();
                // Query store for statuses of any children present locally
                match store_clone
                    .get_statuses_by_external_ids(&repo_owned, &child_exts)
                    .await
                { Ok(map) => {
                    for (k, v) in map {
                        child_statuses.insert(k, v);
                    }
                } _ => {
                    tracing::debug!(
                        task_id = task_id.0,
                        "failed to batch-query store for child statuses; falling back to per-child lookups"
                    );
                }}

                for child_id in &children {
                    // Check store first
                    if let Some(status) = child_statuses.get(&child_id.0) {
                        if *status != TaskStatus::Done {
                            all_done = false;
                            break;
                        }
                        continue;
                    }

                    // Not found in store — fall back to backend check
                    match backend_clone.get_task(child_id).await {
                        Ok(child) => {
                            if !child.labels.iter().any(|l| l == Status::Done.as_label()) {
                                all_done = false;
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::debug!(task_id = task_id.0, child = %child_id.0, ?e, "failed to fetch child task from backend — treating as not done");
                            all_done = false;
                            break;
                        }
                    }
                }

                if all_done {
                    tracing::info!(
                        task_id = task_id.0,
                        children = children.len(),
                        "all children done, unblocking parent"
                    );
                    if let Err(e) =
                        task_manager_clone.update_task_status(task_id, Status::New).await
                    {
                        tracing::warn!(task_id = task_id.0, ?e, "failed to unblock parent");
                    }
                }
            })
        })
        .collect();

    for handle in handles {
        if let Err(e) = handle.await {
            tracing::warn!(?e, "phase 4 sub-issue check panicked");
        }
    }
    Ok(())
}

/// Phase 5 of tick: run cron job matching and fire any due jobs.
pub(crate) async fn tick_job_scheduler(
    jobs_path: &Path,
    backend: &Arc<dyn ExternalBackend>,
    store: Option<&Arc<TaskStore>>,
    repo: &str,
    transport: Option<&Arc<Transport>>,
) -> anyhow::Result<()> {
    jobs::tick(jobs_path, backend, store, repo, transport).await
}

/// Core tick for one project, up to the routing phase.
///
/// Every phase here is scoped to one project, so all projects can run this at the same time:
/// 1. `tick_check_session_completions` — poll tmux for finished sessions
/// 2. `tick_detect_silent_agents`      — cooldown agents that produced no output
/// 3. `tick_recover_stuck_tasks`       — reset in_progress/in_review tasks with no session
///
/// The store-wide work that used to sit in here runs once per tick in `tick_all`:
/// `tick_global_sweeps` and `reconcile_dependencies`.
#[expect(
    clippy::too_many_arguments,
    reason = "each argument is engine state the tick passes down"
)]
pub(crate) async fn tick_before_routing(
    backend: &Arc<dyn ExternalBackend>,
    tmux: &Arc<TmuxManager>,
    repo: &str,
    capture: &Arc<CaptureService>,
    config: &EngineConfig,
    task_manager: &Arc<TaskManager>,
    dispatching: &Arc<DashMap<String, String>>,
    store: &Arc<TaskStore>,
    session_map: &HashMap<String, bool>,
) {
    let mut phases: Vec<(&str, u128)> = Vec::new();
    let mut at = Instant::now();

    if let Err(e) = tick_check_session_completions(tmux, repo, capture, store).await {
        tracing::warn!(err = %e, "session completion phase failed");
    }
    phases.push(("sessions", at.elapsed().as_millis()));

    // Every node tends its own sessions, only the leader decides. Followers dispatch once
    // tasks are assigned to nodes
    if !store.is_leader() {
        super::log_phase_timings("tick.pre_routing", repo, &phases);
        return;
    }

    at = Instant::now();
    if let Err(e) =
        tick_detect_silent_agents(tmux, repo, capture, backend, task_manager, config, store).await
    {
        tracing::warn!(err = %e, "silent-agent detection phase failed");
    }
    phases.push(("silent_agents", at.elapsed().as_millis()));

    at = Instant::now();
    if let Err(e) = tick_recover_stuck_tasks(
        backend,
        tmux,
        repo,
        task_manager,
        config,
        store,
        session_map,
        dispatching,
    )
    .await
    {
        tracing::warn!(err = %e, "stuck-task recovery phase failed");
    }
    phases.push(("stuck_tasks", at.elapsed().as_millis()));

    super::log_phase_timings("tick.pre_routing", repo, &phases);
}

/// Core tick for one project, after the routing phase.
///
/// Takes the router's decision as a `DispatchMode` value instead of the router itself, so
/// every project can run this at the same time:
/// 1. `tick_dispatch_tasks`  — spawn agents for status:routed tasks
/// 2. `tick_unblock_parents` — unblock parents whose sub-issues are all done
/// 3. `tick_job_scheduler`   — fire due cron jobs
#[expect(
    clippy::too_many_arguments,
    reason = "each argument is engine state the tick passes down"
)]
pub(crate) async fn tick_after_routing(
    backend: &Arc<dyn ExternalBackend>,
    tmux: &Arc<TmuxManager>,
    repo: &str,
    runner: &Arc<TaskRunner>,
    capture: &Arc<CaptureService>,
    semaphore: &Arc<Semaphore>,
    project_dir: &Path,
    task_manager: &Arc<TaskManager>,
    weight_tx: &mpsc::Sender<WeightSignal>,
    dispatch_mode: DispatchMode,
    dispatching: &Arc<DashMap<String, String>>,
    store: &Arc<TaskStore>,
    transport: Option<&Arc<Transport>>,
    session_map: &HashMap<String, bool>,
) {
    let mut phases: Vec<(&str, u128)> = Vec::new();
    let mut at = Instant::now();

    if let Err(e) = tick_dispatch_tasks(
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
    )
    .await
    {
        tracing::warn!(err = %e, "dispatch phase failed");
    }
    phases.push(("dispatch", at.elapsed().as_millis()));

    at = Instant::now();
    if let Err(e) = tick_unblock_parents(backend, task_manager, store, repo).await {
        tracing::warn!(err = %e, "parent-unblock phase failed");
    }
    phases.push(("unblock_parents", at.elapsed().as_millis()));

    at = Instant::now();
    let jobs_path = project_dir.join(".orch.yml");
    if let Err(e) = tick_job_scheduler(&jobs_path, backend, Some(store), repo, transport).await {
        tracing::error!(?e, "job scheduler tick failed");
    }
    phases.push(("jobs", at.elapsed().as_millis()));

    super::log_phase_timings("tick.post_routing", repo, &phases);
}

/// Core tick — runs every 10s for every project.
///
/// Projects no longer wait on each other: the phases around routing run for all of them at
/// once, so tick time tracks the slowest project instead of the sum. Routing stays serial
/// because it needs the router write lock and one shared `RoutingTickQuota`, which is what
/// gives `router.max_tasks_per_tick` its meaning.
///
/// The tmux session map, the global sweeps and the dependency reconcile are the same for
/// every project, so they run once per tick rather than once per project.
#[expect(
    clippy::too_many_arguments,
    reason = "each argument is engine state the tick passes down"
)]
pub(crate) async fn tick_all(
    engines: &[ProjectEngine],
    tmux: &Arc<TmuxManager>,
    capture: &Arc<CaptureService>,
    semaphore: &Arc<Semaphore>,
    config: &EngineConfig,
    router: &Arc<RwLock<Router>>,
    weight_tx: &mpsc::Sender<WeightSignal>,
    dispatching: &Arc<DashMap<String, String>>,
    transport: Option<&Arc<Transport>>,
    active_repos: &HashSet<String>,
    auto_merge_in_flight: &Arc<DashSet<String>>,
) {
    let _tick_span = tracing::info_span!("engine.tick").entered();

    let Some(first) = engines.first() else {
        return;
    };
    // All project engines share one SQLite store, so leadership and the global sweeps below
    // are the same whichever project they are read from
    let store = &first.store;

    // Calls made since the previous tick, so hourly quota exhaustion can be traced
    for (project, call_type, calls) in http::drain_call_counts().into_iter().take(10) {
        tracing::debug!(
            project,
            call_type,
            calls,
            "github api calls since last tick"
        );
    }

    // One `tmux list-panes -a` for every project's stuck-task recovery, instead of one each
    let session_map = tmux.batch_session_active().await;

    let mut passes = Vec::with_capacity(engines.len());
    for engine in engines {
        passes.push(REPO_CONTEXT.scope(
            engine.repo.clone(),
            tick_before_routing(
                &engine.backend,
                tmux,
                &engine.repo,
                capture,
                config,
                &engine.task_manager,
                dispatching,
                &engine.store,
                &session_map,
            ),
        ));
    }
    future::join_all(passes).await;

    if !store.is_leader() {
        return;
    }

    // Store-wide, so once per tick rather than once per project. Both read every repo, and
    // the NeedsReview sweep increments a counter it just read, which concurrent per-project
    // passes would each consume (#3864)
    let at = Instant::now();
    {
        let router_guard = router.read().await;
        tick_global_sweeps(
            &first.task_manager,
            store,
            &router_guard,
            dispatching,
            auto_merge_in_flight,
            active_repos,
        )
        .await;
    }
    let sweeps_ms = at.elapsed().as_millis();

    let at = Instant::now();
    recover_untracked_repo_stuck_tasks(
        tmux,
        config,
        store,
        &session_map,
        dispatching,
        active_repos,
    )
    .await;
    let untracked_ms = at.elapsed().as_millis();

    let at = Instant::now();
    if let Err(err) = dependencies::reconcile_dependencies(
        first.backend.as_ref(),
        store,
        transport.map(AsRef::as_ref),
    )
    .await
    {
        tracing::warn!(err = %err, "dependency reconcile failed");
    }
    super::log_phase_timings(
        "tick.global",
        &first.repo,
        &[
            ("global_sweeps", sweeps_ms),
            ("untracked_stuck", untracked_ms),
            ("dependencies", at.elapsed().as_millis()),
        ],
    );

    {
        let mut router_guard = router.write().await;
        let mut routing_quota = RoutingTickQuota::new(router_config::max_tasks_per_routing_tick());
        for engine in engines {
            let at = Instant::now();
            REPO_CONTEXT
                .scope(engine.repo.clone(), async {
                    if let Err(e) = tick_route_tasks(
                        &engine.backend,
                        &engine.task_manager,
                        &mut router_guard,
                        &engine.store,
                        &engine.repo,
                        &mut routing_quota,
                    )
                    .await
                    {
                        tracing::warn!(err = %e, "routing phase failed");
                    }
                })
                .await;
            super::log_phase_timings(
                "tick.routing",
                &engine.repo,
                &[("route", at.elapsed().as_millis())],
            );
        }

        // Global routing sweep for tasks from inactive or removed repos.
        // tick_route_tasks is scoped to the active repo; tasks from projects no longer
        // in config are never returned by list_routable and would stay in 'new' indefinitely.
        if let Err(e) =
            route_new_tasks_global(&mut router_guard, store, active_repos, &mut routing_quota).await
        {
            tracing::warn!(err = %e, "global new-task routing sweep failed");
        }
    }

    let dispatch_mode = dispatch_mode_from_router(&*router.read().await);

    // Re-read the session map: the completion phase above kills the sessions of finished
    // agents, and dispatch must not treat a killed session as one that still blocks a task
    let session_map = tmux.batch_session_active().await;

    let mut passes = Vec::with_capacity(engines.len());
    for engine in engines {
        passes.push(REPO_CONTEXT.scope(
            engine.repo.clone(),
            tick_after_routing(
                &engine.backend,
                tmux,
                &engine.repo,
                &engine.runner,
                capture,
                semaphore,
                &engine.project_dir,
                &engine.task_manager,
                weight_tx,
                dispatch_mode,
                dispatching,
                &engine.store,
                transport,
                &session_map,
            ),
        ));
    }
    future::join_all(passes).await;

    // Global dispatch sweep for routed tasks from inactive or removed repos.
    // tick_dispatch_tasks is scoped to each active repo; tasks from projects no longer
    // in config are never returned by list_external_by_status and would stay in
    // 'routed' indefinitely (see #3413). Excludes the full active-repo set, not just
    // one repo, so it doesn't treat other active repos as orphaned.
    if let Err(e) = dispatch_routed_tasks_global(
        tmux,
        capture,
        semaphore,
        weight_tx,
        dispatch_mode,
        dispatching,
        store,
        &session_map,
        active_repos,
    )
    .await
    {
        tracing::warn!(err = %e, "global routed-task dispatch sweep failed");
    }
}

/// Status a finished run moves to. Only a successful run with no PR (or with the review agent
/// off) becomes `done`. A failed run stays in `needs_review` with its issue open
pub(crate) fn display_status_for(
    signal: &WeightSignal,
    enable_review: bool,
    has_pr: bool,
) -> &'static str {
    match signal {
        WeightSignal::Success { .. } if enable_review => {
            if has_pr {
                "needs_review"
            } else {
                "done"
            }
        }
        // Silence detection or another non-rate-limit reset already moved the task to "new".
        // Converting it to needs_review would trigger the no-code shortcut and falsely close it
        WeightSignal::RateLimited { .. } | WeightSignal::Rerouted => "new",
        WeightSignal::Blocked => "blocked",
        // The review path re-routes a task with no PR and no commits
        WeightSignal::Failed { .. } => "needs_review",
        // None means the runner guard skipped the task or the status was unrecognised
        WeightSignal::None if enable_review => "needs_review",
        WeightSignal::Success { .. } | WeightSignal::None => "done",
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod recovery_tests;

#[cfg(test)]
mod dispatch_tests;
