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
    github_circuit_remaining_secs, is_github_circuit_open, record_silence_detection,
    set_agent_cooldown, set_model_cooldown, SILENCE_AGENT_COOLDOWN_SECS,
    SILENCE_EXTENDED_COOLDOWN_SECS,
};
use crate::engine::dispatch_guard::DispatchGuard;
use crate::engine::jobs;
use crate::engine::router::{get_route_result, RouteResultError, Router};
use crate::engine::runner::{TaskRunner, WeightSignal};
use crate::engine::tasks::{is_internal_id, TaskManager};
use crate::repo_context::REPO_CONTEXT;
use crate::store::TaskStore;
use crate::tmux::TmuxManager;
use dashmap::DashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::SystemTime;
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
use crate::store::{
    review_session_expected, set_review_session_expected, store_set_by_id, store_touch_updated_at,
};

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

async fn stuck_task_timing(
    tmux: &Arc<TmuxManager>,
    repo: &str,
    task_id: &str,
    session_task_id: &str,
    updated_at: &str,
    config: &EngineConfig,
    parse_error_message: &'static str,
) -> Option<StuckTaskTiming> {
    let session_name = tmux.session_name(repo, session_task_id);
    let has_session = tmux.session_is_running(&session_name).await;
    let threshold = if has_session {
        config.stuck_timeout
    } else {
        config.no_session_stuck_timeout
    };

    let updated = match chrono::DateTime::parse_from_rfc3339(updated_at) {
        Ok(dt) => dt.with_timezone(&chrono::Utc),
        Err(e) => {
            tracing::warn!(
                task_id,
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

    for (task_id, session_name) in silent_sessions {
        let use_backend = should_use_backend(&task_id);
        // Look up agent + model from the store so we can cooldown the right model.
        let store_task = match store.resolve_task_id(repo, &task_id).await {
            Ok(Some(store_id)) => match store.get(store_id).await {
                Ok(task) => Some(task),
                Err(e) => {
                    tracing::warn!(task_id, error = %e, "failed to fetch task from store during silence detection");
                    None
                }
            },
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(task_id, error = %e, "failed to resolve task id during silence detection");
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
            set_model_cooldown(&agent_name, &model_name, config.silence_cooldown);
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
            set_agent_cooldown(&agent_name, SILENCE_AGENT_COOLDOWN_SECS);
        }

        // 4. Pick a fallback agent and set to Routed (not New) to preserve progress.
        // Setting to New would clear routing state and trigger a full LLM re-routing
        // cycle, losing intermediate context. Routed skips re-routing and re-dispatches
        // directly with the chosen fallback agent.
        let task_eid = ExternalId(task_id.clone());

        // Build available agents list and reroute chain for failover
        let available: Vec<String> = crate::engine::router::config::DEFAULT_AGENTS
            .iter()
            .filter(|a| crate::cmd_cache::command_exists(a))
            .map(|s| s.to_string())
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
                "silence detection: no fallback agents available, marking needs_review"
            );
            (Status::NeedsReview, None)
        };

        if use_backend {
            if let Some(ref st) = store_task {
                for label in &st.labels {
                    if label.starts_with("agent:")
                        || label.starts_with("complexity:")
                        || label.starts_with("model:")
                    {
                        if let Err(e) = backend.remove_label(&task_eid, label).await {
                            tracing::warn!(task_id, label, error = %e, "failed to remove label during silence detection re-route");
                        }
                    }
                }
            }
        }

        if let Some(store_id) = store_task.as_ref().map(|t| t.id) {
            if next_agent.is_some() {
                store_set_by_id(
                    &Some(Arc::clone(store)),
                    store_id,
                    &[
                        ("agent", serde_json::json!("")),
                        ("model", serde_json::json!("")),
                        (
                            "last_error",
                            serde_json::json!(format!(
                                "silence detected after {}s, clearing agent/model for re-route",
                                config.silence_grace_period
                            )),
                        ),
                    ],
                )
                .await;
            } else {
                store_set_by_id(
                    &Some(Arc::clone(store)),
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
            if let Err(e) = backend.post_comment(&task_eid, &comment).await {
                tracing::warn!(task_id, ?e, "failed to post silence detection comment");
            }
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
        let Some(timing) = stuck_task_timing(
            tmux,
            repo,
            &task.id.0,
            &task.id.0,
            &task.updated_at,
            config,
            "cannot parse updated_at, skipping stuck-task check",
        )
        .await
        else {
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
        // Remove stale agent/model labels so the LLM router re-routes properly
        for label in &task.labels {
            if label.starts_with("agent:") || label.starts_with("model:") {
                backend.remove_label(&task.id, label).await.ok();
            }
        }
        if let Ok(Some(store_id)) = store.resolve_task_id(repo, &task.id.0).await {
            store_set_by_id(
                &Some(Arc::clone(store)),
                store_id,
                &[
                    ("agent", serde_json::Value::Null),
                    ("model", serde_json::Value::Null),
                    ("route_attempts", serde_json::json!(0)),
                ],
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
        let Some(timing) = stuck_task_timing(
            tmux,
            repo,
            &task_id,
            &task_id,
            &task.updated_at,
            config,
            "cannot parse updated_at, skipping stuck internal-task check",
        )
        .await
        else {
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
        // Reset routing state so the LLM router is used on the next attempt
        // (same reset that external tasks perform).
        if let Ok(Some(store_id)) = store.resolve_task_id(repo, &task_id).await {
            store_set_by_id(
                &Some(Arc::clone(store)),
                store_id,
                &[
                    ("agent", serde_json::Value::Null),
                    ("model", serde_json::Value::Null),
                    ("route_attempts", serde_json::json!(0)),
                ],
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
    for task in &in_review {
        if !review_session_expected(store, repo, &task.id.0).await {
            tracing::debug!(
                task_id = task.id.0,
                "in_review task is waiting on PR review, skipping stuck-session recovery"
            );
            continue;
        }

        let review_task_id = format!("{}-review", task.id.0);
        let Some(timing) = stuck_task_timing(
            tmux,
            repo,
            &task.id.0,
            &review_task_id,
            &task.updated_at,
            config,
            "cannot parse updated_at, skipping stuck in_review check",
        )
        .await
        else {
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
        if !review_session_expected(store, repo, &task_id).await {
            tracing::debug!(
                task_id,
                "internal in_review task is waiting on PR review, skipping stuck-session recovery"
            );
            continue;
        }

        let review_task_id = format!("{}-review", task_id);
        let Some(timing) = stuck_task_timing(
            tmux,
            repo,
            &task_id,
            &review_task_id,
            &task.updated_at,
            config,
            "cannot parse updated_at, skipping stuck internal in_review check",
        )
        .await
        else {
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
        router.refresh_health(store).await;
    }

    for task in routable {
        let _task_span = tracing::info_span!("engine.route", task_id = %task.id.0).entered();
        match router.route(task, store, repo).await {
            Ok(result) => {
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
                    }
                } else {
                    // Remove old agent/complexity/model labels to avoid duplicates on re-route
                    for label in &task.labels {
                        if label.starts_with("agent:")
                            || label.starts_with("complexity:")
                            || label.starts_with("model:")
                        {
                            backend.remove_label(&task.id, label).await.ok();
                        }
                    }

                    // Add agent, complexity, and model labels
                    let mut labels = vec![
                        format!("agent:{}", result.agent),
                        format!("complexity:{}", result.complexity),
                    ];
                    if let Some(ref model) = result.model {
                        labels.push(format!("model:{model}"));
                    }
                    if let Err(e) = backend.set_labels(&task.id, &labels).await {
                        tracing::warn!(task_id = task.id.0, ?e, "failed to set routing labels");
                    }

                    // Transition to routed
                    if let Err(e) = task_manager
                        .update_task_status(&task.id, Status::Routed)
                        .await
                    {
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
                            .update_status(store_id, crate::store::TaskStatus::Routed)
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
    router: &Router,
    dispatching: &Arc<DashSet<String>>,
    store: &Arc<TaskStore>,
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

    if dispatchable.is_empty() {
        tracing::debug!(count = 0, "dispatchable tasks found");
    } else {
        tracing::info!(count = dispatchable.len(), "dispatchable tasks found");
    }

    // Check if we are in degraded mode (fewer than threshold healthy agents)
    let threshold = crate::engine::router::config::min_healthy_agents_threshold();
    let healthy_count = router.healthy_agent_count("simple");
    let is_degraded = healthy_count < threshold;

    if is_degraded {
        tracing::warn!(
            healthy_agents = healthy_count,
            threshold = threshold,
            "degraded mode: using sequential dispatch"
        );
    }

    let sequential_delay = if is_degraded {
        crate::engine::router::config::sequential_dispatch_delay_ms()
    } else {
        0
    };

    for (idx, task) in dispatchable.into_iter().enumerate() {
        // In degraded mode, add delay between dispatches to pace the system
        if idx > 0 && is_degraded {
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
        if !dispatching.insert(dispatch_key.clone()) {
            tracing::debug!(
                task_id = task.id.0,
                "task already dispatching, skipping duplicate"
            );
            continue;
        }
        // RAII guard — removes dispatch_key on drop even if the spawned task panics.
        let dispatch_guard = DispatchGuard::new(dispatching.clone(), dispatch_key.clone());

        // Check if already running (has active session)
        let session_name = tmux.session_name(repo, &task.id.0);
        if tmux.session_blocks_dispatch(&session_name).await {
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
                                Ok(Some(store_id)) => store_for_spawn
                                    .get(store_id)
                                    .await
                                    .ok()
                                    .and_then(|t| t.pr_number)
                                    .is_some(),
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
                        // None means the runner guard skipped the task or the task
                        // completed with an unrecognised status string. Only route to
                        // needs_review when the review agent is actually enabled;
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

        // Check if every child is done using the local store (falls back to GitHub API
        // only when the child is not yet in the store).
        let mut all_done = true;
        for child_id in &children {
            if !task_manager.is_child_done(child_id).await {
                all_done = false;
                break;
            }
        }

        if all_done {
            tracing::info!(
                task_id = task.id.0,
                children = children.len(),
                "all children done, unblocking parent"
            );
            if let Err(e) = task_manager.update_task_status(&task.id, Status::New).await {
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
    jobs_path: &std::path::PathBuf,
    router: &mut Router,
    task_manager: &Arc<TaskManager>,
    weight_tx: &mpsc::Sender<WeightSignal>,
    dispatching: &Arc<DashSet<String>>,
    store: &Arc<TaskStore>,
    transport: Option<&Arc<crate::channels::transport::Transport>>,
) -> anyhow::Result<()> {
    let _tick_span = tracing::info_span!("engine.tick").entered();

    // Run startup cleanup once to kill stale sessions from previous runs.
    startup_cleanup(tmux).await;

    tick_check_session_completions(tmux, repo, capture, store).await?;
    tick_detect_silent_agents(tmux, repo, capture, backend, task_manager, config, store).await?;
    tick_recover_stuck_tasks(backend, tmux, repo, task_manager, config, store).await?;
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
        router,
        dispatching,
        store,
    )
    .await?;
    tick_unblock_parents(backend, task_manager).await?;
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
            .create_internal("owner/repo", "Internal InReview", "", "cron", "1")
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
            .create_internal("owner/repo", "Silent internal", "", "cron", "1")
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
        // otherwise → NeedsReview. Either is acceptable; the key invariant is that the
        // task is no longer InProgress.
        assert!(
            matches!(
                task.status,
                crate::store::TaskStatus::Routed | crate::store::TaskStatus::NeedsReview
            ),
            "internal task should be routed to fallback or needs_review, got {:?}",
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
            .create_internal("owner/repo", "Completed task", "", "cron", "1")
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

        let config = EngineConfig {
            no_session_stuck_timeout: 600,
            stuck_timeout: 1800,
            ..EngineConfig::default()
        };
        let updated_at = (chrono::Utc::now()
            - chrono::Duration::seconds(config.no_session_stuck_timeout as i64 + 5))
        .to_rfc3339();

        let timing = stuck_task_timing(
            &tmux,
            repo,
            task_id,
            task_id,
            &updated_at,
            &config,
            "parse failure",
        )
        .await
        .expect("dead session should be treated as missing");

        assert!(!timing.has_session);
        assert_eq!(timing.threshold, config.no_session_stuck_timeout);

        let _ = tmux.kill_session(&session_name).await;
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
        let dispatching = Arc::new(DashSet::new());
        let transport = Arc::new(Transport::new());
        let capture = Arc::new(CaptureService::new(transport));

        let router_config = crate::engine::router::RouterConfig::default();
        let router = Router::new(router_config);
        let router_arc = Arc::new(RwLock::new(router));

        // Simulate what the main loop does: hold a write lock, then call
        // tick_dispatch_tasks. The function now takes &Router (dereferenced
        // from the guard), so no lock re-acquisition occurs.
        let write_guard = router_arc.write().await;

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
                &write_guard,
                &dispatching,
                &store,
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

        // Now run phase2 — the updated_at should have been touched so no reclaim occurs
        tick_recover_stuck_tasks(
            &backend,
            &tmux,
            "owner/repo",
            &task_manager,
            &config,
            &store,
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
}
