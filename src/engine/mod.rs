//! Engine — the core orchestration loop.
//!
//! The engine is the central coordinator. It owns the tick loop, backend
//! connections, channel registry, transport layer, and tmux session manager.
//!
//! Submodules handle the heavy lifting:
//! - [`tick`] — core tick phases (session polling, stuck recovery, routing, dispatch)
//! - [`sync`] — periodic sync operations (cleanup, PR review, mentions, skills)
//! - [`review`] — PR review pipeline (review agent, auto-merge, change handling)
//! - [`cleanup`] — worktree cleanup and merged-PR detection
//!
//! This file contains struct definitions, project initialization, and the
//! main event loop (`serve()`).

pub mod auto_merge;
pub mod channel_handler;
pub mod cleanup;
pub mod commands;
pub mod cooldown;
pub mod dependencies;
pub mod dispatch_guard;
pub mod events;
pub mod jobs;
pub mod relay;
pub mod review;
pub mod review_gate;
pub mod review_poll;
pub mod router;
pub mod runner;
pub mod subscribers;
pub mod suspend;
pub mod sync;
pub mod tasks;
pub mod tick;

/// Standard Orch attribution footer for issue bodies, PR bodies, and comments.
///
/// Append to every user-visible string posted to GitHub so activity is
/// clearly attributed to orch rather than appearing as a human post.
pub fn orch_footer() -> &'static str {
    "\n\n---\n*Posted by [Orch](https://github.com/gabrielkoerich/orch)*"
}

/// Build a standard attribution footer for GitHub comments posted by orch bots.
///
/// `verb` is "Created", "Reviewed", or "Commented" depending on context.
pub fn attribution_footer(verb: &str, agent: &str, model: Option<&str>) -> String {
    match model {
        Some(m) => format!(
            "\n\n---\n*{} by {}[bot] via [Orch](https://github.com/gabrielkoerich/orch) using `{}`*",
            verb, agent, m
        ),
        None => format!(
            "\n\n---\n*{} by {}[bot] via [Orch](https://github.com/gabrielkoerich/orch)*",
            verb, agent
        ),
    }
}

use crate::backends::{ExternalBackend, ExternalId};
use crate::channels::capture::CaptureService;
use crate::channels::discord_ws::DiscordGateway;
use crate::channels::github::start_webhook_server;
use crate::channels::notification::NotificationLevel;
use crate::channels::routing::{ChannelRouter, GlobalChannelConfig, ProjectChannelConfig};
use crate::channels::slack::SlackChannel;
use crate::channels::telegram::TelegramChannel;
use crate::channels::tmux::TmuxChannel;
use crate::channels::transport::Transport;
use crate::channels::{Channel, ChannelRegistry, IncomingMessage, OutgoingMessage};
use crate::cmd::CommandErrorContext;
use crate::config;
use crate::engine::cleanup::remove_worktree_and_branch;
use crate::engine::router::{Router, RouterConfig};
use crate::engine::runner::worktree::{
    abort_worktree_rebase, list_project_worktrees, rebase_worktree_on_origin_main,
    task_id_from_worktree_name, validate_worktree_gitdir,
};
use crate::engine::tasks::TaskManager;
use crate::github::http::{rate_limit_metrics, GhHttp};
use crate::repo_context::REPO_CONTEXT;
use crate::store::{TaskStatus, TaskStore};
use crate::tmux::TmuxManager;
use runner::WeightSignal;
// AtomicBool/Ordering removed — shutdown is now immediate (reset tasks + break)
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::{mpsc, Notify, RwLock, Semaphore};

use crate::backends::Status;

/// Lightweight reference tuple for channel message handling.
type EngineRef = (
    String,
    Arc<dyn ExternalBackend>,
    Arc<TaskManager>,
    Option<Arc<TaskStore>>,
);

/// Per-project engine state.
///
/// Each project has its own backend, task runner, and task manager,
/// but they share the global tmux manager, transport, and semaphore.
#[derive(Clone)]
pub struct ProjectEngine {
    pub repo: String,
    pub project_dir: std::path::PathBuf,
    pub backend: Arc<dyn ExternalBackend>,
    pub task_manager: Arc<TaskManager>,
    pub runner: Arc<runner::TaskRunner>,
    pub store: Arc<TaskStore>,
}

/// A restart ends every review session, so an `in_review` task with no session has no reviewer.
/// Hand it back to `needs_review` at start, so it does not wait for the stuck-review recovery
pub(crate) async fn reset_orphaned_reviews(
    task_manager: &TaskManager,
    store: &Arc<TaskStore>,
    repo: &str,
    tmux: &TmuxManager,
) -> usize {
    // The store holds external and internal tasks, the second list is the legacy internal table
    let mut ids: Vec<ExternalId> = store
        .list_by_status(repo, crate::store::TaskStatus::InReview)
        .await
        .unwrap_or_default()
        .iter()
        .map(|t| crate::engine::tasks::store_task_to_external(t).id)
        .collect();
    if let Ok(legacy) = task_manager
        .list_internal_by_status(crate::store::TaskStatus::InReview)
        .await
    {
        ids.extend(legacy.into_iter().map(|t| t.id));
    }
    ids.sort_by(|a, b| a.0.cmp(&b.0));
    ids.dedup_by(|a, b| a.0 == b.0);

    let mut reset = 0;
    for id in &ids {
        let session = tmux.session_name(repo, &format!("{}-review", id.0));

        if tmux.session_exists(&session).await {
            continue;
        }

        if let Err(e) = task_manager
            .update_task_status(id, Status::NeedsReview)
            .await
        {
            tracing::warn!(
                task_id = id.0,
                ?e,
                "failed to reset an orphaned review at start"
            );
            continue;
        }

        if let Ok(Some(store_id)) = store.resolve_task_id(repo, &id.0).await {
            let reason = "startup: review session gone, reset to needs_review";

            if let Err(e) = store
                .finalize_incomplete_runs(store_id, "aborted", reason)
                .await
            {
                tracing::warn!(
                    task_id = id.0,
                    ?e,
                    "failed to finalize the orphaned review's runs"
                );
            }
        }

        reset += 1;
    }

    reset
}

/// Starts the relay client when `relay.url` is set. The token is read in the background, so a
/// passbox prompt nobody answers never holds up the engine
fn start_relay_client(project_engines: &[ProjectEngine], notify: &Arc<Notify>) {
    let url = crate::config::get("relay.url").unwrap_or_default();
    let Some(store) = project_engines.first().map(|e| Arc::clone(&e.store)) else {
        return;
    };

    if url.is_empty() {
        return;
    }

    let token_ref = crate::config::get("relay.token").unwrap_or_default();
    let notify = Arc::clone(notify);
    tokio::spawn(async move {
        let mut warned = false;
        let token = loop {
            let reference = token_ref.clone();
            let read = tokio::task::spawn_blocking(move || crate::secret_ref::resolve(&reference))
                .await
                .map_err(anyhow::Error::from)
                .and_then(|r| r);

            match read {
                Ok(token) if !token.is_empty() => break token,
                Ok(_) => {
                    tracing::warn!("relay.token is empty, the relay client stays off");
                    return;
                }
                Err(e) if !warned => {
                    tracing::warn!(err = %e, "could not read relay.token, retrying every 5 minutes");
                    warned = true;
                }
                Err(_) => {}
            }

            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
        };

        relay::run(url, token, store, notify).await;
    });
}

/**
 * Runs `sync_tick` for every project at once, then the tail that is the same for all of them,
 * in a spawned task. Sync time is the slowest project instead of the sum of all of them
 * (#3864). The caller must already hold `sync_in_progress`; this releases it when done
 **/
#[allow(clippy::too_many_arguments)]
fn spawn_sync_pass(
    project_engines: &[ProjectEngine],
    tmux: &Arc<TmuxManager>,
    engine_config: &EngineConfig,
    router: &Arc<RwLock<Router>>,
    dispatching: &Arc<dashmap::DashMap<String, String>>,
    auto_merge_in_flight: &Arc<dashmap::DashSet<String>>,
    sync_in_progress: &Arc<std::sync::atomic::AtomicBool>,
    trigger: &'static str,
) {
    let engines = project_engines.to_vec();
    let tmux = Arc::clone(tmux);
    let engine_config = engine_config.clone();
    let router = Arc::clone(router);
    let dispatching = Arc::clone(dispatching);
    let auto_merge_in_flight = Arc::clone(auto_merge_in_flight);
    let guard = Arc::clone(sync_in_progress);

    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let mut passes = Vec::with_capacity(engines.len());
        for engine in &engines {
            passes.push(REPO_CONTEXT.scope(
                engine.repo.clone(),
                sync_one_project(
                    engine,
                    &tmux,
                    &engine_config,
                    &router,
                    &dispatching,
                    &auto_merge_in_flight,
                ),
            ));
        }
        futures::future::join_all(passes).await;

        sync::sync_shared_tail(&router, engines.first().map(|e| &e.store)).await;
        tracing::info!(
            trigger,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "sync tick complete"
        );
        guard.store(false, std::sync::atomic::Ordering::Release);
    });
}

async fn sync_one_project(
    engine: &ProjectEngine,
    tmux: &Arc<TmuxManager>,
    engine_config: &EngineConfig,
    router: &Arc<RwLock<Router>>,
    dispatching: &Arc<dashmap::DashMap<String, String>>,
    auto_merge_in_flight: &Arc<dashmap::DashSet<String>>,
) {
    if let Err(e) = sync::sync_tick(
        &engine.backend,
        tmux,
        &engine.repo,
        engine_config,
        router,
        &engine.task_manager,
        &engine.store,
        dispatching,
        auto_merge_in_flight,
    )
    .await
    {
        tracing::error!(repo = %engine.repo, ?e, "sync tick failed for project");
    }
}

/// Milliseconds a group of tick or sync phases may take before it is worth an operator's eye
const SLOW_PHASE_GROUP_MS: u128 = 1_000;

/**
 * Logs how long each phase of a tick or sync group took, for one project. Tick and sync time
 * grows with the project count, and the totals alone did not say which project or phase spent
 * it (#3864). Info once a group crosses `SLOW_PHASE_GROUP_MS`, so a quiet install stays quiet
 **/
pub(crate) fn log_phase_timings(group: &str, repo: &str, phases: &[(&str, u128)]) {
    let total_ms: u128 = phases.iter().map(|(_, ms)| ms).sum();
    if total_ms >= SLOW_PHASE_GROUP_MS {
        tracing::info!(group, repo, total_ms, ?phases, "phase timings");
    } else {
        tracing::debug!(group, repo, total_ms, ?phases, "phase timings");
    }
}

/// Return the configured agent list from top-level `agents` in config.yml,
/// falling back to [`router::config::DEFAULT_AGENTS`] when not set.
///
/// This is the canonical source of truth for "which agents does orch know
/// about" — use it instead of `DEFAULT_AGENTS` directly so that
/// Claude-compatible agents added via config (e.g. `olm`) are recognized
/// without code changes.
///
/// The key is top-level (`agents:`) rather than nested (`engine.agents:`)
/// because `config::get` serializes nested YAML arrays via `serde_norway::to_string`
/// which produces YAML block format, not JSON — breaking downstream parsing.
pub fn configured_agents() -> Vec<String> {
    if let Ok(agents_str) = crate::config::get("agents") {
        if !agents_str.is_empty() && agents_str != "[]" {
            // config::get returns serde_norway::to_string for arrays, which is
            // YAML block format ("- claude\n- codex\n..."). Parse as YAML first.
            if let Ok(agents_arr) = serde_norway::from_str::<Vec<String>>(&agents_str) {
                if !agents_arr.is_empty() {
                    return agents_arr;
                }
            }
            // Fallback: try JSON (inline arrays) or comma-separated
            if let Ok(agents_arr) = serde_json::from_str::<Vec<String>>(&agents_str) {
                if !agents_arr.is_empty() {
                    return agents_arr;
                }
            }
        }
    }
    router::config::DEFAULT_AGENTS
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Engine configuration.
#[derive(Clone)]
pub struct EngineConfig {
    /// Main tick interval
    pub tick_interval: std::time::Duration,
    /// GitHub sync interval (cleanup, PR review, mentions)
    pub sync_interval: std::time::Duration,
    /// Webhook health check interval (seconds)
    pub webhook_health_check_interval: Option<std::time::Duration>,
    /// Maximum parallel task executions
    pub max_parallel: usize,
    /// Stuck task timeout for tasks with an active tmux session (seconds)
    pub stuck_timeout: u64,
    /// Stuck task timeout for tasks with no active tmux session (seconds).
    /// Shorter than `stuck_timeout` because no session means the agent has already exited.
    pub no_session_stuck_timeout: u64,
    /// Stuck task timeout for in_review tasks with no active tmux session (seconds).
    /// Longer than `no_session_stuck_timeout` because a review agent ends its tmux session on
    /// normal completion before its result reaches the engine. A shorter limit resets a task
    /// whose finished review is still being delivered, and the result is dropped as stale.
    /// `from_config` keeps it above the review agent's own timeout plus
    /// `review::REVIEW_RESULT_DELIVERY_SECS`.
    pub in_review_no_session_stuck_timeout: u64,
    /// Auto-create follow-up tasks when PR reviews request changes
    pub auto_create_followup_on_changes: bool,
    /// Graceful shutdown timeout — how long to wait for running agents before exiting.
    pub graceful_shutdown_timeout: std::time::Duration,
    /// Grace period before silence detection kicks in (seconds).
    /// Allows time for agent startup, model download, API handshake.
    pub silence_grace_period: u64,
    /// Cooldown duration for a model detected as silent (seconds).
    pub silence_cooldown: u64,
    /// How often to check for a newer orch release and notify channels (seconds).
    /// Set to 0 to disable. Default: 3600 (1 hour).
    pub upgrade_check_interval: u64,
    /// Seconds of continuous startup failure (GitHub unreachable / auth errors)
    /// before the engine escalates from WARN to ERROR-level logging and sends a
    /// one-time push notification to configured channels. Set to 0 to disable.
    /// Default: 3600 (1 hour).
    pub startup_failure_escalation_secs: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            tick_interval: std::time::Duration::from_secs(10),
            sync_interval: std::time::Duration::from_secs(45),
            webhook_health_check_interval: Some(std::time::Duration::from_secs(60)),
            max_parallel: 4,
            stuck_timeout: 1800,
            no_session_stuck_timeout: 600,
            in_review_no_session_stuck_timeout: 1920,
            auto_create_followup_on_changes: true,
            graceful_shutdown_timeout: std::time::Duration::from_secs(600),
            silence_grace_period: 300,
            silence_cooldown: 3600,
            upgrade_check_interval: 3600,
            startup_failure_escalation_secs: 3600,
        }
    }
}

