//! Stuck task and silent agent recovery

use super::{recover_routed_blocked_dispatch, run_tier, should_use_backend, tick_router};
use crate::backends::{ExternalBackend, ExternalId, Status};
use crate::channels::capture::CaptureService;
use crate::cmd_cache;
use crate::engine;
use crate::engine::EngineConfig;
use crate::engine::cooldown::{
    SILENCE_AGENT_COOLDOWN_SECS, SILENCE_EXTENDED_COOLDOWN_SECS, record_agent_failure_with_message,
    record_silence_detection, set_agent_cooldown, set_model_cooldown,
};
use crate::engine::router::RouterConfig;
use crate::engine::runner::response;
use crate::engine::tasks::TaskManager;
use crate::store;
use crate::store::TaskStatus;
use crate::store::TaskStatus as DbStatus;
use crate::store::TaskStore;
use crate::store::store_set_by_id;
use crate::suspend;
use crate::tmux::TmuxManager;
use dashmap::DashMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

#[derive(Debug)]
pub(super) struct StuckTaskTiming {
    pub(super) has_session: bool,
    pub(super) age: chrono::Duration,
    pub(super) threshold: u64,
}

pub(super) fn stuck_task_timing_from_map(
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
    let suspended = suspend::suspended_duration_since(updated);
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
pub(super) async fn finalize_open_runs_for_recovery(
    store: &TaskStore,
    store_id: i64,
    reason: &str,
) {
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
