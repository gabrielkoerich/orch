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

use super::{EngineConfig, ProjectEngine};
use crate::backends::github::GitHubBackend;
use crate::backends::{ExternalBackend, ExternalId, ExternalTask, Status};
use crate::channels::capture::CaptureService;
use crate::channels::transport::Transport;
use crate::cmd_cache;
use crate::config;
use crate::engine;
use crate::engine::cooldown::{
    SILENCE_AGENT_COOLDOWN_SECS, SILENCE_EXTENDED_COOLDOWN_SECS, github_circuit_remaining_secs,
    is_agent_in_cooldown, is_github_circuit_open, is_model_in_cooldown,
    record_agent_failure_with_message, record_silence_detection, set_agent_cooldown,
    set_model_cooldown,
};
use crate::engine::dependencies;
use crate::engine::dispatch_guard::DispatchGuard;
use crate::engine::jobs;
use crate::engine::priority;
use crate::engine::priority::Priority;
use crate::engine::router::config as router_config;
use crate::engine::router::{
    AllAgentsCooledError, RouteResultError, Router, RouterConfig, get_route_result,
};
use crate::engine::runner::response;
use crate::engine::runner::{NoProjectDir, TaskRunner, WeightSignal};
use crate::engine::sync;
use crate::engine::tasks;
use crate::engine::tasks::{TaskManager, is_internal_id};
use crate::github::http;
use crate::repo_context::REPO_CONTEXT;
use crate::store;
use crate::store::StoreRoute;
use crate::store::Task;
use crate::store::TaskStatus;
use crate::store::TaskStatus as DbStatus;
use crate::store::TaskStore;
use crate::store::WaitingOnDependency;
use crate::store::{store_set_by_id, store_touch_updated_at};
use crate::tmux::TmuxManager;
use dashmap::mapref::entry::Entry;
use dashmap::{DashMap, DashSet};
use futures::future;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::{Instant, SystemTime};
use tokio::sync::RwLock;
use tokio::sync::{Semaphore, mpsc};
use tokio::time;

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

#[derive(Debug)]
struct StuckTaskTiming {
    has_session: bool,
    age: chrono::Duration,
    threshold: u64,
}

fn stuck_task_timing_from_map(
    tmux: &Arc<TmuxManager>,
    repo: &str,
    session_task_id: &str,
    updated_at: &str,
    config: &EngineConfig,
    parse_error_message: &'static str,
    session_map: &HashMap<String, bool>,
) -> Option<StuckTaskTiming> {
    let has_session = session_alive(tmux, repo, session_task_id, session_map);
    let threshold = if has_session {
        config.stuck_timeout
    } else {
        config.no_session_stuck_timeout
    };

    let updated = match chrono::DateTime::parse_from_rfc3339(updated_at) {
        Ok(dt) => dt.with_timezone(&chrono::Utc),
        Err(e) => {
            tracing::warn!(
                task_id = session_task_id,
                updated_at,
                ?e,
                error_message = parse_error_message,
                "cannot parse updated_at"
            );
            return None;
        }
    };

    let raw_age = chrono::Utc::now() - updated;
    // Host suspend (laptop sleep) freezes the process just like it freezes the agent
    // running inside it — that frozen time is not evidence of a hang. Discount any
    // detected suspend/resume gaps that occurred since this task was last touched.
    let suspended = super::suspend::suspended_duration_since(updated);
    let age = (raw_age - suspended).max(chrono::Duration::zero());
    if age.num_seconds() <= i64::try_from(threshold).unwrap_or(i64::MAX) {
        return None;
    }

    Some(StuckTaskTiming {
        has_session,
        age,
        threshold,
    })
}

