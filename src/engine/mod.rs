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
pub mod router;
pub mod runner;
pub mod tasks;

use crate::backends::{ExternalBackend, ExternalId, ExternalTask, Status};
use crate::channels::capture::CaptureService;
use crate::channels::discord::DiscordChannel;
use crate::channels::github::start_webhook_server;
use crate::channels::notification::{NotificationLevel, TaskNotification};
use crate::channels::telegram::TelegramChannel;
use crate::channels::tmux::TmuxChannel;
use crate::channels::transport::Transport;
use crate::channels::{Channel, ChannelRegistry, IncomingMessage, OutgoingMessage};
use crate::cmd::CommandErrorContext;
use crate::config;
use crate::db::{Db, TaskStatus};
use crate::engine::router::{get_route_result, Router};
use crate::engine::tasks::TaskManager;
use crate::github::cli::GhCli;
use crate::github::types::{GitHubReviewComment, PullRequestReview};
use crate::sidecar;
use crate::tmux::TmuxManager;
use runner::{TaskRunner, WeightSignal};
use std::sync::Arc;
use tokio::process::Command;
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
    /// Fallback sync interval when webhooks are unavailable (seconds)
    pub fallback_sync_interval: Option<std::time::Duration>,
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
            fallback_sync_interval: Some(std::time::Duration::from_secs(30)),
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

        if let Ok(val) = crate::config::get("engine.fallback_sync_interval") {
            if let Ok(secs) = val.parse::<u64>() {
                if secs > 0 {
                    config.fallback_sync_interval = Some(std::time::Duration::from_secs(secs));
                } else {
                    config.fallback_sync_interval = None;
                }
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

    // Initialize channel registry
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
    for mut rx in channel_receivers {
        let transport = transport_for_messages.clone();
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                tracing::debug!(channel = %msg.channel, thread = %msg.thread_id, "received message from channel");

                // Route the message through transport
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

    // Spawn notification dispatcher — reads task completion notifications
    // from transport and broadcasts to all configured channels.
    {
        let mut notification_rx = transport.subscribe_notifications();
        let channels = channel_registry.clone();
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
                                // GitHub is already handled by backend.post_comment()
                                // tmux doesn't need task completion notifications
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

    // Notify used by webhook events to wake up the engine tick immediately
    let webhook_notify = Arc::new(Notify::new());

    // Track webhook state for health checks and fallback.
    // When webhooks are disabled, `in_fallback_mode` stays true so sync
    // uses the faster fallback interval for polling.
    let webhook_port: Option<u16>;
    let mut webhook_healthy: bool;
    let mut last_webhook_health_check = std::time::Instant::now();
    let mut in_fallback_mode: bool;

    // Start webhook server if configured
    let webhook_enabled = crate::config::get("webhook.enabled")
        .map(|v| v == "true")
        .unwrap_or(false);

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

        // Spawn the HTTP server (runs until shutdown)
        tokio::spawn(async move {
            if let Err(e) = start_webhook_server(port, secret, webhook_repo, tx).await {
                tracing::error!(?e, "webhook server failed");
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
                let _ = transport_for_webhook.route(&msg).await;
                // Wake up the engine tick immediately instead of waiting up to 10s
                notify.notify_one();
            }
        });

        webhook_healthy = true;
        in_fallback_mode = false;
        tracing::info!(port, "webhook server started");
    } else {
        webhook_port = None;
        webhook_healthy = false;
        in_fallback_mode = true;
        tracing::info!("webhook server disabled, using polling fallback mode");
    }

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
                if let Some(remaining) = GhCli::is_rate_limited() {
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
                            &engine.task_manager,
                            &weight_tx,
                            &transport,
                        ).await {
                            tracing::error!(repo = %engine.repo, ?e, "tick failed for project");
                        }
                    }
                    drop(router_guard);

                    // Periodic sync (less frequent)
                    // Use fallback interval if in fallback mode
                    let current_sync_interval = if in_fallback_mode {
                        config.fallback_sync_interval.unwrap_or(config.sync_interval)
                    } else {
                        config.sync_interval
                    };

                    if last_sync.elapsed() >= current_sync_interval {
                        for engine in &project_engines {
                            if let Err(e) = sync_tick(&engine.backend, &tmux, &engine.repo, &db, &config).await {
                                tracing::error!(repo = %engine.repo, ?e, "sync tick failed for project");
                            }
                        }
                        last_sync = std::time::Instant::now();
                    }
                }

                // Periodic webhook health check
                if webhook_enabled {
                    let health_check_interval = config.webhook_health_check_interval
                        .unwrap_or(std::time::Duration::from_secs(60));

                    if last_webhook_health_check.elapsed() >= health_check_interval {
                        if let Some(port) = webhook_port {
                            let health = crate::channels::github::check_webhook_health(port).await;
                            if health != webhook_healthy {
                                webhook_healthy = health;
                                if webhook_healthy {
                                    in_fallback_mode = false;
                                    tracing::info!(port, "webhook health restored, exiting polling fallback mode");
                                } else {
                                    in_fallback_mode = true;
                                    tracing::warn!(port, "webhook health check failed, entering polling fallback mode");
                                }
                            }
                        }
                        last_webhook_health_check = std::time::Instant::now();
                    }
                }
            }
            // Webhook events trigger an immediate tick (bypass polling interval)
            _ = webhook_notify.notified() => {
                if let Some(remaining) = GhCli::is_rate_limited() {
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
    task_manager: &Arc<TaskManager>,
    weight_tx: &mpsc::Sender<WeightSignal>,
    transport: &Arc<Transport>,
) -> anyhow::Result<()> {
    let _tick_span = tracing::info_span!("engine.tick").entered();

    // Phase 1: Check active tmux sessions for completions
    let _phase1 = tracing::info_span!("engine.tick.phase1.sessions").entered();
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
    drop(_phase1);

    // Phase 2: Recover stuck tasks
    let _phase2 = tracing::info_span!("engine.tick.phase2.stuck_tasks").entered();
    let in_progress = task_manager
        .list_external_by_status(Status::InProgress)
        .await?;
    for task in &in_progress {
        let session_name = tmux.session_name(repo, &task.id.0);
        let has_session = tmux.session_exists(&session_name).await;

        if !has_session {
            // No tmux session — check if stuck
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
                // Remove stale agent label so the LLM router re-routes properly
                for label in &task.labels {
                    if label.starts_with("agent:") {
                        backend.remove_label(&task.id, label).await.ok();
                    }
                }
                sidecar::set(
                    &task.id.0,
                    &[
                        "agent=".to_string(),
                        "model=".to_string(),
                        "route_attempts=0".to_string(),
                    ],
                )?;
                backend.update_status(&task.id, Status::New).await?;
                backend
                    .post_comment(
                        &task.id,
                        &format!(
                            "[{}] recovered: stuck in_progress for {}m with no active session (cleared agent for re-routing)",
                            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
                            age.num_minutes()
                        ),
                    )
                    .await?;
            }
        }
    }
    drop(_phase2);

    // Phase 3a: Route new tasks (includes issues with status:new or no status:* label)
    let _phase3a = tracing::info_span!("engine.tick.phase3a.route").entered();
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
    drop(_phase3a);

    // Phase 3b: Dispatch routed tasks.
    // Note: Routed tasks should never have no-agent (filtered during Phase 3a routing),
    // but we keep this filter as defense-in-depth.
    let _phase3b = tracing::info_span!("engine.tick.phase3b.dispatch").entered();
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

        tokio::spawn(async move {
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

                    // Trigger review agent if task is done (enabled by default)
                    tracing::debug!(task_id, %status, "checking review trigger");
                    if status == "done" {
                        let enable_review = config::get("workflow.enable_review_agent")
                            .map(|v| v != "false")
                            .unwrap_or(true);
                        // Guard against duplicate review spawns
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
                            tokio::spawn(async move {
                                if let Err(e) = review_and_merge(
                                    &task_owned_clone,
                                    &backend_clone,
                                    &tmux_clone,
                                    &repo_owned,
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
        });
    }

    // Phase 4: Unblock parents (blocked tasks whose children are all done)
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

    // Phase 5: Check job schedules
    if let Err(e) = jobs::tick(jobs_path, backend, db).await {
        tracing::error!(?e, "job scheduler tick failed");
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
    _tmux: &Arc<TmuxManager>,
    repo: &str,
    db: &Arc<Db>,
    config: &EngineConfig,
) -> anyhow::Result<()> {
    tracing::debug!("sync tick");

    // 1. Cleanup worktrees for done tasks
    if let Err(e) = cleanup_done_worktrees(backend, repo).await {
        tracing::warn!(err = %e, "worktree cleanup failed");
    }

    // 2. Check for merged PRs (in_review → done)
    if let Err(e) = check_merged_prs(backend).await {
        tracing::warn!(err = %e, "PR merge check failed");
    }

    // 3. Scan for @mentions
    if let Err(e) = scan_mentions(backend, db).await {
        tracing::warn!(err = %e, "mention scan failed");
    }

    // 4. Review open PRs (parse review comments, create follow-ups)
    if let Err(e) = review_open_prs(backend, db, repo, config).await {
        tracing::warn!(err = %e, "PR review failed");
    }

    // 5. Scan for owner /slash commands in issue comments
    if let Err(e) = commands::scan_commands(backend, db, repo).await {
        tracing::warn!(err = %e, "owner command scan failed");
    }

    // 6. Sync skill repositories
    if let Err(e) = skills_sync().await {
        tracing::warn!(err = %e, "skills sync failed");
    }

    Ok(())
}

/// Sync skill repositories from config.
///
/// Reads the `skills:` list from config and clones/pulls each repository
/// to `~/.orch/skills/{repo}/`. This keeps skill documentation up-to-date
/// for agents.
async fn skills_sync() -> anyhow::Result<()> {
    use tokio::process::Command;

    let skills = match crate::config::get_skills() {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(err = %e, "no skills configured");
            return Ok(());
        }
    };

    if skills.is_empty() {
        tracing::debug!("no skills configured, skipping sync");
        return Ok(());
    }

    let skills_base = crate::home::skills_dir()?;
    let git_timeout = std::time::Duration::from_secs(60);

    for skill in skills {
        // Validate repo format to prevent path traversal
        if skill.repo.contains("..") || skill.repo.matches('/').count() != 1 {
            tracing::warn!(repo = %skill.repo, "invalid skill repo format, expected 'owner/repo'");
            continue;
        }

        let repo_dir = skills_base.join(&skill.repo);
        let repo_url = format!("https://github.com/{}.git", skill.repo);

        if repo_dir.exists() {
            // Pull latest changes with timeout
            tracing::debug!(repo = %skill.repo, "pulling skill repo");
            let pull_result = tokio::time::timeout(
                git_timeout,
                Command::new("git")
                    .args(["pull", "--ff-only", "--prune"])
                    .current_dir(&repo_dir)
                    .output_with_context(),
            )
            .await;

            match pull_result {
                Ok(Ok(output)) if !output.status.success() => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    tracing::warn!(repo = %skill.repo, err = %stderr, "git pull failed");
                }
                Ok(Ok(_)) => {
                    tracing::debug!(repo = %skill.repo, "skill repo updated");
                }
                Ok(Err(e)) => {
                    tracing::warn!(repo = %skill.repo, err = %e, "git pull error");
                }
                Err(_) => {
                    tracing::warn!(repo = %skill.repo, "git pull timed out after 60s");
                }
            }
        } else {
            // Clone the repository (shallow for efficiency)
            tracing::debug!(repo = %skill.repo, "cloning skill repo");
            let parent = repo_dir
                .parent()
                .ok_or_else(|| anyhow::anyhow!("skill repo path has no parent directory"))?;
            std::fs::create_dir_all(parent)?;
            let repo_dir_str = repo_dir
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("skill repo path is not valid UTF-8"))?;

            let clone_result = tokio::time::timeout(
                git_timeout,
                Command::new("git")
                    .args(["clone", "--depth", "1", &repo_url, repo_dir_str])
                    .output_with_context(),
            )
            .await;

            match clone_result {
                Ok(Ok(output)) if !output.status.success() => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    tracing::warn!(repo = %skill.repo, err = %stderr, "git clone failed");
                    // Clean up partial clone to allow retry on next tick
                    let _ = std::fs::remove_dir_all(&repo_dir);
                }
                Ok(Ok(_)) => {
                    tracing::info!(repo = %skill.repo, "skill repo cloned");
                }
                Ok(Err(e)) => {
                    tracing::warn!(repo = %skill.repo, err = %e, "git clone error");
                    let _ = std::fs::remove_dir_all(&repo_dir);
                }
                Err(_) => {
                    tracing::warn!(repo = %skill.repo, "git clone timed out after 60s");
                    let _ = std::fs::remove_dir_all(&repo_dir);
                }
            }
        }
    }

    Ok(())
}

