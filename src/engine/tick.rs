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
use crate::config;
use crate::engine::cooldown::{
    github_circuit_remaining_secs, is_github_circuit_open, record_agent_failure_with_message,
    record_silence_detection, set_agent_cooldown, set_model_cooldown, SILENCE_AGENT_COOLDOWN_SECS,
    SILENCE_EXTENDED_COOLDOWN_SECS,
};
use crate::engine::dispatch_guard::DispatchGuard;
use crate::engine::jobs;
use crate::engine::router::{get_route_result, AllAgentsCooledError, RouteResultError, Router};
use crate::engine::runner::{TaskRunner, WeightSignal};
use crate::engine::tasks::{is_internal_id, TaskManager};
use crate::repo_context::REPO_CONTEXT;
use crate::store::TaskStore;
use crate::tmux::TmuxManager;
use dashmap::DashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::{Instant, SystemTime};
use tokio::sync::{mpsc, Semaphore};

/// Unix timestamp when the engine process started.
/// Used to identify tmux sessions from previous runs.
static ENGINE_START_TIME: LazyLock<u64> = LazyLock::new(|| {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
});

/// Flag to ensure startup cleanup only runs once.
static STARTUP_CLEANUP_DONE: AtomicBool = AtomicBool::new(false);

/// Cleanup stale tmux sessions from previous engine runs.
/// Called once at startup to kill sessions created before this process started.
async fn startup_cleanup(tmux: &TmuxManager) {
    // Use compare_exchange to ensure this only runs once across all threads
    if STARTUP_CLEANUP_DONE
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    let start_time = *ENGINE_START_TIME;
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

use super::EngineConfig;
use crate::store::{set_review_session_expected, store_set_by_id, store_touch_updated_at};

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

    if let Some(store_id) = match store.resolve_task_id(repo, &task.id.0).await {
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
    } {
        if let Err(e) = store_set_by_id(
            &Some(store),
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
    }

    if let Err(e) = task_manager.update_task_status(&task.id, Status::New).await {
        tracing::warn!(
            task_id = task.id.0,
            ?e,
            "failed to reset routed stuck task status"
        );
        return;
    }

    crate::store::store_log_activity(
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
            crate::engine::orch_footer()
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
    // Derive the short project name from repo (owner/repo -> repo)
    let repo_name = repo.rsplit('/').next().unwrap_or(repo);
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

#[allow(clippy::too_many_arguments)]
fn stuck_task_timing_from_map(
    tmux: &Arc<TmuxManager>,
    repo: &str,
    session_task_id: &str,
    updated_at: &str,
    config: &EngineConfig,
    parse_error_message: &'static str,
    session_map: &std::collections::HashMap<String, bool>,
) -> Option<StuckTaskTiming> {
    let session_name = tmux.session_name(repo, session_task_id);
    // A session is "running" if it appears in the map with alive=true.
    let has_session = session_map.get(&session_name).copied().unwrap_or(false);
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

    let age = chrono::Utc::now() - updated;
    if age.num_seconds() <= threshold as i64 {
        return None;
    }

    Some(StuckTaskTiming {
        has_session,
        age,
        threshold,
    })
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
    let grace = std::time::Duration::from_secs(config.silence_grace_period);
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

        let mut extended_note = String::new();

        // 3. Cooldown the specific model (not the whole agent)
        if !agent_name.is_empty() && !model_name.is_empty() {
            set_model_cooldown(&agent_name, &model_name, config.silence_cooldown).await;
            if let Some(result) = record_silence_detection(&agent_name, &model_name).await {
                if result.extended_cooldown_applied {
                    extended_note = format!(
                        " ({} silences in 24h -> extended cooldown {}s)",
                        result.count, SILENCE_EXTENDED_COOLDOWN_SECS
                    );
                }
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
        let available: Vec<String> = crate::engine::configured_agents()
            .into_iter()
            .filter(|a| crate::cmd_cache::command_exists(a))
            .collect();
        let chain = crate::engine::runner::response::get_reroute_chain(
            &task_id,
            &Some(Arc::clone(store)),
            repo,
        )
        .await;
        let chain = crate::engine::runner::response::update_reroute_chain(
            &task_id,
            &agent_name,
            &chain,
            &Some(Arc::clone(store)),
            repo,
        )
        .await;

        let (next_status, next_agent) = if let Some(fallback) =
            crate::engine::runner::response::pick_fallback_agent(&agent_name, &chain, &available)
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

        if use_backend {
            if let Some(ref st) = store_task {
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
                            if let Err(e) = backend_clone.remove_label(&task_eid_clone, label).await
                            {
                                tracing::warn!(task_id = task_eid_clone.0, label, error = %e, "failed to remove label during silence detection re-route");
                            }
                        }
                    });
                }
            }
        }

        if let Some(store_id) = store_task.as_ref().map(|t| t.id) {
            if let Some(ref fallback) = next_agent {
                // Write the fallback agent into the store so dispatch can proceed
                // directly without an LLM re-routing cycle. Clearing model forces
                // model_for_complexity to pick the best available model for the new agent.
                let _ = store_set_by_id(
                    &Some(store),
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
                    &Some(store),
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
        crate::store::store_log_activity(
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
            crate::engine::orch_footer(),
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

/// Phase 2 of tick: detect tasks stuck in_progress or in_review without an active tmux session and reset them.
/// - `in_progress` tasks → reset to `New` (clears routing state so the LLM router re-routes)
/// - `in_review` tasks   → reset to `NeedsReview` (keeps routing state; review agent re-triggers)
pub(crate) async fn tick_recover_stuck_tasks(
    backend: &Arc<dyn ExternalBackend>,
    tmux: &Arc<TmuxManager>,
    repo: &str,
    task_manager: &Arc<TaskManager>,
    config: &EngineConfig,
    store: &Arc<TaskStore>,
    session_map: &std::collections::HashMap<String, bool>,
) -> anyhow::Result<()> {
    let _span = tracing::info_span!("engine.tick.phase2.stuck_tasks").entered();

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
        let Some(timing) = stuck_task_timing_from_map(
            tmux,
            repo,
            &task.id.0,
            &task.updated_at,
            config,
            "cannot parse updated_at, skipping stuck-task check",
            session_map,
        ) else {
            continue;
        };

        if timing.has_session {
            tracing::warn!(
                task_id = task.id.0,
                age_mins = timing.age.num_minutes(),
                threshold_mins = timing.threshold / 60,
                "recovering stuck task: timed out with active session → new"
            );
        } else {
            tracing::warn!(
                task_id = task.id.0,
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
                set_model_cooldown(&agent_name, &model_name, config.silence_cooldown).await;
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
                &Some(store),
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
        crate::store::store_log_activity(
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
                crate::engine::orch_footer()
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
        .list_internal_by_status(crate::store::TaskStatus::Routed)
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

    // Global recovery for external tasks from repos that are not currently being ticked.
    // These tasks would otherwise remain in_progress forever if their project was removed
    // from active config and never enters this tick loop again.
    let all_external_in_progress = match store
        .list_all_by_status_global(crate::store::TaskStatus::InProgress)
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
        .filter(|t| t.origin != "internal" && t.repo != repo)
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
                age_mins = timing.age.num_minutes(),
                threshold_mins = timing.threshold / 60,
                "recovering cross-repo stuck task: no session found — reclaiming early → new"
            );
        }

        if let Err(e) = store_set_by_id(
            &Some(store),
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

        if let Err(e) = store
            .update_status(task.id, crate::store::TaskStatus::New)
            .await
        {
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
        crate::store::store_log_activity(
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

    // Recover internal (SQLite) tasks stuck in in_progress.
    // These have no GitHub labels or comments — just reset the DB status to New.
    use crate::store::TaskStatus as DbStatus;
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
        let Some(timing) = stuck_task_timing_from_map(
            tmux,
            repo,
            &task_id,
            &task.updated_at,
            config,
            "cannot parse updated_at, skipping stuck internal-task check",
            session_map,
        ) else {
            continue;
        };

        if timing.has_session {
            tracing::warn!(
                task_id,
                age_mins = timing.age.num_minutes(),
                threshold_mins = timing.threshold / 60,
                "recovering stuck task: timed out with active session → new"
            );
        } else {
            tracing::warn!(
                task_id,
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
                set_model_cooldown(&agent_name, &model_name, config.silence_cooldown).await;
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
                &Some(store),
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
        }
        if let Err(e) = task_manager
            .update_task_status(&ExternalId(task_id.clone()), Status::New)
            .await
        {
            tracing::warn!(task_id, ?e, "failed to reset stuck internal task status");
        }
    }

    // Recover external tasks stuck in in_review.
    // Unlike in_progress recovery, we reset to NeedsReview (not New) so the review
    // agent re-triggers without clearing the routing state.
    let in_review = match task_manager.list_external_by_status(Status::InReview).await {
        Ok(tasks) => tasks,
        Err(e) => {
            tracing::warn!(?e, "failed to list in_review tasks for stuck recovery");
            vec![]
        }
    };
    // Read the stored in_review rows once to avoid per-task DB lookups for
    // `review_session_expected`. Build a set of external_ids that have the
    // flag set so the per-task loop can check it cheaply.
    let store_in_review = match store
        .list_by_status(repo, crate::store::TaskStatus::InReview)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                ?e,
                "failed to list store in_review tasks for review_session_expected prefetch"
            );
            vec![]
        }
    };
    let review_expected_set: std::collections::HashSet<String> = store_in_review
        .into_iter()
        .filter(|t| t.review_session_expected)
        .filter_map(|t| t.external_id)
        .collect();
    for task in &in_review {
        if !review_expected_set.contains(&task.id.0) {
            tracing::debug!(
                task_id = task.id.0,
                "in_review task is waiting on PR review, skipping stuck-session recovery"
            );
            continue;
        }

        let review_task_id = format!("{}-review", task.id.0);
        // Use in_review_no_session_stuck_timeout instead of the standard
        // no_session_stuck_timeout. Review agents exit their tmux session on
        // normal completion before delivering the result to the engine, so a
        // short no-session timeout causes a race: the stuck recovery fires,
        // resets the task to needs_review, and the completed result arriving
        // moments later is discarded as stale. A longer threshold gives the
        // result delivery enough time to complete.
        let in_review_config = crate::engine::EngineConfig {
            no_session_stuck_timeout: config.in_review_no_session_stuck_timeout,
            ..config.clone()
        };
        let Some(timing) = stuck_task_timing_from_map(
            tmux,
            repo,
            &review_task_id,
            &task.updated_at,
            &in_review_config,
            "cannot parse updated_at, skipping stuck in_review check",
            session_map,
        ) else {
            continue;
        };

        tracing::warn!(
            task_id = task.id.0,
            age_mins = timing.age.num_minutes(),
            threshold_mins = timing.threshold / 60,
            "recovering stuck in_review task: no session found — resetting to needs_review"
        );
        if let Err(e) = task_manager
            .update_task_status(&task.id, Status::NeedsReview)
            .await
        {
            tracing::warn!(
                task_id = task.id.0,
                ?e,
                "failed to reset stuck in_review task status"
            );
            continue;
        } else {
            set_review_session_expected(store, repo, &task.id.0, false).await;
        }

        // Log activity for stuck in_review recovery
        let details = serde_json::json!({
            "failure_reason": "stuck_in_review",
            "age_minutes": timing.age.num_minutes(),
            "had_session": timing.has_session,
        });
        crate::store::store_log_activity(
            &Some(Arc::clone(store)),
            repo,
            &task.id.0,
            "rerouted",
            Some("in_review"),
            Some("needs_review"),
            None::<&str>,
            None::<&str>,
            Some(&details),
        )
        .await;
        if let Err(e) = backend
            .post_comment(
                &task.id,
                &format!(
                    "[{}] recovered: stuck in_review — no session found after {}m, resetting to needs_review{}",
                    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
                    timing.age.num_minutes(),
                    crate::engine::orch_footer()
                ),
            )
            .await
        {
            tracing::warn!(
                task_id = task.id.0,
                ?e,
                "failed to post stuck in_review recovery comment"
            );
        }
    }

    // Recover internal (SQLite) tasks stuck in in_review.
    let internal_in_review = match task_manager
        .list_internal_by_status(DbStatus::InReview)
        .await
    {
        Ok(tasks) => tasks,
        Err(e) => {
            tracing::warn!(
                ?e,
                "failed to list internal in_review tasks for stuck recovery"
            );
            vec![]
        }
    };
    for task in &internal_in_review {
        let task_id = task.id.0.clone();
        if !review_expected_set.contains(&task_id) {
            tracing::debug!(
                task_id,
                "internal in_review task is waiting on PR review, skipping stuck-session recovery"
            );
            continue;
        }

        let review_task_id = format!("{}-review", task_id);
        let Some(timing) = stuck_task_timing_from_map(
            tmux,
            repo,
            &review_task_id,
            &task.updated_at,
            config,
            "cannot parse updated_at, skipping stuck internal in_review check",
            session_map,
        ) else {
            continue;
        };

        tracing::warn!(
            task_id,
            age_mins = timing.age.num_minutes(),
            threshold_mins = timing.threshold / 60,
            "recovering stuck internal in_review task: no session found — resetting to needs_review"
        );
        if let Err(e) = task_manager
            .update_task_status(&ExternalId(task_id.clone()), Status::NeedsReview)
            .await
        {
            tracing::warn!(
                task_id,
                ?e,
                "failed to reset stuck internal in_review task status"
            );
        } else {
            set_review_session_expected(store, repo, &task_id, false).await;
        }
    }

    // Global sweep for CI-failure blocked tasks from inactive or removed projects.
    // auto_unblock_blocked_tasks runs inside sync_tick and is scoped to the active repo.
    // Tasks blocked for projects no longer in orch config are never processed there.
    // This sweep covers all repos so stale CI-failure blocks are eventually resolved.
    if let Err(e) =
        crate::engine::sync::auto_unblock_ci_failure_blocked_tasks_global(task_manager, store).await
    {
        tracing::warn!(err = %e, "global CI-failure blocked sweep failed");
    }

    Ok(())
}

/// Phase 3a of tick: route status:new tasks to an agent and transition them to status:routed.
pub(crate) async fn tick_route_tasks(
    backend: &Arc<dyn ExternalBackend>,
    task_manager: &Arc<TaskManager>,
    router: &mut Router,
    store: &Arc<TaskStore>,
    repo: &str,
) -> anyhow::Result<()> {
    let _span = tracing::info_span!("engine.tick.phase3a.route").entered();
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
        .filter(|t| !t.labels.iter().any(|l| l == "no-agent"))
        .collect();

    // Pre-emptive health check: refresh degraded-agent flags once per tick,
    // not per-task (previously this ran inside route() causing N redundant
    // DB queries when routing N tasks in the same tick).
    if !routable.is_empty() {
        let start = std::time::Instant::now();
        router.refresh_health(store).await;
        tracing::debug!(
            duration_ms = start.elapsed().as_millis(),
            "router.refresh_health completed"
        );
    }

    // Limit routing to at most N tasks per tick to prevent blocking on LLM calls
    let max_per_tick = crate::engine::router::config::max_tasks_per_routing_tick();
    for task in routable.into_iter().take(max_per_tick) {
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
                            if label.starts_with("agent:")
                                || label.starts_with("complexity:")
                                || label.starts_with("model:")
                            {
                                if let Err(e) =
                                    backend_clone.remove_label(&task_id_clone, label).await
                                {
                                    tracing::warn!(task_id = task_id_clone.0, label, error = %e, "failed to remove stale routing label during re-route");
                                }
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
                        let skills_json =
                            serde_json::to_string(&result.selected_skills).unwrap_or_default();
                        if let Err(e) = store
                            .store_route(&crate::store::StoreRoute {
                                id: store_id,
                                agent: &result.agent,
                                model: result.model.as_deref(),
                                complexity: &result.complexity,
                                estimate: result.estimate,
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
                            .update_status(store_id, crate::store::TaskStatus::Routed)
                            .await
                        {
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
                        "ALL AGENTS COOLED — no agent available to route this task. \
                         Task stays in 'new' and will be retried on every tick. \
                         Check `orch cooldown list` to see what is blocking dispatch."
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
pub(crate) async fn route_new_tasks_global(
    router: &mut Router,
    store: &Arc<TaskStore>,
    current_repo: &str,
) -> anyhow::Result<()> {
    use crate::store::{StoreRoute, TaskStatus};

    let all_new = store.list_all_by_status_global(TaskStatus::New).await?;
    let orphaned: Vec<&crate::store::Task> = all_new
        .iter()
        .filter(|t| t.repo != current_repo)
        .filter(|t| !t.labels.iter().any(|l| l == "no-agent"))
        .collect();

    if orphaned.is_empty() {
        return Ok(());
    }

    tracing::info!(
        count = orphaned.len(),
        "global routing sweep: found new tasks from inactive repos"
    );

    router.refresh_health(store).await;

    let max_per_tick = crate::engine::router::config::max_tasks_per_routing_tick();
    for task in orphaned.into_iter().take(max_per_tick) {
        let ext_task = crate::engine::tasks::store_task_to_external(task);
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
                let skills_json =
                    serde_json::to_string(&result.selected_skills).unwrap_or_default();
                if let Err(e) = store
                    .store_route(&StoreRoute {
                        id: task.id,
                        agent: &result.agent,
                        model: result.model.as_deref(),
                        complexity: &result.complexity,
                        estimate: result.estimate,
                        reason: &result.reason,
                        profile: &profile_json,
                        skills: &skills_json,
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
                if let Err(e) = store.update_status(task.id, TaskStatus::Routed).await {
                    tracing::warn!(
                        task_id = %ext_task.id.0,
                        repo = %task.repo,
                        err = %e,
                        "global sweep: failed to set status:routed"
                    );
                } else {
                    tracing::info!(
                        task_id = %ext_task.id.0,
                        repo = %task.repo,
                        agent = %result.agent,
                        complexity = %result.complexity,
                        "global sweep: task routed"
                    );
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
                        "global sweep: ALL AGENTS COOLED — task stays in 'new'"
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
    let threshold = crate::engine::router::config::min_healthy_agents_threshold();
    let healthy_agents = router.healthy_agent_count("simple");
    let is_degraded = router.is_degraded(threshold);
    DispatchMode {
        is_degraded,
        healthy_agents,
        threshold,
    }
}

/// Phase 3b of tick: spawn agents for all status:routed tasks up to the parallel limit.
#[allow(clippy::too_many_arguments)]
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
    session_map: &std::collections::HashMap<String, bool>,
) -> anyhow::Result<()> {
    // Note: Routed tasks should never have no-agent (filtered during Phase 3a routing),
    // but we keep this filter as defense-in-depth.
    let mut routed_tasks = task_manager.list_external_by_status(Status::Routed).await?;

    // Also include internal tasks in Routed status.
    use crate::store::TaskStatus as DbStatus;
    let internal_routed = task_manager
        .list_internal_by_status(DbStatus::Routed)
        .await?;
    routed_tasks.extend(internal_routed);

    // Filter and collect owned tasks to avoid lifetime issues
    let dispatchable: Vec<ExternalTask> = routed_tasks
        .into_iter()
        .filter(|t| !t.labels.iter().any(|l| l == "no-agent"))
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
#[allow(clippy::too_many_arguments)]
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
    session_map: &std::collections::HashMap<String, bool>,
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
        crate::engine::router::config::sequential_dispatch_delay_ms()
    } else {
        0
    };

    for (idx, task) in dispatchable.into_iter().enumerate() {
        // In degraded mode, add delay between dispatches to pace the system
        if idx > 0 && dispatch_mode.is_degraded {
            let delay_ms = sequential_delay;
            tracing::debug!(
                task_id = task.id.0,
                delay_ms = delay_ms,
                "sequential dispatch: waiting before dispatch"
            );
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
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
            use dashmap::mapref::entry::Entry;
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
                }
            }
        }
        // RAII guard — removes dispatch_key on drop even if the spawned task panics.
        let dispatch_guard = DispatchGuard::new(dispatching.clone(), dispatch_key.clone());

        // Check if already running (has active session)
        let session_name = tmux.session_name(repo, &task.id.0);
        if tmux
            .session_blocks_dispatch_from_map(&session_name, session_map)
            .await
        {
            tracing::warn!(
                task_id = task.id.0,
                session_name,
                "task has existing tmux session, skipping dispatch"
            );
            continue; // dispatch_guard drops here, removing the key
        }

        // Try to acquire a slot
        let permit = match semaphore.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                tracing::debug!("all parallel slots busy, skipping remaining tasks");
                break; // dispatch_guard drops here, removing the key
            }
        };

        // Mark in_progress BEFORE spawning to prevent double dispatch.
        let task_id = task.id.0.clone();
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
        let session_name = tmux.session_name(repo, &task_id);
        capture
            .register_session(repo, &task_id, &session_name)
            .await;

        // Dispatch task
        let runner = runner.clone();
        let backend = backend.clone();
        let tmux = tmux.clone();
        let capture = capture.clone();
        let task_id_for_cleanup = task_id.clone();
        let task_owned = task;
        let weight_tx = weight_tx.clone();
        let repo_owned = repo.to_string();
        let task_manager_for_spawn = task_manager.clone();
        let store_for_spawn = store.clone();

        // Load routing result from store (stored during Phase 3a)
        let route_result = match get_route_result(store, repo, &task_id).await {
            Ok(r) => Some(r),
            Err(RouteResultError::NoAgent { .. }) => {
                tracing::warn!(
                    task_id,
                    "routed task missing agent — resetting to new for re-routing (#1604)"
                );
                if let Err(e2) = task_manager
                    .update_task_status(&task_owned.id, Status::New)
                    .await
                {
                    tracing::error!(task_id, error = %e2, "failed to reset task to new");
                }
                continue;
            }
            Err(e) => {
                tracing::warn!(task_id, error = %e, "get_route_result failed — dispatching without route info");
                None
            }
        };

        let repo_ctx = repo_owned.clone();
        tokio::spawn(REPO_CONTEXT.scope(repo_ctx, async move {
            let _dispatch_guard = dispatch_guard; // released on drop (normal or panic)
            let dispatch_start = std::time::Instant::now();
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
                        .map(|v| v != "false")
                        .unwrap_or(true);
                    let display_status = match &signal {
                        WeightSignal::Success { .. } if enable_review => {
                            // Check if response handler already set done (no-op task).
                            // The task_manager.update_task_status() was already called
                            // by response_handler, so read the actual status from store.
                            let has_pr = match store_for_spawn
                                .resolve_task_id(&repo_owned, &task_id)
                                .await
                            {
                                Ok(Some(store_id)) => match store_for_spawn
                                    .get(store_id)
                                    .await
                                {
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
                            };
                            if has_pr {
                                "needs_review"
                            } else {
                                "done"
                            }
                        }
                        WeightSignal::Success { .. } => "done",
                        WeightSignal::RateLimited { .. } => "new",
                        WeightSignal::Blocked => "blocked",
                        // Rerouted: silence detection or other non-rate-limit reset already
                        // moved the task to "new". Stay in "new" — never convert to
                        // needs_review, which would trigger the no-code shortcut and falsely
                        // close the task as done without a successful agent run.
                        WeightSignal::Rerouted => "new",
                        // None means the runner guard skipped the task (e.g. tmux session
                        // already exists) or the task completed with an unrecognised status.
                        // Route to needs_review only when the review agent is enabled;
                        // otherwise mark done so the task is not permanently stuck.
                        WeightSignal::None if enable_review => "needs_review",
                        WeightSignal::None => "done",
                    };

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
                    if is_internal_id(&task_id) {
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

/// Filter routed tasks down to those belonging to a repo outside `active_repos`,
/// drop `no-agent` tasks, and group the rest by repo.
///
/// Pure/side-effect-free so it can be unit tested without touching real tmux
/// sessions or spawning a runner — see `dispatch_routed_tasks_global`, which is
/// the only caller and does the actual (side-effectful) dispatch.
fn group_orphaned_routed_tasks(
    all_routed: Vec<crate::store::Task>,
    active_repos: &std::collections::HashSet<String>,
) -> std::collections::HashMap<String, Vec<crate::store::Task>> {
    let mut by_repo: std::collections::HashMap<String, Vec<crate::store::Task>> =
        std::collections::HashMap::new();
    for task in all_routed
        .into_iter()
        .filter(|t| !active_repos.contains(&t.repo))
        .filter(|t| !t.labels.iter().any(|l| l == "no-agent"))
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
/// not just the repo of the tick that called this. `tick()` runs once per
/// active project; passing only that project's own repo would make every
/// other active repo's routed tasks look "orphaned" too and dispatch them a
/// second time through this fallback path, racing the real per-repo dispatch.
///
/// `TaskRunner::resolve_project_dir` already falls back to the deterministic
/// `~/.orch/projects/<owner>/<repo>.git` bare-clone convention when a repo is
/// not listed in the active config, so dispatch works without a `ProjectEngine`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn dispatch_routed_tasks_global(
    tmux: &Arc<TmuxManager>,
    capture: &Arc<CaptureService>,
    semaphore: &Arc<Semaphore>,
    weight_tx: &mpsc::Sender<WeightSignal>,
    dispatch_mode: DispatchMode,
    dispatching: &Arc<DashMap<String, String>>,
    store: &Arc<TaskStore>,
    session_map: &std::collections::HashMap<String, bool>,
    active_repos: &std::collections::HashSet<String>,
) -> anyhow::Result<()> {
    use crate::store::TaskStatus;

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
        let backend: Arc<dyn ExternalBackend> = match crate::backends::github::GitHubBackend::new(
            repo.clone(),
        ) {
            Ok(b) => Arc::new(b),
            Err(e) => {
                tracing::warn!(repo = %repo, err = %e, "global sweep: failed to construct backend, skipping repo");
                continue;
            }
        };
        let task_manager = Arc::new(TaskManager::with_store(
            backend.clone(),
            store.clone(),
            repo.clone(),
        ));
        let runner = Arc::new(TaskRunner::new(repo.clone()).with_store(store.clone()));
        let dispatchable: Vec<ExternalTask> = tasks
            .iter()
            .map(crate::engine::tasks::store_task_to_external)
            .collect();

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
    let store_blocked = store
        .list_by_status(repo, crate::store::TaskStatus::Blocked)
        .await?;
    let has_block_reason: std::collections::HashSet<String> = store_blocked
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
                let mut child_statuses: std::collections::HashMap<
                    String,
                    crate::store::TaskStatus,
                > = std::collections::HashMap::new();

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
                            let mut seen: std::collections::HashSet<String> =
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
                    child_ext_strs.iter().map(|s| s.as_str()).collect();
                // Query store for statuses of any children present locally
                if let Ok(map) = store_clone
                    .get_statuses_by_external_ids(&repo_owned, &child_exts)
                    .await
                {
                    for (k, v) in map.into_iter() {
                        child_statuses.insert(k, v);
                    }
                } else {
                    tracing::debug!(
                        task_id = task_id.0,
                        "failed to batch-query store for child statuses; falling back to per-child lookups"
                    );
                }

                for child_id in &children {
                    // Check store first
                    if let Some(status) = child_statuses.get(&child_id.0) {
                        if *status != crate::store::TaskStatus::Done {
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
    jobs_path: &std::path::Path,
    backend: &Arc<dyn ExternalBackend>,
    store: Option<&Arc<TaskStore>>,
    repo: &str,
    transport: Option<&Arc<crate::channels::transport::Transport>>,
) -> anyhow::Result<()> {
    jobs::tick(jobs_path, backend, store, repo, transport).await
}

/// Core tick — runs every 10s.
///
/// Delegates to named phase functions in order:
/// 1. `tick_check_session_completions` — poll tmux for finished sessions
/// 2. `tick_recover_stuck_tasks`       — reset in_progress/in_review tasks with no active session
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
    jobs_path: &std::path::Path,
    router: &mut Router,
    task_manager: &Arc<TaskManager>,
    weight_tx: &mpsc::Sender<WeightSignal>,
    dispatching: &Arc<DashMap<String, String>>,
    store: &Arc<TaskStore>,
    transport: Option<&Arc<crate::channels::transport::Transport>>,
    active_repos: &std::collections::HashSet<String>,
) -> anyhow::Result<()> {
    let _tick_span = tracing::info_span!("engine.tick").entered();

    // Run startup cleanup once to kill stale sessions from previous runs.
    startup_cleanup(tmux).await;

    tick_check_session_completions(tmux, repo, capture, store).await?;
    tick_detect_silent_agents(tmux, repo, capture, backend, task_manager, config, store).await?;
    // Fetch session map once and share between stuck-task recovery and dispatch to avoid
    // spawning two `tmux list-panes -a` subprocesses per tick.
    let session_map = tmux.batch_session_active().await;
    tick_recover_stuck_tasks(
        backend,
        tmux,
        repo,
        task_manager,
        config,
        store,
        &session_map,
    )
    .await?;
    tick_route_tasks(backend, task_manager, router, store, repo).await?;
    // Global routing sweep for tasks from inactive or removed repos.
    // tick_route_tasks is scoped to the active repo; tasks from projects no longer
    // in config are never returned by list_routable and would stay in 'new' indefinitely.
    if let Err(e) = route_new_tasks_global(router, store, repo).await {
        tracing::warn!(err = %e, "global new-task routing sweep failed");
    }
    let dispatch_mode = dispatch_mode_from_router(router);
    tick_dispatch_tasks(
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
        &session_map,
    )
    .await?;
    // Global dispatch sweep for routed tasks from inactive or removed repos.
    // tick_dispatch_tasks is scoped to the active repo; tasks from projects no longer
    // in config are never returned by list_external_by_status and would stay in
    // 'routed' indefinitely (see #3413). Excludes the full active-repo set, not just
    // `repo`, so this per-project tick doesn't treat other active repos as orphaned.
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
    tick_unblock_parents(backend, task_manager, store, repo).await?;
    if let Err(e) = tick_job_scheduler(jobs_path, backend, Some(store), repo, transport).await {
        tracing::error!(?e, "job scheduler tick failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::{ExternalId, ExternalTask, Mention, Status};
    use crate::channels::transport::Transport;
    use crate::engine::tasks::TaskManager;
    use crate::store::TaskStore;
    use crate::tmux::TmuxManager;
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
        /// Recorded `get_sub_issues` calls.
        get_sub_issues_calls: Arc<Mutex<Vec<String>>>,
        /// Recorded `update_status` calls.
        status_updates: Arc<Mutex<Vec<(String, Status)>>>,
        /// Recorded `remove_label` calls.
        removed_labels: Arc<Mutex<Vec<(String, String)>>>,
        /// Recorded `post_comment` calls.
        posted_comments: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl MockBackend {
        fn new() -> Self {
            Self {
                blocked_tasks: vec![],
                sub_issues: Default::default(),
                tasks_by_id: Default::default(),
                get_sub_issues_calls: Arc::new(Mutex::new(vec![])),
                status_updates: Arc::new(Mutex::new(vec![])),
                removed_labels: Arc::new(Mutex::new(vec![])),
                posted_comments: Arc::new(Mutex::new(vec![])),
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
            self.posted_comments
                .lock()
                .unwrap()
                .push((_id.0.clone(), _body.to_string()));
            Ok(())
        }
        async fn set_labels(&self, _id: &ExternalId, _labels: &[String]) -> anyhow::Result<()> {
            Ok(())
        }
        async fn remove_label(&self, _id: &ExternalId, _label: &str) -> anyhow::Result<()> {
            self.removed_labels
                .lock()
                .unwrap()
                .push((_id.0.clone(), _label.to_string()));
            Ok(())
        }
        async fn get_sub_issues(&self, id: &ExternalId) -> anyhow::Result<Vec<ExternalId>> {
            self.get_sub_issues_calls.lock().unwrap().push(id.0.clone());
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
        Arc::new(TaskManager::new(backend))
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
        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        tick_unblock_parents(&backend, &task_manager, &store, "test/repo")
            .await
            .unwrap();

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
        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        tick_unblock_parents(&backend, &task_manager, &store, "test/repo")
            .await
            .unwrap();

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
        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        tick_unblock_parents(&backend, &task_manager, &store, "test/repo")
            .await
            .unwrap();

        let updates = status_updates.lock().unwrap();
        assert!(
            updates.is_empty(),
            "should not unblock tasks with no sub-issues"
        );
    }

    #[tokio::test]
    async fn unblock_parents_skips_task_with_block_reason_in_store() {
        // A blocked task that has a known block_reason in the store should never
        // trigger a get_sub_issues GraphQL call — it is not waiting on children.
        let mut mock = MockBackend::new();

        // Blocked parent in the backend list
        let parent = make_task("60", &["status:blocked"]);
        mock.blocked_tasks.push(parent.clone());
        // Give it children in the backend so that, if the skip logic is wrong,
        // the parent would get unblocked.
        mock.sub_issues
            .insert("60".to_string(), vec![ExternalId("61".to_string())]);
        mock.tasks_by_id
            .insert("61".to_string(), make_task("61", &["status:done"]));

        let status_updates = mock.status_updates.clone();
        let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
        let task_manager = make_task_manager(backend.clone());

        // Insert the blocked task into the store with a block_reason set.
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let task_id = store
            .upsert_external(&crate::store::UpsertExternal {
                repo: "test/repo",
                ext_id: "60",
                title: "Task 60",
                body: "",
                author: "bot",
                url: "https://github.com/test/test/issues/60",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store
            .update_status(task_id, crate::store::TaskStatus::Blocked)
            .await
            .unwrap();
        store
            .set_block_reason(task_id, Some("CI failure limit reached"))
            .await
            .unwrap();

        tick_unblock_parents(&backend, &task_manager, &store, "test/repo")
            .await
            .unwrap();

        let updates = status_updates.lock().unwrap();
        assert!(
            updates.is_empty(),
            "task with block_reason should be skipped — no API call, no unblock"
        );
    }

    #[tokio::test]
    async fn unblock_parents_unblocks_internal_parent_from_store_children() {
        let mock = MockBackend::new();
        let get_sub_issues_calls = mock.get_sub_issues_calls.clone();
        let status_updates = mock.status_updates.clone();
        let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let task_manager = Arc::new(TaskManager::with_store(
            backend.clone(),
            store.clone(),
            "test/repo".to_string(),
        ));

        let parent_id = store
            .create_internal("test/repo", "Parent", "", "job", "daily", None)
            .await
            .unwrap();
        store
            .update_status(parent_id, crate::store::TaskStatus::Blocked)
            .await
            .unwrap();
        let child_id = store
            .create(&crate::store::NewTask {
                external_id: Some("91".to_string()),
                repo: "test/repo".to_string(),
                origin: "github".to_string(),
                title: "Child".to_string(),
                body: String::new(),
                source: String::new(),
                source_id: String::new(),
                author: String::new(),
                url: String::new(),
                labels: vec![],
                parent_id: Some(parent_id),
            })
            .await
            .unwrap();
        store
            .update_status(child_id, crate::store::TaskStatus::Done)
            .await
            .unwrap();

        tick_unblock_parents(&backend, &task_manager, &store, "test/repo")
            .await
            .unwrap();

        let parent = store.get(parent_id).await.unwrap();
        assert_eq!(parent.status, crate::store::TaskStatus::New);
        assert!(
            status_updates.lock().unwrap().is_empty(),
            "internal parent should be updated in store without backend mirroring"
        );
        assert!(
            get_sub_issues_calls.lock().unwrap().is_empty(),
            "internal parent should not query backend sub-issues"
        );
    }

    // ── tick_recover_stuck_tasks (InReview) ──────────────────────────────────

    /// Set `updated_at` to a far-past date for a task in an in-memory store.
    #[cfg(test)]
    async fn set_task_updated_at_past(store: &TaskStore, task_id: i64) {
        sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
            .bind(task_id)
            .execute(store.pool())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn recover_stuck_tasks_resets_external_in_review_to_needs_review() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let mock = MockBackend::new();
        let status_updates = mock.status_updates.clone();
        let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
        let task_manager = Arc::new(TaskManager::with_store(
            backend.clone(),
            store.clone(),
            "owner/repo".to_string(),
        ));
        let tmux = Arc::new(TmuxManager::new());
        let config = EngineConfig {
            no_session_stuck_timeout: 600,
            stuck_timeout: 1800,
            ..EngineConfig::default()
        };

        // Insert an external task and transition it to InReview with an old updated_at
        let id = store
            .upsert_external(&crate::store::UpsertExternal {
                repo: "owner/repo",
                ext_id: "99",
                title: "Stuck InReview",
                body: "",
                author: "",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store
            .update_status(id, crate::store::TaskStatus::InReview)
            .await
            .unwrap();
        set_review_session_expected(&store, "owner/repo", "99", true).await;
        set_task_updated_at_past(&store, id).await;

        tick_recover_stuck_tasks(
            &backend,
            &tmux,
            "owner/repo",
            &task_manager,
            &config,
            &store,
            &std::collections::HashMap::new(),
        )
        .await
        .unwrap();

        let updates = status_updates.lock().unwrap();
        assert_eq!(updates.len(), 1, "stuck InReview task should be recovered");
        assert_eq!(updates[0].0, "99");
        assert_eq!(updates[0].1, Status::NeedsReview);
    }

    #[tokio::test]
    async fn recover_stuck_tasks_skips_recent_in_review() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let mock = MockBackend::new();
        let status_updates = mock.status_updates.clone();
        let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
        let task_manager = Arc::new(TaskManager::with_store(
            backend.clone(),
            store.clone(),
            "owner/repo".to_string(),
        ));
        let tmux = Arc::new(TmuxManager::new());
        let config = EngineConfig {
            no_session_stuck_timeout: 600, // 10 minutes — recent task won't trigger
            stuck_timeout: 1800,
            ..EngineConfig::default()
        };

        // Insert a recently-updated InReview task (updated_at = now)
        let id = store
            .upsert_external(&crate::store::UpsertExternal {
                repo: "owner/repo",
                ext_id: "100",
                title: "Recent InReview",
                body: "",
                author: "",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store
            .update_status(id, crate::store::TaskStatus::InReview)
            .await
            .unwrap();
        // updated_at stays at now — age < 600s → should NOT be recovered

        tick_recover_stuck_tasks(
            &backend,
            &tmux,
            "owner/repo",
            &task_manager,
            &config,
            &store,
            &std::collections::HashMap::new(),
        )
        .await
        .unwrap();

        let updates = status_updates.lock().unwrap();
        assert!(
            updates.is_empty(),
            "recent InReview task should not be recovered"
        );
    }

    #[tokio::test]
    async fn recover_stuck_tasks_resets_internal_in_review_to_needs_review() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
        let task_manager = Arc::new(TaskManager::with_store(
            backend.clone(),
            store.clone(),
            "owner/repo".to_string(),
        ));
        let tmux = Arc::new(TmuxManager::new());
        let config = EngineConfig {
            no_session_stuck_timeout: 600,
            stuck_timeout: 1800,
            ..EngineConfig::default()
        };

        // Create an internal task in InReview with old updated_at
        let id = store
            .create_internal("owner/repo", "Internal InReview", "", "cron", "1", None)
            .await
            .unwrap();
        store
            .update_status(id, crate::store::TaskStatus::InReview)
            .await
            .unwrap();
        set_review_session_expected(&store, "owner/repo", "internal:1", true).await;
        set_task_updated_at_past(&store, id).await;

        tick_recover_stuck_tasks(
            &backend,
            &tmux,
            "owner/repo",
            &task_manager,
            &config,
            &store,
            &std::collections::HashMap::new(),
        )
        .await
        .unwrap();

        let task = store.get(id).await.unwrap();
        assert_eq!(
            task.status,
            crate::store::TaskStatus::NeedsReview,
            "stuck internal InReview task should be reset to NeedsReview"
        );
    }

    #[tokio::test]
    async fn recover_stuck_tasks_resets_cross_repo_external_in_progress_to_new() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let mock = MockBackend::new();
        let status_updates = mock.status_updates.clone();
        let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
        let task_manager = Arc::new(TaskManager::with_store(
            backend.clone(),
            store.clone(),
            "owner/repo".to_string(),
        ));
        let tmux = Arc::new(TmuxManager::new());
        let config = EngineConfig {
            no_session_stuck_timeout: 600,
            stuck_timeout: 1800,
            ..EngineConfig::default()
        };

        // Cross-repo task should still be recovered even when current tick repo is owner/repo.
        let id = store
            .upsert_external(&crate::store::UpsertExternal {
                repo: "owner/other",
                ext_id: "777",
                title: "Cross-repo stuck task",
                body: "",
                author: "",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store
            .update_status(id, crate::store::TaskStatus::InProgress)
            .await
            .unwrap();
        set_task_updated_at_past(&store, id).await;

        tick_recover_stuck_tasks(
            &backend,
            &tmux,
            "owner/repo",
            &task_manager,
            &config,
            &store,
            &std::collections::HashMap::new(),
        )
        .await
        .unwrap();

        let task = store.get(id).await.unwrap();
        assert_eq!(
            task.status,
            crate::store::TaskStatus::New,
            "cross-repo stale in_progress task should be reset to New"
        );
        assert_eq!(task.route_attempts, 0, "route attempts should be cleared");
        assert!(task.agent.is_none(), "agent should be cleared");
        assert!(task.model.is_none(), "model should be cleared");

        // Cross-repo global sweep is store-only and must not call backend status updates.
        assert!(
            status_updates.lock().unwrap().is_empty(),
            "cross-repo recovery should not call backend update_status"
        );
    }

    #[tokio::test]
    async fn recover_stuck_tasks_skips_external_in_review_waiting_for_human_review() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let mock = MockBackend::new();
        let status_updates = mock.status_updates.clone();
        let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
        let task_manager = Arc::new(TaskManager::with_store(
            backend.clone(),
            store.clone(),
            "owner/repo".to_string(),
        ));
        let tmux = Arc::new(TmuxManager::new());
        let config = EngineConfig {
            no_session_stuck_timeout: 600,
            stuck_timeout: 1800,
            ..EngineConfig::default()
        };

        let id = store
            .upsert_external(&crate::store::UpsertExternal {
                repo: "owner/repo",
                ext_id: "101",
                title: "Waiting For Human Review",
                body: "",
                author: "",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store
            .update_status(id, crate::store::TaskStatus::InReview)
            .await
            .unwrap();
        set_task_updated_at_past(&store, id).await;

        tick_recover_stuck_tasks(
            &backend,
            &tmux,
            "owner/repo",
            &task_manager,
            &config,
            &store,
            &std::collections::HashMap::new(),
        )
        .await
        .unwrap();

        let updates = status_updates.lock().unwrap();
        assert!(
            updates.is_empty(),
            "InReview task waiting for human review should not be reset"
        );
    }

    #[tokio::test]
    async fn unblock_parents_no_blocked_tasks() {
        let mock = MockBackend::new(); // no blocked tasks
        let status_updates = mock.status_updates.clone();
        let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
        let task_manager = make_task_manager(backend.clone());
        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        tick_unblock_parents(&backend, &task_manager, &store, "test/repo")
            .await
            .unwrap();

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
        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        tick_unblock_parents(&backend, &task_manager, &store, "test/repo")
            .await
            .unwrap();

        let updates = status_updates.lock().unwrap();
        assert_eq!(updates.len(), 1, "only one parent should be unblocked");
        assert_eq!(updates[0].0, "40", "task 40 should be unblocked");
        assert_eq!(updates[0].1, Status::New);
    }

    #[tokio::test]
    async fn detect_silent_agents_skips_backend_for_internal_tasks() {
        let transport = Arc::new(Transport::new());
        let capture = Arc::new(CaptureService::new(transport));
        let mock = Arc::new(MockBackend::new());
        let backend: Arc<dyn ExternalBackend> = mock.clone();
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let task_manager = Arc::new(TaskManager::with_store(
            backend.clone(),
            store.clone(),
            "owner/repo".to_string(),
        ));
        let tmux = Arc::new(TmuxManager::new());
        let config = EngineConfig {
            silence_grace_period: 0,
            ..EngineConfig::default()
        };

        let internal_id = store
            .create_internal("owner/repo", "Silent internal", "", "cron", "1", None)
            .await
            .unwrap();
        store
            .update_status(internal_id, crate::store::TaskStatus::InProgress)
            .await
            .unwrap();

        let task_id = format!("internal:{internal_id}");
        capture
            .register_session("owner/repo", &task_id, "orch-test-internal")
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

        tick_detect_silent_agents(
            &tmux,
            "owner/repo",
            &capture,
            &backend,
            &task_manager,
            &config,
            &store,
        )
        .await
        .unwrap();

        assert!(
            mock.removed_labels.lock().unwrap().is_empty(),
            "internal tasks should skip backend label removal"
        );
        assert!(
            mock.posted_comments.lock().unwrap().is_empty(),
            "internal tasks should skip backend comments"
        );

        let task = store.get(internal_id).await.unwrap();
        // Silence detection now uses failover: if a fallback agent is found → Routed,
        // otherwise → New (reset for re-routing when cooldowns expire). Either is
        // acceptable; the key invariant is that the task is no longer InProgress.
        assert!(
            matches!(
                task.status,
                crate::store::TaskStatus::Routed | crate::store::TaskStatus::New
            ),
            "internal task should be routed to fallback or reset to new, got {:?}",
            task.status
        );
    }

    /// Regression test for the race between Phase 1 (tick_check_session_completions)
    /// and Phase 2 (tick_recover_stuck_tasks).
    ///
    /// Before the fix, Phase 1 killed the tmux session without touching updated_at.
    /// Phase 2 could then see status=in_progress, has_session=false, and an old
    /// updated_at, and would reset the task to New — even though the runner's
    /// wait_for_completion() poll had not yet returned.
    ///
    /// The fix: Phase 1 touches updated_at as soon as it detects the session is done.
    /// This test verifies that after the touch, Phase 2 does NOT reclaim the task.
    #[tokio::test]
    async fn phase1_session_completion_touch_prevents_phase2_reclaim() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let mock = MockBackend::new();
        let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
        let task_manager = Arc::new(TaskManager::with_store(
            backend.clone(),
            store.clone(),
            "owner/repo".to_string(),
        ));
        let tmux = Arc::new(TmuxManager::new());
        let config = EngineConfig {
            no_session_stuck_timeout: 600,
            stuck_timeout: 1800,
            ..EngineConfig::default()
        };

        // Create an in-progress internal task with old updated_at.
        // Without the fix this task would be reclaimed by Phase 2 (age >> threshold,
        // no session running in the test environment).
        let id = store
            .create_internal("owner/repo", "Completed task", "", "cron", "1", None)
            .await
            .unwrap();
        store
            .update_status(id, crate::store::TaskStatus::InProgress)
            .await
            .unwrap();
        set_task_updated_at_past(&store, id).await;

        // Simulate Phase 1: touch updated_at when the session is detected as done.
        // In production this happens inside tick_check_session_completions.
        let task_id = format!("internal:{id}");
        store_touch_updated_at(&Some(Arc::clone(&store)), "owner/repo", &task_id).await;

        // Phase 2 runs — updated_at is fresh so the task must NOT be reclaimed.
        tick_recover_stuck_tasks(
            &backend,
            &tmux,
            "owner/repo",
            &task_manager,
            &config,
            &store,
            &std::collections::HashMap::new(),
        )
        .await
        .unwrap();

        let task = store.get(id).await.unwrap();
        assert_eq!(
            task.status,
            crate::store::TaskStatus::InProgress,
            "Phase 2 must not reclaim a task whose updated_at was just touched by Phase 1"
        );
    }

    #[tokio::test]
    async fn stuck_task_timing_treats_dead_session_as_missing() {
        let tmux = Arc::new(TmuxManager::new());
        let repo = "owner/repo";
        let task_id = "internal:33498";
        let session_name = tmux.session_name(repo, task_id);

        let create_result = tokio::process::Command::new("tmux")
            .args(["new-session", "-d", "-s", &session_name, "-c", "/tmp"])
            .output()
            .await;

        match create_result {
            Ok(o) if o.status.success() => {}
            _ => {
                eprintln!("Skipping test: tmux not available or failed to create test session");
                return;
            }
        }

        let set_option_result = tokio::process::Command::new("tmux")
            .args(["set-option", "-t", &session_name, "remain-on-exit", "on"])
            .output()
            .await;
        if !matches!(set_option_result, Ok(ref o) if o.status.success()) {
            eprintln!("Skipping test: unable to set tmux remain-on-exit option");
            let _ = tmux.kill_session(&session_name).await;
            return;
        }

        let send_exit_result = tokio::process::Command::new("tmux")
            .args(["send-keys", "-t", &session_name, "exit", "Enter"])
            .output()
            .await;
        if !matches!(send_exit_result, Ok(ref o) if o.status.success()) {
            eprintln!("Skipping test: unable to exit tmux pane");
            let _ = tmux.kill_session(&session_name).await;
            return;
        }

        // Poll until pane is dead (up to 2s) — fixed sleep is unreliable under load.
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if !tmux.session_is_running(&session_name).await {
                break;
            }
        }

        // Also wait for batch_session_active to reflect the dead state.
        // session_is_running uses `list-panes -t <session>` which may settle
        // before the global `list-panes -a` snapshot used by the production code.
        // On CI under load these two tmux commands can transiently disagree.
        let mut session_map = tmux.batch_session_active().await;
        for _ in 0..20 {
            if !session_map.get(&session_name).copied().unwrap_or(false) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            session_map = tmux.batch_session_active().await;
        }
        if session_map.get(&session_name).copied().unwrap_or(false) {
            eprintln!("Skipping test: session still alive in batch_session_active after retries (CI tmux lag)");
            let _ = tmux.kill_session(&session_name).await;
            return;
        }

        let config = EngineConfig {
            no_session_stuck_timeout: 600,
            stuck_timeout: 1800,
            ..EngineConfig::default()
        };
        let updated_at = (chrono::Utc::now()
            - chrono::Duration::seconds(config.no_session_stuck_timeout as i64 + 5))
        .to_rfc3339();

        let timing = stuck_task_timing_from_map(
            &tmux,
            repo,
            task_id,
            &updated_at,
            &config,
            "parse failure",
            &session_map,
        )
        .expect("dead session should be treated as missing");

        assert!(!timing.has_session);
        assert_eq!(timing.threshold, config.no_session_stuck_timeout);

        let _ = tmux.kill_session(&session_name).await;
    }

    #[tokio::test]
    async fn recover_stuck_tasks_reclaims_routed_task_blocked_by_stale_session() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let mock = MockBackend::new();
        let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
        let task_manager = Arc::new(TaskManager::with_store(
            backend.clone(),
            store.clone(),
            "owner/repo".to_string(),
        ));
        let tmux = Arc::new(TmuxManager::new());
        let config = EngineConfig {
            no_session_stuck_timeout: 600,
            stuck_timeout: 1,
            ..EngineConfig::default()
        };

        let id = store
            .create_internal("owner/repo", "Blocked routed task", "", "cron", "1", None)
            .await
            .unwrap();
        store
            .update_status(id, crate::store::TaskStatus::Routed)
            .await
            .unwrap();
        set_task_updated_at_past(&store, id).await;

        let task_id = format!("internal:{id}");
        let session_name = tmux.session_name("owner/repo", &task_id);
        let create_result = tokio::process::Command::new("tmux")
            .args([
                "new-session",
                "-d",
                "-s",
                &session_name,
                "-c",
                "/tmp",
                "sleep 60",
            ])
            .output()
            .await;

        match create_result {
            Ok(o) if o.status.success() => {}
            _ => {
                eprintln!("Skipping test: tmux not available or failed to create test session");
                return;
            }
        }

        let session_map = tmux.batch_session_active().await;
        assert_eq!(session_map.get(&session_name).copied(), Some(true));

        tick_recover_stuck_tasks(
            &backend,
            &tmux,
            "owner/repo",
            &task_manager,
            &config,
            &store,
            &session_map,
        )
        .await
        .unwrap();

        let task = store.get(id).await.unwrap();
        assert_eq!(
            task.status,
            crate::store::TaskStatus::New,
            "routed task blocked by a stale session should be reset for re-routing"
        );
        assert!(
            !tmux.session_exists(&session_name).await,
            "recovery should kill the stale tmux session so dispatch can proceed"
        );
    }

    // ── tick_dispatch_tasks: deadlock regression test (#1361) ──────────────

    /// Regression test for #1361: tick_dispatch_tasks previously accepted
    /// `&Arc<RwLock<Router>>` and called `router_arc.read().await` internally.
    /// The main loop already held a write lock on the same RwLock, so the read
    /// could never be acquired — guaranteed deadlock on every tick with routed
    /// tasks.
    ///
    /// The fix changed the signature to accept `&Router` directly (the caller
    /// already has the dereferenced guard), eliminating the lock re-acquisition.
    ///
    /// This test verifies the function completes within 2 seconds when the
    /// caller holds the write lock — the old code would deadlock here.
    #[tokio::test]
    async fn dispatch_does_not_deadlock_under_write_lock() {
        use crate::engine::router::Router;
        use tokio::sync::RwLock;

        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let mock = MockBackend::new();
        let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
        let task_manager = Arc::new(TaskManager::with_store(
            backend.clone(),
            store.clone(),
            "owner/repo".to_string(),
        ));
        let tmux = Arc::new(TmuxManager::new());
        let semaphore = Arc::new(Semaphore::new(4));
        let (weight_tx, _weight_rx) = mpsc::channel(16);
        let dispatching = Arc::new(DashMap::new());
        let transport = Arc::new(Transport::new());
        let capture = Arc::new(CaptureService::new(transport));

        let router_config = crate::engine::router::RouterConfig::default();
        let router = Router::new(router_config);
        let router_arc = Arc::new(RwLock::new(router));

        // Simulate what the main loop does: hold a write lock, snapshot
        // dispatch mode, then call tick_dispatch_tasks without passing any
        // lock guard into async dispatch work.
        let write_guard = router_arc.write().await;
        let dispatch_mode = dispatch_mode_from_router(&write_guard);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tick_dispatch_tasks(
                &backend,
                &tmux,
                "owner/repo",
                &Arc::new(crate::engine::runner::TaskRunner::new(
                    "owner/repo".to_string(),
                )),
                &capture,
                &semaphore,
                &task_manager,
                &weight_tx,
                dispatch_mode,
                &dispatching,
                &store,
                &std::collections::HashMap::new(),
            ),
        )
        .await;

        assert!(
            result.is_ok(),
            "tick_dispatch_tasks deadlocked! It tried to acquire a read lock \
             on router_arc while a write lock was already held (issue #1361)"
        );
    }

    /// Regression test for the completed-session -> stuck-task reclaim race.
    ///
    /// Scenario:
    /// - Runner finishes a tmux session and will call store_touch_updated_at shortly after
    /// - Engine's tick may observe the dead session in Phase 1 and Phase 2 may run
    ///   stuck-task recovery before the runner touches the store, causing incorrect reclaim
    /// Fix: tick_check_session_completions now touches the store.updated_at when it
    /// observes a completed session, preventing reclaim. This test simulates the
    /// timing by creating a session, marking the corresponding task InProgress with
    /// an old updated_at, calling tick_check_session_completions (which should touch
    /// the store), then calling tick_recover_stuck_tasks and asserting the task
    /// remains InProgress.
    #[tokio::test]
    async fn completed_session_does_not_cause_stuck_reclaim_race() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let mock = MockBackend::new();
        let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
        let task_manager = Arc::new(TaskManager::with_store(
            backend.clone(),
            store.clone(),
            "owner/repo".to_string(),
        ));
        let tmux = Arc::new(TmuxManager::new());
        let transport = Arc::new(Transport::new());
        let capture = Arc::new(CaptureService::new(transport));

        // Insert external task and set to InProgress with old updated_at
        let id = store
            .upsert_external(&crate::store::UpsertExternal {
                repo: "owner/repo",
                ext_id: "200",
                title: "Race Task",
                body: "",
                author: "",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store
            .update_status(id, crate::store::TaskStatus::InProgress)
            .await
            .unwrap();

        // set updated_at to far past so it would normally be eligible for reclaim
        sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
            .bind(id)
            .execute(store.pool())
            .await
            .unwrap();

        // Register a real tmux session for this task and then exit it so it's observed dead
        let task_id = "200";
        let session_name = tmux.session_name("owner/repo", task_id);
        let create_result = tokio::process::Command::new("tmux")
            .args(["new-session", "-d", "-s", &session_name, "-c", "/tmp"])
            .output()
            .await;
        match create_result {
            Ok(o) if o.status.success() => {}
            _ => {
                eprintln!("Skipping test: tmux not available or failed to create test session");
                return;
            }
        }
        // Ensure remain-on-exit so the session lingers as dead long enough to be snapshot
        let _ = tokio::process::Command::new("tmux")
            .args(["set-option", "-t", &session_name, "remain-on-exit", "on"])
            .output()
            .await;
        let _ = tokio::process::Command::new("tmux")
            .args(["send-keys", "-t", &session_name, "exit", "Enter"])
            .output()
            .await;

        // Poll until pane is dead
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if !tmux.session_is_running(&session_name).await {
                break;
            }
        }

        // Register the session in capture (engine does this during dispatch)
        let task_key = task_id.to_string();
        capture
            .register_session("owner/repo", &task_key, &session_name)
            .await;

        // Call phase1 (now touches store.updated_at for completed session)
        let config = EngineConfig {
            no_session_stuck_timeout: 600,
            stuck_timeout: 1800,
            ..EngineConfig::default()
        };

        tick_check_session_completions(&tmux, "owner/repo", &capture, &store)
            .await
            .unwrap();

        // Verify phase1 actually touched updated_at. It only processes sessions
        // that appear in snapshot() (list_sessions + batch_session_active). On CI,
        // if the session was already fully removed before the snapshot runs (e.g.
        // remain-on-exit semantics differ across tmux versions), phase1 won't see
        // it and the store won't be touched. Skip in that case.
        {
            let task_state = store.get(id).await.unwrap();
            if task_state.updated_at.starts_with("2020") {
                eprintln!(
                    "Skipping test: tick_check_session_completions did not update stored \
                     updated_at — session was not in snapshot (CI tmux environment issue)"
                );
                let _ = tmux.kill_session(&session_name).await;
                return;
            }
        }

        // Now run phase2 — the updated_at should have been touched so no reclaim occurs
        tick_recover_stuck_tasks(
            &backend,
            &tmux,
            "owner/repo",
            &task_manager,
            &config,
            &store,
            &std::collections::HashMap::new(),
        )
        .await
        .unwrap();

        // Verify the task is still InProgress in the store
        let task = store.get(id).await.unwrap();
        assert_eq!(
            task.status,
            crate::store::TaskStatus::InProgress,
            "task should not be reclaimed to New"
        );

        let _ = tmux.kill_session(&session_name).await;
    }

    // ── tick_job_scheduler: resilience to bad project configs ────────────────

    /// A project with a duplicate job id (same id in .orch.yml AND prompts/jobs/)
    /// must not cause tick_job_scheduler to return Err — it should log an error
    /// and return Ok(()) so other projects are unaffected.
    #[tokio::test]
    async fn job_scheduler_returns_ok_on_duplicate_job_config() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join(".orch.yml");
        std::fs::write(
            &cfg,
            "jobs:\n  - id: trading-scan\n    schedule: \"0 9 * * *\"\n    task:\n      title: Scan\n      body: do it\n",
        )
        .unwrap();
        let jobs_dir = tmp.path().join("prompts").join("jobs");
        std::fs::create_dir_all(&jobs_dir).unwrap();
        std::fs::write(
            jobs_dir.join("trading-scan.md"),
            "---\nid: trading-scan\nschedule: '0 9 * * *'\ntitle: Trading Scan\n---\n\nDo trading scan.\n",
        )
        .unwrap();

        let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
        let result = tick_job_scheduler(&cfg, &backend, None, "test/repo", None).await;
        assert!(
            result.is_ok(),
            "duplicate job id must not propagate as Err: {result:?}"
        );
    }

    /// When one project has a duplicate job id (bad config) and a second project
    /// has a valid config, the scheduler tick for the second project must still
    /// succeed — simulating the per-project loop in the engine.
    #[tokio::test]
    async fn job_scheduler_processes_valid_project_when_other_has_duplicate() {
        // Bad project: duplicate id across .orch.yml and prompts/jobs/
        let bad_tmp = tempfile::tempdir().unwrap();
        let bad_cfg = bad_tmp.path().join(".orch.yml");
        std::fs::write(
            &bad_cfg,
            "jobs:\n  - id: dup\n    schedule: \"0 9 * * *\"\n    task:\n      title: Dup\n      body: x\n",
        )
        .unwrap();
        let bad_jobs_dir = bad_tmp.path().join("prompts").join("jobs");
        std::fs::create_dir_all(&bad_jobs_dir).unwrap();
        std::fs::write(
            bad_jobs_dir.join("dup.md"),
            "---\nid: dup\nschedule: '0 9 * * *'\ntitle: Dup\n---\n\nbody\n",
        )
        .unwrap();

        // Good project: valid config, no duplicate
        let good_tmp = tempfile::tempdir().unwrap();
        let good_cfg = good_tmp.path().join(".orch.yml");
        std::fs::write(
            &good_cfg,
            "jobs:\n  - id: valid-job\n    schedule: \"0 9 * * *\"\n    task:\n      title: Valid\n      body: ok\n",
        )
        .unwrap();

        let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());

        let bad_result = tick_job_scheduler(&bad_cfg, &backend, None, "test/bad-repo", None).await;
        assert!(
            bad_result.is_ok(),
            "bad project must not abort scheduler: {bad_result:?}"
        );

        let good_result =
            tick_job_scheduler(&good_cfg, &backend, None, "test/good-repo", None).await;
        assert!(
            good_result.is_ok(),
            "valid project must still run after bad project: {good_result:?}"
        );
    }

    // ── route_new_tasks_global ───────────────────────────────────────────────

    /// Regression test for #3407: tasks from inactive/removed repos were stuck
    /// in `new` indefinitely because `tick_route_tasks` is repo-scoped.
    ///
    /// `route_new_tasks_global` must route tasks whose repo != current_repo
    /// and leave current-repo tasks untouched.
    #[serial_test::serial(cooldown_state)]
    #[tokio::test]
    async fn global_sweep_routes_inactive_repo_tasks() {
        use crate::engine::router::{Router, RouterConfig};
        use crate::store::{TaskStatus, UpsertExternal};

        crate::engine::cooldown::reset_global_state().await;

        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        // Active repo task — must NOT be touched by the global sweep.
        let active_id = store
            .upsert_external(&UpsertExternal {
                repo: "active/repo",
                ext_id: "1",
                title: "Active task",
                body: "",
                author: "user",
                url: "https://github.com/active/repo/issues/1",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();

        // Inactive repo task — must be routed by the global sweep.
        let inactive_id = store
            .upsert_external(&UpsertExternal {
                repo: "inactive/repo",
                ext_id: "42",
                title: "Orphaned task",
                body: "",
                author: "user",
                url: "https://github.com/inactive/repo/issues/42",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();

        assert_eq!(store.get(active_id).await.unwrap().status, TaskStatus::New);
        assert_eq!(
            store.get(inactive_id).await.unwrap().status,
            TaskStatus::New
        );

        let mut model_map: std::collections::HashMap<
            String,
            std::collections::HashMap<String, Vec<String>>,
        > = std::collections::HashMap::new();
        let mut tier = std::collections::HashMap::new();
        tier.insert("claude".to_string(), vec!["haiku".to_string()]);
        model_map.insert("medium".to_string(), tier);
        let config = RouterConfig {
            mode: "round_robin".to_string(),
            agents: vec!["claude".to_string()],
            model_map,
            ..Default::default()
        };
        let mut router = Router::new_for_test(config, vec!["claude".to_string()]);

        route_new_tasks_global(&mut router, &store, "active/repo")
            .await
            .unwrap();

        // Active-repo task must remain new (global sweep skips it).
        assert_eq!(
            store.get(active_id).await.unwrap().status,
            TaskStatus::New,
            "global sweep must not touch tasks for the active repo"
        );

        // Inactive-repo task must now be routed.
        let after = store.get(inactive_id).await.unwrap();
        assert_eq!(
            after.status,
            TaskStatus::Routed,
            "global sweep must route tasks from inactive repos"
        );
        assert!(
            matches!(&after.agent, Some(a) if !a.is_empty()),
            "agent must be set after routing"
        );
    }

    /// Tasks with the `no-agent` label must be skipped by the global sweep.
    #[serial_test::serial(cooldown_state)]
    #[tokio::test]
    async fn global_sweep_skips_no_agent_tasks() {
        use crate::engine::router::{Router, RouterConfig};
        use crate::store::{TaskStatus, UpsertExternal};

        crate::engine::cooldown::reset_global_state().await;

        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        let id = store
            .upsert_external(&UpsertExternal {
                repo: "inactive/repo",
                ext_id: "99",
                title: "No-agent task",
                body: "",
                author: "user",
                url: "https://github.com/inactive/repo/issues/99",
                labels: &["no-agent".to_string()],
                origin: "github",
            })
            .await
            .unwrap();

        let mut model_map: std::collections::HashMap<
            String,
            std::collections::HashMap<String, Vec<String>>,
        > = std::collections::HashMap::new();
        let mut tier = std::collections::HashMap::new();
        tier.insert("claude".to_string(), vec!["haiku".to_string()]);
        model_map.insert("medium".to_string(), tier);
        let config = RouterConfig {
            mode: "round_robin".to_string(),
            agents: vec!["claude".to_string()],
            model_map,
            ..Default::default()
        };
        let mut router = Router::new_for_test(config, vec!["claude".to_string()]);

        route_new_tasks_global(&mut router, &store, "active/repo")
            .await
            .unwrap();

        assert_eq!(
            store.get(id).await.unwrap().status,
            TaskStatus::New,
            "no-agent tasks must not be routed"
        );
    }

    // ── dispatch_routed_tasks_global ─────────────────────────────────────────
    //
    // `dispatch_routed_tasks_global` itself is deliberately not exercised
    // end-to-end here: once it finds an orphaned repo, it spawns the real
    // runner, which creates a real tmux session and touches the network.
    // The existing `dispatch_does_not_deadlock_under_write_lock` test avoids
    // this same hazard by keeping the dispatchable set empty. The selection
    // logic (which tasks belong to which repo) is pulled into the pure
    // `group_orphaned_routed_tasks` helper below so it can be tested without
    // those side effects.

    /// Regression test for #3413: tasks routed for an inactive/removed repo were
    /// stuck in `routed` indefinitely because `tick_dispatch_tasks` is repo-scoped.
    ///
    /// `group_orphaned_routed_tasks` must select tasks whose repo != the active
    /// repo, group them by repo, and leave active-repo tasks out entirely.
    #[tokio::test]
    async fn group_orphaned_routed_tasks_selects_inactive_repo_tasks() {
        use crate::store::{StoreRoute, TaskStatus, UpsertExternal};

        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        async fn make_routed(store: &TaskStore, repo: &str, ext_id: &str) -> i64 {
            let id = store
                .upsert_external(&UpsertExternal {
                    repo,
                    ext_id,
                    title: "Routed task",
                    body: "",
                    author: "user",
                    url: &format!("https://github.com/{repo}/issues/{ext_id}"),
                    labels: &[],
                    origin: "github",
                })
                .await
                .unwrap();
            store
                .store_route(&StoreRoute {
                    id,
                    agent: "claude",
                    model: Some("sonnet"),
                    complexity: "medium",
                    estimate: 0,
                    reason: "test",
                    profile: "{}",
                    skills: "[]",
                })
                .await
                .unwrap();
            store.update_status(id, TaskStatus::Routed).await.unwrap();
            id
        }

        let active_id = make_routed(&store, "active/repo", "1").await;
        let inactive_id_a = make_routed(&store, "inactive/repo", "2").await;
        let other_inactive_id = make_routed(&store, "other-inactive/repo", "3").await;
        let inactive_id_b = make_routed(&store, "inactive/repo", "4").await;

        let all_routed = store
            .list_all_by_status_global(TaskStatus::Routed)
            .await
            .unwrap();
        let active_repos: std::collections::HashSet<String> =
            ["active/repo".to_string()].into_iter().collect();
        let by_repo = group_orphaned_routed_tasks(all_routed, &active_repos);

        assert!(
            !by_repo.contains_key("active/repo"),
            "global sweep must not select tasks for the active repo"
        );
        let inactive_ids: std::collections::HashSet<i64> = by_repo
            .get("inactive/repo")
            .expect("inactive/repo must be selected")
            .iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(
            inactive_ids,
            [inactive_id_a, inactive_id_b].into_iter().collect(),
            "global sweep must group all orphaned tasks for a repo together"
        );
        assert_eq!(
            by_repo
                .get("other-inactive/repo")
                .map(|tasks| tasks.iter().map(|t| t.id).collect::<Vec<_>>()),
            Some(vec![other_inactive_id])
        );

        // Sanity: the active-repo task really was created and routed, it's just
        // excluded from the selection above.
        assert_eq!(
            store.get(active_id).await.unwrap().status,
            TaskStatus::Routed
        );
    }

    /// Regression test for the PR #3413 review finding: `tick()` runs once per active
    /// project, so a single `current_repo` string is the wrong exclusion — repo B's
    /// routed tasks must not look "orphaned" during repo A's tick just because A != B.
    /// `group_orphaned_routed_tasks` must exclude every repo in the full active set.
    #[tokio::test]
    async fn group_orphaned_routed_tasks_excludes_all_active_repos_not_just_current() {
        use crate::store::{StoreRoute, TaskStatus, UpsertExternal};

        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        async fn make_routed(store: &TaskStore, repo: &str, ext_id: &str) -> i64 {
            let id = store
                .upsert_external(&UpsertExternal {
                    repo,
                    ext_id,
                    title: "Routed task",
                    body: "",
                    author: "user",
                    url: &format!("https://github.com/{repo}/issues/{ext_id}"),
                    labels: &[],
                    origin: "github",
                })
                .await
                .unwrap();
            store
                .store_route(&StoreRoute {
                    id,
                    agent: "claude",
                    model: Some("sonnet"),
                    complexity: "medium",
                    estimate: 0,
                    reason: "test",
                    profile: "{}",
                    skills: "[]",
                })
                .await
                .unwrap();
            store.update_status(id, TaskStatus::Routed).await.unwrap();
            id
        }

        let repo_a_id = make_routed(&store, "active/repo-a", "1").await;
        let repo_b_id = make_routed(&store, "active/repo-b", "2").await;
        let orphaned_id = make_routed(&store, "inactive/repo", "3").await;

        let all_routed = store
            .list_all_by_status_global(TaskStatus::Routed)
            .await
            .unwrap();
        // Simulate repo A's tick: current repo is "active/repo-a", but the full
        // active project set also includes "active/repo-b".
        let active_repos: std::collections::HashSet<String> =
            ["active/repo-a".to_string(), "active/repo-b".to_string()]
                .into_iter()
                .collect();
        let by_repo = group_orphaned_routed_tasks(all_routed, &active_repos);

        assert!(
            !by_repo.contains_key("active/repo-a"),
            "the current repo's own tasks must not be swept"
        );
        assert!(
            !by_repo.contains_key("active/repo-b"),
            "another active repo's tasks must not be swept during repo A's tick"
        );
        assert_eq!(
            by_repo
                .get("inactive/repo")
                .map(|tasks| tasks.iter().map(|t| t.id).collect::<Vec<_>>()),
            Some(vec![orphaned_id]),
            "a genuinely inactive repo's tasks must still be swept"
        );

        // Sanity: repo A and repo B tasks were really created and routed, just excluded.
        assert_eq!(
            store.get(repo_a_id).await.unwrap().status,
            TaskStatus::Routed
        );
        assert_eq!(
            store.get(repo_b_id).await.unwrap().status,
            TaskStatus::Routed
        );
    }

    /// Tasks with the `no-agent` label must be skipped by the global dispatch sweep.
    #[tokio::test]
    async fn global_dispatch_sweep_skips_no_agent_tasks() {
        use crate::store::{StoreRoute, TaskStatus, UpsertExternal};

        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        let id = store
            .upsert_external(&UpsertExternal {
                repo: "inactive/repo",
                ext_id: "99",
                title: "No-agent routed task",
                body: "",
                author: "user",
                url: "https://github.com/inactive/repo/issues/99",
                labels: &["no-agent".to_string()],
                origin: "github",
            })
            .await
            .unwrap();
        store
            .store_route(&StoreRoute {
                id,
                agent: "claude",
                model: Some("sonnet"),
                complexity: "medium",
                estimate: 0,
                reason: "test",
                profile: "{}",
                skills: "[]",
            })
            .await
            .unwrap();
        store.update_status(id, TaskStatus::Routed).await.unwrap();

        // Safe to call the full sweep here: with the only orphaned task filtered
        // out by the no-agent label, `group_orphaned_routed_tasks` returns an
        // empty map and `dispatch_routed_tasks_global` returns before ever
        // constructing a backend or spawning a runner.
        let tmux = Arc::new(TmuxManager::new());
        let transport = Arc::new(Transport::new());
        let capture = Arc::new(CaptureService::new(transport));
        let semaphore = Arc::new(Semaphore::new(4));
        let (weight_tx, _weight_rx) = mpsc::channel(16);
        let dispatching = Arc::new(DashMap::new());
        let dispatch_mode = DispatchMode {
            is_degraded: false,
            healthy_agents: 1,
            threshold: 1,
        };

        let active_repos: std::collections::HashSet<String> =
            ["active/repo".to_string()].into_iter().collect();
        dispatch_routed_tasks_global(
            &tmux,
            &capture,
            &semaphore,
            &weight_tx,
            dispatch_mode,
            &dispatching,
            &store,
            &std::collections::HashMap::new(),
            &active_repos,
        )
        .await
        .unwrap();

        assert_eq!(
            store.get(id).await.unwrap().status,
            TaskStatus::Routed,
            "no-agent tasks must not be dispatched"
        );
    }
}