fn session_alive(
    tmux: &TmuxManager,
    repo: &str,
    session_task_id: &str,
    session_map: &HashMap<String, bool>,
) -> bool {
    session_map
        .get(&tmux.session_name(repo, session_task_id))
        .copied()
        .unwrap_or(false)
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

/// Start time and config for the stuck check of an in_progress task. A live session ages from
/// the status entry, since other writes refresh `updated_at` mid-run, and is not stuck before
/// its own run timeout ends. A dead session keeps `updated_at`, which the runner touches when it
/// sees the session end
async fn in_progress_stuck_clock(
    store: &TaskStore,
    repo: &str,
    task_id: &str,
    updated_at: &str,
    has_session: bool,
    config: &EngineConfig,
    router: &OnceLock<RouterConfig>,
) -> (String, EngineConfig) {
    if !has_session {
        return (updated_at.to_string(), config.clone());
    }

    let task = match store.resolve_task_id(repo, task_id).await {
        Ok(Some(store_id)) => store.get(store_id).await.ok(),
        _ => None,
    };
    let Some(task) = task else {
        return (updated_at.to_string(), config.clone());
    };

    let entered_at = match store
        .status_entered_at(task.id, TaskStatus::InProgress)
        .await
    {
        Ok(Some(ts)) => ts,
        Ok(None) => updated_at.to_string(),
        Err(e) => {
            tracing::warn!(
                task_id,
                error = %e,
                "failed to read in_progress entry, falling back to updated_at"
            );
            updated_at.to_string()
        }
    };
    let tier = run_tier(tick_router(router, RouterConfig::from_config), &task);
    let task_config = EngineConfig {
        stuck_timeout: config.live_session_stuck_limit(Some(&tier)),
        ..config.clone()
    };
    (entered_at, task_config)
}

/// When the task last entered in_review, and whether a review decision landed since. Age from the
/// status entry: `review_open_prs` rewrites `updated_at` on every pass, so it never ages
async fn in_review_stuck_entry(store: &TaskStore, store_id: i64, fallback: &str) -> (String, bool) {
    match store.in_review_entry(store_id).await {
        Ok(Some(entry)) => entry,
        Ok(None) => (fallback.to_string(), false),
        Err(e) => {
            tracing::warn!(
                store_id,
                error = %e,
                "failed to read in_review entry, falling back to updated_at"
            );
            (fallback.to_string(), false)
        }
    }
}

/// Finalize any open `task_runs` rows for a task during recovery paths.
/// Best-effort: logs a warning on failure but does not block recovery.
async fn finalize_open_runs_for_recovery(store: &TaskStore, store_id: i64, reason: &str) {
    if let Err(e) = store
        .finalize_incomplete_runs(store_id, "aborted", reason)
        .await
    {
        tracing::warn!(
            store_id,
            error = %e,
            reason,
            "failed to finalize incomplete runs during recovery"
        );
    }
}

/// Phase 1b of tick: detect agents that have produced no output since session start
/// and have exceeded the silence grace period.
///
/// Only triggers on complete silence (agent never produced any output). Agents that
/// produced output then went quiet (e.g. long tool calls) are NOT affected.
///
/// On silence: kills the tmux session, applies model-level cooldown, resets the task
/// to `New` so the router picks a different agent/model.
pub(crate) async fn tick_detect_silent_agents(
    tmux: &Arc<TmuxManager>,
    repo: &str,
    capture: &Arc<CaptureService>,
    backend: &Arc<dyn ExternalBackend>,
    task_manager: &Arc<TaskManager>,
    config: &EngineConfig,
    store: &Arc<TaskStore>,
) -> anyhow::Result<()> {
    let _span = tracing::info_span!("engine.tick.phase1b.silence").entered();
    let grace = Duration::from_secs(config.silence_grace_period);
    let silent_sessions = capture.get_silent_sessions_for_repo(repo, grace).await;

    for (task_id, session_name, silence_age_secs) in silent_sessions {
        let use_backend = should_use_backend(&task_id);
        // Look up agent + model from the store so we can cooldown the right model.
        let store_task = match store.resolve_task_id(repo, &task_id).await {
            Ok(Some(store_id)) => match store.get(store_id).await {
                Ok(task) => Some(task),
                Err(e) => {
                    tracing::warn!(
                        task_id,
                        repo,
                        store_id,
                        operation = "store.get",
                        error = %e,
                        "silence detection: db error fetching task — agent/model cooldown will be skipped"
                    );
                    None
                }
            },
            Ok(None) => {
                tracing::debug!(
                    task_id,
                    repo,
                    operation = "resolve_task_id",
                    "silence detection: task not found in store (may have been cleaned up)"
                );
                None
            }
            Err(e) => {
                tracing::warn!(
                    task_id,
                    repo,
                    operation = "resolve_task_id",
                    error = %e,
                    "silence detection: db error resolving task id — agent/model cooldown will be skipped"
                );
                None
            }
        };
        let agent_name = store_task
            .as_ref()
            .and_then(|t| t.agent.clone())
            .unwrap_or_default();
        let model_name = store_task
            .as_ref()
            .and_then(|t| t.model.clone())
            .unwrap_or_default();

        tracing::warn!(
            task_id,
            agent = %agent_name,
            model = %model_name,
            silence_age_secs,
            grace_secs = config.silence_grace_period,
            cooldown_secs = config.silence_cooldown,
            "agent silent since session start — killing session, cooling down model + agent, failing over"
        );

        // 1. Kill the tmux session
        if let Err(e) = tmux.kill_session(&session_name).await {
            tracing::debug!(
                task_id,
                ?e,
                "kill_session failed for silent agent (may already be gone)"
            );
        }

        // 2. Unregister from capture
        capture.unregister_session(repo, &task_id).await;

        // Finalize any open audit run so the task_runs row is not orphaned
        if let Some(store_id) = store_task.as_ref().map(|t| t.id) {
            finalize_open_runs_for_recovery(store, store_id, "silence detection: session killed")
                .await;
        }

        let mut extended_note = String::new();

        // 3. Cooldown the specific model (not the whole agent)
        if !agent_name.is_empty() && !model_name.is_empty() {
            set_model_cooldown(
                &agent_name,
                &model_name,
                config.silence_cooldown,
                "silence_detected",
            )
            .await;
            if let Some(result) = record_silence_detection(&agent_name, &model_name).await
                && result.extended_cooldown_applied
            {
                extended_note = format!(
                    " ({} silences in 24h -> extended cooldown {}s)",
                    result.count, SILENCE_EXTENDED_COOLDOWN_SECS
                );
            }
        }

        // 3b. Short agent-level cooldown to force router to pick a different agent.
        // Without this, the router picks the same agent with a different model,
        // looping through all models (~2 min each) before the long agent cooldown kicks in.
        if !agent_name.is_empty() {
            set_agent_cooldown(&agent_name, SILENCE_AGENT_COOLDOWN_SECS).await;
        }

        // 4. Pick a fallback agent and set to Routed (not New) to preserve progress.
        // Setting to New would clear routing state and trigger a full LLM re-routing
        // cycle, losing intermediate context. Routed skips re-routing and re-dispatches
        // directly with the chosen fallback agent.
        let task_eid = ExternalId(task_id.clone());

        // Build available agents list and reroute chain for failover
        let available: Vec<String> = engine::configured_agents()
            .into_iter()
            .filter(|a| cmd_cache::command_exists(a))
            .collect();
        let chain = response::get_reroute_chain(&task_id, &Some(Arc::clone(store)), repo).await;
        let chain = response::update_reroute_chain(
            &task_id,
            &agent_name,
            &chain,
            &Some(Arc::clone(store)),
            repo,
        )
        .await;

        let (next_status, next_agent) = if let Some(fallback) =
            response::pick_fallback_agent(&agent_name, &chain, &available)
        {
            tracing::info!(
                task_id,
                from = %agent_name,
                to = %fallback,
                "silence detection: failover to different agent, setting routed"
            );
            (Status::Routed, Some(fallback))
        } else {
            tracing::warn!(
                task_id,
                "silence detection: no fallback agents available, resetting to new for re-routing"
            );
            (Status::New, None)
        };

        if use_backend && let Some(ref st) = store_task {
            // Fire-and-forget: label removals are cosmetic — tick should not block on them.
            let stale_labels: Vec<String> = st
                .labels
                .iter()
                .filter(|l| {
                    l.starts_with("agent:")
                        || l.starts_with("complexity:")
                        || l.starts_with("model:")
                })
                .cloned()
                .collect();
            if !stale_labels.is_empty() {
                let backend_clone = Arc::clone(backend);
                let task_eid_clone = task_eid.clone();
                tokio::spawn(async move {
                    for label in &stale_labels {
                        if let Err(e) = backend_clone.remove_label(&task_eid_clone, label).await {
                            tracing::warn!(task_id = task_eid_clone.0, label, error = %e, "failed to remove label during silence detection re-route");
                        }
                    }
                });
            }
        }

        if let Some(store_id) = store_task.as_ref().map(|t| t.id) {
            if let Some(ref fallback) = next_agent {
                // Write the fallback agent into the store so dispatch can proceed
                // directly without an LLM re-routing cycle. Clearing model forces
                // model_for_complexity to pick the best available model for the new agent.
                let _ = store_set_by_id(
                    Some(store),
                    store_id,
                    &[
                        ("agent", serde_json::json!(fallback)),
                        ("model", serde_json::json!("")),
                        (
                            "last_error",
                            serde_json::json!(format!(
                                "silence detected after {}s, failing over to {}",
                                config.silence_grace_period, fallback
                            )),
                        ),
                    ],
                )
                .await;
            } else {
                let _ = store_set_by_id(
                    Some(store),
                    store_id,
                    &[
                        ("agent", serde_json::Value::Null),
                        ("model", serde_json::Value::Null),
                        (
                            "last_error",
                            serde_json::json!(format!(
                                "silence detected after {}s, no fallback agents available",
                                config.silence_grace_period
                            )),
                        ),
                    ],
                )
                .await;
            }
        }

        if let Err(e) = task_manager
            .update_task_status(&task_eid, next_status)
            .await
        {
            tracing::warn!(task_id, ?e, "failed to update silent task status");
            continue;
        }

        // Log reroute activity
        let details = serde_json::json!({
            "failure_reason": "agent_silence",
            "silence_duration_secs": config.silence_grace_period,
            "cooldown_applied": !agent_name.is_empty() && !model_name.is_empty(),
            "fallback_available": next_agent.is_some(),
        });
        store::store_log_activity(
            &Some(Arc::clone(store)),
            repo,
            &task_id,
            "rerouted",
            Some("in_progress"),
            Some(next_status.as_str()),
            next_agent.as_deref(),
            None,
            Some(&details),
        )
        .await;

        // 5. Post a comment explaining what happened
        let action = if let Some(ref fallback) = next_agent {
            format!("failing over to {fallback}")
        } else {
            "marking needs_review (no fallback agents)".to_string()
        };
        let comment = format!(
            "[{}] agent silent for {}s since session start — killed session, cooled down model `{}:{}` for {}s{}, agent `{}` for {}s, {}{}",
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
            config.silence_grace_period,
            agent_name,
            model_name,
            config.silence_cooldown,
            extended_note,
            agent_name,
            SILENCE_AGENT_COOLDOWN_SECS,
            action,
            engine::orch_footer(),
        );
        if use_backend {
            // Fire-and-forget: comment is informational — tick should not block on it.
            let backend_clone = Arc::clone(backend);
            let task_eid_clone = task_eid.clone();
            tokio::spawn(async move {
                if let Err(e) = backend_clone.post_comment(&task_eid_clone, &comment).await {
                    tracing::warn!(
                        task_id = task_eid_clone.0,
                        ?e,
                        "failed to post silence detection comment"
                    );
                }
            });
        }
    }

    Ok(())
}

fn should_use_backend(task_id: &str) -> bool {
    !is_internal_id(task_id)
}

/// Phase 2 of tick: recover tasks stranded by missing or stale tmux state.
/// - `in_progress` tasks → reset to `New` when their session is missing or the run timed out
/// - `routed` tasks      → reset to `New` when an old live session keeps blocking re-dispatch
/// - `in_review` tasks   → reset to `NeedsReview` when the review session disappeared
#[expect(
    clippy::too_many_arguments,
    reason = "each argument is engine state the tick passes down"
)]
pub(crate) async fn tick_recover_stuck_tasks(
    backend: &Arc<dyn ExternalBackend>,
    tmux: &Arc<TmuxManager>,
    repo: &str,
    task_manager: &Arc<TaskManager>,
    config: &EngineConfig,
    store: &Arc<TaskStore>,
    session_map: &HashMap<String, bool>,
    dispatching: &Arc<DashMap<String, String>>,
) -> anyhow::Result<()> {
    let _span = tracing::info_span!("engine.tick.phase2.stuck_tasks").entered();
    let tick_router_cell = OnceLock::new();

    let in_progress = match task_manager
        .list_external_by_status(Status::InProgress)
        .await
    {
        Ok(tasks) => tasks,
        Err(e) => {
            tracing::warn!(?e, "failed to list in_progress tasks for stuck recovery");
            vec![]
        }
    };
    for task in &in_progress {
        let (since, task_config) = in_progress_stuck_clock(
            store,
            repo,
            &task.id.0,
            &task.updated_at,
            session_alive(tmux, repo, &task.id.0, session_map),
            config,
            &tick_router_cell,
        )
        .await;
        let Some(timing) = stuck_task_timing_from_map(
            tmux,
            repo,
            &task.id.0,
            &since,
            &task_config,
            "cannot parse updated_at, skipping stuck-task check",
            session_map,
        ) else {
            continue;
        };

        // A dead tmux session with no `updated_at` refresh in a long time normally means
        // the runner vanished (crashed, orch restarted). But it also matches the tail end
        // of a *successful* run: the session exits the instant the agent finishes, while
        // the runner is still polling for that exit (up to its own poll interval) and then
        // writing results. If the runner's dispatch guard for this task is still held, the
        // runner is alive and actively processing — let it finish rather than racing it.
        if !timing.has_session {
            let dispatch_key = format!("{repo}/{}", task.id.0);
            if dispatching.contains_key(&dispatch_key) {
                // Promoted from debug! to info!: the default log filter is "orch=info"
                // (see main.rs), so this line — the only direct evidence that the guard
                // check actually found and honored a held dispatch guard — was silently
                // dropped in production. Every prior occurrence of the reclaim race
                // (#3518/#3519/#3523/#3525/#3526) had to be diagnosed from timestamp
                // correlation alone because this and the "dispatch guard
                // claimed"/"released" lines never appeared in orch.log.
                tracing::info!(
                    task_id = task.id.0,
                    dispatch_key,
                    age_mins = timing.age.num_minutes(),
                    "skipping stuck-task reclaim: runner still actively processing this task"
                );
                continue;
            }
        }

        if timing.has_session {
            tracing::warn!(
                task_id = task.id.0,
                age_mins = timing.age.num_minutes(),
                threshold_mins = timing.threshold / 60,
                "recovering stuck task: timed out with active session → new"
            );
            // The agent would keep working under a task that is routed again, and its session
            // name would block the next dispatch
            let session_name = tmux.session_name(repo, &task.id.0);
            if let Err(e) = tmux.kill_session(&session_name).await {
                tracing::warn!(
                    task_id = task.id.0,
                    session = %session_name,
                    error = %e,
                    "failed to kill stuck tmux session, will retry next tick"
                );
                continue;
            }
        } else {
            tracing::warn!(
                task_id = task.id.0,
                dispatch_key = format!("{repo}/{}", task.id.0),
                age_mins = timing.age.num_minutes(),
                threshold_mins = timing.threshold / 60,
                "recovering stuck task: no session found — reclaiming early → new"
            );
        }
        // For stuck tasks with an active session, record agent/model failure + cooldown so the
        // router picks a different agent/model on the next attempt. Without this, the router
        // sees the agent as healthy and selects it again, causing the hang to repeat.
        // (Mirrors the pattern in tick_detect_silent_agents lines 219–284.)
        let mut cached_store_id: Option<i64> = None;
        if timing.has_session {
            let store_task = match store.resolve_task_id(repo, &task.id.0).await {
                Ok(Some(store_id)) => {
                    cached_store_id = Some(store_id);
                    match store.get(store_id).await {
                        Ok(t) => Some(t),
                        Err(e) => {
                            tracing::warn!(task_id = task.id.0, error = %e, "failed to fetch task from store for stuck-task cooldown");
                            None
                        }
                    }
                }
                Ok(None) => None,
                Err(e) => {
                    tracing::warn!(task_id = task.id.0, error = %e, "failed to resolve task id for stuck-task cooldown");
                    None
                }
            };
            let agent_name = store_task
                .as_ref()
                .and_then(|t| t.agent.clone())
                .unwrap_or_default();
            let model_name = store_task
                .as_ref()
                .and_then(|t| t.model.clone())
                .unwrap_or_default();

            if !agent_name.is_empty() && !model_name.is_empty() {
                set_model_cooldown(
                    &agent_name,
                    &model_name,
                    config.silence_cooldown,
                    "silence_detected",
                )
                .await;
                record_agent_failure_with_message(
                    &agent_name,
                    &format!(
                        "stuck with active session after {}m",
                        timing.age.num_minutes()
                    ),
                )
                .await;
            }
            if !agent_name.is_empty() {
                set_agent_cooldown(&agent_name, SILENCE_AGENT_COOLDOWN_SECS).await;
            }
            tracing::warn!(
                task_id = task.id.0,
                agent = %agent_name,
                model = %model_name,
                cooldown_secs = config.silence_cooldown,
                "stuck-task cooldown applied — router will avoid this agent/model on next attempt"
            );
        }

        // Remove stale agent/model labels so the LLM router re-routes properly.
        // Fire-and-forget: cosmetic label operation, tick should not block on it.
        {
            let stale_labels: Vec<String> = task
                .labels
                .iter()
                .filter(|l| l.starts_with("agent:") || l.starts_with("model:"))
                .cloned()
                .collect();
            if !stale_labels.is_empty() {
                let backend_clone = Arc::clone(backend);
                let task_id_clone = task.id.clone();
                tokio::spawn(async move {
                    for label in &stale_labels {
                        if let Err(e) = backend_clone.remove_label(&task_id_clone, label).await {
                            tracing::warn!(task_id = task_id_clone.0, label, error = %e, "failed to remove stale routing label during stuck-task recovery");
                        }
                    }
                });
            }
        }
        let resolved_store_id = match cached_store_id {
            Some(id) => Some(id),
            None => match store.resolve_task_id(repo, &task.id.0).await {
                Ok(Some(id)) => Some(id),
                Ok(None) => {
                    tracing::warn!(
                        task_id = %task.id.0,
                        repo,
                        "resolve_task_id returned None during stuck-task recovery"
                    );
                    None
                }
                Err(e) => {
                    tracing::error!(
                        task_id = %task.id.0,
                        repo,
                        error = %e,
                        "resolve_task_id failed during stuck-task recovery"
                    );
                    None
                }
            },
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
                    "failed to clear routing fields for stuck task — will retry next tick"
                );
                continue;
            }
            finalize_open_runs_for_recovery(
                store,
                store_id,
                if timing.has_session {
                    "stuck-task recovery: active session killed"
                } else {
                    "stuck-task recovery: no session found"
                },
            )
            .await;
        }
        if let Err(e) = task_manager.update_task_status(&task.id, Status::New).await {
            tracing::warn!(task_id = task.id.0, ?e, "failed to reset stuck task status");
            continue;
        }

        // Log reroute activity for stuck task recovery
        let details = serde_json::json!({
            "failure_reason": if timing.has_session { "stuck_with_session" } else { "stuck_no_session" },
            "age_minutes": timing.age.num_minutes(),
            "had_session": timing.has_session,
        });
        store::store_log_activity(
            &Some(Arc::clone(store)),
            repo,
            &task.id.0,
            "rerouted",
            Some("in_progress"),
            Some("new"),
            None::<&str>,
            None::<&str>,
            Some(&details),
        )
        .await;

        let reason = if timing.has_session {
            format!(
                "timed out after {}m with active session (cleared agent for re-routing)",
                timing.age.num_minutes()
            )
        } else {
            format!(
                "no session found — reclaiming early after {}m (cleared agent for re-routing)",
                timing.age.num_minutes()
            )
        };
        // Fire-and-forget: comment is informational — tick should not block on it.
        {
            let backend_clone = Arc::clone(backend);
            let task_id_clone = task.id.clone();
            let body = format!(
                "[{}] recovered: stuck in_progress — {}{}",
                chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
                reason,
                engine::orch_footer()
            );
            tokio::spawn(async move {
                if let Err(e) = backend_clone.post_comment(&task_id_clone, &body).await {
                    tracing::warn!(
                        task_id = task_id_clone.0,
                        ?e,
                        "failed to post stuck-task recovery comment"
                    );
                }
            });
        }
    }

    let mut routed = match task_manager.list_external_by_status(Status::Routed).await {
        Ok(tasks) => tasks,
        Err(e) => {
            tracing::warn!(?e, "failed to list routed tasks for stuck recovery");
            vec![]
        }
    };
    match task_manager
        .list_internal_by_status(TaskStatus::Routed)
        .await
    {
        Ok(tasks) => routed.extend(tasks),
        Err(e) => tracing::warn!(
            ?e,
            "failed to list internal routed tasks for stuck recovery"
        ),
    }
    for task in &routed {
        let Some(timing) = stuck_task_timing_from_map(
            tmux,
            repo,
            &task.id.0,
            &task.updated_at,
            config,
            "cannot parse updated_at, skipping routed stuck-task check",
            session_map,
        ) else {
            continue;
        };

        if !timing.has_session {
            continue;
        }

        let session_name = tmux.session_name(repo, &task.id.0);
        if let Err(e) = tmux.kill_session(&session_name).await {
            tracing::warn!(
                task_id = task.id.0,
                session = %session_name,
                error = %e,
                "failed to kill stale tmux session blocking routed task"
            );
            continue;
        }

        recover_routed_blocked_dispatch(backend, store, task_manager, repo, task, &timing).await;
    }

    // Recover internal (SQLite) tasks stuck in in_progress.
    // These have no GitHub labels or comments — just reset the DB status to New.
    let internal_in_progress = match task_manager
        .list_internal_by_status(DbStatus::InProgress)
        .await
    {
        Ok(tasks) => tasks,
        Err(e) => {
            tracing::warn!(
                ?e,
                "failed to list internal in_progress tasks for stuck recovery"
            );
            vec![]
        }
    };
    for task in &internal_in_progress {
        let task_id = task.id.0.clone();
        let (since, task_config) = in_progress_stuck_clock(
            store,
            repo,
            &task_id,
            &task.updated_at,
            session_alive(tmux, repo, &task_id, session_map),
            config,
            &tick_router_cell,
        )
        .await;
        let Some(timing) = stuck_task_timing_from_map(
            tmux,
            repo,
            &task_id,
            &since,
            &task_config,
            "cannot parse updated_at, skipping stuck internal-task check",
            session_map,
        ) else {
            continue;
        };

        // See the comment on the external in-progress loop above: a dead session with a
        // stale `updated_at` also matches the tail end of a run that just finished
        // successfully. Skip the reclaim while the runner's dispatch guard for this task
        // is still held — this is the exact race that discards completed daily/scheduled
        // internal tasks whose total runtime exceeds `no_session_stuck_timeout`.
        if !timing.has_session {
            let dispatch_key = format!("{repo}/{task_id}");
            if dispatching.contains_key(&dispatch_key) {
                // Promoted from debug! to info! (issue #3526): the default log filter is
                // "orch=info" (main.rs), so this line — along with the DispatchGuard
                // claim/release lines in dispatch_guard.rs and dispatch_tasks_for_repo —
                // never reached orch.log in production. Every occurrence of the reclaim
                // race (#3518, #3519, #3523, #3525, #3526) had to be reasoned about from
                // timestamp correlation across unrelated log lines instead of directly
                // observing whether the guard was held, because the one log line that
                // would prove it was silently filtered out. Promoting these closes that
                // observability gap so the next occurrence carries conclusive evidence.
                tracing::info!(
                    task_id,
                    dispatch_key,
                    age_mins = timing.age.num_minutes(),
                    "skipping stuck-task reclaim: runner still actively processing this task"
                );
                continue;
            }
        }

        if timing.has_session {
            tracing::warn!(
                task_id,
                age_mins = timing.age.num_minutes(),
                threshold_mins = timing.threshold / 60,
                "recovering stuck task: timed out with active session → new"
            );
            let session_name = tmux.session_name(repo, &task_id);
            if let Err(e) = tmux.kill_session(&session_name).await {
                tracing::warn!(
                    task_id,
                    session = %session_name,
                    error = %e,
                    "failed to kill stuck tmux session, will retry next tick"
                );
                continue;
            }
        } else {
            tracing::warn!(
                task_id,
                dispatch_key = format!("{repo}/{task_id}"),
                age_mins = timing.age.num_minutes(),
                threshold_mins = timing.threshold / 60,
                "recovering stuck task: no session found — reclaiming early → new"
            );
        }
        // Apply agent/model cooldown for internal stuck tasks with active sessions,
        // mirroring the external path (lines 501-546). Without this, the router picks
        // the same agent/model that caused the hang, creating an infinite loop.
        let mut cached_store_id: Option<i64> = None;
        if timing.has_session {
            let store_task = match store.resolve_task_id(repo, &task_id).await {
                Ok(Some(store_id)) => {
                    cached_store_id = Some(store_id);
                    match store.get(store_id).await {
                        Ok(t) => Some(t),
                        Err(e) => {
                            tracing::warn!(task_id, error = %e, "failed to fetch task from store for stuck internal-task cooldown");
                            None
                        }
                    }
                }
                Ok(None) => None,
                Err(e) => {
                    tracing::warn!(task_id, error = %e, "failed to resolve task id for stuck internal-task cooldown");
                    None
                }
            };
            let agent_name = store_task
                .as_ref()
                .and_then(|t| t.agent.clone())
                .unwrap_or_default();
            let model_name = store_task
                .as_ref()
                .and_then(|t| t.model.clone())
                .unwrap_or_default();

            if !agent_name.is_empty() && !model_name.is_empty() {
                set_model_cooldown(
                    &agent_name,
                    &model_name,
                    config.silence_cooldown,
                    "silence_detected",
                )
                .await;
                record_agent_failure_with_message(
                    &agent_name,
                    &format!(
                        "internal task stuck with active session after {}m",
                        timing.age.num_minutes()
                    ),
                )
                .await;
            }
            if !agent_name.is_empty() {
                set_agent_cooldown(&agent_name, SILENCE_AGENT_COOLDOWN_SECS).await;
            }
            tracing::warn!(
                task_id,
                agent = %agent_name,
                model = %model_name,
                cooldown_secs = config.silence_cooldown,
                "internal stuck-task cooldown applied — router will avoid this agent/model on next attempt"
            );
        }
        // Reset routing state so the LLM router is used on the next attempt
        // (same reset that external tasks perform).
        let resolved_store_id = match cached_store_id {
            Some(id) => Some(id),
            None => match store.resolve_task_id(repo, &task_id).await {
                Ok(Some(id)) => Some(id),
                Ok(None) => {
                    tracing::warn!(
                        task_id = %task_id,
                        repo,
                        "resolve_task_id returned None during stuck internal-task recovery"
                    );
                    None
                }
                Err(e) => {
                    tracing::error!(
                        task_id = %task_id,
                        repo,
                        error = %e,
                        "resolve_task_id failed during stuck internal-task recovery"
                    );
                    None
                }
            },
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
                    task_id,
                    ?e,
                    "failed to clear routing fields for stuck internal task — will retry next tick"
                );
                continue;
            }
            finalize_open_runs_for_recovery(
                store,
                store_id,
                if timing.has_session {
                    "stuck-task recovery: internal active session killed"
                } else {
                    "stuck-task recovery: internal no session found"
                },
            )
            .await;
        }
        if let Err(e) = task_manager
            .update_task_status(&ExternalId(task_id.clone()), Status::New)
            .await
        {
            tracing::warn!(task_id, ?e, "failed to reset stuck internal task status");
        }
    }

    // Recover tasks stuck in in_review, external and internal in one sweep. Reset to NeedsReview
    // (not New) so the review agent re-triggers without clearing the routing state.
    //
    // This is the only periodic owner of the case where the review session is gone. The sync
    // sweep used to reset the same tasks after one minute, which beat a finished review's result
    // to the engine and dropped it as stale, and left this longer limit unreachable
    let in_review = match store.list_by_status(repo, TaskStatus::InReview).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(?e, "failed to list in_review tasks for stuck recovery");
            vec![]
        }
    };
    // A review agent ends its tmux session before its result reaches the engine, so the
    // no-session limit for a review has to be longer than for a task agent. The floor in
    // `EngineConfig::from_config` keeps it above the review agent's own timeout
    let in_review_config = engine::EngineConfig {
        no_session_stuck_timeout: config.in_review_no_session_stuck_timeout,
        ..config.clone()
    };
    for task in &in_review {
        let Some(task_id) = task.external_id.clone() else {
            tracing::warn!(
                store_id = task.id,
                "in_review task has no external id, skipping stuck recovery"
            );
            continue;
        };

        // The dispatch flow owns a task while it holds the guard
        if dispatching.contains_key(&format!("{repo}/{task_id}")) {
            tracing::debug!(
                task_id,
                "task locked by dispatch flow, skipping stuck check"
            );
            continue;
        }

        // A recorded decision means the review finished and the task waits on the merge, not on
        // a review agent
        let (entered_at, decided) = in_review_stuck_entry(store, task.id, &task.updated_at).await;
        if decided {
            tracing::debug!(
                task_id,
                "in_review task already has a review decision, waiting on merge"
            );
            continue;
        }

        let review_task_id = format!("{task_id}-review");
        let Some(timing) = stuck_task_timing_from_map(
            tmux,
            repo,
            &review_task_id,
            &entered_at,
            &in_review_config,
            "cannot parse in_review timestamp, skipping stuck in_review check",
            session_map,
        ) else {
            continue;
        };

        tracing::warn!(
            task_id,
            age_mins = timing.age.num_minutes(),
            threshold_mins = timing.threshold / 60,
            has_session = timing.has_session,
            "recovering stuck in_review task — resetting to needs_review"
        );

        match task_manager
            .update_task_status_if(
                &ExternalId(task_id.clone()),
                Status::NeedsReview,
                Status::InReview,
            )
            .await
        {
            Err(e) => {
                tracing::warn!(task_id, ?e, "failed to reset stuck in_review task status");
                continue;
            }
            Ok(false) => {
                tracing::debug!(
                    task_id,
                    "stuck in_review reset skipped — task already transitioned"
                );
                continue;
            }
            Ok(true) => {}
        }

        // A dead or hung session is an infrastructure event, a tmux crash or a restart, not an
        // agent parse failure. Keeping the counter would spend the next cycle's quota
        if let Err(e) = store
            .set_fields(task.id, &[("review_agent_failures", serde_json::json!(0))])
            .await
        {
            tracing::warn!(task_id, ?e, "failed to reset review_agent_failures");
        }

        finalize_open_runs_for_recovery(
            store,
            task.id,
            if timing.has_session {
                "stuck-task recovery: in_review session killed"
            } else {
                "stuck-task recovery: in_review no session found"
            },
        )
        .await;

        let details = serde_json::json!({
            "failure_reason": "stuck_in_review",
            "age_minutes": timing.age.num_minutes(),
            "had_session": timing.has_session,
        });
        store::store_log_activity(
            &Some(Arc::clone(store)),
            repo,
            &task_id,
            "rerouted",
            Some("in_review"),
            Some("needs_review"),
            None::<&str>,
            None::<&str>,
            Some(&details),
        )
        .await;

        // An internal task has no issue to comment on
        if task.origin == "internal" {
            continue;
        }

        if let Err(e) = backend
            .post_comment(
                &ExternalId(task_id.clone()),
                &format!(
                    "[{}] recovered: stuck in_review — {} after {}m, resetting to needs_review{}",
                    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
                    if timing.has_session {
                        "review session hung"
                    } else {
                        "no session found"
                    },
                    timing.age.num_minutes(),
                    engine::orch_footer()
                ),
            )
            .await
        {
            tracing::warn!(
                task_id,
                ?e,
                "failed to post stuck in_review recovery comment"
            );
        }
    }

    Ok(())
}