/// Cleanup worktrees for done tasks.
///
/// Queries status:done tasks, checks if worktree exists, removes it,
/// deletes the local branch, and marks the worktree as cleaned.
async fn cleanup_done_worktrees(
    backend: &Arc<dyn ExternalBackend>,
    repo: &str,
) -> anyhow::Result<()> {
    let done_tasks = backend.list_by_status(Status::Done).await?;
    tracing::debug!(count = done_tasks.len(), "checking done tasks for cleanup");

    // Resolve the main repository root for git operations.
    // We need this because worktree removal and branch deletion must run
    // from the main repo, not from the (soon-to-be-deleted) worktree dir.
    let repo_root = resolve_repo_root(repo).await?;

    // Get worktrees base path
    let worktrees_base = crate::home::worktrees_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from(".orch/worktrees"));

    for task in done_tasks {
        let task_id = &task.id.0;

        // Get worktree and branch from sidecar
        let worktree = sidecar::get(task_id, "worktree").ok();
        let branch = sidecar::get(task_id, "branch").ok();
        let worktree_cleaned = sidecar::get(task_id, "worktree_cleaned").ok();

        // Skip if already cleaned
        if worktree_cleaned.as_deref() == Some("true") || worktree_cleaned.as_deref() == Some("1") {
            continue;
        }

        let worktree_path = worktree.as_ref().map(std::path::PathBuf::from);

        // Try to construct default worktree path if not in sidecar
        let worktree_to_remove = if let Some(ref wt) = worktree_path {
            if wt.exists() {
                Some(wt.clone())
            } else {
                None
            }
        } else if let (Some(b), Some(dir)) = (&branch, worktree.as_ref()) {
            // Try: worktrees_base/{project}/{branch}
            let project = std::path::Path::new(dir)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| repo.replace('/', "__"));
            let wt = worktrees_base.join(&project).join(b);
            if wt.exists() {
                Some(wt)
            } else {
                None
            }
        } else {
            None
        };

        if let Some(wt) = worktree_to_remove {
            tracing::info!(task_id, worktree = %wt.display(), "removing worktree");

            // Remove worktree FIRST, then delete the branch.
            // Git refuses to remove a worktree if its branch is already deleted.
            // Both commands run from the main repo root (not the worktree dir).
            let wt_str = wt.to_string_lossy().to_string();
            let remove_result = Command::new("git")
                .args(["-C", &repo_root, "worktree", "remove", &wt_str, "--force"])
                .output_with_context()
                .await;

            match remove_result {
                Ok(output) if output.status.success() => {
                    tracing::info!(task_id, "worktree removed");
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    tracing::warn!(task_id, err = %stderr, "failed to remove worktree");
                }
                Err(e) => {
                    tracing::warn!(task_id, err = %e, "failed to remove worktree");
                }
            }

            // Delete branch from the main repo root (worktree is already gone)
            if let Some(ref br) = branch {
                let branch_delete_result = Command::new("git")
                    .args(["-C", &repo_root, "branch", "-D", br])
                    .output_with_context()
                    .await;

                match branch_delete_result {
                    Ok(output) if output.status.success() => {
                        tracing::info!(task_id, branch = %br, "branch deleted");
                    }
                    Ok(output) => {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        tracing::debug!(task_id, err = %stderr, "branch delete skipped (may not exist)");
                    }
                    Err(e) => {
                        tracing::warn!(task_id, err = %e, "failed to delete branch");
                    }
                }
            }

            // Mark as cleaned in sidecar
            if let Err(e) = sidecar::set(task_id, &["worktree_cleaned=true".to_string()]) {
                tracing::warn!(task_id, err = %e, "failed to mark worktree_cleaned");
            }
        }
    }

    Ok(())
}

