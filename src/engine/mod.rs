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

        // Health check
        if let Err(e) = backend.health_check().await {
            tracing::warn!(repo = %repo, ?e, "backend health check failed, skipping project");
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
        anyhow::bail!("no valid projects configured");
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

    if project_engines.is_empty() {
        anyhow::bail!(
            "no valid projects configured — run `orch init` or add repos to ~/.orch/config.yml"
        );
    }

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

    // Shutdown broadcast channel for graceful shutdown
    let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);

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

        // Get shutdown receiver for webhook server
        let webhook_shutdown = shutdown_tx.subscribe();

        // Spawn the HTTP server with graceful shutdown
        tokio::spawn(async move {
            if let Err(e) = start_webhook_server(port, secret, webhook_repo, tx, webhook_shutdown).await {
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
        tracing::info!(port, "webhook server started with graceful shutdown support");
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
    tracing::info!("initiating graceful shutdown...");
    
    // Signal webhook server to shut down
    if webhook_enabled {
        tracing::info!("signaling webhook server to shut down...");
        let _ = shutdown_tx.send(());
        // Give webhook server time to shut down gracefully
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    
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
                backend.update_status(&task.id, Status::New).await?;
                backend
                    .post_comment(
                        &task.id,
                        &format!(
                            "[{}] recovered: stuck in_progress for {}m with no active session",
                            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
                            age.num_minutes()
                        ),
                    )
                    .await?;
            }
        }
    }
    drop(_phase2);

    // Phase 3a: Route new tasks
    let _phase3a = tracing::info_span!("engine.tick.phase3a.route").entered();
    let new_tasks = task_manager.list_external_by_status(Status::New).await?;
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
                        status,
                        agent: agent_name.clone(),
                        duration_seconds: duration,
                        summary,
                    });
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
    let repo_root = resolve_repo_root().await?;

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
                .output()
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
                    .output()
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

/// Resolve the main git repository root path.
async fn resolve_repo_root() -> anyhow::Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!("failed to resolve git repo root");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
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

/// Review open PRs - parse review comments and create follow-up tasks.
///
/// Lists tasks in review, fetches PR reviews, and creates follow-up tasks
/// for each review comment that requests changes.
async fn review_open_prs(
    backend: &Arc<dyn ExternalBackend>,
    db: &Arc<Db>,
    repo: &str,
    config: &EngineConfig,
) -> anyhow::Result<()> {
    // Get tasks that are in review (have open PRs)
    let in_review_tasks = backend.list_by_status(Status::InReview).await?;

    if in_review_tasks.is_empty() {
        return Ok(());
    }

    // Check if we should process reviews
    let auto_create_followup = config.auto_create_followup_on_changes;
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

        // Get the agent that created this PR (for routing follow-ups)
        let original_agent = task
            .labels
            .iter()
            .find(|l| l.starts_with("agent:"))
            .map(|l| l.strip_prefix("agent:").unwrap_or(l).to_string());

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

        // Check for existing review follow-up tasks for this PR
        let existing_reviews = get_existing_review_tasks(db, task_id).await?;

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

        // Handle fully-approved PRs
        if all_approved && auto_close_task {
            tracing::info!(
                task_id,
                pr_number,
                "PR approved by all reviewers, closing task (marking as done)"
            );
            if let Err(e) = backend.update_status(&task.id, Status::Done).await {
                tracing::warn!(task_id, err = %e, "failed to update task status to done");
            }
            // Note: this only marks the task as Done; actual PR merge
            // is left to GitHub branch-protection / manual action.
            continue;
        }

        // Process reviews that request changes
        if !any_changes_requested {
            continue;
        }

        if !auto_create_followup {
            tracing::debug!(
                task_id,
                pr_number,
                "auto_create_followup_on_changes is disabled, skipping"
            );
            continue;
        }

        // Only process the latest CHANGES_REQUESTED reviews (already deduplicated)
        for review in deduped_reviews
            .values()
            .filter(|r| r.state == "CHANGES_REQUESTED")
        {
            // Get comments for this review
            let review_comments: Vec<GitHubReviewComment> = all_comments
                .iter()
                .filter(|c| {
                    // Comments are associated with reviews via the review_id field
                    // The GitHub API includes this in the response
                    c.user.login == review.user.login && c.created_at >= review.submitted_at
                })
                .cloned()
                .collect();

            let pr_review = PullRequestReview {
                review: (*review).clone(),
                comments: review_comments.clone(),
            };

            // Process actionable comments
            for comment in pr_review.actionable_comments() {
                // Create a unique ID for this review comment
                let review_comment_id = format!("review_comment_{}_{}", pr_number, comment.id);

                // Skip if we already created a task for this comment
                if existing_reviews.contains(&review_comment_id) {
                    tracing::debug!(
                        task_id,
                        comment_id = comment.id,
                        "review comment already has follow-up task"
                    );
                    continue;
                }

                // Create follow-up task
                if let Err(e) = create_review_follow_up_task(
                    backend,
                    db,
                    task_id,
                    &review_comment_id,
                    &pr_review,
                    comment,
                    pr_number,
                    &branch,
                    original_agent.as_deref(),
                )
                .await
                {
                    tracing::warn!(
                        task_id,
                        comment_id = comment.id,
                        err = %e,
                        "failed to create follow-up task"
                    );
                }
            }
        }
    }

    Ok(())
}

