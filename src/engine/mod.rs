//! Engine — the core orchestration loop.
//!
//! The engine is the central coordinator. It owns the tick loop, backend
//! connections, channel registry, transport layer, and tmux session manager.
//!
//! Submodules handle the heavy lifting:
//! - [`tick`] — core tick phases (session polling, stuck recovery, routing, dispatch)
//! - [`sync`] — periodic sync operations (cleanup, PR review, mentions)
//! - [`review`] — PR review pipeline (review agent, auto-merge, change handling)
//! - [`cleanup`] — worktree cleanup and merged-PR detection
//!
//! This file contains struct definitions, project initialization, and the
//! main event loop (`serve()`).

pub mod auto_merge;
pub mod channel_handler;
pub mod ci_bootstrap;
pub mod cleanup;
pub mod commands;
pub mod dependencies;
pub mod dispatch_guard;
pub mod events;
pub mod jobs;
pub mod priority;
pub mod progress;
pub mod push_retry;
pub mod relay;
pub mod review;
pub mod review_gate;
pub mod review_poll;
pub mod review_shipcheck;
pub mod router;
pub mod runner;
mod serve;
mod startup;
pub mod subscribers;
pub mod sync;
pub mod tasks;
pub mod tick;

/// Standard Orch attribution footer for issue bodies, PR bodies, and comments.
///
/// Append to every user-visible string posted to GitHub so activity is
/// clearly attributed to orch rather than appearing as a human post.
use crate::backends::{ExternalBackend, ExternalId};
use crate::channels::capture::CaptureService;
use crate::config;
use crate::engine::router::{Router, RouterConfig};
use crate::engine::tasks::TaskManager;
use crate::repo_context::REPO_CONTEXT;
use crate::store::{TaskStatus, TaskStore};
use crate::tmux::TmuxManager;
use runner::WeightSignal;
// AtomicBool/Ordering removed — shutdown is now immediate (reset tasks + break)
use crate::backends::Status;
use crate::backends::github::GitHubBackend;
use crate::engine::review as engine_review;
use crate::engine::tasks as engine_tasks;
use crate::secret_ref;
use futures::future;
pub use serve::serve;
use startup::{
    check_and_notify_upgrade, fetch_latest_release_version, init_project_engines,
    open_shared_store, read_project_channel_config, reconcile_startup_estimates,
    reconcile_startup_worktrees, send_startup_escalation_notification, should_run_upgrade_check,
};
use std::collections::HashSet;
use std::fmt;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::broadcast::Sender;
use tokio::sync::{Notify, RwLock, Semaphore, mpsc};
use tokio::task;
use tokio::time;

pub fn orch_footer() -> &'static str {
    "\n\n---\n*Posted by [Orch](https://github.com/gabrielkoerich/orch)*"
}

/// Build a standard attribution footer for GitHub comments posted by orch bots.
///
/// `verb` is "Created", "Reviewed", or "Commented" depending on context.
pub fn attribution_footer(verb: &str, agent: &str, model: Option<&str>) -> String {
    match model {
        Some(m) => format!(
            "\n\n---\n*{verb} by {agent}[bot] via [Orch](https://github.com/gabrielkoerich/orch) using `{m}`*"
        ),
        None => format!(
            "\n\n---\n*{verb} by {agent}[bot] via [Orch](https://github.com/gabrielkoerich/orch)*"
        ),
    }
}

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
    pub project_dir: PathBuf,
    pub backend: Arc<dyn ExternalBackend>,
    pub task_manager: Arc<TaskManager>,
    pub runner: Arc<runner::TaskRunner>,
    pub store: Arc<TaskStore>,
}

impl Debug for ProjectEngine {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProjectEngine")
            .field("repo", &self.repo)
            .field("project_dir", &self.project_dir)
            .finish_non_exhaustive()
    }
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
        .list_by_status(repo, TaskStatus::InReview)
        .await
        .unwrap_or_default()
        .iter()
        .map(|t| engine_tasks::store_task_to_external(t).id)
        .collect();
    if let Ok(legacy) = task_manager
        .list_internal_by_status(TaskStatus::InReview)
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
fn start_relay_client(store: &Arc<TaskStore>, notify: &Arc<Notify>) {
    // The global config only: a project file must never point a secret at its own endpoint
    let url = config::get_global("relay.url").unwrap_or_default();
    let store = Arc::clone(store);

    if url.is_empty() {
        return;
    }

    let token_ref = config::get_global("relay.token").unwrap_or_default();
    let notify = Arc::clone(notify);
    tokio::spawn(async move {
        let mut warned = false;
        let token = loop {
            let reference = token_ref.clone();
            let read = task::spawn_blocking(move || secret_ref::resolve(&reference))
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

            time::sleep(Duration::from_secs(300)).await;
        };

        relay::run(url, token, store, notify).await;
    });
}