/// Resolve the main git repository root path for a project.
///
/// Looks up the local project path from config, then verifies it's a git repo.
/// This avoids relying on cwd (which is undefined under launchd services).
async fn resolve_repo_root(repo: &str) -> anyhow::Result<String> {
    // Look up the local path from registered projects
    let paths = crate::config::get_project_paths().unwrap_or_default();
    for path_str in &paths {
        let path = std::path::Path::new(path_str);
        let orch_yml = path.join(".orch.yml");
        let legacy = path.join(".orchestrator.yml");
        let config_file = if orch_yml.exists() {
            orch_yml
        } else if legacy.exists() {
            legacy
        } else {
            continue;
        };
        if let Ok(content) = std::fs::read_to_string(&config_file) {
            if let Ok(doc) = serde_yml::from_str::<serde_yml::Value>(&content) {
                if let Some(r) = doc
                    .get("gh")
                    .and_then(|gh| gh.get("repo"))
                    .and_then(|r| r.as_str())
                {
                    if r == repo {
                        return Ok(path_str.clone());
                    }
                }
            }
        }
    }

    // Fallback: try bare clone in ~/.orch/projects/
    let bare = crate::home::projects_dir()
        .map(|d| d.join(repo.replace('/', "__")))
        .unwrap_or_default();
    if bare.exists() {
        return Ok(bare.display().to_string());
    }

    anyhow::bail!(
        "cannot find local path for project {repo} — \
         checked {} registered project(s) and bare clone at {}",
        paths.len(),
        bare.display()
    )
}