/// Get existing review tasks for a parent task.
async fn get_existing_review_tasks(
    db: &Arc<Db>,
    parent_task_id: &str,
) -> anyhow::Result<std::collections::HashSet<String>> {
    let mut existing = std::collections::HashSet::new();

    // Check internal tasks for review follow-ups
    for status in &[
        TaskStatus::New,
        TaskStatus::InProgress,
        TaskStatus::Done,
        TaskStatus::Blocked,
    ] {
        let tasks = db.list_internal_tasks_by_status(*status).await?;
        for t in tasks {
            if t.source == "pr_review" && t.source_id.starts_with(parent_task_id) {
                // Extract the review comment ID from source_id (format: "{parent_id}:{review_comment_id}")
                if let Some(idx) = t.source_id.find(':') {
                    existing.insert(t.source_id[idx + 1..].to_string());
                }
            }
        }
    }

    Ok(existing)
}

/// Create a follow-up task for a review comment.
#[allow(clippy::too_many_arguments)]
async fn create_review_follow_up_task(
    backend: &Arc<dyn ExternalBackend>,
    db: &Arc<Db>,
    parent_task_id: &str,
    review_comment_id: &str,
    review: &PullRequestReview,
    comment: &GitHubReviewComment,
    pr_number: u64,
    branch: &str,
    original_agent: Option<&str>,
) -> anyhow::Result<()> {
    let reviewer = &review.review.user.login;

    // Build task title
    let title = format!(
        "Address review comment on {} by @{}",
        comment.path, reviewer
    );

    // Build task body with context
    let mut body = format!(
        "## PR Review Comment

**File:** `{}`\n",
        comment.path
    );

    if let Some(line) = comment.line {
        body.push_str(&format!("**Line:** {}\n", line));
    }

    body.push_str(&format!(
        "**Reviewer:** @{}\n**PR:** #{}\n**Branch:** `{}`\n\n",
        reviewer, pr_number, branch
    ));

    // Add diff context if available
    if let Some(ref diff_hunk) = comment.diff_hunk {
        body.push_str("### Diff Context\n```diff\n");
        // Truncate very long diffs
        let diff_preview = if diff_hunk.len() > 2000 {
            format!("{}...", &diff_hunk[..2000])
        } else {
            diff_hunk.clone()
        };
        body.push_str(&diff_preview);
        body.push_str("\n```\n\n");
    }

    body.push_str("### Review Comment\n> ");
    body.push_str(&comment.body.replace('\n', "\n> "));
    body.push_str("\n\n");

    // Add review body if present
    if let Some(ref review_body) = review.review.body {
        if !review_body.trim().is_empty() {
            body.push_str("### Overall Review Notes\n");
            body.push_str(review_body);
            body.push_str("\n\n");
        }
    }

    body.push_str("---\n**Parent Task:** #");
    body.push_str(parent_task_id);
    body.push('\n');

    // Create labels
    let mut labels = vec!["pr-review-followup".to_string(), "status:new".to_string()];

    // Route to the same agent that created the PR
    if let Some(agent) = original_agent {
        labels.push(format!("agent:{}", agent));
    }

    // Create as internal task (can be promoted to GitHub issue if needed)
    let source_id = format!("{}:{}", parent_task_id, review_comment_id);

    let task_id = db
        .create_internal_task(&title, &body, "pr_review", &source_id)
        .await?;

    // Store PR info in sidecar for the follow-up task
    let _ = sidecar::set(
        &task_id.to_string(),
        &[
            format!("pr_number={}", pr_number),
            format!("branch={}", branch),
            format!("reviewer={}", reviewer),
            format!("file_path={}", comment.path),
            format!("parent_task_id={}", parent_task_id),
        ],
    );

    // Set the agent if we know it
    if let Some(agent) = original_agent {
        let _ = db.set_internal_task_agent(task_id, Some(agent)).await;
    }

    tracing::info!(
        parent_task_id,
        follow_up_task_id = task_id,
        pr_number,
        file_path = %comment.path,
        "created follow-up task for review comment"
    );

    // Post comment on the original GitHub issue about the follow-up
    let comment_body = format!(
        "[{}] Created follow-up task #{} to address review comment on `{}` by @{}.",
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
        task_id,
        comment.path,
        reviewer
    );
    let _ = backend
        .post_comment(&ExternalId(parent_task_id.to_string()), &comment_body)
        .await;

    Ok(())
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