/**
 * Runs `sync_tick` for every project at once, then the tail that is the same for all of them,
 * in a spawned task. Sync time is the slowest project instead of the sum of all of them
 * (#3864). The caller must already hold `sync_in_progress`; this releases it when done
 **/
#[expect(
    clippy::too_many_arguments,
    reason = "the spawned task takes its own handle to each piece of engine state"
)]
fn spawn_sync_pass(
    project_engines: &[ProjectEngine],
    tmux: &Arc<TmuxManager>,
    engine_config: &EngineConfig,
    router: &Arc<RwLock<Router>>,
    dispatching: &Arc<dashmap::DashMap<String, String>>,
    auto_merge_in_flight: &Arc<dashmap::DashSet<String>>,
    sync_in_progress: &Arc<AtomicBool>,
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
        let started = Instant::now();
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
        future::join_all(passes).await;

        sync::sync_shared_tail(&router, engines.first().map(|e| &e.store)).await;
        tracing::info!(
            trigger,
            elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "sync tick complete"
        );
        guard.store(false, Ordering::Release);
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
    if let Err(e) = Box::pin(sync::sync_tick(
        &engine.backend,
        tmux,
        &engine.repo,
        engine_config,
        router,
        &engine.task_manager,
        &engine.store,
        dispatching,
        auto_merge_in_flight,
    ))
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
    if let Ok(agents_str) = config::get("agents")
        && !agents_str.is_empty()
        && agents_str != "[]"
    {
        // config::get returns serde_norway::to_string for arrays, which is
        // YAML block format ("- claude\n- codex\n..."). Parse as YAML first.
        if let Ok(agents_arr) = serde_norway::from_str::<Vec<String>>(&agents_str)
            && !agents_arr.is_empty()
        {
            return agents_arr;
        }
        // Fallback: try JSON (inline arrays) or comma-separated
        if let Ok(agents_arr) = serde_json::from_str::<Vec<String>>(&agents_str)
            && !agents_arr.is_empty()
        {
            return agents_arr;
        }
    }
    router::config::DEFAULT_AGENTS
        .iter()
        .map(ToString::to_string)
        .collect()
}

/// Seconds a live session may run past its own timeout before stuck recovery takes it. The
/// `timeout` wrapper ends the agent at the limit, and the runner still collects the result
pub const RUN_TIMEOUT_GRACE_SECS: u64 = 300;

/// The limit the `timeout` wrapper puts on one agent run, per complexity
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunTimeouts {
    pub base: u64,
    pub medium: u64,
    pub complex: u64,
}

impl Default for RunTimeouts {
    fn default() -> Self {
        Self {
            base: 1800,
            medium: 1800,
            complex: 1800,
        }
    }
}

impl RunTimeouts {
    /// `workflow.timeout_seconds` (at least 1800), raised by
    /// `workflow.timeout_by_complexity.{medium,complex}` but never below the base
    pub fn from_config() -> Self {
        let read =
            |key: &str| -> Option<u64> { config::get(key).ok().and_then(|s| s.parse().ok()) };
        let base = read("workflow.timeout_seconds").unwrap_or(1800).max(1800);
        Self {
            base,
            medium: read("workflow.timeout_by_complexity.medium")
                .unwrap_or(base)
                .max(base),
            complex: read("workflow.timeout_by_complexity.complex")
                .unwrap_or(base)
                .max(base),
        }
    }

    pub fn for_complexity(&self, complexity: Option<&str>) -> u64 {
        match complexity {
            Some("complex") => self.complex,
            Some("medium") => self.medium,
            _ => self.base,
        }
    }
}

/// Engine configuration.
#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// Main tick interval
    pub tick_interval: Duration,
    /// GitHub sync interval (cleanup, PR review, mentions)
    pub sync_interval: Duration,
    /// Webhook health check interval (seconds)
    pub webhook_health_check_interval: Option<Duration>,
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
    /// Per-run agent timeouts. A live session is not stuck before its own timeout runs out
    pub run_timeouts: RunTimeouts,
    /// Auto-create follow-up tasks when PR reviews request changes
    pub auto_create_followup_on_changes: bool,
    /// Graceful shutdown timeout — how long to wait for running agents before exiting.
    pub graceful_shutdown_timeout: Duration,
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
            tick_interval: Duration::from_secs(10),
            sync_interval: Duration::from_secs(45),
            webhook_health_check_interval: Some(Duration::from_secs(60)),
            max_parallel: 4,
            stuck_timeout: 1800,
            no_session_stuck_timeout: 600,
            in_review_no_session_stuck_timeout: 1920,
            run_timeouts: RunTimeouts::default(),
            auto_create_followup_on_changes: true,
            graceful_shutdown_timeout: Duration::from_secs(600),
            silence_grace_period: 300,
            silence_cooldown: 3600,
            upgrade_check_interval: 3600,
            startup_failure_escalation_secs: 3600,
        }
    }
}