/// Check for merged PRs and update task status accordingly.
///
/// Queries status:in_review tasks, checks if their PR is merged,
/// and updates status to done if merged.
async fn check_merged_prs(backend: &Arc<dyn ExternalBackend>) -> anyhow::Result<()> {
    let in_review_tasks = backend.list_by_status(Status::InReview).await?;
    tracing::debug!(
        count = in_review_tasks.len(),
        "checking in_review tasks for merged PRs"
    );

    for task in in_review_tasks {
        let task_id = &task.id.0;

        // Get branch from sidecar
        let branch = match sidecar::get(task_id, "branch") {
            Ok(b) if !b.is_empty() => b,
            _ => {
                tracing::debug!(task_id, "no branch info, skipping PR check");
                continue;
            }
        };

        // Check if PR is merged via the backend trait
        match backend.is_pr_merged(&branch).await {
            Ok(true) => {
                tracing::info!(task_id, branch = %branch, "PR merged, marking task complete");

                // Update status to done
                let id = ExternalId(task_id.clone());
                if let Err(e) = backend.update_status(&id, Status::Done).await {
                    tracing::warn!(task_id, err = %e, "failed to update status to done");
                    continue;
                }

                // Post comment
                let comment = "PR merged, marking task complete";
                if let Err(e) = backend.post_comment(&id, comment).await {
                    tracing::warn!(task_id, err = %e, "failed to post comment");
                }
            }
            Ok(false) => {
                // PR not merged, continue
            }
            Err(e) => {
                tracing::warn!(task_id, branch = %branch, err = %e, "failed to check PR merge status");
            }
        }
    }

    Ok(())
}

/// Scan for @mentions and create internal tasks.
///
/// Checks recent issue comments for @orchestrator mentions,
/// creates internal tasks, and acknowledges them.
async fn scan_mentions(backend: &Arc<dyn ExternalBackend>, db: &Arc<Db>) -> anyhow::Result<()> {
    // Get the current user (for mention detection)
    let current_user = match backend.get_authenticated_user().await {
        Ok(Some(u)) => format!("@{}", u),
        Ok(None) => {
            tracing::debug!("backend does not support user identity, skipping mentions");
            return Ok(());
        }
        Err(e) => {
            tracing::warn!(err = %e, "failed to get current user, skipping mentions");
            return Ok(());
        }
    };

    // Use persisted cursor if available, otherwise fall back to 24h ago
    let fallback = chrono::Utc::now() - chrono::Duration::hours(24);
    let since_str = match db.kv_get("mentions_last_checked").await {
        Ok(Some(ts)) => ts,
        _ => fallback.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    };

    let mentions = match backend.get_mentions(&since_str).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(err = %e, "failed to get mentions");
            return Ok(());
        }
    };

    // Get existing mention tasks across ALL statuses to avoid duplicates.
    // Only checking New status would miss tasks that progressed to InProgress/Done,
    // causing duplicate tasks on the next sync tick within the 24h window.
    let mut existing_mentions = std::collections::HashSet::new();
    for status in &[
        TaskStatus::New,
        TaskStatus::InProgress,
        TaskStatus::Done,
        TaskStatus::Blocked,
        TaskStatus::Routed,
        TaskStatus::InReview,
        TaskStatus::NeedsReview,
    ] {
        let tasks = db.list_internal_tasks_by_status(*status).await?;
        for t in tasks {
            if t.source == "mention" {
                existing_mentions.insert(t.source_id.clone());
            }
        }
    }

    for mention in mentions {
        // Skip if already processed
        if existing_mentions.contains(&mention.id) {
            continue;
        }

        if !mention.body.contains(&current_user) && !mention.body.contains("@orchestrator") {
            continue;
        }

        // Create internal task for this mention
        let title = format!("Respond to mention by @{}", mention.author);
        let task_body = format!("Mention by @{}:\n\n{}", mention.author, mention.body);

        let task_id = db
            .create_internal_task(&title, &task_body, "mention", &mention.id)
            .await?;

        tracing::info!(task_id, mention_id = %mention.id, "created mention task");
    }

    // Persist cursor so the next sync tick only fetches newer comments
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    if let Err(e) = db.kv_set("mentions_last_checked", &now).await {
        tracing::warn!(err = %e, "failed to persist mentions cursor");
    }

    Ok(())
}