impl EngineConfig {
    /// Load engine configuration from config files, falling back to defaults.
    pub fn from_config() -> Self {
        let mut config = Self::default();

        if let Ok(val) = crate::config::get("engine.tick_interval") {
            if let Ok(secs) = val.parse::<u64>() {
                config.tick_interval = std::time::Duration::from_secs(secs);
            } else {
                tracing::warn!(key = "engine.tick_interval", value = %val, default_secs = config.tick_interval.as_secs(), "invalid value for engine.tick_interval, using default");
            }
        }

        if let Ok(val) = crate::config::get("engine.sync_interval") {
            if let Ok(secs) = val.parse::<u64>() {
                config.sync_interval = std::time::Duration::from_secs(secs);
            } else {
                tracing::warn!(key = "engine.sync_interval", value = %val, default_secs = config.sync_interval.as_secs(), "invalid value for engine.sync_interval, using default");
            }
        }

        if let Ok(val) = crate::config::get("engine.max_parallel") {
            if let Ok(n) = val.parse::<usize>() {
                config.max_parallel = n;
            } else {
                tracing::warn!(key = "engine.max_parallel", value = %val, default = config.max_parallel, "invalid value for engine.max_parallel, using default");
            }
        }

        if let Ok(val) = crate::config::get("engine.stuck_timeout") {
            if let Ok(secs) = val.parse::<u64>() {
                config.stuck_timeout = secs;
            } else {
                tracing::warn!(key = "engine.stuck_timeout", value = %val, default_secs = config.stuck_timeout, "invalid value for engine.stuck_timeout, using default");
            }
        }

        if let Ok(val) = crate::config::get("engine.no_session_stuck_timeout") {
            if let Ok(secs) = val.parse::<u64>() {
                config.no_session_stuck_timeout = secs;
            } else {
                tracing::warn!(key = "engine.no_session_stuck_timeout", value = %val, default_secs = config.no_session_stuck_timeout, "invalid value for engine.no_session_stuck_timeout, using default");
            }
        }

        let mut in_review_from_config = false;
        if let Ok(val) = crate::config::get("engine.in_review_no_session_stuck_timeout") {
            if let Ok(secs) = val.parse::<u64>() {
                config.in_review_no_session_stuck_timeout = secs;
                in_review_from_config = true;
            } else {
                tracing::warn!(key = "engine.in_review_no_session_stuck_timeout", value = %val, default_secs = config.in_review_no_session_stuck_timeout, "invalid value for engine.in_review_no_session_stuck_timeout, using default");
            }
        }

        // Keep the no-session review limit over the review agent's own timeout plus delivery
        // time, whatever either is set to. Under that, recovery resets a task whose finished
        // review is still on its way to the engine and the result is dropped as stale
        let review_timeout = crate::engine::review::review_timeout_seconds();
        let floored = crate::engine::review::no_session_review_floor(
            config.in_review_no_session_stuck_timeout,
            review_timeout,
        );
        if floored != config.in_review_no_session_stuck_timeout {
            if in_review_from_config {
                tracing::warn!(
                    configured_secs = config.in_review_no_session_stuck_timeout,
                    review_timeout_secs = review_timeout,
                    raised_to_secs = floored,
                    "engine.in_review_no_session_stuck_timeout is below the review timeout plus result delivery time, raising it"
                );
            } else {
                tracing::debug!(
                    review_timeout_secs = review_timeout,
                    raised_to_secs = floored,
                    "raised the default in_review no-session limit over a long review timeout"
                );
            }
            config.in_review_no_session_stuck_timeout = floored;
        }

        if let Ok(val) = crate::config::get("engine.webhook_health_check_interval") {
            if let Ok(secs) = val.parse::<u64>() {
                if secs > 0 {
                    config.webhook_health_check_interval =
                        Some(std::time::Duration::from_secs(secs));
                } else {
                    config.webhook_health_check_interval = None;
                }
            } else {
                tracing::warn!(key = "engine.webhook_health_check_interval", value = %val, default = ?config.webhook_health_check_interval, "invalid value for engine.webhook_health_check_interval, using default");
            }
        }

        if let Ok(val) = crate::config::get("workflow.auto_create_followup_on_changes") {
            config.auto_create_followup_on_changes = !val.eq_ignore_ascii_case("false");
        }

        if let Ok(val) = crate::config::get("engine.graceful_shutdown_timeout") {
            if let Ok(secs) = val.parse::<u64>() {
                config.graceful_shutdown_timeout = std::time::Duration::from_secs(secs);
            } else {
                tracing::warn!(key = "engine.graceful_shutdown_timeout", value = %val, default_secs = config.graceful_shutdown_timeout.as_secs(), "invalid value for engine.graceful_shutdown_timeout, using default");
            }
        }

        if let Ok(val) = crate::config::get("engine.silence_grace_period") {
            if let Ok(secs) = val.parse::<u64>() {
                config.silence_grace_period = secs;
            } else {
                tracing::warn!(key = "engine.silence_grace_period", value = %val, default = config.silence_grace_period, "invalid value for engine.silence_grace_period, using default");
            }
        }

        if let Ok(val) = crate::config::get("engine.silence_cooldown") {
            if let Ok(secs) = val.parse::<u64>() {
                config.silence_cooldown = secs;
            } else {
                tracing::warn!(key = "engine.silence_cooldown", value = %val, default = config.silence_cooldown, "invalid value for engine.silence_cooldown, using default");
            }
        }

        if let Ok(val) = crate::config::get("engine.upgrade_check_interval") {
            if let Ok(secs) = val.parse::<u64>() {
                config.upgrade_check_interval = secs;
            } else {
                tracing::warn!(key = "engine.upgrade_check_interval", value = %val, default = config.upgrade_check_interval, "invalid value for engine.upgrade_check_interval, using default");
            }
        }

        if let Ok(val) = crate::config::get("engine.startup_failure_escalation_secs") {
            if let Ok(secs) = val.parse::<u64>() {
                config.startup_failure_escalation_secs = secs;
            } else {
                tracing::warn!(
                    key = "engine.startup_failure_escalation_secs",
                    value = %val,
                    default_secs = config.startup_failure_escalation_secs,
                    "invalid value for engine.startup_failure_escalation_secs, using default"
                );
            }
        }

        config
    }
}

/// KV key holding the epoch second the GitHub core window resets, so `orch task status`
/// can show the limit from a separate process.
pub const RATE_LIMITED_UNTIL_KEY: &str = "github:rate_limited_until";

/// Format an epoch second as a local `HH:MM:SS` for operator-facing output.
pub fn format_rate_limited_until(reset_at: i64) -> String {
    chrono::DateTime::from_timestamp(reset_at, 0)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%H:%M:%S %Z")
                .to_string()
        })
        .unwrap_or_else(|| reset_at.to_string())
}

/// Announce a GitHub rate-limit window on every operator surface: the log (`orch log`),
/// a KV flag (`orch task status`) and the channels (Telegram, Discord).
async fn report_rate_limited(
    reset_at: i64,
    store: Option<&Arc<TaskStore>>,
    transport: Option<&Arc<crate::channels::transport::Transport>>,
) {
    let m = rate_limit_metrics();
    let until = format_rate_limited_until(reset_at);
    tracing::warn!(
        reset_at,
        rate_limit_hits = m.hits,
        skipped_calls = m.skipped_calls,
        five_xx_hits = m.five_xx_hits,
        rest_remaining = ?m.rest_remaining,
        graphql_remaining = ?m.graphql_remaining,
        "GitHub API rate limited until {until} — skipping GitHub calls, local work continues"
    );

    if let Some(store) = store {
        if let Err(e) = store
            .kv_set(RATE_LIMITED_UNTIL_KEY, &reset_at.to_string())
            .await
        {
            tracing::warn!(err = %e, "failed to persist GitHub rate-limit flag");
        }
    }

    if let Some(transport) = transport {
        transport.push_notification(crate::channels::notification::TaskNotification {
            task_id: "github".to_string(),
            title: "GitHub rate limit".to_string(),
            status: "blocked".to_string(),
            agent: "orch".to_string(),
            duration_seconds: 0.0,
            summary: format!(
                "rate limited until {until}. GitHub calls are skipped until then; running agents and local work continue."
            ),
            repo: None,
            notify_target: None,
            pr_number: None,
        });
    }
}

/// Grow or shrink the dispatch pool toward `target` without cancelling running work.
///
/// Growing adds permits at once. Shrinking can only take back the permits that
/// are free right now, so the return value is the limit actually in force; call
/// again as running tasks release theirs
fn resize_dispatch_pool(semaphore: &Semaphore, in_force: usize, target: usize) -> usize {
    if target > in_force {
        semaphore.add_permits(target - in_force);
        return target;
    }

    if target < in_force {
        return in_force - semaphore.forget_permits(in_force - target);
    }

    in_force
}

/// Config subtrees the engine only reads at startup: the channel gateways hold
/// long-lived websockets and the webhook listener binds its port once.
const RESTART_ONLY_KEYS: [&str; 3] = ["channels", "webhook", "relay"];

/// Read the restart-only subtrees so a later reload can tell they changed.
fn restart_only_snapshot() -> Vec<Option<String>> {
    RESTART_ONLY_KEYS
        .iter()
        .map(|key| crate::config::get(key).ok())
        .collect()
}

/// Report the restart-only keys an edit changed. Only the key is logged, never
/// the value, because `channels.*` holds bot tokens
fn log_restart_only_changes(old: &[Option<String>], new: &[Option<String>]) -> Vec<&'static str> {
    let changed: Vec<&'static str> = RESTART_ONLY_KEYS
        .iter()
        .zip(old)
        .zip(new)
        .filter(|((_, old), new)| old != new)
        .map(|((key, _), _)| *key)
        .collect();

    for key in &changed {
        tracing::info!(key = key, "config changed, takes effect after restart");
    }

    changed
}

/// Log every engine setting whose value changed across a config reload, so
/// `orch log` shows what the operator's edit actually did.
fn log_engine_config_changes(old: &EngineConfig, new: &EngineConfig) {
    macro_rules! log_changed {
        ($($key:literal => $field:ident),+ $(,)?) => {
            $(if old.$field != new.$field {
                tracing::info!(key = $key, old = ?old.$field, new = ?new.$field, "config changed");
            })+
        };
    }

    log_changed!(
        "engine.tick_interval" => tick_interval,
        "engine.sync_interval" => sync_interval,
        "engine.webhook_health_check_interval" => webhook_health_check_interval,
        "engine.max_parallel" => max_parallel,
        "engine.stuck_timeout" => stuck_timeout,
        "engine.no_session_stuck_timeout" => no_session_stuck_timeout,
        "engine.in_review_no_session_stuck_timeout" => in_review_no_session_stuck_timeout,
        "engine.graceful_shutdown_timeout" => graceful_shutdown_timeout,
        "engine.silence_grace_period" => silence_grace_period,
        "engine.silence_cooldown" => silence_cooldown,
        "engine.upgrade_check_interval" => upgrade_check_interval,
        "engine.startup_failure_escalation_secs" => startup_failure_escalation_secs,
        "workflow.auto_create_followup_on_changes" => auto_create_followup_on_changes,
    );
}

fn should_run_upgrade_check(
    last_upgrade_check: Option<std::time::Instant>,
    upgrade_check_interval: u64,
) -> bool {
    if upgrade_check_interval == 0 {
        return false;
    }

    last_upgrade_check
        .map(|last| last.elapsed() >= std::time::Duration::from_secs(upgrade_check_interval))
        .unwrap_or(true)
}

/// Connect one project's backend and build its task manager and runner over
/// the shared store.
///
/// `event_tx` wires status transitions into the event bus; pass `None` during
/// startup, where the bus does not exist yet and task managers are rebuilt
/// once it does
async fn connect_project_engine(
    repo: String,
    project_dir: std::path::PathBuf,
    store: Arc<TaskStore>,
    event_tx: Option<tokio::sync::broadcast::Sender<events::TaskEvent>>,
) -> anyhow::Result<ProjectEngine> {
    let backend: Arc<dyn ExternalBackend> =
        Arc::new(crate::backends::github::GitHubBackend::new(repo.clone())?);

    // A failed health check (rate limit, network, auth) must not drop the project: it would have
    // no review subscriber until the next restart. Later ticks hit the API again and retry on
    // their own, so build the engine and warn while the check keeps failing.
    match backend.health_check().await {
        Ok(()) => tracing::info!(repo = %repo, backend = backend.name(), "backend connected"),
        Err(e) => tracing::warn!(
            repo = %repo,
            error = %e,
            "GitHub health check failed for {repo}, starting engine anyway (rate limited, offline or bad credentials?)"
        ),
    }

    let task_manager = Arc::new(match event_tx {
        Some(tx) => TaskManager::with_events(backend.clone(), store.clone(), repo.clone(), tx),
        None => TaskManager::with_store(backend.clone(), store.clone(), repo.clone()),
    });
    let runner = Arc::new(runner::TaskRunner::new(repo.clone()).with_store(store.clone()));

    Ok(ProjectEngine {
        repo,
        project_dir,
        backend,
        task_manager,
        runner,
        store,
    })
}

/**
 * Split the configured project list into the ones not running yet and the names of the ones that
 * went away. Kept separate from `sync_project_engines` so the comparison can be tested without a
 * GitHub backend: getting it wrong either spawns a second set of subscribers for a project already
 * running, or silently never picks a new one up
 **/
fn diff_configured_projects(
    configured: &[(String, std::path::PathBuf)],
    active_repos: &std::collections::HashSet<String>,
) -> (Vec<(String, std::path::PathBuf)>, Vec<String>) {
    let configured_repos: std::collections::HashSet<&str> =
        configured.iter().map(|(r, _)| r.as_str()).collect();

    let added = configured
        .iter()
        .filter(|(repo, _)| !active_repos.contains(repo))
        .cloned()
        .collect();
    let removed = active_repos
        .iter()
        .filter(|repo| !configured_repos.contains(repo.as_str()))
        .cloned()
        .collect();

    (added, removed)
}

