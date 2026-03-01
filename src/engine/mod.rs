//! Engine — the core orchestration loop.
//!
//! Replaces `serve.sh` + `poll.sh` + `jobs_tick.sh` with a single async loop.
//! The engine owns:
//! - The tick loop (poll for new tasks, check job schedules)
//! - The backend connection (GitHub Issues)
//! - The channel registry (all I/O surfaces)
//! - The transport layer (routes messages ↔ tmux sessions)
//! - The tmux session manager (create, monitor, cleanup)
//!
//! All state transitions go through the engine. Channels and backends are
//! pluggable — the engine doesn't know which ones are active.
//!
//! The engine owns the full loop: task polling, routing, agent invocation,
//! git workflow, prompt building, and result handling — all in Rust.

pub mod commands;
pub mod internal_tasks;
pub mod jobs;
pub mod pr_review;
pub mod router;
pub mod runner;
pub mod sync_ops;
pub mod tasks;

use crate::backends::{ExternalBackend, ExternalTask, Status};
use crate::channels::capture::CaptureService;
use crate::channels::discord::DiscordChannel;
use crate::channels::github::start_webhook_server;
use crate::channels::notification::{NotificationLevel, TaskNotification};
use crate::channels::telegram::TelegramChannel;
use crate::channels::tmux::TmuxChannel;
use crate::channels::transport::Transport;
use crate::channels::{Channel, ChannelRegistry, IncomingMessage, OutgoingMessage};
use crate::config;
use crate::db::Db;
use crate::engine::router::{get_route_result, Router};
use crate::github::http::GhHttp;
use crate::engine::tasks::TaskManager;
use crate::sidecar;
use crate::tmux::TmuxManager;
use runner::{TaskRunner, WeightSignal};
use std::sync::Arc;
use tokio::sync::{mpsc, Notify, RwLock, Semaphore};

/// Per-project engine state.
///
/// Each project has its own backend, task runner, and task manager,
/// but they share the global tmux manager, transport, and semaphore.
pub struct ProjectEngine {
    pub repo: String,
    pub backend: Arc<dyn ExternalBackend>,
    pub task_manager: Arc<TaskManager>,
    pub runner: Arc<TaskRunner>,
}

/// Engine configuration.
pub struct EngineConfig {
    /// Main tick interval
    pub tick_interval: std::time::Duration,
    /// GitHub sync interval (cleanup, PR review, mentions)
    pub sync_interval: std::time::Duration,
    /// Webhook health check interval (seconds)
    pub webhook_health_check_interval: Option<std::time::Duration>,
    /// Maximum parallel task executions
    pub max_parallel: usize,
    /// Stuck task timeout (seconds)
    pub stuck_timeout: u64,
    /// Auto-create follow-up tasks when PR reviews request changes
    pub auto_create_followup_on_changes: bool,
    /// Auto-close task (mark Done) when all PR reviews are approved.
    /// Note: this does NOT merge the PR itself -- only updates the task status.
    pub auto_close_task_on_approval: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            tick_interval: std::time::Duration::from_secs(10),
            sync_interval: std::time::Duration::from_secs(120),
            webhook_health_check_interval: Some(std::time::Duration::from_secs(60)),
            max_parallel: 4,
            stuck_timeout: 1800,
            auto_create_followup_on_changes: true,
            auto_close_task_on_approval: false,
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
            }
        }

        if let Ok(val) = crate::config::get("engine.sync_interval") {
            if let Ok(secs) = val.parse::<u64>() {
                config.sync_interval = std::time::Duration::from_secs(secs);
            }
        }

        if let Ok(val) = crate::config::get("engine.max_parallel") {
            if let Ok(n) = val.parse::<usize>() {
                config.max_parallel = n;
            }
        }

        if let Ok(val) = crate::config::get("engine.stuck_timeout") {
            if let Ok(secs) = val.parse::<u64>() {
                config.stuck_timeout = secs;
            }
        }

        if let Ok(val) = crate::config::get("engine.webhook_health_check_interval") {
            if let Ok(secs) = val.parse::<u64>() {
                if secs > 0 {
                    config.webhook_health_check_interval =
                        Some(std::time::Duration::from_secs(secs));
                } else {
                    config.webhook_health_check_interval = None;
                }
            }
        }

        if let Ok(val) = crate::config::get("workflow.auto_create_followup_on_changes") {
            config.auto_create_followup_on_changes = !val.eq_ignore_ascii_case("false");
        }

        if let Ok(val) = crate::config::get("workflow.auto_close_task_on_approval") {
            config.auto_close_task_on_approval = val.eq_ignore_ascii_case("true");
        }

        config
    }
}