/// External tasks stuck in_progress in a repo no project ticks, such as a project removed from
/// the config. Store-wide, so `tick_all` runs it once per tick rather than once per project
pub(crate) async fn recover_untracked_repo_stuck_tasks(
    tmux: &Arc<TmuxManager>,
    config: &EngineConfig,
    store: &Arc<TaskStore>,
    session_map: &HashMap<String, bool>,
    dispatching: &Arc<DashMap<String, String>>,
    active_repos: &HashSet<String>,
) {
    let all_external_in_progress = match store
        .list_all_by_status_global(TaskStatus::InProgress)
        .await
    {
        Ok(tasks) => tasks,
        Err(e) => {
            tracing::warn!(
                ?e,
                "failed to list global in_progress tasks for cross-repo stuck recovery"
            );
            vec![]
        }
    };
    for task in all_external_in_progress
        .iter()
        .filter(|t| t.origin != "internal" && !active_repos.contains(&t.repo))
    {
        let external_id = task
            .external_id
            .clone()
            .unwrap_or_else(|| task.id.to_string());
        let Some(timing) = stuck_task_timing_from_map(
            tmux,
            &task.repo,
            &external_id,
            &task.updated_at,
            config,
            "cannot parse updated_at, skipping cross-repo stuck-task check",
            session_map,
        ) else {
            continue;
        };

        // See the comment on the in-progress loop in `tick_recover_stuck_tasks`: a dead session with a stale
        // `updated_at` also matches the tail end of a run that just finished successfully.
        // Skip the reclaim while the runner's dispatch guard for this task is still held.
        if !timing.has_session {
            let dispatch_key = format!("{}/{external_id}", task.repo);
            if dispatching.contains_key(&dispatch_key) {
                // See the same promotion note on the internal-task loop in `tick_recover_stuck_tasks`: this was
                // debug! (silently dropped under the default "orch=info" filter), leaving
                // every prior reclaim-race occurrence undiagnosable from logs alone.
                tracing::info!(
                    task_id = task.id,
                    repo = %task.repo,
                    dispatch_key,
                    age_mins = timing.age.num_minutes(),
                    "skipping cross-repo stuck-task reclaim: runner still actively processing this task"
                );
                continue;
            }
        }

        if timing.has_session {
            tracing::warn!(
                task_id = task.id,
                repo = %task.repo,
                age_mins = timing.age.num_minutes(),
                threshold_mins = timing.threshold / 60,
                "recovering cross-repo stuck task: timed out with active session → new"
            );
        } else {
            tracing::warn!(
                task_id = task.id,
                repo = %task.repo,
                dispatch_key = format!("{}/{external_id}", task.repo),
                age_mins = timing.age.num_minutes(),
                threshold_mins = timing.threshold / 60,
                "recovering cross-repo stuck task: no session found — reclaiming early → new"
            );
        }

        if let Err(e) = store_set_by_id(
            Some(store),
            task.id,
            &[
                ("agent", serde_json::Value::Null),
                ("model", serde_json::Value::Null),
                ("route_attempts", serde_json::json!(0)),
            ],
        )
        .await
        {
            tracing::warn!(
                task_id = task.id,
                repo = %task.repo,
                ?e,
                "failed to clear routing fields for cross-repo stuck task — will retry next tick"
            );
            continue;
        }

        finalize_open_runs_for_recovery(
            store,
            task.id,
            if timing.has_session {
                "stuck-task recovery: cross-repo active session killed"
            } else {
                "stuck-task recovery: cross-repo no session found"
            },
        )
        .await;

        if let Err(e) = store.update_status(task.id, TaskStatus::New).await {
            tracing::warn!(
                task_id = task.id,
                repo = %task.repo,
                ?e,
                "failed to reset cross-repo stuck task status"
            );
            continue;
        }

        let details = serde_json::json!({
            "failure_reason": if timing.has_session { "stuck_with_session" } else { "stuck_no_session" },
            "age_minutes": timing.age.num_minutes(),
            "had_session": timing.has_session,
            "scope": "cross_repo_global_sweep",
        });
        store::store_log_activity(
            &Some(Arc::clone(store)),
            &task.repo,
            &external_id,
            "rerouted",
            Some("in_progress"),
            Some("new"),
            None::<&str>,
            None::<&str>,
            Some(&details),
        )
        .await;
    }
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
                match store.ensure_external_task(repo, task).await {
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
async fn block_task_without_project(
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

fn skips_agent(labels: &[String]) -> bool {
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
fn group_orphaned_routed_tasks(
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