/// Pick up projects added to `projects:` since startup, and report the ones
/// that were removed.
///
/// A removed project keeps running until the next restart: its subscriber
/// tasks and any agent it already dispatched outlive the config read
#[allow(clippy::too_many_arguments)]
async fn sync_project_engines(
    project_engines: &mut Vec<ProjectEngine>,
    active_repos: &mut std::collections::HashSet<String>,
    event_bus: &events::EventBus,
    tmux: &Arc<TmuxManager>,
    capture: &Arc<CaptureService>,
    semaphore: &Arc<Semaphore>,
    weight_tx: &mpsc::Sender<WeightSignal>,
    router: &Arc<RwLock<Router>>,
    dispatching: &Arc<dashmap::DashMap<String, String>>,
) {
    let configured = match config::get_projects_with_paths() {
        Ok(p) if !p.is_empty() => p,
        Ok(_) => return,
        Err(e) => {
            tracing::warn!(err = %e, "could not re-read the project list, keeping the current one");
            return;
        }
    };
    let Some(store) = project_engines.first().map(|e| e.store.clone()) else {
        return;
    };

    let (added, removed) = diff_configured_projects(&configured, active_repos);
    for repo in removed {
        tracing::info!(
            repo = %repo,
            "project removed from config, takes effect after restart"
        );
    }

    for (repo, project_dir) in added {
        config::watch(&project_dir.join(".orch.yml"));
        let engine = match connect_project_engine(
            repo.clone(),
            project_dir,
            store.clone(),
            Some(event_bus.sender()),
        )
        .await
        {
            Ok(engine) => engine,
            Err(e) => {
                // A failed health check no longer lands here, so this is a backend that could
                // not be built at all, such as a repo slug that is not `owner/name`
                tracing::warn!(
                    repo = %repo,
                    err = %e,
                    "project added to config but its backend could not be built, retrying on the next config change"
                );
                continue;
            }
        };

        subscribers::dispatch::spawn(
            event_bus.subscribe(),
            engine.backend.clone(),
            tmux.clone(),
            engine.runner.clone(),
            capture.clone(),
            semaphore.clone(),
            engine.task_manager.clone(),
            weight_tx.clone(),
            router.clone(),
            dispatching.clone(),
            engine.store.clone(),
            engine.repo.clone(),
        );
        subscribers::review::spawn(
            event_bus.subscribe(),
            engine.backend.clone(),
            tmux.clone(),
            semaphore.clone(),
            engine.task_manager.clone(),
            router.clone(),
            dispatching.clone(),
            engine.store.clone(),
            engine.repo.clone(),
        );
        subscribers::unblock::spawn(
            event_bus.subscribe(),
            engine.backend.clone(),
            engine.task_manager.clone(),
            engine.store.clone(),
            engine.repo.clone(),
        );

        active_repos.insert(repo.clone());
        project_engines.push(engine);
        tracing::info!(repo = %repo, "project added from config, now syncing");
    }
}

/// Initialize all project engines from config.
///
/// Returns a vector of ProjectEngine, one for each configured project.
async fn init_project_engines() -> anyhow::Result<Vec<ProjectEngine>> {
    let projects = config::get_projects_with_paths()?;
    let repos: Vec<&str> = projects.iter().map(|(r, _)| r.as_str()).collect();
    tracing::info!(repos = ?repos, "loading projects from config");

    if projects.is_empty() {
        let config_path = crate::home::config_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "~/.orch/config.yml".to_string());
        anyhow::bail!(
            "no projects configured — add repos under `projects:` in {}",
            config_path
        );
    }

    let mut engines = Vec::new();

    // Initialize unified task store once — all project engines share the same SQLite file.
    // Creating one pool here avoids N separate connection pools for the same database.
    let store = Arc::new(TaskStore::open(&crate::store::default_db_path().await?).await?);
    if crate::cluster::enabled() {
        crate::cluster::start(&store, &crate::home::orch_home()?.join("raft.db")).await?;
        crate::cluster::local::serve((*store).clone()).await?;
    }

    // Load persisted model cooldowns and register store for future writes.
    // Runs once here so the global cooldown store is not overwritten per project.
    crate::engine::cooldown::init_cooldown_store(store.clone()).await;

    for (repo, project_dir) in projects {
        tracing::info!(repo = %repo, "initializing project engine");
        config::watch(&project_dir.join(".orch.yml"));

        engines.push(connect_project_engine(repo, project_dir, store.clone(), None).await?);
    }

    Ok(engines)
}

async fn reconcile_startup_worktrees(project_engines: &[ProjectEngine]) -> anyhow::Result<()> {
    for engine in project_engines {
        let repo_root =
            crate::engine::runner::worktree::resolve_main_repo(&engine.project_dir).await;
        let repo_root_path = std::path::PathBuf::from(&repo_root);
        let default_branch =
            crate::engine::runner::worktree::detect_default_branch(&repo_root_path).await;
        let worktrees = list_project_worktrees(&repo_root_path).await?;

        // Fetch refs once for all worktrees in this project, so rebase below
        // operates on up-to-date origin/* refs without N sequential fetches.
        if let Err(e) = crate::engine::runner::git_ops::git()
            .await
            .args(["-C", &repo_root.to_string_lossy(), "fetch", "origin"])
            .output_with_context()
            .await
        {
            tracing::warn!(repo = %engine.repo, err = %e, "startup git fetch origin failed, rebases may use stale refs");
        }

        let mut orphans_removed: u32 = 0;
        let mut invalid_removed: u32 = 0;
        let mut cleaned: u32 = 0;
        let mut valid_kept: u32 = 0;

        for worktree_dir in worktrees {
            let Some(name) = worktree_dir.file_name().and_then(|n| n.to_str()) else {
                continue;
            };

            // Validate gitdir link before doing anything else — a broken .git file
            // means git commands on this worktree will fail with "not a git repository".
            if !validate_worktree_gitdir(&worktree_dir).await {
                tracing::warn!(repo = %engine.repo, worktree = %worktree_dir.display(), "worktree has invalid or missing gitdir, removing");
                remove_worktree_and_branch(name, &worktree_dir, Some(name), &repo_root_path, false)
                    .await;
                invalid_removed += 1;
                continue;
            }

            let Some(task_id) = task_id_from_worktree_name(name) else {
                tracing::info!(repo = %engine.repo, worktree = %worktree_dir.display(), "orphan worktree without task id, removing");
                remove_worktree_and_branch(name, &worktree_dir, Some(name), &repo_root_path, false)
                    .await;
                orphans_removed += 1;
                continue;
            };

            let Some(store_id) = engine.store.resolve_task_id(&engine.repo, &task_id).await? else {
                tracing::info!(repo = %engine.repo, task_id = %task_id, worktree = %worktree_dir.display(), "worktree has no matching task, removing");
                remove_worktree_and_branch(
                    &task_id,
                    &worktree_dir,
                    Some(name),
                    &repo_root_path,
                    false,
                )
                .await;
                orphans_removed += 1;
                continue;
            };

            let task = engine.store.get(store_id).await?;
            let branch_name = if task.branch.is_empty() {
                name
            } else {
                task.branch.as_str()
            };

            match task.status {
                TaskStatus::New
                | TaskStatus::Routed
                | TaskStatus::InProgress
                | TaskStatus::NeedsReview
                | TaskStatus::InReview => {
                    tracing::info!(repo = %engine.repo, task_id = %task_id, worktree = %worktree_dir.display(), "rebasing startup worktree");
                    if let Err(e) =
                        rebase_worktree_on_origin_main(&worktree_dir, &default_branch).await
                    {
                        // If rebase failed we already made efforts to stash and
                        // restore changes safely. Log the error and reset the
                        // task as before. The worktree removal path is used
                        // to avoid leaving corrupted worktrees around.
                        tracing::warn!(repo = %engine.repo, task_id = %task_id, err = %e, "startup rebase failed, resetting task");
                        abort_worktree_rebase(&worktree_dir).await;
                        let keep_remote = task.pr_number.is_some();
                        remove_worktree_and_branch(
                            &task_id,
                            &worktree_dir,
                            Some(branch_name),
                            &repo_root_path,
                            keep_remote,
                        )
                        .await;
                        match engine.store.resolve_task_id(&engine.repo, &task_id).await {
                            Ok(Some(reset_id)) => {
                                if let Err(e) = engine.store.reset_to_new(reset_id).await {
                                    tracing::warn!(task_id, err = %e, "failed to reset task to new after rebase failure");
                                }
                            }
                            Ok(None) => {
                                tracing::warn!(
                                    task_id,
                                    "task not found in store during reconciliation"
                                );
                            }
                            Err(e) => {
                                tracing::warn!(task_id, err = %e, "failed to resolve task ID during reconciliation");
                            }
                        }
                        if let Err(e) = engine
                            .task_manager
                            .update_task_status(
                                &ExternalId(task_id.clone()),
                                crate::backends::Status::New,
                            )
                            .await
                        {
                            tracing::warn!(task_id, err = %e, "failed to update backend status during reconciliation");
                        }
                        cleaned += 1;
                    } else {
                        valid_kept += 1;
                    }
                }
                // A blocked or waiting task resumes from this work, and without a PR the branch
                // may hold the only copy of it. An approved task's PR is updated on GitHub
                TaskStatus::Blocked | TaskStatus::Waiting | TaskStatus::Approved => {
                    valid_kept += 1;
                }
                TaskStatus::Done => {
                    tracing::info!(repo = %engine.repo, task_id = %task_id, worktree = %worktree_dir.display(), status = ?task.status, "terminal task worktree, removing");
                    remove_worktree_and_branch(
                        &task_id,
                        &worktree_dir,
                        Some(branch_name),
                        &repo_root_path,
                        false,
                    )
                    .await;
                    cleaned += 1;
                }
            }
        }

        tracing::info!(
            repo = %engine.repo,
            orphans_removed,
            invalid_removed,
            cleaned,
            valid_kept,
            "worktree reconciliation: {orphans_removed} orphans removed, {invalid_removed} invalid removed, {cleaned} cleaned, {valid_kept} valid kept"
        );

        // Prune stale .git/worktrees/ entries whose directories no longer exist.
        // This handles entries left behind by crashes or manual directory removal.
        let prune = Command::new("git")
            .args(["-C", &repo_root.to_string_lossy(), "worktree", "prune"])
            .output_with_context()
            .await;
        match prune {
            Ok(output) if output.status.success() => {
                tracing::debug!(repo = %engine.repo, "startup worktree prune completed");
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::warn!(repo = %engine.repo, err = %stderr, "startup worktree prune failed");
            }
            Err(e) => {
                tracing::warn!(repo = %engine.repo, err = %e, "startup worktree prune failed");
            }
        }
    }

    Ok(())
}

/// Sync Fibonacci estimates stored in SQLite to the GitHub Projects board for
/// all tasks that have a positive estimate.
///
/// Runs once on startup to catch tasks that were routed before the estimate
/// sync feature was available or before `project_estimate_field_id` was
/// configured. The underlying mutations are idempotent, so re-syncing an
/// already-correct estimate is harmless.
async fn reconcile_startup_estimates(project_engines: &[ProjectEngine]) {
    use crate::backends::ExternalId;

    for engine in project_engines {
        let tasks = match engine.store.list_external_tasks_with_estimates().await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(repo = %engine.repo, err = %e, "startup estimate reconciliation: store query failed");
                continue;
            }
        };

        if tasks.is_empty() {
            continue;
        }

        let mut synced: u32 = 0;
        let mut failed: u32 = 0;
        for (external_id, estimate) in &tasks {
            match engine
                .backend
                .sync_estimate_to_project(&ExternalId(external_id.clone()), *estimate)
                .await
            {
                Ok(()) => synced += 1,
                Err(e) => {
                    tracing::debug!(
                        repo = %engine.repo,
                        task_id = %external_id,
                        estimate,
                        err = %e,
                        "startup estimate reconciliation: sync failed"
                    );
                    failed += 1;
                }
            }
        }

        if synced > 0 || failed > 0 {
            tracing::info!(
                repo = %engine.repo,
                synced,
                failed,
                "startup estimate reconciliation complete"
            );
        }
    }
}

/// Read per-project channel configuration from `.orch.yml`.
fn read_project_channel_config(project_dir: &std::path::Path) -> ProjectChannelConfig {
    let config_path = project_dir.join(".orch.yml");
    if !config_path.exists() {
        return ProjectChannelConfig::default();
    }
    let content = match std::fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                path = %config_path.display(),
                error = %e,
                "failed to read .orch.yml; channel config will use defaults"
            );
            return ProjectChannelConfig::default();
        }
    };
    let val: serde_norway::Value = match serde_norway::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                path = %config_path.display(),
                error = %e,
                "failed to parse .orch.yml; channel config will use defaults"
            );
            return ProjectChannelConfig::default();
        }
    };
    let channels = match val.get("channels") {
        Some(c) => c,
        None => return ProjectChannelConfig::default(),
    };
    let get_str = |section: &str, key: &str| -> Option<String> {
        channels
            .get(section)
            .and_then(|s| s.get(key))
            .and_then(|v| v.as_str())
            .map(String::from)
    };
    ProjectChannelConfig {
        telegram_topic_id: get_str("telegram", "topic_id"),
        telegram_bot_token: get_str("telegram", "bot_token"),
        telegram_chat_id: get_str("telegram", "chat_id"),
        discord_channel_id: get_str("discord", "channel_id"),
        discord_bot_token: get_str("discord", "bot_token"),
        discord_guild_id: get_str("discord", "guild_id"),
    }
}

/// Fetch the latest orch release tag from GitHub via the `gh` CLI (async).
///
/// Returns the version string (without the leading "v"), or `None` when the
/// check fails (no network, unauthenticated, `gh` not installed, etc.).
async fn fetch_latest_release_version() -> Option<String> {
    let output = Command::new("gh")
        .args([
            "api",
            "repos/gabrielkoerich/orch/releases/latest",
            "--jq",
            ".tag_name",
        ])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let tag = String::from_utf8(output.stdout).ok()?;
    let version = tag.trim().trim_start_matches('v').to_string();
    if version.is_empty() {
        None
    } else {
        Some(version)
    }
}