/// Initialize all project engines from config.
///
/// Returns a vector of ProjectEngine, one for each configured project.
async fn init_project_engines() -> anyhow::Result<Vec<ProjectEngine>> {
    let repos = config::get_projects()?;
    tracing::info!(repos = ?repos, "loading projects from config");

    let mut engines = Vec::new();

    for repo in repos {
        tracing::info!(repo = %repo, "initializing project engine");

        // Initialize backend
        let backend: Arc<dyn ExternalBackend> =
            Arc::new(crate::backends::github::GitHubBackend::new(repo.clone()));

        // Health check — verifies `gh auth status` succeeds
        if let Err(e) = backend.health_check().await {
            tracing::warn!(
                repo = %repo,
                error = %e,
                "backend health check failed (`gh auth status`), skipping project"
            );
            continue;
        }
        tracing::info!(repo = %repo, backend = backend.name(), "backend connected");

        // Initialize database (shared between task manager and runner for metrics)
        let db = Arc::new(Db::open(&crate::db::default_path()?)?);
        db.migrate().await?;

        // Initialize task manager
        let task_manager = Arc::new(TaskManager::new(db.clone(), backend.clone()));

        // Task runner (with db for metrics)
        let runner = Arc::new(TaskRunner::new(repo.clone()).with_db(db.clone()));

        engines.push(ProjectEngine {
            repo,
            backend,
            task_manager,
            runner,
        });
    }

    if engines.is_empty() {
        let config_path = crate::home::config_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "~/.orch/config.yml".to_string());
        anyhow::bail!(
            "no valid projects configured — all backends failed health checks. \
             Config: {config_path}. Run `orch init` to set up a project."
        );
    }

    Ok(engines)
}