/// Review open PRs - re-dispatch agent to address review feedback.
///
/// Lists tasks in review, fetches PR reviews, and re-dispatches the agent
/// when a reviewer requests changes. The review context is stored in the
/// sidecar and injected into the agent prompt.
async fn review_open_prs(
    backend: &Arc<dyn ExternalBackend>,
    _db: &Arc<Db>,
    repo: &str,
    config: &EngineConfig,
) -> anyhow::Result<()> {
    // Get tasks that are in review (have open PRs)
    let in_review_tasks = backend.list_by_status(Status::InReview).await?;

    if in_review_tasks.is_empty() {
        return Ok(());
    }

    // Check if we should process reviews
    let auto_close_task = config.auto_close_task_on_approval;

    tracing::info!(
        count = in_review_tasks.len(),
        "checking in_review tasks for PR reviews"
    );

    let gh = GhCli::new();

    for task in in_review_tasks {
        let task_id = &task.id.0;

        // Get branch from sidecar
        let branch = match sidecar::get(task_id, "branch") {
            Ok(b) if !b.is_empty() => b,
            _ => continue,
        };

        // Get PR number from branch
        let pr_number = match gh.get_pr_number(repo, &branch).await {
            Ok(Some(n)) => n,
            Ok(None) => {
                tracing::debug!(task_id, branch = %branch, "no open PR found");
                continue;
            }
            Err(e) => {
                tracing::warn!(task_id, branch = %branch, err = %e, "failed to get PR number");
                continue;
            }
        };

        // Store PR number in sidecar for follow-up tasks
        if let Err(e) = sidecar::set(task_id, &[format!("pr_number={}", pr_number)]) {
            tracing::warn!(task_id, err = %e, "failed to store PR number in sidecar");
        }

        // Get the last processed review timestamp to avoid re-processing the same reviews
        let last_review_ts = sidecar::get(task_id, "last_review_ts").unwrap_or_default();

        // Fetch PR reviews
        let reviews = match gh.get_pr_reviews(repo, pr_number).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(task_id, pr_number, err = %e, "failed to get PR reviews");
                continue;
            }
        };

        // Get all review comments for this PR (more efficient than per-review)
        let all_comments = match gh.get_pr_comments(repo, pr_number).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(task_id, pr_number, err = %e, "failed to get PR comments");
                continue;
            }
        };

        // Deduplicate reviews: keep only the latest review per reviewer.
        // GitHub returns reviews chronologically; when a reviewer submits
        // multiple reviews (e.g. CHANGES_REQUESTED then APPROVED), we only
        // care about the most recent one.
        let deduped_reviews = {
            let mut by_reviewer: std::collections::HashMap<
                String,
                &crate::github::types::GitHubReview,
            > = std::collections::HashMap::new();
            for review in &reviews {
                // Skip COMMENTED and DISMISSED — they don't express approval/rejection
                if review.state != "APPROVED" && review.state != "CHANGES_REQUESTED" {
                    continue;
                }
                let existing = by_reviewer.get(&review.user.login);
                let dominated = match existing {
                    Some(prev) => review.submitted_at > prev.submitted_at,
                    None => true,
                };
                if dominated {
                    by_reviewer.insert(review.user.login.clone(), review);
                }
            }
            by_reviewer
        };

        // Check aggregate state: if any reviewer still requests changes,
        // the PR is not fully approved.
        let any_changes_requested = deduped_reviews
            .values()
            .any(|r| r.state == "CHANGES_REQUESTED");
        let all_approved =
            !deduped_reviews.is_empty() && deduped_reviews.values().all(|r| r.state == "APPROVED");

        // Also check automated review comments on the PR (comment-based review workflow).
        // This is the primary review mechanism since GitHub doesn't allow reviewing your own PRs.
        let automated_review = gh
            .get_automated_review_status(repo, pr_number)
            .await
            .unwrap_or(None);

        let comment_approved = automated_review.as_deref() == Some("approve");
        let comment_changes_requested = automated_review.as_deref() == Some("changes_requested");

        // Handle fully-approved PRs (either via PR review API or comment-based review)
        if (all_approved || comment_approved) && auto_close_task && !comment_changes_requested {
            tracing::info!(
                task_id,
                pr_number,
                comment_approved,
                "PR approved, closing task (marking as done)"
            );
            if let Err(e) = backend.update_status(&task.id, Status::Done).await {
                tracing::warn!(task_id, err = %e, "failed to update task status to done");
            }
            continue;
        }

        // Process reviews that request changes (from either PR review API or comments)
        if !any_changes_requested && !comment_changes_requested {
            continue;
        }

        // Build review context for re-dispatch
        let mut review_context = String::new();
        let mut latest_review_ts = last_review_ts.clone();

        // Only process the latest CHANGES_REQUESTED reviews (already deduplicated)
        for review in deduped_reviews
            .values()
            .filter(|r| r.state == "CHANGES_REQUESTED")
        {
            // Track the latest review timestamp
            if review.submitted_at > latest_review_ts {
                latest_review_ts = review.submitted_at.clone();
            }

            // Skip if we've already processed this review
            if !last_review_ts.is_empty() && review.submitted_at <= last_review_ts {
                continue;
            }
            // Get comments for this review
            let review_comments: Vec<GitHubReviewComment> = all_comments
                .iter()
                .filter(|c| {
                    c.user.login == review.user.login && c.created_at >= review.submitted_at
                })
                .cloned()
                .collect();

            let pr_review = PullRequestReview {
                review: (*review).clone(),
                comments: review_comments.clone(),
            };

            // Add review info
            review_context.push_str(&format!(
                "### Review by @{} (CHANGES REQUESTED)\n",
                pr_review.review.user.login
            ));

            // Add overall review body if present
            if let Some(ref body) = pr_review.review.body {
                if !body.trim().is_empty() {
                    review_context.push_str(&format!("**Overall Feedback:** {}\n\n", body));
                }
            }

            // Add actionable comments
            let actionable = pr_review.actionable_comments();
            if !actionable.is_empty() {
                review_context.push_str("**Comments to address:**\n\n");
                for comment in actionable {
                    review_context.push_str(&format!(
                        "#### File: `{}` (line {})\n",
                        comment.path,
                        comment.line.map(|l| l.to_string()).unwrap_or_default()
                    ));

                    if let Some(ref diff_hunk) = comment.diff_hunk {
                        review_context.push_str("```diff\n");
                        review_context.push_str(diff_hunk);
                        review_context.push_str("\n```\n\n");
                    }

                    review_context.push_str(&format!("> {}\n\n", comment.body));
                }
            }
        }

        // Also include comment-based review feedback (from "Automated Review — Changes Requested" comments)
        if comment_changes_requested && review_context.is_empty() {
            // The PR review API had no changes, but the comment-based review does.
            // Fetch the latest "Automated Review — Changes Requested" comment body.
            let last_comment_ts =
                sidecar::get(task_id, "last_comment_review_ts").unwrap_or_default();
            if let Ok(comments) = gh.list_comments(repo, &pr_number.to_string()).await {
                for c in comments.iter().rev() {
                    if c.body
                        .starts_with("## Automated Review — Changes Requested")
                    {
                        // Skip if already processed
                        if !last_comment_ts.is_empty() && c.created_at <= last_comment_ts {
                            break;
                        }
                        // Extract the review body (skip the header line)
                        let body: String = c.body.lines().skip(1).collect::<Vec<_>>().join("\n");
                        review_context.push_str("### Automated Review (Changes Requested)\n\n");
                        review_context.push_str(&body);
                        review_context.push('\n');

                        // Track the timestamp
                        let _ = sidecar::set(
                            task_id,
                            &[format!("last_comment_review_ts={}", c.created_at)],
                        );
                        break; // Only the latest
                    }
                }
            }
        }

        // Cap review context to avoid oversized sidecar values
        const MAX_REVIEW_CONTEXT_BYTES: usize = 16 * 1024;
        if review_context.len() > MAX_REVIEW_CONTEXT_BYTES {
            // Find safe UTF-8 char boundary
            let mut boundary = MAX_REVIEW_CONTEXT_BYTES;
            while !review_context.is_char_boundary(boundary) {
                boundary -= 1;
            }
            if let Some(pos) = review_context[..boundary].rfind('\n') {
                review_context.truncate(pos);
            } else {
                review_context.truncate(boundary);
            }
            review_context.push_str("\n... (review context truncated)");
        }

        // If we have new review feedback, store it and re-dispatch the task
        if !review_context.is_empty() {
            // Store the review context in the sidecar
            if let Err(e) =
                sidecar::set(task_id, &[format!("pr_review_context={}", review_context)])
            {
                tracing::warn!(task_id, err = %e, "failed to store pr_review_context");
            }

            // Update the last review timestamp
            if let Err(e) = sidecar::set(task_id, &[format!("last_review_ts={}", latest_review_ts)])
            {
                tracing::warn!(task_id, err = %e, "failed to update last_review_ts");
            }

            // Re-dispatch the task by setting status back to routed
            if let Err(e) = backend.update_status(&task.id, Status::Routed).await {
                tracing::warn!(task_id, err = %e, "failed to set status to routed for re-dispatch");
            } else {
                tracing::info!(task_id, "re-dispatching task to address review feedback");
            }
        }
    }

    Ok(())
}