/// Check if a newer orch release is available and send channel notifications
/// asking the operator to upgrade manually. Upgrading is operator-only — the
/// service never runs `brew` itself.
///
/// Uses the store's KV to throttle checks (`upgrade:last_check_at`) and
/// deduplicate notifications (`upgrade:last_notified_version`). Each distinct
/// latest version is notified about once; the notification repeats only when
/// an even newer version is released.
async fn check_and_notify_upgrade(
    store: &Arc<TaskStore>,
    channels: &Arc<ChannelRegistry>,
    current_version: &str,
    check_interval: std::time::Duration,
) {
    let last_check_key = "upgrade:last_check_at";
    let last_notified_key = "upgrade:last_notified_version";

    // Throttle: only check once per interval.
    let now = chrono::Utc::now().timestamp();
    if let Ok(Some(last_check_str)) = store.kv_get(last_check_key).await {
        if let Ok(last_check) = last_check_str.parse::<i64>() {
            if now - last_check < check_interval.as_secs() as i64 {
                return;
            }
        }
    }

    // Record check timestamp (best-effort).
    let _ = store.kv_set(last_check_key, &now.to_string()).await;

    let latest = match fetch_latest_release_version().await {
        Some(v) => v,
        None => {
            tracing::debug!("upgrade check: failed to fetch latest release version");
            return;
        }
    };

    if latest == current_version {
        tracing::debug!(current = %current_version, "orch is up to date");
        return;
    }

    tracing::warn!(
        current_version = %current_version,
        latest_version = %latest,
        "orch upgrade available"
    );

    // Operator-only upgrade: send channel notifications so the operator can
    // upgrade on their own schedule. The service never runs brew itself.

    // Deduplicate: don't re-notify for the same latest version.
    if let Ok(Some(last_notified)) = store.kv_get(last_notified_key).await {
        if last_notified == latest {
            tracing::debug!(latest = %latest, "upgrade already notified");
            return;
        }
    }

    let msg_telegram = format!(
        "⚠️ <b>Orch Upgrade Available</b>\n\n\
         Service: <code>{current_version}</code>\n\
         Latest:  <code>{latest}</code>\n\n\
         Run: <code>brew update && brew upgrade orch && brew services restart orch</code>"
    );

    let msg_discord = format!(
        "⚠️ **Orch Upgrade Available**\n\n\
         Service: `{current_version}`\n\
         Latest:  `{latest}`\n\n\
         Run: `brew update && brew upgrade orch && brew services restart orch`"
    );

    let msg_slack = format!(
        "⚠️ *Orch Upgrade Available*\n\n\
         Service: `{current_version}`\n\
         Latest:  `{latest}`\n\n\
         Run: `brew update && brew upgrade orch && brew services restart orch`"
    );

    for channel in channels.iter() {
        let (body, metadata, topic_id) = match channel.name() {
            "telegram" => (
                msg_telegram.clone(),
                serde_json::json!({"preformatted_html": true}),
                None,
            ),
            "discord" => (msg_discord.clone(), serde_json::json!({}), None),
            "slack" => (msg_slack.clone(), serde_json::json!({}), None),
            _ => continue,
        };

        let msg = OutgoingMessage {
            thread_id: "upgrade".to_string(),
            body,
            reply_to: None,
            metadata,
            topic_id,
        };

        if let Err(e) = channel.send(&msg).await {
            tracing::warn!(
                channel = channel.name(),
                ?e,
                "failed to send upgrade notification"
            );
        }
    }

    // Record that we notified for this latest version.
    let _ = store.kv_set(last_notified_key, &latest).await;
}

/// Send a one-time escalation notification when the startup retry loop has been
/// spinning for longer than `startup_failure_escalation_secs`.
///
/// This is called *before* the channel registry is initialised (the retry loop
/// sits above it in `serve()`), so we construct channel objects directly from
/// config — mirroring the approach used during normal channel registration, but
/// without relying on `ChannelRegistry`.
async fn send_startup_escalation_notification(elapsed_secs: u64) {
    let duration = crate::channels::notification::format_duration(elapsed_secs as f64);

    let msg_telegram = format!(
        "🚨 <b>Orch startup blocked — GitHub unreachable</b>\n\n\
         The engine has been retrying backend connections for {}\n\
         without success. All task processing, routing, and scheduled\n\
         jobs are paused. Check service logs with `orch log -f`.",
        duration
    );

    let msg_discord = format!(
        "🚨 **Orch startup blocked — GitHub unreachable**\n\n\
         The engine has been retrying backend connections for {}\n\
         without success. All task processing, routing, and scheduled\n\
         jobs are paused. Check service logs with `orch log -f`.",
        duration
    );

    let msg_slack = format!(
        "🚨 *Orch startup blocked — GitHub unreachable*\n\n\
         The engine has been retrying backend connections for {}\n\
         without success. All task processing, routing, and scheduled\n\
         jobs are paused. Check service logs with `orch log -f`.",
        duration
    );

    // Telegram
    let tg_token = crate::config::get("channels.telegram.bot_token");
    let tg_chat_id = crate::config::get("channels.telegram.chat_id");
    if let (Ok(token), Ok(chat_id)) = (&tg_token, &tg_chat_id) {
        if !token.is_empty() && !chat_id.is_empty() {
            if let Ok(channel) = TelegramChannel::new(token.clone(), Some(chat_id.clone())) {
                let msg = OutgoingMessage {
                    thread_id: "startup_escalation".to_string(),
                    body: msg_telegram.clone(),
                    reply_to: None,
                    metadata: serde_json::json!({ "preformatted_html": true }),
                    topic_id: crate::config::get("channels.telegram.general_topic_id").ok(),
                };
                if let Err(e) = channel.send(&msg).await {
                    tracing::warn!(?e, "failed to send startup escalation via telegram");
                } else {
                    tracing::info!("startup escalation notification sent to telegram");
                }
            }
        }
    }

    // Discord
    if let Ok(token) = crate::config::get("channels.discord.bot_token") {
        let channel_id = crate::config::get("channels.discord.channel_id")
            .or_else(|_| crate::config::get("channels.discord.general_channel_id"));
        if !token.is_empty() {
            if let Some(cid) = channel_id.ok().filter(|s| !s.is_empty()) {
                let shard_id = crate::config::get("channels.discord.shard_id")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0);
                let shard_count = crate::config::get("channels.discord.shard_count")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(1);
                if let Ok(gateway) = DiscordGateway::new(token, Some(cid), shard_id, shard_count) {
                    let msg = OutgoingMessage {
                        thread_id: "startup_escalation".to_string(),
                        body: msg_discord.clone(),
                        reply_to: None,
                        metadata: serde_json::Value::Null,
                        topic_id: None,
                    };
                    if let Err(e) = gateway.send(&msg).await {
                        tracing::warn!(?e, "failed to send startup escalation via discord");
                    } else {
                        tracing::info!("startup escalation notification sent to discord");
                    }
                }
            }
        }
    }

    // Slack
    if let Ok(token) = crate::config::get("channels.slack.bot_token") {
        let channel_id = crate::config::get("channels.slack.channel_id");
        if !token.is_empty() {
            if let Ok(cid) = channel_id {
                if let Ok(channel) = SlackChannel::new(token, Some(cid)) {
                    let msg = OutgoingMessage {
                        thread_id: "startup_escalation".to_string(),
                        body: msg_slack.clone(),
                        reply_to: None,
                        metadata: serde_json::Value::Null,
                        topic_id: None,
                    };
                    if let Err(e) = channel.send(&msg).await {
                        tracing::warn!(?e, "failed to send startup escalation via slack");
                    } else {
                        tracing::info!("startup escalation notification sent to slack");
                    }
                }
            }
        }
    }
}