/// Start the orchestrator service.
///
/// This is the main entry point — called by `orch serve`.
pub async fn serve() -> anyhow::Result<()> {
    tracing::info!("orch engine starting");

    let mut config = EngineConfig::from_config();

    // Initialize internal database (shared across all projects)
    let db = Arc::new(Db::open(&crate::db::default_path()?)?);
    db.migrate().await?;
    tracing::info!("internal database ready");

    // Initialize project engines
    let mut project_engines = init_project_engines().await?;

    tracing::info!(
        project_count = project_engines.len(),
        "initialized project engines"
    );

    // Re-create task managers with shared db
    for engine in &mut project_engines {
        engine.task_manager = Arc::new(TaskManager::new(db.clone(), engine.backend.clone()));
    }

    // Initialize tmux manager (shared across all projects)
    let tmux = Arc::new(TmuxManager::new());

    // Initialize transport
    let transport = Arc::new(Transport::new());

    // Initialize capture service and start background loop
    let capture = Arc::new(CaptureService::new(transport.clone()));
    let capture_for_tick = capture.clone();
    tokio::spawn(async move {
        capture.start().await;
    });

    // Initialize and start notification channels
    let (channel_registry, channel_receivers) = setup_channels(&transport).await;
    spawn_channel_message_handlers(channel_receivers, &transport);
    spawn_notification_dispatcher(&transport, &channel_registry);

    // Initialize webhook server
    let webhook_notify = Arc::new(Notify::new());
    let mut webhook_state = setup_webhook(&project_engines, &transport, &webhook_notify).await;
    let mut last_webhook_health_check = std::time::Instant::now();

    // Agent router (selects agent + model per task) - shared across projects
    let router = Arc::new(RwLock::new(Router::from_config()));
    {
        let r = router.read().await;
        tracing::info!(
            mode = %r.config.mode,
            agents = ?r.available_agents,
            fallback = %r.config.fallback_executor,
            "router initialized"
        );
    }

    // Jobs config path (from .orchestrator.yml or global config)
    let mut jobs_path = jobs::resolve_jobs_path();

    // Concurrency limiter (shared across all projects)
    let semaphore = Arc::new(Semaphore::new(config.max_parallel));

    // Subscribe to config file changes for hot reload
    let mut config_rx = crate::config::subscribe();

    // Track sync interval
    let mut last_sync = std::time::Instant::now();

    // Channel for weight signals from task runners back to the router
    let (weight_tx, mut weight_rx) = mpsc::channel::<WeightSignal>(64);

    // Reset stale review_started flags on startup (prevents stuck reviews after restart)
    for engine in &project_engines {
        if let Ok(in_review) = engine.backend.list_by_status(Status::InReview).await {
            for task in &in_review {
                let _ = sidecar::set(&task.id.0, &["review_started=false".to_string()]);
            }
            if !in_review.is_empty() {
                tracing::info!(
                    repo = %engine.repo,
                    count = in_review.len(),
                    "reset review_started flags on startup"
                );
            }
        }
    }

    // Main loop
    tracing::info!(
        tick = ?config.tick_interval,
        sync = ?config.sync_interval,
        parallel = config.max_parallel,
        "entering main loop"
    );
    let mut interval = tokio::time::interval(config.tick_interval);

    // SIGTERM handler (launchd/systemd send SIGTERM to stop services)
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    loop {
        tokio::select! {
            _ = interval.tick() => {
                // Drain any pending weight signals from completed tasks
                while let Ok(signal) = weight_rx.try_recv() {
                    let mut rw = router.write().await;
                    match signal {
                        WeightSignal::RateLimited { ref agent } => {
                            rw.record_rate_limit(agent);
                        }
                        WeightSignal::Success { ref agent } => {
                            rw.record_success(agent);
                        }
                        WeightSignal::None => {}
                    }
                }

                // Tick weight recovery for rate-limited agents
                {
                    let mut rw = router.write().await;
                    rw.tick_weight_recovery();
                }

                // Skip tick/sync entirely if GitHub API is rate-limited
                if let Some(remaining) = GhHttp::is_rate_limited() {
                    tracing::warn!(
                        remaining_secs = remaining.as_secs(),
                        "GitHub API rate-limited, skipping tick"
                    );
                } else {
                    // Core tick: poll tasks for all projects
                    let router_guard = router.read().await;
                    for engine in &project_engines {
                        if let Err(e) = tick(
                            &engine.backend,
                            &tmux,
                            &engine.repo,
                            &engine.runner,
                            &capture_for_tick,
                            &semaphore,
                            &config,
                            &jobs_path,
                            &db,
                            &router_guard,
                            &router,
                            &engine.task_manager,
                            &weight_tx,
                            &transport,
                        ).await {
                            tracing::error!(repo = %engine.repo, ?e, "tick failed for project");
                        }
                    }
                    drop(router_guard);

                    // Periodic sync (less frequent)
                    if last_sync.elapsed() >= config.sync_interval {
                        for engine in &project_engines {
                            if let Err(e) = sync_tick(&engine.backend, &tmux, &engine.repo, &db, &config, &router).await {
                                tracing::error!(repo = %engine.repo, ?e, "sync tick failed for project");
                            }
                        }
                        last_sync = std::time::Instant::now();
                    }
                }

                // Periodic webhook health check
                if let Some(port) = webhook_state.port {
                    let health_check_interval = config.webhook_health_check_interval
                        .unwrap_or(std::time::Duration::from_secs(60));

                    if last_webhook_health_check.elapsed() >= health_check_interval {
                        let health = crate::channels::github::check_webhook_health(port).await;
                        if health != webhook_state.healthy {
                            webhook_state.healthy = health;
                            if webhook_state.healthy {
                                tracing::info!(port, "webhook health restored");
                            } else {
                                tracing::warn!(port, "webhook health check failed");
                            }
                        }
                        last_webhook_health_check = std::time::Instant::now();
                    }
                }
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

                    let router_guard = router.read().await;
                    for engine in &project_engines {
                        if let Err(e) = tick(
                            &engine.backend,
                            &tmux,
                            &engine.repo,
                            &engine.runner,
                            &capture_for_tick,
                            &semaphore,
                            &config,
                            &jobs_path,
                            &db,
                            &router_guard,
                            &router,
                            &engine.task_manager,
                            &weight_tx,
                            &transport,
                        ).await {
                            tracing::error!(repo = %engine.repo, ?e, "webhook-triggered tick failed");
                        }
                    }
                    drop(router_guard);
                }

                // Also reset the interval so we don't get a redundant tick right after
                interval.reset();
            }
            result = config_rx.recv() => {
                match result {
                    Ok(path) => {
                        tracing::info!(path = %path.display(), "config file changed, reloading");

                        // Reload engine config
                        let new_config = EngineConfig::from_config();
                        let tick_changed = new_config.tick_interval != config.tick_interval;
                        config = new_config;

                        // Reset tick interval if it changed
                        if tick_changed {
                            interval = tokio::time::interval(config.tick_interval);
                            tracing::info!(tick = ?config.tick_interval, "tick interval updated");
                        }

                        // Reload router config
                        {
                            let mut router_guard = router.write().await;
                            router_guard.reload();
                        }

                        // Reload jobs path
                        jobs_path = jobs::resolve_jobs_path();

                        tracing::info!(
                            tick = ?config.tick_interval,
                            sync = ?config.sync_interval,
                            parallel = config.max_parallel,
                            "config reloaded"
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(missed = n, "config change receiver lagged, reloading");
                        // Reload everything since we missed events
                        config = EngineConfig::from_config();
                        interval = tokio::time::interval(config.tick_interval);
                        {
                            let mut router_guard = router.write().await;
                            router_guard.reload();
                        }
                        jobs_path = jobs::resolve_jobs_path();
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        tracing::warn!("config change channel closed");
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received SIGINT, shutting down");
                break;
            }
            _ = sigterm.recv() => {
                tracing::info!("received SIGTERM, shutting down");
                break;
            }
        }
    }

    // Graceful shutdown
    tracing::info!("draining active sessions...");
    let sessions = tmux.list_sessions().await?;
    if !sessions.is_empty() {
        tracing::info!(
            count = sessions.len(),
            "active sessions will continue running"
        );
    }

    // transport and channels drop here at end of scope
    let _ = transport;
    tracing::info!("orch engine stopped");
    Ok(())
}

/// Core tick — runs every 10s.
///
/// Phases (matching v0 poll.sh):
/// 1. Monitor active tmux sessions (detect completions)
/// 2. Recover stuck in_progress tasks
/// 3. Dispatch new/routed tasks
#[allow(clippy::too_many_arguments)]
async fn tick(
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

    // Phase 1: Check active tmux sessions for completions
    check_session_completions(tmux, repo, capture).await;

    // Phase 2: Recover stuck tasks
    recover_stuck_tasks(backend, tmux, repo, task_manager, config).await?;

    // Phase 3a: Route new tasks
    route_new_tasks(backend, task_manager, router).await?;

    // Phase 3b: Dispatch routed tasks
    dispatch_routed_tasks(
        backend, tmux, repo, runner, capture, semaphore,
        router_arc, task_manager, weight_tx, transport,
    ).await?;

    // Phase 4: Unblock parents (blocked tasks whose children are all done)
    unblock_parent_tasks(backend, task_manager).await?;

    // Phase 5: Check job schedules
    if let Err(e) = jobs::tick(jobs_path, backend, db).await {
        tracing::error!(?e, "job scheduler tick failed");
    }

    Ok(())
}

/// Phase 1: Check active tmux sessions for completions.
///
/// Monitors the tmux session snapshot and cleans up completed sessions.
async fn check_session_completions(
    tmux: &Arc<TmuxManager>,
    repo: &str,
    capture: &Arc<CaptureService>,
) {
    let _span = tracing::info_span!("engine.tick.phase1.sessions").entered();
    let session_snapshot = tmux.snapshot().await;
    for (task_id, active) in &session_snapshot {
        if !active {
            tracing::info!(task_id, "session completed, collecting results");
            capture.unregister_session(task_id).await;
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
}

/// Phase 2: Recover tasks stuck in `in_progress` without an active session.
///
/// Checks each in-progress task for a corresponding tmux session. If no session
/// exists and the task has been stuck longer than `config.stuck_timeout`, resets
/// it to `new` for re-routing.
async fn recover_stuck_tasks(
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

        if !has_session {
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

            if age.num_seconds() > config.stuck_timeout as i64 {
                tracing::warn!(
                    task_id = task.id.0,
                    age_mins = age.num_minutes(),
                    "recovering stuck task → new"
                );
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
                if let Err(e) = backend
                    .post_comment(
                        &task.id,
                        &format!(
                            "[{}] recovered: stuck in_progress for {}m with no active session (cleared agent for re-routing)",
                            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
                            age.num_minutes()
                        ),
                    )
                    .await
                {
                    tracing::warn!(task_id = task.id.0, ?e, "failed to post stuck-task recovery comment");
                    continue;
                }
            }
        }
    }
    Ok(())
}

/// Phase 3a: Route new tasks via the LLM router.
///
/// Finds tasks with `status:new` (excluding `no-agent` labeled), routes them
/// to an agent, and transitions to `status:routed`.
async fn route_new_tasks(
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
                if let Err(e) = router.store_route_result(&task.id.0, &result) {
                    tracing::warn!(task_id = task.id.0, ?e, "failed to store route result");
                }

                let labels = vec![
                    format!("agent:{}", result.agent),
                    format!("complexity:{}", result.complexity),
                ];
                if let Err(e) = backend.set_labels(&task.id, &labels).await {
                    tracing::warn!(task_id = task.id.0, ?e, "failed to set routing labels");
                }

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

/// Phase 3b: Dispatch routed tasks by spawning agent runners.
///
/// Acquires semaphore permits for concurrency control, marks tasks as
/// `in_progress`, and spawns each task in a tokio task with its own
/// tmux session.
#[allow(clippy::too_many_arguments)]
async fn dispatch_routed_tasks(
    backend: &Arc<dyn ExternalBackend>,
    tmux: &Arc<TmuxManager>,
    repo: &str,
    runner: &Arc<TaskRunner>,
    capture: &Arc<CaptureService>,
    semaphore: &Arc<Semaphore>,
    router_arc: &Arc<RwLock<Router>>,
    task_manager: &Arc<TaskManager>,
    weight_tx: &mpsc::Sender<WeightSignal>,
    transport: &Arc<Transport>,
) -> anyhow::Result<()> {
    let _span = tracing::info_span!("engine.tick.phase3b.dispatch").entered();
    let routed_tasks = task_manager.list_external_by_status(Status::Routed).await?;
    let dispatchable: Vec<&ExternalTask> = routed_tasks
        .iter()
        .filter(|t| !t.labels.iter().any(|l| l == "no-agent"))
        .collect();

    if !dispatchable.is_empty() {
        tracing::info!(count = dispatchable.len(), "dispatchable tasks found");
    }

    for task in dispatchable {
        let session_name = tmux.session_name(repo, &task.id.0);
        if tmux.session_exists(&session_name).await {
            continue;
        }

        let permit = match semaphore.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                tracing::debug!("all parallel slots busy, skipping remaining tasks");
                break;
            }
        };

        let task_id = task.id.0.clone();
        if let Err(e) = backend.update_status(&task.id, Status::InProgress).await {
            tracing::error!(task_id, ?e, "failed to set in_progress, skipping dispatch");
            drop(permit);
            continue;
        }

        let session_name = tmux.session_name(repo, &task_id);
        capture.register_session(&task_id, &session_name).await;

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

        let route_result = get_route_result(&task_id).ok();
        let agent_name = route_result
            .as_ref()
            .map(|r| r.agent.clone())
            .unwrap_or_else(|| "claude".to_string());

        tokio::spawn(async move {
            tracing::info!(task_id, "dispatching task");

            let dispatch_start = std::time::Instant::now();

            match runner
                .run_with_context(&task_owned, &backend, &tmux, route_result.as_ref())
                .await
            {
                Ok(signal) => {
                    tracing::info!(task_id, "task runner completed");
                    let _ = weight_tx.send(signal).await;

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

                    tracing::debug!(task_id, %status, "checking review trigger");
                    if status == "in_review" {
                        let enable_review = config::get("workflow.enable_review_agent")
                            .map(|v| v != "false")
                            .unwrap_or(true);
                        let already_reviewing = sidecar::get(&task_id, "review_started")
                            .map(|v| v == "true")
                            .unwrap_or(false);
                        tracing::info!(
                            task_id,
                            enable_review,
                            already_reviewing,
                            "review gate check"
                        );
                        if enable_review && !already_reviewing {
                            let _ = sidecar::set(&task_id, &["review_started=true".to_string()]);
                            let backend_clone = backend.clone();
                            let tmux_clone = tmux.clone();
                            let task_owned_clone = task_owned.clone();
                            let router_for_review = router_clone.clone();
                            tokio::spawn(async move {
                                if let Err(e) = pr_review::review_and_merge(
                                    &task_owned_clone,
                                    &backend_clone,
                                    &tmux_clone,
                                    &repo_owned,
                                    &router_for_review,
                                )
                                .await
                                {
                                    tracing::error!(task_id, error = %e, "review_and_merge failed");
                                }
                            });
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(task_id, ?e, "task runner failed");
                    if let Err(comment_err) = backend
                        .post_comment(
                            &crate::backends::ExternalId(task_id.clone()),
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

            capture.unregister_session(&task_id_for_cleanup).await;
            drop(permit);
        });
    }
    Ok(())
}

/// Phase 4: Unblock parent tasks whose children are all done.
///
/// Scans `status:blocked` tasks, checks if all sub-issues are `status:done`,
/// and transitions the parent back to `status:new` for re-routing.
async fn unblock_parent_tasks(
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

        if children.is_empty() {
            continue;
        }

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

/// Sync tick — runs every 120s.
///
/// Handles less-frequent operations:
/// - Cleanup finished worktrees
/// - Check for merged PRs → mark tasks done
/// - Scan for @mentions
async fn sync_tick(
    backend: &Arc<dyn ExternalBackend>,
    tmux: &Arc<TmuxManager>,
    repo: &str,
    db: &Arc<Db>,
    config: &EngineConfig,
    router: &Arc<RwLock<Router>>,
) -> anyhow::Result<()> {
    tracing::debug!("sync tick");

    // 1. Cleanup worktrees for done tasks
    if let Err(e) = sync_ops::cleanup_done_worktrees(backend, repo).await {
        tracing::warn!(err = %e, "worktree cleanup failed");
    }

    // 2. Check for merged PRs (in_review → done)
    if let Err(e) = sync_ops::check_merged_prs(backend).await {
        tracing::warn!(err = %e, "PR merge check failed");
    }

    // 3. Scan for @mentions
    if let Err(e) = sync_ops::scan_mentions(backend, db).await {
        tracing::warn!(err = %e, "mention scan failed");
    }

    // 4. Review open PRs (parse review comments, create follow-ups)
    if let Err(e) = pr_review::review_open_prs(backend, db, repo, config).await {
        tracing::warn!(err = %e, "PR review failed");
    }

    // 5. Trigger review agent for in_review tasks not yet reviewed
    let enable_review = config::get("workflow.enable_review_agent")
        .map(|v| v != "false")
        .unwrap_or(true);
    if enable_review {
        if let Ok(in_review) = backend.list_by_status(Status::InReview).await {
            for task in in_review {
                let task_id = &task.id.0;
                let already = sidecar::get(task_id, "review_started")
                    .map(|v| v == "true")
                    .unwrap_or(false);
                if !already {
                    tracing::info!(task_id, "triggering review agent for in_review task");
                    let _ = sidecar::set(task_id, &["review_started=true".to_string()]);
                    let backend_c = backend.clone();
                    let tmux_c = tmux.clone();
                    let task_c = task.clone();
                    let repo_s = repo.to_string();
                    let router_c = router.clone();
                    tokio::spawn(async move {
                        if let Err(e) =
                            pr_review::review_and_merge(&task_c, &backend_c, &tmux_c, &repo_s, &router_c).await
                        {
                            tracing::error!(
                                task_id = task_c.id.0,
                                error = %e,
                                "review_and_merge failed"
                            );
                        }
                    });
                }
            }
        }
    }

    // 6. Scan for owner /slash commands in issue comments
    if let Err(e) = commands::scan_commands(backend, db, repo).await {
        tracing::warn!(err = %e, "owner command scan failed");
    }

    // 7. Sync skill repositories
    if let Err(e) = sync_ops::skills_sync().await {
        tracing::warn!(err = %e, "skills sync failed");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// serve() setup helpers
// ---------------------------------------------------------------------------

/// Webhook server state returned by `setup_webhook()`.
struct WebhookState {
    /// Listening port (None if webhooks are disabled).
    port: Option<u16>,
    /// Whether the last health check succeeded.
    healthy: bool,
}

/// Initialize the channel registry with configured notification channels.
///
/// Registers Telegram, Discord, and tmux channels based on configuration,
/// starts each channel, and returns the registry with message receivers.
async fn setup_channels(
    transport: &Arc<Transport>,
) -> (Arc<ChannelRegistry>, Vec<mpsc::Receiver<IncomingMessage>>) {
    let mut channel_registry = ChannelRegistry::new();

    // Try to initialize Telegram channel
    if let Ok(token) = crate::config::get("channels.telegram.bot_token") {
        if !token.is_empty() {
            let chat_id = crate::config::get("channels.telegram.chat_id").ok();
            let telegram = TelegramChannel::new(token, chat_id);
            if let Err(e) = telegram.health_check().await {
                tracing::warn!(?e, "telegram channel health check failed, skipping");
            } else {
                channel_registry.register(Box::new(telegram));
                tracing::info!("telegram channel registered");
            }
        }
    }

    // Try to initialize Discord channel
    if let Ok(token) = crate::config::get("channels.discord.bot_token") {
        if !token.is_empty() {
            let channel_id = crate::config::get("channels.discord.channel_id").ok();
            let discord = DiscordChannel::new(token, channel_id);
            if let Err(e) = discord.health_check().await {
                tracing::warn!(?e, "discord channel health check failed, skipping");
            } else {
                channel_registry.register(Box::new(discord));
                tracing::info!("discord channel registered");
            }
        }
    }

    // Initialize tmux channel with transport for output streaming
    let tmux_channel = TmuxChannel::with_transport(transport.clone());
    channel_registry.register(Box::new(tmux_channel));
    tracing::info!("tmux channel registered");

    // Start all channels and collect their message receivers
    let mut channel_receivers: Vec<mpsc::Receiver<IncomingMessage>> = Vec::new();
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

    (Arc::new(channel_registry), channel_receivers)
}

/// Spawn tasks to forward incoming channel messages through the transport layer.
fn spawn_channel_message_handlers(
    receivers: Vec<mpsc::Receiver<IncomingMessage>>,
    transport: &Arc<Transport>,
) {
    for mut rx in receivers {
        let transport = transport.clone();
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                tracing::debug!(channel = %msg.channel, thread = %msg.thread_id, "received message from channel");

                match transport.route(&msg).await {
                    crate::channels::transport::MessageRoute::TaskSession { task_id } => {
                        tracing::debug!(task_id = %task_id, "message routed to existing session");
                    }
                    crate::channels::transport::MessageRoute::Command { raw } => {
                        tracing::debug!(command = %raw, "message is a command");
                    }
                    crate::channels::transport::MessageRoute::NewTask => {
                        tracing::debug!("message would create new task");
                    }
                }
            }
        });
    }
}

/// Spawn the notification dispatcher that broadcasts task events to channels.
///
/// Reads task completion notifications from the transport layer and forwards
/// them to Telegram, Discord, etc. based on the configured notification level.
fn spawn_notification_dispatcher(
    transport: &Arc<Transport>,
    channels: &Arc<ChannelRegistry>,
) {
    let mut notification_rx = transport.subscribe_notifications();
    let channels = channels.clone();
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
                        "broadcasting notification to channels"
                    );

                    for channel in channels.iter() {
                        let (body, should_send) = match channel.name() {
                            "telegram" => (notification.format_telegram(), true),
                            "discord" => (notification.format_discord(), true),
                            _ => (String::new(), false),
                        };

                        if !should_send {
                            continue;
                        }

                        let msg = OutgoingMessage {
                            thread_id: notification.task_id.clone(),
                            body,
                            reply_to: None,
                            metadata: serde_json::json!({}),
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

/// Start the webhook server if configured.
///
/// When enabled, spawns the HTTP server and a message forwarding task that
/// wakes the engine tick immediately on incoming events.
async fn setup_webhook(
    project_engines: &[ProjectEngine],
    transport: &Arc<Transport>,
    webhook_notify: &Arc<Notify>,
) -> WebhookState {
    let webhook_enabled = crate::config::get("webhook.enabled")
        .map(|v| v == "true")
        .unwrap_or(false);

    if !webhook_enabled {
        tracing::info!("webhook server disabled, using polling fallback mode");
        return WebhookState {
            port: None,
            healthy: false,
        };
    }

    let port: u16 = crate::config::get("webhook.port")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8080);
    let secret = crate::config::get("webhook.secret").unwrap_or_default();
    let webhook_repo = project_engines
        .first()
        .map(|e| e.repo.clone())
        .unwrap_or_default();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<IncomingMessage>(64);

    tokio::spawn(async move {
        if let Err(e) = start_webhook_server(port, secret, webhook_repo, tx).await {
            tracing::error!(?e, "webhook server failed");
        }
    });

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
            let _ = transport_for_webhook.route(&msg).await;
            notify.notify_one();
        }
    });

    tracing::info!(port, "webhook server started");
    WebhookState {
        port: Some(port),
        healthy: true,
    }
}







#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::types::{GitHubReview, GitHubReviewComment, GitHubUser, PullRequestReview};

    #[test]
    fn engine_config_defaults() {
        let config = EngineConfig::default();
        assert_eq!(config.tick_interval, std::time::Duration::from_secs(10));
        assert_eq!(config.sync_interval, std::time::Duration::from_secs(120));
        assert_eq!(config.max_parallel, 4);
        assert_eq!(config.stuck_timeout, 1800);
    }

    #[test]
    fn engine_config_from_config_uses_defaults_when_no_config() {
        // Without config files, from_config() should return defaults
        let config = EngineConfig::from_config();
        assert_eq!(config.tick_interval, std::time::Duration::from_secs(10));
        assert_eq!(config.sync_interval, std::time::Duration::from_secs(120));
        assert_eq!(config.max_parallel, 4);
        assert_eq!(config.stuck_timeout, 1800);
    }

    #[test]
    fn test_pull_request_review_requests_changes() {
        let review = PullRequestReview {
            review: GitHubReview {
                id: 1,
                user: GitHubUser {
                    login: "reviewer".to_string(),
                },
                body: Some("Please fix".to_string()),
                state: "CHANGES_REQUESTED".to_string(),
                html_url: None,
                submitted_at: "2024-01-01T00:00:00Z".to_string(),
                commit_id: None,
            },
            comments: vec![],
        };

        assert!(review.requests_changes());
    }

    #[test]
    fn test_pull_request_review_does_not_request_changes() {
        let review = PullRequestReview {
            review: GitHubReview {
                id: 1,
                user: GitHubUser {
                    login: "reviewer".to_string(),
                },
                body: Some("LGTM".to_string()),
                state: "APPROVED".to_string(),
                html_url: None,
                submitted_at: "2024-01-01T00:00:00Z".to_string(),
                commit_id: None,
            },
            comments: vec![],
        };

        assert!(!review.requests_changes());
    }

    #[test]
    fn test_pull_request_review_actionable_comments_filters_empty_and_replies() {
        let review = PullRequestReview {
            review: GitHubReview {
                id: 1,
                user: GitHubUser {
                    login: "reviewer".to_string(),
                },
                body: None,
                state: "CHANGES_REQUESTED".to_string(),
                html_url: None,
                submitted_at: "2024-01-01T00:00:00Z".to_string(),
                commit_id: None,
            },
            comments: vec![
                GitHubReviewComment {
                    id: 1,
                    user: GitHubUser {
                        login: "reviewer".to_string(),
                    },
                    body: "Fix this issue".to_string(),
                    path: "src/main.rs".to_string(),
                    line: Some(10),
                    original_line: Some(10),
                    commit_id: "abc".to_string(),
                    original_commit_id: "abc".to_string(),
                    html_url: "url".to_string(),
                    created_at: "2024-01-01T00:00:00Z".to_string(),
                    updated_at: "2024-01-01T00:00:00Z".to_string(),
                    in_reply_to_id: None,
                    diff_hunk: Some(
                        "@@ -8,5 +8,5 @@ fn main() {\n-    let x = 1;\n+    let x = 2;".to_string(),
                    ),
                },
                GitHubReviewComment {
                    id: 2,
                    user: GitHubUser {
                        login: "reviewer".to_string(),
                    },
                    body: "".to_string(), // Empty - should be filtered out
                    path: "src/lib.rs".to_string(),
                    line: Some(20),
                    original_line: Some(20),
                    commit_id: "abc".to_string(),
                    original_commit_id: "abc".to_string(),
                    html_url: "url".to_string(),
                    created_at: "2024-01-01T00:00:00Z".to_string(),
                    updated_at: "2024-01-01T00:00:00Z".to_string(),
                    in_reply_to_id: None,
                    diff_hunk: None,
                },
                GitHubReviewComment {
                    id: 3,
                    user: GitHubUser {
                        login: "reviewer".to_string(),
                    },
                    body: "Reply to this".to_string(),
                    path: "src/lib.rs".to_string(),
                    line: Some(30),
                    original_line: Some(30),
                    commit_id: "abc".to_string(),
                    original_commit_id: "abc".to_string(),
                    html_url: "url".to_string(),
                    created_at: "2024-01-01T00:00:00Z".to_string(),
                    updated_at: "2024-01-01T00:00:00Z".to_string(),
                    in_reply_to_id: Some(1), // Reply - should be filtered out
                    diff_hunk: None,
                },
            ],
        };

        let actionable = review.actionable_comments();
        assert_eq!(actionable.len(), 1);
        assert_eq!(actionable[0].id, 1);
        assert_eq!(actionable[0].body, "Fix this issue");
        assert_eq!(actionable[0].path, "src/main.rs");
        assert_eq!(
            actionable[0].diff_hunk.as_ref().unwrap(),
            "@@ -8,5 +8,5 @@ fn main() {\n-    let x = 1;\n+    let x = 2;"
        );
    }

}