/// Review agent decision result.
#[derive(Debug, Clone)]
enum ReviewDecision {
    /// Review approved, PR can be merged.
    Approve,
    /// Changes requested, PR needs fixes.
    RequestChanges {
        notes: String,
        issues: Vec<crate::engine::runner::response::ReviewIssue>,
    },
    /// Review agent failed or crashed (reason stored for logging).
    Failed(String),
}

/// Run the review agent on a completed task and handle the outcome.
///
/// This is called after a task completes with status:done and a PR is created.
/// The review agent checks the changes and either approves (triggers auto-merge)
/// or requests changes (re-dispatches the original agent).
async fn review_and_merge(
    task: &ExternalTask,
    backend: &Arc<dyn ExternalBackend>,
    tmux: &Arc<TmuxManager>,
    repo: &str,
) -> anyhow::Result<ReviewDecision> {
    // 2. Load sidecar for worktree path, branch, agent
    let worktree = sidecar::get(&task.id.0, "worktree").ok();
    let branch = sidecar::get(&task.id.0, "branch").ok();
    let agent_summary = sidecar::get(&task.id.0, "summary").unwrap_or_default();

    let worktree_path = match worktree {
        Some(w) if !w.is_empty() => std::path::PathBuf::from(w),
        _ => {
            tracing::warn!(task_id = task.id.0, "no worktree found for review");
            return Ok(ReviewDecision::Failed("no worktree found".to_string()));
        }
    };

    let branch_name = match branch {
        Some(b) if !b.is_empty() => b,
        _ => {
            tracing::warn!(task_id = task.id.0, "no branch found for review");
            return Ok(ReviewDecision::Failed("no branch found".to_string()));
        }
    };

    // 3. Get git diff and log
    let default_branch = config::get("gh.default_branch").unwrap_or_else(|_| "main".to_string());
    let git_diff = runner::context::build_git_diff(&worktree_path, &default_branch).await;
    let git_log = runner::context::build_git_log(&worktree_path, &default_branch).await;

    // 4. Build review prompt
    let review_prompt =
        runner::agent::build_review_prompt(task, &agent_summary, &git_diff, &git_log);

    // 5. Pick review agent (config: workflow.review_agent, default: claude)
    let review_agent =
        config::get("workflow.review_agent").unwrap_or_else(|_| "claude".to_string());
    let review_model = get_model_for_complexity("review", &review_agent);

    tracing::info!(
        task_id = task.id.0,
        agent = %review_agent,
        model = %review_model,
        "spawning review agent"
    );

    // 6. Build agent invocation for review
    let review_task_id = format!("{}-review", task.id.0);
    let review_attempt_dir = crate::home::task_attempt_dir(repo, &review_task_id, 1)?;
    let output_file = review_attempt_dir.join("output.json");

    let git_name = config::get("git.name").unwrap_or_else(|_| format!("{review_agent}[bot]"));
    let git_email = config::get("git.email")
        .unwrap_or_else(|_| format!("{review_agent}[bot]@users.noreply.github.com"));

    let system_prompt = runner::agent::review_system_prompt();

    let invocation = runner::agent::AgentInvocation {
        agent: review_agent.clone(),
        model: Some(review_model.clone()),
        work_dir: worktree_path.clone(),
        system_prompt,
        agent_message: review_prompt,
        task_id: review_task_id.clone(),
        branch: branch_name.clone(),
        main_project_dir: worktree_path.clone(), // Use worktree as main dir for review
        disallowed_tools: vec![],
        git_author_name: git_name,
        git_author_email: git_email,
        output_file: output_file.clone(),
        timeout_seconds: 600, // 10 minute timeout for review
        repo: repo.to_string(),
        attempt: 1,
    };

    // 7. Spawn review agent in tmux
    let session = match runner::agent::spawn_in_tmux(tmux, &invocation).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(task_id = task.id.0, error = %e, "failed to spawn review agent");
            return Ok(ReviewDecision::Failed(format!("spawn failed: {e}")));
        }
    };

    // 8. Wait for completion
    let poll_interval = std::time::Duration::from_secs(5);
    let timeout_duration = std::time::Duration::from_secs(600);

    let wait_result = tokio::time::timeout(
        timeout_duration,
        tmux.wait_for_completion(&session, poll_interval),
    )
    .await;

    match wait_result {
        Ok(Ok(_)) => {
            tracing::info!(task_id = task.id.0, "review agent completed");
            // Clean up tmux session on success
            let _ = tmux.kill_session(&session).await;
        }
        Ok(Err(e)) => {
            tracing::error!(task_id = task.id.0, error = %e, "review agent error");
            let _ = tmux.kill_session(&session).await;
            return Ok(ReviewDecision::Failed(format!("agent error: {e}")));
        }
        Err(_) => {
            tracing::error!(task_id = task.id.0, "review agent timed out");
            let _ = tmux.kill_session(&session).await;
            return Ok(ReviewDecision::Failed("timeout".to_string()));
        }
    }

    // 9. Read and parse response
    let raw_output = runner::response::read_output_file(&review_task_id, &output_file, repo);

    let review_response = match runner::response::parse_review_response(&raw_output) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(task_id = task.id.0, error = %e, "failed to parse review response");
            return Ok(ReviewDecision::Failed(format!("parse error: {e}")));
        }
    };

    // 10. Build automated review comment for the PR (before moving fields)
    let review_notes_for_comment = review_response.notes.clone();

    // 11. Convert to ReviewDecision
    let decision = match review_response.decision.as_str() {
        "approve" => ReviewDecision::Approve,
        "request_changes" => ReviewDecision::RequestChanges {
            notes: review_response.notes,
            issues: review_response.issues,
        },
        _ => ReviewDecision::Failed(format!("unknown decision: {}", review_response.decision)),
    };

    tracing::info!(
        task_id = task.id.0,
        decision = ?decision,
        "review agent decision received"
    );

    // 12. Post automated review comment on the PR
    let gh = GhCli::new();
    let pr_number = gh.get_pr_number(repo, &branch_name).await.ok().flatten();

    if let Some(pr_num) = pr_number {
        let pr_comment = match &decision {
            ReviewDecision::Approve => {
                format!(
                    "## Automated Review \u{2014} Approve\n\n{}",
                    review_notes_for_comment
                )
            }
            ReviewDecision::RequestChanges { notes, issues } => {
                let mut body = format!(
                    "## Automated Review \u{2014} Changes Requested\n\n{}\n",
                    notes
                );
                if !issues.is_empty() {
                    body.push_str("\n**Issues Found:**\n");
                    for issue in issues {
                        body.push_str(&format!(
                            "- `{}` line {}: {} [{}]\n",
                            issue.file,
                            issue
                                .line
                                .map(|l| l.to_string())
                                .unwrap_or_else(|| "?".to_string()),
                            issue.description,
                            issue.severity
                        ));
                    }
                }
                body
            }
            _ => String::new(),
        };

        if !pr_comment.is_empty() {
            if let Err(e) = gh.add_comment(repo, &pr_num.to_string(), &pr_comment).await {
                tracing::warn!(
                    task_id = task.id.0,
                    pr_number = pr_num,
                    error = %e,
                    "failed to post automated review comment on PR"
                );
            }
        }
    }

    // 13. Handle the decision
    match decision {
        ReviewDecision::Approve => {
            let auto_merge = config::get("workflow.auto_merge")
                .map(|v| v == "true")
                .unwrap_or(true);

            if auto_merge {
                if let Err(e) = auto_merge_pr(task, &branch_name, backend, repo).await {
                    tracing::error!(task_id = task.id.0, error = %e, "auto-merge failed");
                    return Ok(ReviewDecision::Failed(format!("merge failed: {e}")));
                }
            }
            Ok(ReviewDecision::Approve)
        }
        ReviewDecision::RequestChanges {
            ref notes,
            ref issues,
        } => {
            handle_review_changes(task, notes, issues, backend, repo).await?;
            Ok(decision)
        }
        _ => Ok(decision),
    }
}