impl EngineConfig {
    pub fn tick_interval_secs(&self) -> u64 {
        self.tick_interval.as_secs()
    }

    /// How often to check the webhook server's health, 60 s unless configured
    pub fn webhook_health_check_interval(&self) -> Duration {
        self.webhook_health_check_interval
            .unwrap_or(Duration::from_secs(60))
    }

    pub fn graceful_shutdown_secs(&self) -> u64 {
        self.graceful_shutdown_timeout.as_secs()
    }

    /// Stuck limit for an in_progress task whose session is alive: `stuck_timeout`, raised to the
    /// run's own timeout plus `RUN_TIMEOUT_GRACE_SECS`
    pub fn live_session_stuck_limit(&self, complexity: Option<&str>) -> u64 {
        self.stuck_timeout.max(
            self.run_timeouts
                .for_complexity(complexity)
                .saturating_add(RUN_TIMEOUT_GRACE_SECS),
        )
    }

    /// Load engine configuration from config files, falling back to defaults.
    pub fn from_config() -> Self {
        let mut config = Self::default();

        if let Ok(val) = config::get("engine.tick_interval") {
            if let Ok(secs) = val.parse::<u64>() {
                config.tick_interval = Duration::from_secs(secs);
            } else {
                tracing::warn!(key = "engine.tick_interval", value = %val, default_secs = config.tick_interval.as_secs(), "invalid value for engine.tick_interval, using default");
            }
        }

        if let Ok(val) = config::get("engine.sync_interval") {
            if let Ok(secs) = val.parse::<u64>() {
                config.sync_interval = Duration::from_secs(secs);
            } else {
                tracing::warn!(key = "engine.sync_interval", value = %val, default_secs = config.sync_interval.as_secs(), "invalid value for engine.sync_interval, using default");
            }
        }

        if let Ok(val) = config::get("engine.max_parallel") {
            if let Ok(n) = val.parse::<usize>() {
                config.max_parallel = n;
            } else {
                tracing::warn!(key = "engine.max_parallel", value = %val, default = config.max_parallel, "invalid value for engine.max_parallel, using default");
            }
        }

        if let Ok(val) = config::get("engine.stuck_timeout") {
            if let Ok(secs) = val.parse::<u64>() {
                config.stuck_timeout = secs;
            } else {
                tracing::warn!(key = "engine.stuck_timeout", value = %val, default_secs = config.stuck_timeout, "invalid value for engine.stuck_timeout, using default");
            }
        }

        if let Ok(val) = config::get("engine.no_session_stuck_timeout") {
            if let Ok(secs) = val.parse::<u64>() {
                config.no_session_stuck_timeout = secs;
            } else {
                tracing::warn!(key = "engine.no_session_stuck_timeout", value = %val, default_secs = config.no_session_stuck_timeout, "invalid value for engine.no_session_stuck_timeout, using default");
            }
        }

        let mut in_review_from_config = false;
        if let Ok(val) = config::get("engine.in_review_no_session_stuck_timeout") {
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
        let review_timeout = engine_review::review_timeout_seconds();
        let floored = engine_review::no_session_review_floor(
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

        config.run_timeouts = RunTimeouts::from_config();

        if let Ok(val) = config::get("engine.webhook_health_check_interval") {
            if let Ok(secs) = val.parse::<u64>() {
                if secs > 0 {
                    config.webhook_health_check_interval = Some(Duration::from_secs(secs));
                } else {
                    config.webhook_health_check_interval = None;
                }
            } else {
                tracing::warn!(key = "engine.webhook_health_check_interval", value = %val, default = ?config.webhook_health_check_interval, "invalid value for engine.webhook_health_check_interval, using default");
            }
        }

        if let Ok(val) = config::get("workflow.auto_create_followup_on_changes") {
            config.auto_create_followup_on_changes = !val.eq_ignore_ascii_case("false");
        }

        if let Ok(val) = config::get("engine.graceful_shutdown_timeout") {
            if let Ok(secs) = val.parse::<u64>() {
                config.graceful_shutdown_timeout = Duration::from_secs(secs);
            } else {
                tracing::warn!(key = "engine.graceful_shutdown_timeout", value = %val, default_secs = config.graceful_shutdown_timeout.as_secs(), "invalid value for engine.graceful_shutdown_timeout, using default");
            }
        }

        if let Ok(val) = config::get("engine.silence_grace_period") {
            if let Ok(secs) = val.parse::<u64>() {
                config.silence_grace_period = secs;
            } else {
                tracing::warn!(key = "engine.silence_grace_period", value = %val, default = config.silence_grace_period, "invalid value for engine.silence_grace_period, using default");
            }
        }

        if let Ok(val) = config::get("engine.silence_cooldown") {
            if let Ok(secs) = val.parse::<u64>() {
                config.silence_cooldown = secs;
            } else {
                tracing::warn!(key = "engine.silence_cooldown", value = %val, default = config.silence_cooldown, "invalid value for engine.silence_cooldown, using default");
            }
        }

        if let Ok(val) = config::get("engine.upgrade_check_interval") {
            if let Ok(secs) = val.parse::<u64>() {
                config.upgrade_check_interval = secs;
            } else {
                tracing::warn!(key = "engine.upgrade_check_interval", value = %val, default = config.upgrade_check_interval, "invalid value for engine.upgrade_check_interval, using default");
            }
        }

        if let Ok(val) = config::get("engine.startup_failure_escalation_secs") {
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
    chrono::DateTime::from_timestamp(reset_at, 0).map_or_else(
        || reset_at.to_string(),
        |t| {
            t.with_timezone(&chrono::Local)
                .format("%H:%M:%S %Z")
                .to_string()
        },
    )
}

/// Connect one project's backend and build its task manager and runner over
/// the shared store.
///
/// `event_tx` wires status transitions into the event bus; pass `None` during
/// startup, where the bus does not exist yet and task managers are rebuilt
/// once it does
async fn connect_project_engine(
    repo: String,
    project_dir: PathBuf,
    store: Arc<TaskStore>,
    event_tx: Option<Sender<events::TaskEvent>>,
) -> anyhow::Result<ProjectEngine> {
    let backend: Arc<dyn ExternalBackend> = Arc::new(GitHubBackend::new(repo.clone())?);

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
        Some(tx) => {
            TaskManager::with_events(Arc::clone(&backend), Arc::clone(&store), repo.clone(), tx)
        }
        None => TaskManager::with_store(Arc::clone(&backend), Arc::clone(&store), repo.clone()),
    });
    let runner = Arc::new(runner::TaskRunner::new(repo.clone()).with_store(Arc::clone(&store)));

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
    configured: &[(String, PathBuf)],
    active_repos: &HashSet<String>,
) -> (Vec<(String, PathBuf)>, Vec<String>) {
    let configured_repos: HashSet<&str> = configured.iter().map(|(r, _)| r.as_str()).collect();

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
#[expect(
    clippy::too_many_arguments,
    reason = "a new project engine needs every shared engine handle"
)]
async fn sync_project_engines(
    store: &Arc<TaskStore>,
    project_engines: &mut Vec<ProjectEngine>,
    active_repos: &mut HashSet<String>,
    event_bus: &events::EventBus,
    tmux: &Arc<TmuxManager>,
    capture: &Arc<CaptureService>,
    semaphore: &Arc<Semaphore>,
    review_semaphore: &Arc<Semaphore>,
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
            Arc::clone(store),
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
            Arc::clone(&engine.backend),
            Arc::clone(tmux),
            Arc::clone(&engine.runner),
            Arc::clone(capture),
            Arc::clone(semaphore),
            Arc::clone(&engine.task_manager),
            weight_tx.clone(),
            Arc::clone(router),
            Arc::clone(dispatching),
            Arc::clone(&engine.store),
            engine.repo.clone(),
        );
        subscribers::review::spawn(
            event_bus.subscribe(),
            Arc::clone(&engine.backend),
            Arc::clone(tmux),
            Arc::clone(review_semaphore),
            Arc::clone(&engine.task_manager),
            Arc::clone(router),
            Arc::clone(dispatching),
            Arc::clone(&engine.store),
            engine.repo.clone(),
        );
        subscribers::unblock::spawn(
            event_bus.subscribe(),
            Arc::clone(&engine.backend),
            Arc::clone(&engine.task_manager),
            Arc::clone(&engine.store),
            engine.repo.clone(),
        );

        active_repos.insert(repo.clone());
        project_engines.push(engine);
        tracing::info!(repo = %repo, "project added from config, now syncing");
    }
}

#[cfg(test)]
mod tests;