/// Start the orch service.
///
/// This is the main entry point — called by `orch serve`.
pub async fn serve() -> anyhow::Result<()> {
    let killed = crate::home::kill_other_orch_serve_processes(std::time::Duration::from_secs(30));
    if killed > 0 {
        tracing::info!(count = killed, "terminated stale orch serve processes");
    }

    tracing::info!("orch engine starting");

    // Pre-create standard directories with async I/O so subsequent synchronous
    // callers (state_dir, orch_home, etc.) find them already present and their
    // create_dir_all calls become fast no-ops.
    if let Err(e) = crate::home::init_dirs().await {
        tracing::warn!("failed to initialize orch directories: {e}");
    }

    // Pre-warm the config cache with async I/O so get() / get_list() calls in
    // hot async paths never block a Tokio thread on std::fs::read_to_string.
    if let Err(e) = crate::config::warm_cache().await {
        tracing::warn!("failed to warm config cache: {e}");
    }

    let mut config = EngineConfig::from_config();

    tracing::info!("internal database ready");

    // Initialize project engines — retry with backoff so a network outage at
    // startup doesn't cause a crash-loop (launchd KeepAlive would restart us
    // immediately, filling the error log with 600+ identical messages).
    //
    // IMPORTANT: errors from init_project_engines() must NEVER propagate past
    // this loop to main().  If they did, anyhow would write "Error: …" to stderr,
    // which brew routes to orch.error.log — polluting it even when projects ARE
    // configured correctly but GitHub is temporarily unreachable.
    let startup_escalation_secs = config.startup_failure_escalation_secs;
    let mut project_engines = {
        let mut delay_secs = 5u64;
        let mut attempt = 0u32;
        let mut escalated = false;
        let start = std::time::Instant::now();
        loop {
            attempt += 1;
            match init_project_engines().await {
                Ok(engines) => break engines,
                Err(e) => {
                    let elapsed = start.elapsed().as_secs();

                    // Short blips (under the escalation threshold) stay at WARN
                    // so they don't flood the operator's attention. Once the
                    // outage crosses the threshold, escalate to ERROR-level
                    // logging and send a one-time push notification to configured
                    // channels — "transient" has clearly become "prolonged."
                    if startup_escalation_secs > 0
                        && !escalated
                        && elapsed >= startup_escalation_secs
                    {
                        escalated = true;
                        tracing::error!(
                            delay_secs,
                            attempt,
                            elapsed_secs = elapsed,
                            threshold_secs = startup_escalation_secs,
                            "project backends unavailable for {} — escalating startup failure: {e}",
                            crate::channels::notification::format_duration(elapsed as f64)
                        );
                        // Send one-time notification to configured channels.
                        // Channels aren't registered yet at this stage, so we
                        // construct them directly from config (mirrors orch notify).
                        send_startup_escalation_notification(elapsed).await;
                    } else {
                        tracing::warn!(
                            delay_secs,
                            attempt,
                            "project backends unavailable, retrying: {e}"
                        );
                    }

                    tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
                    delay_secs = (delay_secs * 2).min(120);
                }
            }
        }
    };

    tracing::info!(
        project_count = project_engines.len(),
        "initialized project engines"
    );

    // Full set of currently-configured project repos, used by the per-tick global
    // sweeps (e.g. `dispatch_routed_tasks_global`) to distinguish "orphaned" tasks
    // (repo no longer in config) from tasks belonging to some other active project.
    let mut active_repos: std::collections::HashSet<String> =
        project_engines.iter().map(|e| e.repo.clone()).collect();

    // Create the event bus for task status transitions
    let event_bus = events::EventBus::new(256);
    if let Err(e) = event_bus.start_ws_server().await {
        tracing::warn!(
            ?e,
            "failed to start event websocket server, continuing without it"
        );
    }

    // Check for a newer orch release in the background and log if the service is behind.
    // The periodic checker sends channel notifications after channels are initialized;
    // upgrading is operator-only and never runs automatically.
    {
        let current = env!("ORCH_VERSION").to_string();
        tokio::spawn(async move {
            if let Some(latest) = fetch_latest_release_version().await {
                if latest != current {
                    tracing::warn!(
                        current_version = %current,
                        latest_version = %latest,
                        "orch service is behind the latest release — run: brew update && brew upgrade orch && brew services restart orch"
                    );
                }
            }
        });
    }

    // Re-create task managers with shared store and event bus
    for engine in &mut project_engines {
        engine.task_manager = Arc::new(TaskManager::with_events(
            engine.backend.clone(),
            engine.store.clone(),
            engine.repo.clone(),
            event_bus.sender(),
        ));
    }

    // Startup reconciliation decides task status, so only the leader runs it
    let leads = project_engines.first().is_none_or(|e| e.store.is_leader());
    if leads {
        if let Err(e) = reconcile_startup_worktrees(&project_engines).await {
            tracing::warn!(err = %e, "startup worktree reconciliation failed");
        }
    }

    if leads {
        // Run estimate reconciliation in the background so it never blocks startup.
        // Each GraphQL mutation takes ~0.8 s; with hundreds of tasks this would
        // otherwise delay the engine reaching its main loop by several minutes.
        let engines_snapshot: Vec<ProjectEngine> = project_engines.clone();
        tokio::spawn(async move {
            reconcile_startup_estimates(&engines_snapshot).await;
        });
    }

    // Build ChannelRouter from global config + per-project configs
    let global_channel_config = GlobalChannelConfig {
        telegram_general_topic_id: crate::config::get("channels.telegram.general_topic_id").ok(),
        discord_general_channel_id: crate::config::get("channels.discord.general_channel_id").ok(),
        control_telegram_topic_id: crate::config::get("control.channels.telegram.topic_id").ok(),
        control_discord_channel_id: crate::config::get("control.channels.discord.channel_id").ok(),
    };
    let project_channel_configs: Vec<(String, ProjectChannelConfig)> = project_engines
        .iter()
        .map(|e| {
            let cfg = read_project_channel_config(&e.project_dir);
            (e.repo.clone(), cfg)
        })
        .collect();
    let channel_router = Arc::new(ChannelRouter::new(
        &global_channel_config,
        &project_channel_configs,
    ));
    tracing::info!(
        projects = ?channel_router.projects(),
        "channel router initialized"
    );

    // Initialize tmux manager (shared across all projects)
    let tmux = Arc::new(TmuxManager::new());
    // Kill leftover sessions from a previous process. This is daemon start only,
    // never a tick, because nextest makes a once-flag in tick_all kill live agents
    tick::startup_cleanup(&tmux).await;

    // Initialize transport
    let transport = Arc::new(Transport::new());

    // Initialize capture service and start background loop
    let capture = Arc::new(CaptureService::new(transport.clone()));
    let capture_for_tick = capture.clone();
    tokio::spawn(async move {
        capture.start().await;
    });

    // Initialize channel registry
    let mut channel_registry = ChannelRegistry::new();

    // Try to initialize Telegram channel
    if let Ok(token) = crate::config::get("channels.telegram.bot_token") {
        if !token.is_empty() {
            let chat_id = crate::config::get("channels.telegram.chat_id").ok();
            match TelegramChannel::new(token, chat_id) {
                Ok(telegram) => {
                    if let Err(e) = telegram.health_check().await {
                        tracing::warn!(?e, "telegram channel health check failed, skipping");
                    } else {
                        channel_registry.register(Box::new(telegram));
                        tracing::info!("telegram channel registered");
                    }
                }
                Err(e) => {
                    tracing::warn!(?e, "failed to create telegram channel, skipping");
                }
            }
        }
    }

    // Try to initialize Discord Gateway channel (websocket)
    if let Ok(token) = crate::config::get("channels.discord.bot_token") {
        if !token.is_empty() {
            let channel_id = crate::config::get("channels.discord.channel_id").ok();
            let shard_id = crate::config::get("channels.discord.shard_id")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            let shard_count = crate::config::get("channels.discord.shard_count")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(1);
            match DiscordGateway::new(token, channel_id, shard_id, shard_count) {
                Ok(discord) => {
                    if let Err(e) = discord.health_check().await {
                        tracing::warn!(?e, "discord gateway health check failed, skipping");
                    } else {
                        channel_registry.register(Box::new(discord));
                        tracing::info!(shard_id, shard_count, "discord gateway registered");
                    }
                }
                Err(e) => {
                    tracing::warn!(?e, "failed to create discord gateway, skipping");
                }
            }
        }
    }

    // Try to initialize Slack channel
    if let Ok(token) = crate::config::get("channels.slack.bot_token") {
        if !token.is_empty() {
            let channel_id = crate::config::get("channels.slack.channel_id").ok();
            match SlackChannel::new(token, channel_id) {
                Ok(slack) => {
                    if let Err(e) = slack.health_check().await {
                        tracing::warn!(?e, "slack channel health check failed, skipping");
                    } else {
                        channel_registry.register(Box::new(slack));
                        tracing::info!("slack channel registered");
                    }
                }
                Err(e) => {
                    tracing::warn!(?e, "failed to create slack channel, skipping");
                }
            }
        }
    }

    // Initialize tmux channel with transport for output streaming
    let tmux_channel = TmuxChannel::with_transport(transport.clone());
    channel_registry.register(Box::new(tmux_channel));
    tracing::info!("tmux channel registered");

    // Start all channels and collect their message receivers
    let mut channel_receivers: Vec<tokio::sync::mpsc::Receiver<IncomingMessage>> = Vec::new();
    for channel in channel_registry.iter() {
        match channel.start().await {
            Ok(rx) => {
                tracing::info!(channel = channel.name(), "channel started");
                channel_receivers.push(rx);
            }
            Err(e) => {
                tracing::warn!(channel = channel.name(), ?e, "failed to start channel");
            }
        }
    }

    // Wrap channel registry in Arc for shared access (notification dispatcher needs it)
    let channel_registry = Arc::new(channel_registry);

    // Spawn tasks to handle incoming channel messages (if any channels are active)
    let transport_for_messages = transport.clone();
    let tmux_for_messages = tmux.clone();
    let capture_for_messages = capture_for_tick.clone();
    let channels_for_messages = channel_registry.clone();
    // Lightweight engine references for command execution and task creation
    let engine_refs: Vec<EngineRef> = project_engines
        .iter()
        .map(|e| {
            (
                e.repo.clone(),
                e.backend.clone(),
                e.task_manager.clone(),
                Some(e.store.clone()),
            )
        })
        .collect();
    // Shared in-memory map for pending project picks (General channel multi-project picker).
    let pending_picks: channel_handler::PendingPicks =
        std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let router_for_messages = channel_router.clone();
    for mut rx in channel_receivers {
        let transport = transport_for_messages.clone();
        let tmux = tmux_for_messages.clone();
        let capture = capture_for_messages.clone();
        let channels = channels_for_messages.clone();
        let engine_refs = engine_refs.clone();
        let ch_router = router_for_messages.clone();
        let picks = pending_picks.clone();
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                tracing::debug!(channel = %msg.channel, thread = %msg.thread_id, "received message from channel");
                channel_handler::handle_channel_message(
                    msg,
                    &transport,
                    &tmux,
                    &capture,
                    &channels,
                    &engine_refs,
                    &ch_router,
                    &picks,
                )
                .await;
            }
        });
    }

    // Post-restart confirmation: if the service was restarted via the /restart control command,
    // a KV marker was stored before shutdown. Send a confirmation to the originating channel now
    // that we're back online. Use a short delay to let the channel connections re-establish.
    {
        let restart_channels = channel_registry.clone();
        let restart_store = project_engines.first().map(|e| e.store.clone());
        if let Some(store) = restart_store {
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                match store.kv_get("control:restart_pending").await {
                    Ok(Some(conv_key)) => {
                        if let Some((ch_name, thread_id, topic_id)) =
                            crate::channels::transport::parse_conversation_key(&conv_key)
                        {
                            for ch in restart_channels.iter() {
                                if ch.name() == ch_name {
                                    let msg = OutgoingMessage {
                                        thread_id: thread_id.to_string(),
                                        body: "orch restarted successfully.".to_string(),
                                        reply_to: None,
                                        metadata: serde_json::json!({}),
                                        topic_id: topic_id.map(|t| t.to_string()),
                                    };
                                    if let Err(e) = ch.send(&msg).await {
                                        tracing::warn!(
                                            channel = ch_name,
                                            error = %e,
                                            "failed to send restart confirmation"
                                        );
                                    }
                                    break;
                                }
                            }
                        }
                        if let Err(e) = store.kv_delete("control:restart_pending").await {
                            tracing::warn!(error = %e, "failed to clear control:restart_pending");
                        }
                    }
                    Ok(None) => {} // no pending restart, nothing to do
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to read control:restart_pending");
                    }
                }
            });
        }
    }

    // Spawn notification dispatcher — reads task completion notifications
    // from transport and broadcasts to configured channels.
    //
    // Routing priority:
    // 1. Dedicated channel: if the notification has a `repo` and the
    //    ChannelRouter maps it to a topic/channel, send there with `topic_id`.
    // 2. Subscribed channels: send to each (channel, thread_id) subscribed to
    //    the repo, using `format_with_project()` so recipients know the source.
    // 3. Fallback: if no repo or no dedicated channel, broadcast to all
    //    channels without a topic_id (legacy behaviour).
    {
        let mut notification_rx = transport.subscribe_notifications();
        let channels = channel_registry.clone();
        let notif_router = channel_router.clone();
        // Grab a store reference for subscription lookups.
        let notif_store: Option<Arc<TaskStore>> = project_engines.first().map(|e| e.store.clone());
        tokio::spawn(async move {
            loop {
                match notification_rx.recv().await {
                    Ok(notification) => {
                        let level = NotificationLevel::from_config();
                        if !level.should_notify(&notification.status) {
                            tracing::debug!(
                                task_id = %notification.task_id,
                                status = %notification.status,
                                "notification suppressed by level={:?}",
                                level
                            );
                            continue;
                        }

                        tracing::info!(
                            task_id = %notification.task_id,
                            status = %notification.status,
                            repo = ?notification.repo,
                            "routing notification to channels"
                        );

                        // Track whether we sent to at least one dedicated/subscribed target.
                        let mut routed = false;

                        // 0. Direct target: if notify_target is set, send straight to
                        //    that Telegram chat_id and skip all other routing.
                        if let Some(ref target) = notification.notify_target {
                            let telegram = channels.iter().find(|c| c.name() == "telegram");
                            if let Some(channel) = telegram {
                                let body = notification.format_telegram();
                                let metadata = serde_json::json!({
                                    "preformatted_html": true,
                                    "chat_id_override": target
                                });
                                let msg = OutgoingMessage {
                                    thread_id: notification.task_id.clone(),
                                    body,
                                    reply_to: None,
                                    metadata,
                                    topic_id: None,
                                };
                                if let Err(e) = channel.send(&msg).await {
                                    tracing::warn!(
                                        task_id = %notification.task_id,
                                        target,
                                        ?e,
                                        "failed to send to notify_target"
                                    );
                                } else {
                                    routed = true;
                                }
                            }
                        }

                        if routed {
                            // notify_target handled — skip further routing
                        } else if let Some(repo) = notification.repo.as_deref() {
                            // Track destinations already sent to in this iteration so
                            // the dedicated (path 1) and subscribed (path 2) loops
                            // don't double-fire the same Telegram/Discord destination
                            // when a repo's configured topic and a `channel_subscriptions`
                            // row resolve to the same `(channel, topic_id)`.
                            let mut sent: std::collections::HashSet<(String, Option<String>)> =
                                std::collections::HashSet::new();

                            // 1. Dedicated channel targets for this repo.
                            for channel in channels.iter() {
                                let ch_name = channel.name();
                                if let Some(topic_id) =
                                    notif_router.target_for_project(repo, ch_name)
                                {
                                    let body = match ch_name {
                                        "telegram" => notification.format_telegram(),
                                        "discord" => notification.format_discord(),
                                        "slack" => notification.format_slack(),
                                        _ => continue,
                                    };
                                    let metadata = if ch_name == "telegram" {
                                        serde_json::json!({"preformatted_html": true})
                                    } else {
                                        serde_json::json!({})
                                    };
                                    let resolved_topic = Some(topic_id.to_string());
                                    let msg = OutgoingMessage {
                                        thread_id: notification.task_id.clone(),
                                        body,
                                        reply_to: None,
                                        metadata,
                                        topic_id: resolved_topic.clone(),
                                    };
                                    if let Err(e) = channel.send(&msg).await {
                                        tracing::warn!(
                                            channel = ch_name,
                                            task_id = %notification.task_id,
                                            ?e,
                                            "failed to send to dedicated channel"
                                        );
                                    } else {
                                        sent.insert((ch_name.to_string(), resolved_topic));
                                        routed = true;
                                    }
                                }
                            }

                            // 2. Subscribed channels for this repo.
                            if let Some(store) = &notif_store {
                                match store.list_subscribers_for_repo(repo).await {
                                    Ok(subscribers) => {
                                        for (ch_name, thread_id, topic_id) in subscribers {
                                            let channel =
                                                channels.iter().find(|c| c.name() == ch_name);
                                            let Some(channel) = channel else {
                                                continue;
                                            };
                                            let resolved_topic = if topic_id.is_empty() {
                                                None
                                            } else {
                                                Some(topic_id.clone())
                                            };
                                            // Skip if a dedicated send to the same
                                            // (channel, topic_id) already happened.
                                            if sent.contains(&(
                                                ch_name.clone(),
                                                resolved_topic.clone(),
                                            )) {
                                                tracing::debug!(
                                                    channel = %ch_name,
                                                    task_id = %notification.task_id,
                                                    topic_id = ?resolved_topic,
                                                    "skipping duplicate subscriber dispatch — destination already received from dedicated path"
                                                );
                                                continue;
                                            }
                                            let body = notification.format_with_project(&ch_name);
                                            let metadata = if ch_name == "telegram" {
                                                serde_json::json!({"preformatted_html": true})
                                            } else {
                                                serde_json::json!({})
                                            };
                                            let msg = OutgoingMessage {
                                                thread_id: thread_id.clone(),
                                                body,
                                                reply_to: None,
                                                metadata,
                                                topic_id: resolved_topic.clone(),
                                            };
                                            if let Err(e) = channel.send(&msg).await {
                                                tracing::warn!(
                                                    channel = %ch_name,
                                                    task_id = %notification.task_id,
                                                    ?e,
                                                    "failed to send to subscribed channel"
                                                );
                                            } else {
                                                sent.insert((ch_name.clone(), resolved_topic));
                                                routed = true;
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            repo,
                                            ?e,
                                            "failed to list subscribers for repo"
                                        );
                                    }
                                }
                            }
                        }

                        // 3. Fallback: broadcast to all channels when no routing happened.
                        if !routed {
                            for channel in channels.iter() {
                                let (body, should_send, is_telegram) = match channel.name() {
                                    "telegram" => (notification.format_telegram(), true, true),
                                    "discord" => (notification.format_discord(), true, false),
                                    "slack" => (notification.format_slack(), true, false),
                                    // GitHub is already handled by backend.post_comment()
                                    // tmux doesn't need task completion notifications
                                    _ => (String::new(), false, false),
                                };

                                if !should_send {
                                    continue;
                                }

                                let metadata = if is_telegram {
                                    serde_json::json!({"preformatted_html": true})
                                } else {
                                    serde_json::json!({})
                                };
                                let msg = OutgoingMessage {
                                    thread_id: notification.task_id.clone(),
                                    body,
                                    reply_to: None,
                                    metadata,
                                    topic_id: None,
                                };

                                if let Err(e) = channel.send(&msg).await {
                                    tracing::warn!(
                                        channel = channel.name(),
                                        task_id = %notification.task_id,
                                        ?e,
                                        "failed to send notification"
                                    );
                                }
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(missed = n, "notification receiver lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        tracing::debug!("notification channel closed");
                        break;
                    }
                }
            }
        });
        tracing::info!("notification dispatcher started");
    }

    // Notify used by webhook events to wake up the engine tick immediately
    let webhook_notify = Arc::new(Notify::new());

    // Track webhook state for health checks and fallback.
    // When webhooks are disabled, `in_fallback_mode` stays true so sync
    // uses the faster fallback interval for polling.
    let webhook_port: Option<u16>;
    let mut webhook_healthy: bool;
    let mut last_webhook_health_check = std::time::Instant::now();
    // Start webhook server if configured
    let webhook_enabled = crate::config::get("webhook.enabled")
        .map(|v| v == "true")
        .unwrap_or(false);

    // Shared status updated by the server-spawn task and the health-check loop.
    let webhook_status = Arc::new(tokio::sync::Mutex::new(
        crate::webhook_status::WebhookStatus::default(),
    ));

    if webhook_enabled {
        let port: u16 = crate::config::get("webhook.port")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8080);
        webhook_port = Some(port);
        let secret = crate::config::get("webhook.secret").unwrap_or_default();
        let webhook_repo = project_engines
            .first()
            .map(|e| e.repo.clone())
            .unwrap_or_default();

        let (tx, mut rx) = tokio::sync::mpsc::channel::<IncomingMessage>(64);

        // Initialise status: configured, not yet healthy.
        let to_save = {
            let mut s = webhook_status.lock().await;
            s.configured = true;
            s.port = Some(port);
            s.healthy = false;
            s.fallback_mode = false;
            s.clone()
        };
        to_save.save().await;

        // Spawn the HTTP server with exponential-backoff retry on transient
        // bind errors (e.g. EADDRINUSE).  After MAX_ATTEMPTS the engine falls
        // back to polling and updates the shared status accordingly.
        const WEBHOOK_MAX_ATTEMPTS: u32 = 5;
        let status_for_spawn = webhook_status.clone();
        tokio::spawn(async move {
            use crate::channels::github::{is_transient_bind_error, webhook_backoff_delay};
            let mut attempt = 0u32;
            loop {
                attempt += 1;
                tracing::info!(attempt, port, "attempting to start webhook server");
                match start_webhook_server(port, secret.clone(), webhook_repo.clone(), tx.clone())
                    .await
                {
                    Ok(_) => {
                        tracing::info!(attempt, "webhook server exited cleanly");
                        break;
                    }
                    Err(e) => {
                        let reason = e.to_string();
                        tracing::error!(attempt, %reason, "webhook server failed");
                        if attempt >= WEBHOOK_MAX_ATTEMPTS || !is_transient_bind_error(&e) {
                            let kind = if attempt >= WEBHOOK_MAX_ATTEMPTS {
                                "max attempts reached"
                            } else {
                                "non-transient error"
                            };
                            tracing::error!(
                                attempt,
                                %reason,
                                kind,
                                orch_webhook_in_fallback = true,
                                "webhook server giving up, switching to polling fallback"
                            );
                            let to_save = {
                                let mut s = status_for_spawn.lock().await;
                                s.fallback_mode = true;
                                s.healthy = false;
                                s.last_failure_reason = Some(reason);
                                s.startup_attempts = attempt;
                                s.clone()
                            };
                            to_save.save().await;
                            break;
                        }
                        let delay = webhook_backoff_delay(attempt);
                        tracing::warn!(
                            attempt,
                            delay_ms = delay.as_millis(),
                            %reason,
                            "webhook bind failed (transient), retrying with backoff"
                        );
                        {
                            let to_save = {
                                let mut s = status_for_spawn.lock().await;
                                s.startup_attempts = attempt;
                                s.last_failure_reason = Some(reason);
                                s.clone()
                            };
                            to_save.save().await;
                        }
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        });

        // Spawn the message forwarding task (reads from webhook channel)
        let notify = webhook_notify.clone();
        let transport_for_webhook = transport.clone();
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                tracing::info!(
                    channel = %msg.channel,
                    thread = %msg.thread_id,
                    event = %msg.metadata.get("event").and_then(|v| v.as_str()).unwrap_or("unknown"),
                    action = %msg.metadata.get("action").and_then(|v| v.as_str()).unwrap_or("unknown"),
                    "webhook event received, triggering immediate tick"
                );
                let route = transport_for_webhook.route(&msg).await;
                tracing::info!(
                    channel = %msg.channel,
                    route = ?route,
                    "webhook event routed"
                );
                // Wake up the engine tick immediately instead of waiting up to 10s
                notify.notify_one();
            }
        });

        webhook_healthy = true;
        tracing::info!(port, orch_webhook_up = true, "webhook server started");
    } else {
        webhook_port = None;
        webhook_healthy = false;
        {
            let to_save = {
                let mut s = webhook_status.lock().await;
                s.configured = false;
                s.fallback_mode = true;
                s.clone()
            };
            to_save.save().await;
        }
        tracing::info!(
            orch_webhook_in_fallback = true,
            "webhook server disabled, using polling fallback mode"
        );
    }

    start_relay_client(&project_engines, &webhook_notify);
    crate::github::app::start_loader();

    // Agent router (selects agent + model per task) - shared across projects.
    // Router::from_config() may run `opencode models` when the config uses opencode.
    // spawn_blocking keeps the Tokio runtime thread free during that I/O
    let router = Arc::new(RwLock::new(
        match tokio::task::spawn_blocking(Router::from_config).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(?e, "router init panicked in spawn_blocking, using default");
                tokio::task::spawn_blocking(|| Router::new(RouterConfig::default()))
                    .await
                    .unwrap_or_else(|e2| {
                        panic!("router default init also panicked in spawn_blocking — cannot start engine without a router: {e2:?}");
                    })
            }
        },
    ));
    {
        let r = router.read().await;
        tracing::info!(
            mode = %r.config.mode,
            agents = ?r.available_agents,
            fallback = %r.config.fallback_executor,
            "router initialized"
        );
    }

    // Concurrency limiter (shared across all projects). `parallel_in_force` is the
    // limit actually handed out, which lags `config.max_parallel` after a shrink
    // until the running tasks give their permits back.
    let semaphore = Arc::new(Semaphore::new(config.max_parallel));
    let mut parallel_in_force = config.max_parallel;

    // In-memory dispatch guard: tracks task IDs currently being dispatched.
    // Guards against GitHub API eventual consistency — after update_status(InProgress),
    // the label-removal webhook can trigger an immediate tick where list_by_status(Routed)
    // still returns the task (search index propagation delay). The tmux session does not
    // exist until the runner completes worktree setup (~10s later), so session_exists
    // alone is insufficient. Keyed by "{repo}/{task_id}".
    let dispatching: Arc<dashmap::DashMap<String, String>> = Arc::new(dashmap::DashMap::new());

    // In-memory guard to prevent double-spawn of auto-merge background tasks.
    // Since review_open_prs runs every sync_tick (~10s) but CI polling can take
    // up to 10 minutes, we track which tasks already have an auto-merge in flight.
    let auto_merge_in_flight: Arc<dashmap::DashSet<String>> = Arc::new(dashmap::DashSet::new());

    // Subscribe to config file changes for hot reload
    let mut config_rx = crate::config::subscribe();

    // Track sync interval
    let mut last_sync = std::time::Instant::now();

    // Track upgrade check interval
    let mut last_upgrade_check: Option<std::time::Instant> = None;

    // Reset epoch of the GitHub rate-limit window already reported, so the log line,
    // the KV flag and the channel notification fire once per window instead of per tick
    let mut rate_limit_reported: Option<i64> = None;

    // Channel for weight signals from task runners back to the router
    let (weight_tx, mut weight_rx) = mpsc::channel::<WeightSignal>(64);

    // A restart ends every review session, so reset each in_review task left without one
    for engine in project_engines.iter().filter(|_| leads) {
        let reset =
            reset_orphaned_reviews(&engine.task_manager, &engine.store, &engine.repo, &tmux).await;

        if reset > 0 {
            tracing::info!(repo = %engine.repo, reset, "reset orphaned reviews to needs_review on startup");
        }
    }

    // Spawn dispatch subscribers — react to Routed events immediately
    for engine in &project_engines {
        subscribers::dispatch::spawn(
            event_bus.subscribe(),
            engine.backend.clone(),
            tmux.clone(),
            engine.runner.clone(),
            capture_for_tick.clone(),
            semaphore.clone(),
            engine.task_manager.clone(),
            weight_tx.clone(),
            router.clone(),
            dispatching.clone(),
            engine.store.clone(),
            engine.repo.clone(),
        );
    }
    tracing::info!("dispatch subscriber started");

    // Spawn review subscribers — react to NeedsReview events immediately
    for engine in &project_engines {
        subscribers::review::spawn(
            event_bus.subscribe(),
            engine.backend.clone(),
            tmux.clone(),
            semaphore.clone(),
            engine.task_manager.clone(),
            router.clone(),
            dispatching.clone(),
            engine.store.clone(),
            engine.repo.clone(),
        );
    }
    tracing::info!("review subscriber started");

    // Spawn unblock subscribers — react to Done events, unblock parent tasks immediately
    for engine in &project_engines {
        subscribers::unblock::spawn(
            event_bus.subscribe(),
            engine.backend.clone(),
            engine.task_manager.clone(),
            engine.store.clone(),
            engine.repo.clone(),
        );
    }
    tracing::info!("unblock subscriber started");

    // Spawn notify subscriber — reacts to ALL events, pushes to transport.
    // Spawned once (not per-project) since the transport handles all repos.
    subscribers::notify::spawn(event_bus.subscribe(), transport.clone());
    tracing::info!("notify subscriber started");

    // Main loop
    tracing::info!(
        tick = ?config.tick_interval,
        sync = ?config.sync_interval,
        parallel = config.max_parallel,
        "entering main loop"
    );
    // Values of the settings that only take effect at startup, so a reload can
    // tell the operator their edit needs a restart.
    let mut restart_only = restart_only_snapshot();

    let mut interval = tokio::time::interval(config.tick_interval);

    // Signal handlers (launchd/systemd send SIGTERM to stop services, SIGHUP for reload/restart)
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;

    // Guard to prevent concurrent sync_tick runs (sync is spawned as a background task).
    let sync_in_progress = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Tick watchdog: detects when the main loop hasn't completed a tick in > 60s.
    // Updates an atomic timestamp at the end of each tick iteration.
    let last_tick_epoch = Arc::new(std::sync::atomic::AtomicU64::new(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    ));
    {
        let last_tick_epoch = Arc::clone(&last_tick_epoch);
        let tick_interval_secs = config.tick_interval.as_secs();
        tokio::spawn(async move {
            // Check every 30s whether the main loop is still ticking.
            let mut watchdog_interval = tokio::time::interval(std::time::Duration::from_secs(30));
            // Own suspend baseline, independent of the main loop's checkpoint(). The
            // watchdog's interval timer also freezes during a host suspend, so this
            // detects the gap from its own wake-up instead of racing the main loop's
            // checkpoint() for the same GAP_LOG entry (see #3544).
            let mut suspend_detector = crate::engine::suspend::SuspendDetector::new();
            loop {
                watchdog_interval.tick().await;
                let own_gap = suspend_detector.check();
                let last = last_tick_epoch.load(std::sync::atomic::Ordering::Relaxed);
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let stale_secs = now.saturating_sub(last);
                // Warn if the tick loop hasn't completed in > 6× the tick interval (at least 60s).
                let threshold = (tick_interval_secs * 6).max(60);
                if stale_secs > threshold {
                    // A host suspend/resume gap fully explains a stale window that
                    // isn't an actual stall — the process (and its timers) were
                    // simply not scheduled while the host was asleep. Prefer this
                    // watchdog's own detector (immune to the checkpoint race above);
                    // fall back to the shared log in case the main loop's checkpoint()
                    // already ran and recorded a larger gap.
                    let shared_gap = crate::engine::suspend::gap_detected_within(
                        std::time::Duration::from_secs(stale_secs),
                    );
                    let gap = match (own_gap, shared_gap) {
                        (Some(a), Some(b)) => Some(a.max(b)),
                        (Some(a), None) => Some(a),
                        (None, Some(b)) => Some(b),
                        (None, None) => None,
                    };
                    if let Some(gap) = gap {
                        let gap_secs = gap.num_seconds() as u64;
                        if gap_secs >= stale_secs {
                            tracing::info!(
                                stale_secs,
                                threshold,
                                suspend_gap_secs = gap_secs,
                                "tick loop stale window explained by host suspend/resume, not a stall"
                            );
                        } else {
                            tracing::error!(
                                stale_secs,
                                threshold,
                                suspend_gap_secs = gap_secs,
                                "WATCHDOG: tick loop has not completed a tick in {}s (threshold {}s) — suspend gap of {}s only partially explains the stall",
                                stale_secs,
                                threshold,
                                gap_secs,
                            );
                        }
                    } else {
                        tracing::error!(
                            stale_secs,
                            threshold,
                            "WATCHDOG: tick loop has not completed a tick in {}s (threshold {}s) — possible stall",
                            stale_secs,
                            threshold,
                        );
                    }
                }
            }
        });
    }

    loop {
        tokio::select! {
            _ = interval.tick() => {
                let tick_start = std::time::Instant::now();

                // Detect a host suspend/resume gap once per iteration, in one place.
                // The watchdog and stuck-task recovery both read from this instead of
                // re-deriving suspend information from their own wall-clock deltas.
                if let Some(gap) = crate::engine::suspend::checkpoint() {
                    tracing::info!(
                        gap_secs = gap.num_seconds(),
                        "host suspend/resume gap detected, discounting from stuck-task age checks"
                    );
                }

                // Finish a pending shrink of the dispatch pool as running tasks
                // release their permits. Only logs when a permit was actually
                // taken back, so a long-running task does not log every tick.
                if parallel_in_force != config.max_parallel {
                    let resized =
                        resize_dispatch_pool(&semaphore, parallel_in_force, config.max_parallel);
                    if resized != parallel_in_force {
                        parallel_in_force = resized;
                        tracing::info!(
                            in_force = parallel_in_force,
                            target = config.max_parallel,
                            "dispatch pool resized"
                        );
                    }
                }

                // Tick weight recovery for rate-limited agents
                {
                    let mut rw = router.write().await;
                    rw.tick_weight_recovery();
                }

                // Report the GitHub rate limit once per window, then tick anyway. Calls
                // into GitHub now fail fast instead of sleeping until the reset, so the
                // local phases keep moving and the watchdog does not see a stalled loop.
                if let Some(remaining) = GhHttp::is_rate_limited() {
                    let reset_at = chrono::Utc::now()
                        .timestamp()
                        .saturating_add(i64::try_from(remaining.as_secs()).unwrap_or(i64::MAX));
                    // Tolerate jitter in the header so one window reports once
                    if rate_limit_reported.is_none_or(|prev| (reset_at - prev).abs() > 60) {
                        rate_limit_reported = Some(reset_at);
                        report_rate_limited(reset_at, project_engines.first().map(|e| &e.store), Some(&transport)).await;
                    }
                } else if let Some(prev) = rate_limit_reported.take() {
                    tracing::info!(
                        limited_until = prev,
                        "GitHub API rate limit cleared"
                    );
                    if let Some(engine) = project_engines.first() {
                        let _ = engine.store.kv_delete(RATE_LIMITED_UNTIL_KEY).await;
                    }
                }

                // Core tick: poll tasks for all projects
                tick::tick_all(
                    &project_engines,
                    &tmux,
                    &capture_for_tick,
                    &semaphore,
                    &config,
                    &router,
                    &weight_tx,
                    &dispatching,
                    Some(&transport),
                    &active_repos,
                    &auto_merge_in_flight,
                ).await;

                // Drain pending weight signals after tick processing so the router
                // learns from outcomes produced by this iteration's task dispatches.
                while let Ok(signal) = weight_rx.try_recv() {
                    let mut rw = router.write().await;
                    match signal {
                        WeightSignal::RateLimited { ref agent } => {
                            rw.record_rate_limit(agent);
                        }
                        WeightSignal::Success { ref agent } => {
                            rw.record_success(agent);
                        }
                        WeightSignal::Rerouted | WeightSignal::Blocked | WeightSignal::None => {}
                    }
                }

                // Periodic sync (less frequent) — spawned as a background task so it
                // cannot block the main tick loop. A stalled sync (hung HTTP call, slow
                // DB query, lock contention) previously parked all tokio workers for 12+
                // minutes because it ran inline. See issue #2574.
                if last_sync.elapsed() >= config.sync_interval {
                    if sync_in_progress.compare_exchange(
                        false,
                        true,
                        std::sync::atomic::Ordering::AcqRel,
                        std::sync::atomic::Ordering::Relaxed,
                    ).is_ok() {
                        spawn_sync_pass(
                            &project_engines,
                            &tmux,
                            &config,
                            &router,
                            &dispatching,
                            &auto_merge_in_flight,
                            &sync_in_progress,
                            "periodic",
                        );
                    } else {
                        tracing::debug!("sync tick still in progress, skipping");
                    }
                    // Records sync *schedule* time (not completion). This prevents
                    // re-entering the sync branch every tick while one is in flight.
                    // The sync_in_progress guard handles actual concurrency control.
                    last_sync = std::time::Instant::now();
                }

                // Periodic webhook health check
                if webhook_enabled {
                    let health_check_interval = config.webhook_health_check_interval
                        .unwrap_or(std::time::Duration::from_secs(60));

                    if last_webhook_health_check.elapsed() >= health_check_interval {
                        if let Some(port) = webhook_port {
                            let (health, failure_reason) =
                                crate::channels::github::check_webhook_health(port).await;
                            if health != webhook_healthy {
                                webhook_healthy = health;
                                if webhook_healthy {
                                    tracing::info!(port, orch_webhook_up = true, "webhook health restored");
                                } else {
                                    tracing::warn!(
                                        port,
                                        orch_webhook_up = false,
                                        reason = failure_reason.as_deref().unwrap_or("unknown"),
                                        "webhook health check failed"
                                    );
                                }
                            }
                            // Persist updated status.
                            {
                                let to_save = {
                                    let mut s = webhook_status.lock().await;
                                    s.healthy = health;
                                    s.last_check_utc = Some(chrono::Utc::now());
                                    if health {
                                        s.last_failure_reason = None;
                                    } else if failure_reason.is_some() {
                                        s.last_failure_reason = failure_reason;
                                    }
                                    s.clone()
                                };
                                to_save.save().await;
                            }
                        }
                        last_webhook_health_check = std::time::Instant::now();
                    }
                }

                // Periodic upgrade check — notify channels when a newer release is available.
                if should_run_upgrade_check(last_upgrade_check, config.upgrade_check_interval) {
                    if let Some(store) = project_engines.first().map(|e| e.store.clone()) {
                        let channels = channel_registry.clone();
                        let current = env!("ORCH_VERSION").to_string();
                        let check_interval =
                            std::time::Duration::from_secs(config.upgrade_check_interval);
                        tokio::spawn(async move {
                            check_and_notify_upgrade(&store, &channels, &current, check_interval)
                                .await;
                        });
                    }
                    last_upgrade_check = Some(std::time::Instant::now());
                }

                // Update watchdog timestamp + log tick duration.
                let tick_elapsed = tick_start.elapsed();
                if tick_elapsed.as_secs() > 30 {
                    tracing::warn!(elapsed_ms = tick_elapsed.as_millis() as u64, "slow tick");
                } else {
                    tracing::debug!(elapsed_ms = tick_elapsed.as_millis() as u64, "tick complete");
                }
                last_tick_epoch.store(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    std::sync::atomic::Ordering::Relaxed,
                );
            }
            // Webhook events trigger an immediate tick (bypass polling interval)
            _ = webhook_notify.notified() => {
                if let Some(remaining) = GhHttp::is_rate_limited() {
                    tracing::warn!(
                        remaining_secs = remaining.as_secs(),
                        "GitHub API rate-limited, skipping webhook-triggered tick"
                    );
                } else {
                    tracing::info!("webhook event triggered immediate tick");

                    tick::tick_all(
                        &project_engines,
                        &tmux,
                        &capture_for_tick,
                        &semaphore,
                        &config,
                        &router,
                        &weight_tx,
                        &dispatching,
                        Some(&transport),
                        &active_repos,
                        &auto_merge_in_flight,
                    ).await;

                // Drain pending weight signals after webhook-triggered tick processing.
                while let Ok(signal) = weight_rx.try_recv() {
                    let mut rw = router.write().await;
                    match signal {
                        WeightSignal::RateLimited { ref agent } => {
                            rw.record_rate_limit(agent);
                        }
                        WeightSignal::Success { ref agent } => {
                            rw.record_success(agent);
                        }
                        WeightSignal::Rerouted | WeightSignal::Blocked | WeightSignal::None => {}
                    }
                }

                // Also attempt to run an immediate sync (ingest/cleanup) in the
                // background so webhook-driven events (e.g. new issues) are
                // picked up without waiting for the periodic sync interval.
                // Reuse the same sync_in_progress guard used by the periodic
                // branch to avoid concurrent syncs.
                if sync_in_progress.compare_exchange(
                    false,
                    true,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Relaxed,
                ).is_ok() {
                    spawn_sync_pass(
                        &project_engines,
                        &tmux,
                        &config,
                        &router,
                        &dispatching,
                        &auto_merge_in_flight,
                        &sync_in_progress,
                        "webhook",
                    );
                    // Record schedule time so periodic sync doesn't immediately re-run.
                    last_sync = std::time::Instant::now();
                } else {
                    tracing::debug!("sync tick still in progress, skipping webhook-triggered immediate sync");
                }
                }

                // Also reset the interval so we don't get a redundant tick right after
                interval.reset();
            }
            result = config_rx.recv() => {
                // A notification only arrives for a file that parsed, so there is no
                // broken-config case to guard against here (see `config::reload_file`).
                let reload = match result {
                    Ok(path) => {
                        tracing::info!(path = %path.display(), "config file changed, reloading");
                        true
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(missed = n, "config change receiver lagged, reloading");
                        true
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        tracing::warn!("config change channel closed");
                        false
                    }
                };

                if reload {
                    // Reload engine config
                    let new_config = EngineConfig::from_config();
                    log_engine_config_changes(&config, &new_config);

                    let new_restart_only = restart_only_snapshot();
                    log_restart_only_changes(&restart_only, &new_restart_only);
                    restart_only = new_restart_only;

                    let tick_changed = new_config.tick_interval != config.tick_interval;
                    config = new_config;

                    // Reset tick interval if it changed
                    if tick_changed {
                        interval = tokio::time::interval(config.tick_interval);
                        tracing::info!(tick = ?config.tick_interval, "tick interval updated");
                    }

                    // Resize the dispatch pool. A shrink only takes the permits that are
                    // free right now; the rest are taken as running tasks finish.
                    if parallel_in_force != config.max_parallel {
                        parallel_in_force = resize_dispatch_pool(
                            &semaphore,
                            parallel_in_force,
                            config.max_parallel,
                        );
                        tracing::info!(
                            in_force = parallel_in_force,
                            target = config.max_parallel,
                            "dispatch pool resized"
                        );
                    }

                    // Reload router config (async-safe: use reload_async to avoid blocking the runtime)
                    {
                        let mut router_guard = router.write().await;
                        router_guard.reload_async().await;
                    }

                    // Pick up projects added to `projects:`
                    sync_project_engines(
                        &mut project_engines,
                        &mut active_repos,
                        &event_bus,
                        &tmux,
                        &capture_for_tick,
                        &semaphore,
                        &weight_tx,
                        &router,
                        &dispatching,
                    ).await;

                    tracing::info!(
                        tick = ?config.tick_interval,
                        sync = ?config.sync_interval,
                        parallel = config.max_parallel,
                        projects = project_engines.len(),
                        "config reloaded"
                    );
                }
            }
            signal_name = async {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => "SIGINT",
                    _ = sigterm.recv() => "SIGTERM",
                    _ = sighup.recv() => "SIGHUP",
                }
            } => {
                tracing::info!(signal = signal_name, "beginning graceful shutdown");

                // Wrap the entire shutdown sequence in a timeout to enforce the configured limit.
                // This prevents indefinite blocking if status updates or tmux operations hang.
                let shutdown_result = tokio::time::timeout(
                    config.graceful_shutdown_timeout,
                    async {
                        // Reset in_progress tasks to routed so they re-dispatch after restart.
                        // Also reset in_review tasks to needs_review — their review agent
                        // tmux sessions will be killed when the process exits.
                        let mut reset_count = 0u32;
                        let mut review_reset_count = 0u32;
                        for engine in &project_engines {
                            if let Ok(tasks) = engine.task_manager.list_external_by_status(Status::InProgress).await {
                                for task in &tasks {
                                    if let Err(e) = engine.task_manager.update_task_status(&task.id, Status::Routed).await {
                                        tracing::warn!(task_id = task.id.0, ?e, "failed to reset task on shutdown");
                                    } else {
                                        if let Ok(Some(store_id)) =
                                            engine.store.resolve_task_id(&engine.repo, &task.id.0).await
                                        {
                                            if let Err(e) = engine
                                                .store
                                                .finalize_incomplete_runs(
                                                    store_id,
                                                    "aborted",
                                                    "graceful shutdown: reset in_progress task to routed",
                                                )
                                                .await
                                            {
                                                tracing::warn!(
                                                    task_id = task.id.0,
                                                    ?e,
                                                    "failed to finalize incomplete runs on shutdown"
                                                );
                                            }
                                        }
                                        reset_count += 1;
                                    }
                                }
                            }
                            // Also reset internal in_progress tasks
                            if let Ok(tasks) = engine.store.list_internal_by_status(&engine.repo, crate::store::TaskStatus::InProgress).await {
                                for task in &tasks {
                                    let task_id = format!("internal:{}", task.id);
                                    if let Err(e) = engine.task_manager.update_task_status(
                                        &crate::backends::ExternalId(task_id.clone()),
                                        Status::Routed,
                                    ).await {
                                        tracing::warn!(task_id, ?e, "failed to reset internal task on shutdown");
                                    } else {
                                        if let Err(e) = engine
                                            .store
                                            .finalize_incomplete_runs(
                                                task.id,
                                                "aborted",
                                                "graceful shutdown: reset in_progress task to routed",
                                            )
                                            .await
                                        {
                                            tracing::warn!(
                                                task_id,
                                                ?e,
                                                "failed to finalize incomplete internal runs on shutdown"
                                            );
                                        }
                                        reset_count += 1;
                                    }
                                }
                            }
                            // Reset in_review tasks — review agent sessions die on shutdown
                            if let Ok(tasks) = engine.task_manager.list_external_by_status(Status::InReview).await {
                                for task in &tasks {
                                    if let Err(e) = engine.task_manager.update_task_status(&task.id, Status::NeedsReview).await {
                                        tracing::warn!(task_id = task.id.0, ?e, "failed to reset in_review task on shutdown");
                                    } else {
                                        if let Ok(Some(store_id)) =
                                            engine.store.resolve_task_id(&engine.repo, &task.id.0).await
                                        {
                                            if let Err(e) = engine
                                                .store
                                                .finalize_incomplete_runs(
                                                    store_id,
                                                    "aborted",
                                                    "graceful shutdown: reset in_review task to needs_review",
                                                )
                                                .await
                                            {
                                                tracing::warn!(
                                                    task_id = task.id.0,
                                                    ?e,
                                                    "failed to finalize incomplete review runs on shutdown"
                                                );
                                            }
                                        }
                                        review_reset_count += 1;
                                    }
                                }
                            }
                            // Also reset internal in_review tasks
                            if let Ok(tasks) = engine.store.list_internal_by_status(&engine.repo, crate::store::TaskStatus::InReview).await {
                                for task in &tasks {
                                    let task_id = format!("internal:{}", task.id);
                                    if let Err(e) = engine.task_manager.update_task_status(
                                        &crate::backends::ExternalId(task_id.clone()),
                                        Status::NeedsReview,
                                    ).await {
                                        tracing::warn!(task_id, ?e, "failed to reset internal in_review task on shutdown");
                                    } else {
                                        if let Err(e) = engine
                                            .store
                                            .finalize_incomplete_runs(
                                                task.id,
                                                "aborted",
                                                "graceful shutdown: reset in_review task to needs_review",
                                            )
                                            .await
                                        {
                                            tracing::warn!(
                                                task_id,
                                                ?e,
                                                "failed to finalize incomplete internal review runs on shutdown"
                                            );
                                        }
                                        review_reset_count += 1;
                                    }
                                }
                            }
                        }
                        if reset_count > 0 {
                            tracing::info!(reset_count, "reset in_progress tasks to routed for re-dispatch");
                        }
                        if review_reset_count > 0 {
                            tracing::info!(review_reset_count, "reset in_review tasks to needs_review for re-dispatch");
                        }

                        // Kill all orch-managed tmux sessions so stale sessions don't
                        // block dispatch after restart (session_exists check).
                        let mut killed_sessions = Vec::new();
                        if let Ok(sessions) = tmux.list_sessions().await {
                            for session in &sessions {
                                if session.name.starts_with("orch-") {
                                    if let Err(e) = tmux.kill_session(&session.name).await {
                                        tracing::warn!(session = %session.name, error = %e, "failed to kill tmux session");
                                    } else {
                                        killed_sessions.push(session.name.clone());
                                    }
                                }
                            }
                            if !killed_sessions.is_empty() {
                                tracing::info!(killed = killed_sessions.len(), "killing orch tmux sessions on shutdown");
                            }
                        }

                        // Wait for sessions to actually die before exiting.
                        // Prevents race where process exits before tmux finishes cleanup.
                        if !killed_sessions.is_empty() {
                            let still_alive = tmux
                                .wait_for_sessions_dead(
                                    &killed_sessions,
                                    std::time::Duration::from_millis(100),
                                    std::time::Duration::from_secs(5),
                                )
                                .await;
                            if still_alive > 0 {
                                tracing::warn!(
                                    still_alive,
                                    "some tmux sessions did not terminate cleanly - they will be cleaned up on startup"
                                );
                            }
                            tracing::info!(killed = killed_sessions.len() - still_alive, "confirmed tmux sessions terminated");
                        }
                    },
                ).await;

                if shutdown_result.is_err() {
                    tracing::error!(
                        timeout_secs = config.graceful_shutdown_timeout.as_secs(),
                        "graceful shutdown timed out, forcing exit"
                    );
                }

                break;
            }
        }
    }

    // Clean up event bus port file and service version file
    events::cleanup_port_file().await;
    crate::cluster::local::cleanup().await;
    events::cleanup_version_file();

    // transport and channels drop here at end of scope
    let _ = transport;
    tracing::info!("orch engine stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn in_review_task(store: &TaskStore, number: &str) -> i64 {
        let id = store
            .upsert_external(&crate::store::UpsertExternal {
                repo: "owner/repo",
                ext_id: number,
                title: "Under review",
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
        id
    }

    #[tokio::test]
    async fn a_review_without_a_session_goes_back_to_needs_review_and_a_live_one_stays() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> =
            Arc::new(crate::backends::test_helpers::NoopBackend);
        let task_manager =
            TaskManager::with_store(backend, store.clone(), "owner/repo".to_string());
        let tmux = TmuxManager::isolated();
        let orphaned = in_review_task(&store, "11").await;
        let live = in_review_task(&store, "12").await;
        let session = tmux
            .create_session("owner/repo", "12-review", "/tmp", "sleep 30", &[])
            .await
            .unwrap();
        let _guard = crate::tmux::SessionGuard(session);

        let reset = reset_orphaned_reviews(&task_manager, &store, "owner/repo", &tmux).await;

        assert_eq!(reset, 1);
        assert_eq!(
            store.get(orphaned).await.unwrap().status,
            crate::store::TaskStatus::NeedsReview
        );
        assert_eq!(
            store.get(live).await.unwrap().status,
            crate::store::TaskStatus::InReview
        );
    }

    #[test]
    fn growing_max_parallel_lets_more_tasks_run_without_a_restart() {
        let semaphore = Arc::new(Semaphore::new(4));
        let held: Vec<_> = (0..4)
            .map(|_| semaphore.clone().try_acquire_owned().unwrap())
            .collect();
        assert!(
            semaphore.clone().try_acquire_owned().is_err(),
            "pool is full"
        );

        let in_force = resize_dispatch_pool(&semaphore, 4, 8);
        assert_eq!(in_force, 8);

        let grown: Vec<_> = (0..4)
            .map(|_| semaphore.clone().try_acquire_owned().unwrap())
            .collect();
        assert_eq!(held.len() + grown.len(), 8, "8 tasks run at once");
        assert!(
            semaphore.clone().try_acquire_owned().is_err(),
            "pool is full"
        );
    }

    #[test]
    fn shrinking_max_parallel_never_cancels_a_running_task() {
        let semaphore = Arc::new(Semaphore::new(8));
        let mut held: Vec<_> = (0..8)
            .map(|_| semaphore.clone().try_acquire_owned().unwrap())
            .collect();

        // Nothing is free, so all 8 tasks keep their permits and keep running.
        let mut in_force = resize_dispatch_pool(&semaphore, 8, 4);
        assert_eq!(in_force, 8);
        assert_eq!(held.len(), 8);

        // Three finish. Their permits are taken back instead of handed out again.
        held.truncate(5);
        in_force = resize_dispatch_pool(&semaphore, in_force, 4);
        assert_eq!(in_force, 5);
        assert!(semaphore.clone().try_acquire_owned().is_err());

        // One more finishes and the pool settles at the new limit.
        held.truncate(4);
        in_force = resize_dispatch_pool(&semaphore, in_force, 4);
        assert_eq!(in_force, 4);
        assert!(semaphore.clone().try_acquire_owned().is_err());

        // A slot freed after that is handed out, since 4 is now the real limit.
        held.truncate(3);
        assert!(semaphore.clone().try_acquire_owned().is_ok());
    }

    #[test]
    fn a_project_added_to_the_config_is_picked_up_and_running_ones_are_not_respawned() {
        let configured = vec![
            (
                "owner/one".to_string(),
                std::path::PathBuf::from("/srv/one"),
            ),
            (
                "owner/two".to_string(),
                std::path::PathBuf::from("/srv/two"),
            ),
        ];
        let active: std::collections::HashSet<String> =
            ["owner/one".to_string()].into_iter().collect();

        let (added, removed) = diff_configured_projects(&configured, &active);
        assert_eq!(
            added,
            vec![(
                "owner/two".to_string(),
                std::path::PathBuf::from("/srv/two")
            )],
            "only the new project is started, so owner/one keeps its single set of subscribers"
        );
        assert!(removed.is_empty());
    }

    #[test]
    fn a_project_dropped_from_the_config_is_reported_and_not_restarted() {
        let configured = vec![(
            "owner/one".to_string(),
            std::path::PathBuf::from("/srv/one"),
        )];
        let active: std::collections::HashSet<String> =
            ["owner/one".to_string(), "owner/gone".to_string()]
                .into_iter()
                .collect();

        let (added, removed) = diff_configured_projects(&configured, &active);
        assert!(added.is_empty(), "nothing new to start");
        assert_eq!(removed, vec!["owner/gone".to_string()]);
    }

    #[test]
    fn an_unchanged_project_list_starts_nothing() {
        let configured = vec![(
            "owner/one".to_string(),
            std::path::PathBuf::from("/srv/one"),
        )];
        let active: std::collections::HashSet<String> =
            ["owner/one".to_string()].into_iter().collect();

        assert_eq!(
            diff_configured_projects(&configured, &active),
            (vec![], vec![])
        );
    }

    #[test]
    fn restart_only_keys_are_the_settings_read_once_at_startup() {
        // Guards the docs table: every key listed here is documented as needing
        // `orch service restart`, and nothing reloadable may be added to it.
        assert_eq!(RESTART_ONLY_KEYS, ["channels", "webhook", "relay"]);
    }

    #[test]
    fn a_changed_restart_only_key_is_reported_and_an_unchanged_one_is_not() {
        let before = vec![Some("bot_token: a\n".to_string()), None];

        assert!(
            log_restart_only_changes(&before, &before.clone()).is_empty(),
            "a reload that did not touch these keys must stay quiet"
        );

        // An edited token and a newly added webhook block are both changes
        let after = vec![
            Some("bot_token: b\n".to_string()),
            Some("port: 8080\n".to_string()),
        ];
        assert_eq!(
            log_restart_only_changes(&before, &after),
            ["channels", "webhook"]
        );

        // Only the edited key is reported when the other is untouched
        let token_only = vec![Some("bot_token: b\n".to_string()), None];
        assert_eq!(log_restart_only_changes(&before, &token_only), ["channels"]);
    }

    #[test]
    fn resize_dispatch_pool_is_a_noop_at_the_current_limit() {
        let semaphore = Arc::new(Semaphore::new(4));
        assert_eq!(resize_dispatch_pool(&semaphore, 4, 4), 4);
        assert_eq!(semaphore.available_permits(), 4);
    }

    #[test]
    fn engine_config_defaults() {
        let config = EngineConfig::default();
        assert_eq!(config.tick_interval, std::time::Duration::from_secs(10));
        assert_eq!(config.sync_interval, std::time::Duration::from_secs(45));
        assert_eq!(config.max_parallel, 4);
        assert_eq!(config.stuck_timeout, 1800);
        assert_eq!(config.no_session_stuck_timeout, 600);
        assert!(
            config.no_session_stuck_timeout < config.stuck_timeout,
            "no_session_stuck_timeout must be shorter than stuck_timeout"
        );
        assert_eq!(
            config.graceful_shutdown_timeout,
            std::time::Duration::from_secs(600)
        );
        assert_eq!(config.upgrade_check_interval, 3600);
    }

    #[test]
    fn engine_config_from_config_uses_defaults_when_no_config() {
        let config = EngineConfig::from_config();
        assert_eq!(config.tick_interval, std::time::Duration::from_secs(10));
        assert_eq!(config.sync_interval, std::time::Duration::from_secs(45));
        // max_parallel may be overridden by user config; just check it's reasonable
        assert!(config.max_parallel >= 1 && config.max_parallel <= 64);
        assert_eq!(config.stuck_timeout, 1800);
        assert_eq!(config.no_session_stuck_timeout, 600);
        assert_eq!(
            config.graceful_shutdown_timeout,
            std::time::Duration::from_secs(600)
        );
        assert_eq!(config.upgrade_check_interval, 3600);
    }

    #[test]
    fn engine_config_no_session_timeout_is_under_15_minutes() {
        let config = EngineConfig::default();
        assert!(
            config.no_session_stuck_timeout <= 900,
            "no_session_stuck_timeout ({}) exceeds 15 minutes",
            config.no_session_stuck_timeout
        );
    }

    #[test]
    fn upgrade_check_runs_immediately_when_never_checked() {
        assert!(should_run_upgrade_check(None, 3600));
    }

    #[test]
    fn upgrade_check_waits_for_interval_after_first_run() {
        assert!(!should_run_upgrade_check(
            Some(std::time::Instant::now()),
            3600
        ));
    }

    #[test]
    fn upgrade_check_can_be_disabled() {
        assert!(!should_run_upgrade_check(None, 0));
    }

    #[test]
    fn configured_agents_parses_yaml_block_array() {
        // config::get returns serde_norway::to_string for arrays, which is YAML block format
        let yaml_str = "- claude\n- codex\n- opencode\n- olm\n";
        let parsed: Vec<String> = serde_norway::from_str(yaml_str).unwrap();
        assert_eq!(parsed, vec!["claude", "codex", "opencode", "olm"]);
    }

    #[test]
    fn configured_agents_parses_json_inline_array() {
        let json_str = r#"["claude","codex","olm"]"#;
        let parsed: Vec<String> = serde_json::from_str(json_str).unwrap();
        assert_eq!(parsed, vec!["claude", "codex", "olm"]);
    }

    #[test]
    fn configured_agents_fallback_returns_default_agents() {
        // When no config is present, configured_agents() returns DEFAULT_AGENTS
        let defaults = configured_agents();
        assert!(defaults.contains(&"claude".to_string()));
        assert!(defaults.contains(&"codex".to_string()));
        assert!(defaults.contains(&"opencode".to_string()));
    }

    /// Integration test that requires a fully working tmux environment with
    /// reliable capture-pane timing. Runs locally but not in CI (GitHub Actions
    /// has tmux installed but capture-pane timing is unreliable).
    ///
    /// DO NOT REMOVE #[ignore] — this test was un-ignored in PR #608 which
    /// broke CI. The `tmux -V` skip check is insufficient because tmux CAN
    /// create sessions on CI runners, but capture-pane output is empty or
    /// delayed, causing the assertion to fail after a 10s timeout.
    #[tokio::test]
    #[ignore]
    async fn integration_channel_to_tmux_to_capture() {
        use crate::channels::capture::CaptureService;
        use crate::channels::transport::Transport;
        use crate::tmux::{SessionGuard, TmuxManager};
        use std::sync::Arc;

        // Create transport and capture service
        let transport = Arc::new(Transport::new());
        let capture = Arc::new(CaptureService::new(transport.clone()));
        let tmux = TmuxManager::new();

        let task_id = "testtask1".to_string();
        let session_name = format!("orch-test-{}", task_id);

        // Start a detached tmux session
        let _ = tmux
            .command()
            .args(["new-session", "-d", "-s", &session_name])
            .output()
            .await
            .expect("failed to start tmux session");

        // Guard ensures session cleanup even on panic
        let _guard = SessionGuard(session_name.clone());

        // Register session with capture service and transport binding
        capture
            .register_session("owner/repo", &task_id, &session_name)
            .await;
        transport
            .bind(
                "owner/repo",
                &task_id,
                &session_name,
                "telegram",
                "12345",
                None,
            )
            .await;

        // Spawn capture.run() which exits when session is unregistered
        let capture_clone = capture.clone();
        let capture_handle = tokio::spawn(async move { capture_clone.run().await });

        // Subscribe to transport output for this task
        let mut rx = transport
            .subscribe("owner/repo", &task_id)
            .await
            .expect("no subscription");

        // Send a message into the tmux session via tmux send-keys (simulate channel input)
        let send = tmux
            .command()
            .args([
                "send-keys",
                "-t",
                &session_name,
                "echo from-channel",
                "Enter",
            ])
            .output()
            .await
            .expect("failed to send keys");
        assert!(send.status.success());

        // Wait for an output chunk from capture -> transport
        let mut got = false;
        for _ in 0..20 {
            if let Ok(Ok(c)) =
                tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await
            {
                if c.content.contains("from-channel") {
                    got = true;
                    break;
                }
            }
        }

        // Clean up: unregister session (tmux kill handled by SessionGuard on drop)
        capture.unregister_session("owner/repo", &task_id).await;

        // Wait for capture.run to finish
        let _ = capture_handle.await;

        assert!(got, "did not observe tmux output via capture/transport");
    }

    // ── read_project_channel_config tests ─────────────────────────────────

    #[test]
    fn read_project_channel_config_returns_defaults_when_no_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = read_project_channel_config(dir.path());
        assert!(cfg.telegram_topic_id.is_none());
        assert!(cfg.discord_channel_id.is_none());
    }

    #[test]
    fn read_project_channel_config_parses_telegram_and_discord() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orch_yml = dir.path().join(".orch.yml");
        std::fs::write(
            &orch_yml,
            "channels:\n  telegram:\n    topic_id: \"42\"\n  discord:\n    channel_id: \"9999\"\n",
        )
        .expect("write .orch.yml");
        let cfg = read_project_channel_config(dir.path());
        assert_eq!(cfg.telegram_topic_id.as_deref(), Some("42"));
        assert_eq!(cfg.discord_channel_id.as_deref(), Some("9999"));
    }

    #[test]
    fn read_project_channel_config_handles_partial_channels() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orch_yml = dir.path().join(".orch.yml");
        std::fs::write(&orch_yml, "channels:\n  telegram:\n    topic_id: \"7\"\n")
            .expect("write .orch.yml");
        let cfg = read_project_channel_config(dir.path());
        assert_eq!(cfg.telegram_topic_id.as_deref(), Some("7"));
        assert!(cfg.discord_channel_id.is_none());
    }

    #[test]
    fn read_project_channel_config_handles_malformed_yaml() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orch_yml = dir.path().join(".orch.yml");
        std::fs::write(&orch_yml, "channels: [\nbad yaml").expect("write .orch.yml");
        let cfg = read_project_channel_config(dir.path());
        // Should return defaults (not panic) and emit a warn! log with the parse error.
        assert!(cfg.telegram_topic_id.is_none());
        assert!(cfg.discord_channel_id.is_none());
    }

    #[test]
    #[cfg(unix)]
    fn read_project_channel_config_handles_unreadable_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A directory fails to read for every user, root included, where chmod 000 does not
        std::fs::create_dir(dir.path().join(".orch.yml")).expect("create .orch.yml dir");
        let cfg = read_project_channel_config(dir.path());
        // Should return defaults (not panic) and emit a warn! log with the read error.
        assert!(cfg.telegram_topic_id.is_none());
        assert!(cfg.discord_channel_id.is_none());
    }
}