/// Auto-merge a PR after review approval.
///
/// Checks that the automated review comment says "approve" and that CI checks
/// are green before merging. If the review gate workflow failed, re-triggers it
/// and polls until CI passes (up to a timeout).
async fn auto_merge_pr(
    task: &ExternalTask,
    branch: &str,
    backend: &Arc<dyn ExternalBackend>,
    repo: &str,
) -> anyhow::Result<()> {
    // 1. Get PR number from branch
    let gh = GhCli::new();
    let pr_number = match gh.get_pr_number(repo, branch).await? {
        Some(n) => n,
        None => {
            anyhow::bail!("no open PR found for branch {}", branch);
        }
    };

    // 2. Verify the automated review comment says "approve"
    let review_status = gh.get_automated_review_status(repo, pr_number).await?;
    match review_status.as_deref() {
        Some("approve") => {
            tracing::info!(task_id = task.id.0, pr_number, "automated review approved");
        }
        Some("changes_requested") => {
            tracing::warn!(
                task_id = task.id.0,
                pr_number,
                "automated review says changes_requested — skipping merge"
            );
            return Ok(());
        }
        _ => {
            tracing::info!(
                task_id = task.id.0,
                pr_number,
                "no automated review comment found — proceeding with merge"
            );
        }
    }

    // 3. Re-trigger the review gate workflow so it picks up the approve comment
    if let Err(e) = gh.dispatch_workflow(repo, "orch-review.yml", branch).await {
        tracing::debug!(
            task_id = task.id.0,
            error = %e,
            "failed to dispatch orch-review workflow (may not exist yet)"
        );
    }

    // 4. Wait for CI checks to pass (poll up to 5 minutes)
    let max_wait = std::time::Duration::from_secs(300);
    let poll_interval = std::time::Duration::from_secs(15);
    let start = std::time::Instant::now();

    loop {
        let (state, total, passing, failing, pending) =
            gh.get_combined_status(repo, branch).await?;

        tracing::info!(
            task_id = task.id.0,
            pr_number,
            state = %state,
            total,
            passing,
            failing,
            pending,
            "CI status check"
        );

        match state.as_str() {
            "success" => break,
            "failure" => {
                // If there are failed runs, try re-running them once
                if start.elapsed() < std::time::Duration::from_secs(30) {
                    if let Ok(Some((run_id, _, _))) =
                        gh.get_latest_run_for_branch(repo, branch).await
                    {
                        let _ = gh.rerun_failed_jobs(repo, run_id).await;
                        tracing::info!(task_id = task.id.0, run_id, "re-triggered failed CI jobs");
                    }
                }

                if start.elapsed() >= max_wait {
                    tracing::warn!(task_id = task.id.0, "CI checks still failing after timeout");
                    backend
                        .post_comment(
                            &task.id,
                            "⚠️ Auto-merge: CI checks are failing. PR is approved but cannot be merged until CI passes.",
                        )
                        .await?;
                    backend.update_status(&task.id, Status::NeedsReview).await?;
                    return Ok(());
                }
            }
            _ => {
                // pending
                if start.elapsed() >= max_wait {
                    tracing::warn!(task_id = task.id.0, "CI checks still pending after timeout");
                    backend
                        .post_comment(
                            &task.id,
                            "⚠️ Auto-merge: CI checks still pending after timeout. Will merge when checks complete.",
                        )
                        .await?;
                    // Don't merge with unknown CI status — set status to in_review
                    // so the next engine tick re-checks
                    backend.update_status(&task.id, Status::InReview).await?;
                    return Ok(());
                }
            }
        }

        tokio::time::sleep(poll_interval).await;
    }

    tracing::info!(
        task_id = task.id.0,
        pr_number,
        branch = %branch,
        "merging PR"
    );

    // 5. Merge via gh CLI
    if let Err(e) = gh.merge_pr(repo, pr_number, true).await {
        // Merge failed (conflicts, branch protection, etc.)
        tracing::error!(task_id = task.id.0, error = %e, "merge failed");
        backend.update_status(&task.id, Status::NeedsReview).await?;
        backend
            .post_comment(&task.id, &format!("Auto-merge failed: {}", e))
            .await?;
        return Err(e);
    }

    // 6. Update status to done
    backend.update_status(&task.id, Status::Done).await?;

    // 7. Close issue if auto_close enabled
    let auto_close = config::get("workflow.auto_close")
        .map(|v| v == "true")
        .unwrap_or(true);

    if auto_close {
        if let Err(e) = gh.close_issue(repo, &task.id.0).await {
            tracing::warn!(task_id = task.id.0, error = %e, "failed to close issue after merge");
        }
    }

    // 8. Mark worktree for cleanup
    let _ = sidecar::set(&task.id.0, &["worktree_cleaned=false".to_string()]);

    // 9. Post final comment on task issue
    backend
        .post_comment(&task.id, "✅ PR reviewed, approved, and merged.")
        .await?;

    tracing::info!(task_id = task.id.0, "auto-merge completed");

    Ok(())
}

/// Handle review changes request — re-dispatch the original agent.
async fn handle_review_changes(
    task: &ExternalTask,
    notes: &str,
    issues: &[crate::engine::runner::response::ReviewIssue],
    backend: &Arc<dyn ExternalBackend>,
    _repo: &str,
) -> anyhow::Result<()> {
    // 1. Check review cycle count (max 2 review rounds)
    let review_cycles: u32 = sidecar::get(&task.id.0, "review_cycles")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let max_cycles: u32 = config::get("workflow.max_review_cycles")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);

    if review_cycles >= max_cycles {
        // Too many review cycles — escalate to human
        tracing::warn!(
            task_id = task.id.0,
            review_cycles,
            max_cycles,
            "max review cycles exceeded, escalating to human"
        );
        backend.update_status(&task.id, Status::NeedsReview).await?;
        backend.post_comment(&task.id, &format!(
            "🔍 Review agent requested changes after {} cycles. Escalating to human.\n\n**Review Notes:**\n{}",
            review_cycles, notes
        )).await?;
        return Ok(());
    }

    // 2. Post review feedback as comment
    let mut comment = format!(
        "🔍 Review agent requested changes (cycle {} of {}):\n\n{}",
        review_cycles + 1,
        max_cycles,
        notes
    );

    if !issues.is_empty() {
        comment.push_str("\n\n**Issues Found:**\n");
        for issue in issues {
            comment.push_str(&format!(
                "- `{}` line {}: {} [{}]\n",
                issue.file,
                issue
                    .line
                    .map(|l| l.to_string())
                    .unwrap_or_else(|| "?".to_string()),
                issue.description,
                issue.severity
            ));
        }
    }

    backend.post_comment(&task.id, &comment).await?;

    // 3. Update sidecar with review context (including pr_review_context so the
    //    re-dispatched agent can see what the reviewer found wrong)
    let review_context = format!(
        "A reviewer has requested changes on your PR. Please address the following feedback:\n\n{}",
        comment
    );
    let _ = sidecar::set(
        &task.id.0,
        &[
            format!("review_cycles={}", review_cycles + 1),
            format!("review_notes={}", notes),
            format!("pr_review_context={}", review_context),
            "review_started=false".to_string(),
            "status=routed".to_string(),
        ],
    );

    // 4. Re-dispatch — set status back to routed (keeps same agent/branch/worktree)
    backend.update_status(&task.id, Status::Routed).await?;

    tracing::info!(
        task_id = task.id.0,
        review_cycles = review_cycles + 1,
        "re-dispatched task for review changes"
    );

    Ok(())
}

/// Get the model for a given complexity and agent.
fn get_model_for_complexity(complexity: &str, agent: &str) -> String {
    // Read from config model_map
    let config_key = format!("model_map.{}.{}", complexity, agent);
    match config::get(&config_key) {
        Ok(model) => model,
        Err(_) => {
            // Defaults
            match agent {
                "claude" => match complexity {
                    "simple" => "haiku".to_string(),
                    "medium" => "sonnet".to_string(),
                    "complex" | "review" => "sonnet".to_string(),
                    _ => "sonnet".to_string(),
                },
                "codex" => match complexity {
                    "simple" => "gpt-5.1-codex-mini".to_string(),
                    "medium" | "review" => "gpt-5.2".to_string(),
                    "complex" => "gpt-5.3-codex".to_string(),
                    _ => "gpt-5.2".to_string(),
                },
                _ => "sonnet".to_string(),
            }
        }
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

    #[test]
    fn test_get_model_for_complexity_returns_nonempty() {
        // Should always return a non-empty model name (from config or defaults)
        assert!(!get_model_for_complexity("simple", "claude").is_empty());
        assert!(!get_model_for_complexity("medium", "claude").is_empty());
        assert!(!get_model_for_complexity("complex", "claude").is_empty());
        assert!(!get_model_for_complexity("review", "claude").is_empty());
    }

    #[test]
    fn test_get_model_for_complexity_unknown_agent() {
        // Unknown agents should still return a model name
        let model = get_model_for_complexity("simple", "unknown_agent_xyz");
        assert!(!model.is_empty());
    }
}
