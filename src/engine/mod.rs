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

pub mod cleanup;
pub mod commands;
pub mod jobs;
pub mod review;
pub mod router;
pub mod runner;
pub mod sync;
pub mod tasks;
pub mod tick;

/// Standard Orch attribution footer for issue bodies, PR bodies, and comments.
///
/// Append to every user-visible string posted to GitHub so activity is
/// clearly attributed to the orchestrator rather than appearing as a human post.
pub fn orch_footer() -> &'static str {
    "\n\n---\n*Posted by [Orch](https://github.com/gabrielkoerich/orch)*"
}

use crate::backends::{ExternalBackend, ExternalId};
use crate::channels::capture::CaptureService;
use crate::channels::discord_ws::DiscordGateway;
use crate::channels::github::start_webhook_server;
use crate::channels::notification::NotificationLevel;
use crate::channels::slack::SlackChannel;
use crate::channels::telegram::TelegramChannel;
use crate::channels::tmux::TmuxChannel;
use crate::channels::transport::Transport;
use crate::channels::{Channel, ChannelRegistry, IncomingMessage, OutgoingMessage};
use crate::config;
use crate::db::Db;
use crate::engine::router::Router;
use crate::engine::tasks::TaskManager;
use crate::github::http::{rate_limit_metrics, GhHttp};
use crate::repo_context::REPO_CONTEXT;
use crate::store::TaskStore;
use crate::tmux::TmuxManager;
use runner::WeightSignal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
pub struct ProjectEngine {
    pub repo: String,
    pub backend: Arc<dyn ExternalBackend>,
    pub task_manager: Arc<TaskManager>,
    pub runner: Arc<runner::TaskRunner>,
    pub store: Arc<TaskStore>,
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
    /// Stuck task timeout for tasks with an active tmux session (seconds)
    pub stuck_timeout: u64,
    /// Stuck task timeout for tasks with no active tmux session (seconds).
    /// Shorter than `stuck_timeout` because no session means the agent has already exited.
    pub no_session_stuck_timeout: u64,
    /// Auto-create follow-up tasks when PR reviews request changes
    pub auto_create_followup_on_changes: bool,
    /// Auto-close task (mark Done) when all PR reviews are approved.
    /// Note: this does NOT merge the PR itself -- only updates the task status.
    pub auto_close_task_on_approval: bool,
    /// Graceful shutdown timeout — how long to wait for running agents before exiting.
    pub graceful_shutdown_timeout: std::time::Duration,
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
            auto_create_followup_on_changes: true,
            auto_close_task_on_approval: false,
            graceful_shutdown_timeout: std::time::Duration::from_secs(600),
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

        if let Ok(val) = crate::config::get("engine.no_session_stuck_timeout") {
            if let Ok(secs) = val.parse::<u64>() {
                config.no_session_stuck_timeout = secs;
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

        // Check auto_close_task_on_approval first; fall back to workflow.auto_close
        // (common config uses "auto_close: true" which should also enable approval handling)
        if let Ok(val) = crate::config::get("workflow.auto_close_task_on_approval") {
            config.auto_close_task_on_approval = val.eq_ignore_ascii_case("true");
        } else if let Ok(val) = crate::config::get("workflow.auto_close") {
            config.auto_close_task_on_approval = val.eq_ignore_ascii_case("true");
        }

        if let Ok(val) = crate::config::get("engine.graceful_shutdown_timeout") {
            if let Ok(secs) = val.parse::<u64>() {
                config.graceful_shutdown_timeout = std::time::Duration::from_secs(secs);
            }
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

    if repos.is_empty() {
        let config_path = crate::home::config_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "~/.orch/config.yml".to_string());
        anyhow::bail!(
            "no projects configured — add repos under `projects:` in {}",
            config_path
        );
    }

    let mut engines = Vec::new();
    let mut auth_failures = 0usize;
    let mut network_failures = 0usize;

    for repo in repos {
        tracing::info!(repo = %repo, "initializing project engine");

        // Initialize backend
        let backend: Arc<dyn ExternalBackend> =
            Arc::new(crate::backends::github::GitHubBackend::new(repo.clone()));

        // Health check — verifies network and GitHub authentication
        if let Err(e) = backend.health_check().await {
            let err_str = e.to_string();
            if err_str.contains("401")
                || err_str.contains("Bad credentials")
                || err_str.contains("authentication")
                || err_str.contains("No GitHub token")
            {
                auth_failures += 1;
                tracing::warn!(
                    repo = %repo,
                    error = %e,
                    "GitHub auth failed for {repo} — run `gh auth login`"
                );
            } else {
                network_failures += 1;
                tracing::warn!(
                    repo = %repo,
                    error = %e,
                    "GitHub unreachable for {repo} (network unavailable?)"
                );
            }
            continue;
        }
        tracing::info!(repo = %repo, backend = backend.name(), "backend connected");

        // Initialize database (shared between task manager and runner for metrics)
        let db = Arc::new(Db::open(&crate::db::default_path()?)?);
        db.migrate().await?;

        // Initialize unified task store (sqlx)
        let store = Arc::new(TaskStore::open(&crate::db::default_path()?).await?);

        // Initialize task manager (with unified store for dual-write)
        let task_manager = Arc::new(TaskManager::with_store(
            db.clone(),
            backend.clone(),
            store.clone(),
            repo.clone(),
        ));

        // Task runner (with db for metrics)
        let runner = Arc::new(
            runner::TaskRunner::new(repo.clone())
                .with_db(db.clone())
                .with_store(store.clone()),
        );

        engines.push(ProjectEngine {
            repo,
            backend,
            task_manager,
            runner,
            store,
        });
    }

    if engines.is_empty() {
        if auth_failures > 0 && network_failures == 0 {
            anyhow::bail!("GitHub auth failed for all projects — run `gh auth login`");
        } else if network_failures > 0 && auth_failures == 0 {
            anyhow::bail!("GitHub unreachable for all projects — check network connectivity");
        } else {
            anyhow::bail!(
                "all project backends failed ({auth_failures} auth, {network_failures} network)"
            );
        }
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

    // Initialize project engines — retry with backoff so a network outage at
    // startup doesn't cause a crash-loop (launchd KeepAlive would restart us
    // immediately, filling the error log with 600+ identical messages).
    let mut project_engines = {
        let mut delay_secs = 5u64;
        loop {
            match init_project_engines().await {
                Ok(engines) => break engines,
                Err(e) => {
                    tracing::warn!(delay_secs, "project engine init failed, retrying: {e}");
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

    // Re-create task managers with shared db and store
    for engine in &mut project_engines {
        engine.task_manager = Arc::new(TaskManager::with_store(
            db.clone(),
            engine.backend.clone(),
            engine.store.clone(),
            engine.repo.clone(),
        ));
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
            let discord = DiscordGateway::new(token, channel_id, shard_id, shard_count);
            if let Err(e) = discord.health_check().await {
                tracing::warn!(?e, "discord gateway health check failed, skipping");
            } else {
                channel_registry.register(Box::new(discord));
                tracing::info!(shard_id, shard_count, "discord gateway registered");
            }
        }
    }

    // Try to initialize Slack channel
    if let Ok(token) = crate::config::get("channels.slack.bot_token") {
        if !token.is_empty() {
            let channel_id = crate::config::get("channels.slack.channel_id").ok();
            let slack = SlackChannel::new(token, channel_id);
            if let Err(e) = slack.health_check().await {
                tracing::warn!(?e, "slack channel health check failed, skipping");
            } else {
                channel_registry.register(Box::new(slack));
                tracing::info!("slack channel registered");
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
    for mut rx in channel_receivers {
        let transport = transport_for_messages.clone();
        let tmux = tmux_for_messages.clone();
        let capture = capture_for_messages.clone();
        let channels = channels_for_messages.clone();
        let engine_refs = engine_refs.clone();
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                tracing::debug!(channel = %msg.channel, thread = %msg.thread_id, "received message from channel");
                handle_channel_message(msg, &transport, &tmux, &capture, &channels, &engine_refs)
                    .await;
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
                                "slack" => (notification.format_slack(), true),
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
    // Start webhook server if configured
    let webhook_enabled = crate::config::get("webhook.enabled")
        .map(|v| v == "true")
        .unwrap_or(false);

    // Shared status updated by the server-spawn task and the health-check loop.
    let webhook_status = std::sync::Arc::new(std::sync::Mutex::new(
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
        {
            let mut s = webhook_status.lock().unwrap();
            s.configured = true;
            s.port = Some(port);
            s.healthy = false;
            s.fallback_mode = false;
            s.save();
        }

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
                            let mut s = status_for_spawn.lock().unwrap();
                            s.fallback_mode = true;
                            s.healthy = false;
                            s.last_failure_reason = Some(reason);
                            s.startup_attempts = attempt;
                            s.save();
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
                            let mut s = status_for_spawn.lock().unwrap();
                            s.startup_attempts = attempt;
                            s.last_failure_reason = Some(reason);
                            s.save();
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
                let _ = transport_for_webhook.route(&msg).await;
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
            let mut s = webhook_status.lock().unwrap();
            s.configured = false;
            s.fallback_mode = true;
            s.save();
        }
        tracing::info!(
            orch_webhook_in_fallback = true,
            "webhook server disabled, using polling fallback mode"
        );
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

    // In-memory dispatch guard: tracks task IDs currently being dispatched.
    // Guards against GitHub API eventual consistency — after update_status(InProgress),
    // the label-removal webhook can trigger an immediate tick where list_by_status(Routed)
    // still returns the task (search index propagation delay). The tmux session does not
    // exist until the runner completes worktree setup (~10s later), so session_exists
    // alone is insufficient. Keyed by "{repo}/{task_id}".
    let dispatching: Arc<std::sync::Mutex<std::collections::HashSet<String>>> =
        Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));

    // Subscribe to config file changes for hot reload
    let mut config_rx = crate::config::subscribe();

    // Track sync interval
    let mut last_sync = std::time::Instant::now();

    // Channel for weight signals from task runners back to the router
    let (weight_tx, mut weight_rx) = mpsc::channel::<WeightSignal>(64);

    // Reset stale InReview tasks on startup — if a review agent was running when
    // the engine restarted, the tmux session is gone. Move back to NeedsReview
    // so the next tick re-triggers the review agent.
    for engine in &project_engines {
        if let Ok(in_review) = engine.backend.list_by_status(Status::InReview).await {
            for task in &in_review {
                if let Err(e) = engine
                    .backend
                    .update_status(&task.id, Status::NeedsReview)
                    .await
                {
                    tracing::warn!(
                        task_id = task.id.0,
                        err = %e,
                        "failed to reset stale InReview task on startup"
                    );
                }
            }
            if !in_review.is_empty() {
                tracing::info!(
                    repo = %engine.repo,
                    count = in_review.len(),
                    "reset stale InReview tasks to NeedsReview on startup"
                );
            }
        }

        // Also reset internal (SQLite) InReview tasks on startup.
        use crate::db::TaskStatus as DbStatus;
        if let Ok(internal_in_review) = engine
            .task_manager
            .db_list_internal_by_status(DbStatus::InReview)
            .await
        {
            for task in &internal_in_review {
                let task_id = format!("internal:{}", task.id);
                if let Err(e) = engine
                    .task_manager
                    .update_task_status(&ExternalId(task_id.clone()), Status::NeedsReview)
                    .await
                {
                    tracing::warn!(
                        task_id,
                        err = %e,
                        "failed to reset stale internal InReview task on startup"
                    );
                }
            }
            if !internal_in_review.is_empty() {
                tracing::info!(
                    repo = %engine.repo,
                    count = internal_in_review.len(),
                    "reset stale internal InReview tasks to NeedsReview on startup"
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

    // Signal handlers (launchd/systemd send SIGTERM to stop services, SIGHUP for reload/restart)
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;

    // Graceful shutdown flag — when set, the engine stops dispatching new tasks
    // but continues ticking to monitor running sessions until they complete.
    let shutting_down = Arc::new(AtomicBool::new(false));
    let mut drain_deadline: Option<tokio::time::Instant> = None;

    loop {
        let is_draining = shutting_down.load(Ordering::Relaxed);

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
                        WeightSignal::Blocked | WeightSignal::None => {}
                    }
                }

                // Tick weight recovery for rate-limited agents
                {
                    let mut rw = router.write().await;
                    rw.tick_weight_recovery();
                }

                // During graceful shutdown, only check session completions —
                // no new routing, dispatch, or sync work.
                if is_draining {
                    for engine in &project_engines {
                        let repo = engine.repo.clone();
                        REPO_CONTEXT.scope(repo, async {
                            if let Err(e) = tick::tick_check_session_completions(&tmux, &engine.repo, &capture_for_tick).await {
                                tracing::error!(repo = %engine.repo, ?e, "session completion check failed during drain");
                            }
                        }).await;
                    }

                    // Check if all sessions have finished
                    let sessions = tmux.list_sessions().await.unwrap_or_default();
                    if sessions.is_empty() {
                        tracing::info!("all agent sessions completed, shutting down");
                        break;
                    } else {
                        tracing::info!(
                            remaining = sessions.len(),
                            sessions = ?sessions.iter().map(|s| &s.name).collect::<Vec<_>>(),
                            "waiting for agent sessions to complete"
                        );
                    }
                    continue;
                }

                // Skip tick/sync entirely if GitHub API is rate-limited
                if let Some(remaining) = GhHttp::is_rate_limited() {
                    let m = rate_limit_metrics();
                    tracing::warn!(
                        remaining_secs = remaining.as_secs(),
                        rate_limit_hits = m.hits,
                        proactive_throttles = m.proactive_throttles,
                        wait_secs_total = m.wait_secs,
                        rest_remaining = ?m.rest_remaining,
                        graphql_remaining = ?m.graphql_remaining,
                        "GitHub API rate-limited, skipping tick"
                    );
                } else {
                    // Core tick: poll tasks for all projects
                    let mut router_guard = router.write().await;
                    for engine in &project_engines {
                        let repo = engine.repo.clone();
                        REPO_CONTEXT.scope(repo, async {
                            if let Err(e) = tick::tick(
                                &engine.backend,
                                &tmux,
                                &engine.repo,
                                &engine.runner,
                                &capture_for_tick,
                                &semaphore,
                                &config,
                                &jobs_path,
                                &db,
                                &mut router_guard,
                                &router,
                                &engine.task_manager,
                                &weight_tx,
                                &transport,
                                &dispatching,
                                &engine.store,
                            ).await {
                                tracing::error!(repo = %engine.repo, ?e, "tick failed for project");
                            }
                        }).await;
                    }
                    drop(router_guard);

                    // Periodic sync (less frequent)
                    if last_sync.elapsed() >= config.sync_interval {
                        for engine in &project_engines {
                            let repo = engine.repo.clone();
                            REPO_CONTEXT.scope(repo, async {
                                if let Err(e) = sync::sync_tick(&engine.backend, &tmux, &engine.repo, &db, &config, &semaphore, &router, &engine.task_manager, &engine.store).await {
                                    tracing::error!(repo = %engine.repo, ?e, "sync tick failed for project");
                                }
                            }).await;
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
                                let mut s = webhook_status.lock().unwrap();
                                s.healthy = health;
                                s.last_check_utc = Some(chrono::Utc::now());
                                if health {
                                    s.last_failure_reason = None;
                                } else if failure_reason.is_some() {
                                    s.last_failure_reason = failure_reason;
                                }
                                s.save();
                            }
                        }
                        last_webhook_health_check = std::time::Instant::now();
                    }
                }
            }
            // Webhook events trigger an immediate tick (bypass polling interval)
            _ = webhook_notify.notified(), if !is_draining => {
                if let Some(remaining) = GhHttp::is_rate_limited() {
                    tracing::warn!(
                        remaining_secs = remaining.as_secs(),
                        "GitHub API rate-limited, skipping webhook-triggered tick"
                    );
                } else {
                    tracing::info!("webhook event triggered immediate tick");

                    let mut router_guard = router.write().await;
                    for engine in &project_engines {
                        let repo = engine.repo.clone();
                        REPO_CONTEXT.scope(repo, async {
                            if let Err(e) = tick::tick(
                                &engine.backend,
                                &tmux,
                                &engine.repo,
                                &engine.runner,
                                &capture_for_tick,
                                &semaphore,
                                &config,
                                &jobs_path,
                                &db,
                                &mut router_guard,
                                &router,
                                &engine.task_manager,
                                &weight_tx,
                                &transport,
                                &dispatching,
                                &engine.store,
                            ).await {
                                tracing::error!(repo = %engine.repo, ?e, "webhook-triggered tick failed");
                            }
                        }).await;
                    }
                    drop(router_guard);
                }

                // Also reset the interval so we don't get a redundant tick right after
                interval.reset();
            }
            result = config_rx.recv(), if !is_draining => {
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
            signal_name = async {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => "SIGINT",
                    _ = sigterm.recv() => "SIGTERM",
                    _ = sighup.recv() => "SIGHUP",
                }
            } => {
                if is_draining {
                    tracing::warn!(signal = signal_name, "received signal during drain, forcing immediate shutdown");
                    break;
                }
                tracing::info!(signal = signal_name, "beginning graceful shutdown");
                shutting_down.store(true, Ordering::Relaxed);

                let sessions = tmux.list_sessions().await.unwrap_or_default();
                if sessions.is_empty() {
                    tracing::info!("no active sessions, shutting down immediately");
                    break;
                }
                tracing::info!(
                    count = sessions.len(),
                    timeout_secs = config.graceful_shutdown_timeout.as_secs(),
                    "waiting for running agents to complete before shutdown"
                );
                // Switch to a faster tick for drain monitoring
                interval = tokio::time::interval(std::time::Duration::from_secs(5));
                drain_deadline = Some(tokio::time::Instant::now() + config.graceful_shutdown_timeout);
            }
        }

        // Check drain deadline
        if let Some(deadline) = drain_deadline {
            if tokio::time::Instant::now() >= deadline {
                let sessions = tmux.list_sessions().await.unwrap_or_default();
                tracing::warn!(
                    remaining = sessions.len(),
                    sessions = ?sessions.iter().map(|s| &s.name).collect::<Vec<_>>(),
                    "graceful shutdown timeout reached, exiting with sessions still running"
                );
                break;
            }
        }
    }

    // transport and channels drop here at end of scope
    let _ = transport;
    tracing::info!("orch engine stopped");
    Ok(())
}

/// Send a reply message to a specific channel thread.
///
/// Iterates the channel registry and finds the channel by name,
/// then sends the message to the given thread ID.
async fn send_channel_reply(
    channels: &Arc<ChannelRegistry>,
    channel_name: &str,
    thread_id: &str,
    body: String,
) {
    for ch in channels.iter() {
        if ch.name() == channel_name {
            let msg = OutgoingMessage {
                thread_id: thread_id.to_string(),
                body,
                reply_to: None,
                metadata: serde_json::json!({}),
            };
            if let Err(e) = ch.send(&msg).await {
                tracing::warn!(
                    channel = channel_name,
                    thread_id,
                    ?e,
                    "failed to send channel reply"
                );
            }
            return;
        }
    }
    tracing::debug!(
        channel = channel_name,
        "channel not found in registry for reply"
    );
}

/// Forward a text message to an agent's tmux session via send-keys.
async fn forward_to_tmux(transport: &Arc<Transport>, task_id: &str, text: &str) {
    if let Some(binding) = transport.get_binding(task_id).await {
        if let Err(e) = crate::channels::tmux::send_keys(&binding.tmux_session, text).await {
            tracing::warn!(
                task_id,
                session = %binding.tmux_session,
                ?e,
                "failed to forward message to tmux session"
            );
        } else {
            tracing::debug!(
                task_id,
                session = %binding.tmux_session,
                "forwarded message to tmux"
            );
        }
    } else {
        tracing::warn!(task_id, "no tmux binding found, cannot forward message");
    }
}

/// Handle an incoming channel message by routing it to the appropriate action.
///
/// - `TaskSession`: slash command → execute on task; otherwise → forward to tmux
/// - `Command`: global commands like `/status`; task-specific commands need a task thread
/// - `NewTask`: create an internal task, bind thread, start output fanout
async fn handle_channel_message(
    msg: IncomingMessage,
    transport: &Arc<Transport>,
    _tmux: &Arc<TmuxManager>,
    capture: &Arc<CaptureService>,
    channels: &Arc<ChannelRegistry>,
    engine_refs: &[EngineRef],
) {
    use crate::backends::ExternalId;
    use crate::channels::stream::fanout_output;
    use crate::channels::transport::MessageRoute;
    use crate::engine::commands::{execute_command, parse_command};
    use crate::engine::tasks::{CreateTaskRequest, TaskType};

    match transport.route(&msg).await {
        MessageRoute::TaskSession { task_id } => {
            let body = msg.body.trim().to_string();
            let channel = msg.channel.clone();
            let thread_id = msg.thread_id.clone();

            if body.starts_with('/') {
                // Parse slash command and execute it on the bound task
                if let Some(cmd) = parse_command(&body) {
                    if let Some((repo, backend, _, store)) = engine_refs.first() {
                        let gh = GhHttp::new();
                        let ext_id = ExternalId(task_id.clone());
                        let result =
                            execute_command(backend, &gh, repo, &ext_id, &cmd, store).await;
                        let reply = match result {
                            Ok(r) => r,
                            Err(e) => format!("Command `{cmd}` failed: {e}"),
                        };
                        send_channel_reply(channels, &channel, &thread_id, reply).await;
                    }
                } else {
                    // Unknown command — forward to agent as-is
                    forward_to_tmux(transport, &task_id, &body).await;
                }
            } else {
                // Regular message — forward to the agent's tmux session
                forward_to_tmux(transport, &task_id, &body).await;
            }
        }

        MessageRoute::Command { raw } => {
            let cmd_str = raw.trim().to_string();
            let channel = msg.channel.clone();
            let thread_id = msg.thread_id.clone();

            // /status is not in OwnerCommand — handle it specially
            if cmd_str == "/status" || cmd_str.starts_with("/status ") {
                let mut lines = vec!["**Active tasks:**".to_string()];
                for (repo, _, task_manager, _) in engine_refs {
                    match task_manager
                        .list_external_by_status(Status::InProgress)
                        .await
                    {
                        Ok(tasks) => {
                            for t in &tasks {
                                lines.push(format!("- #{} [{}]: {}", t.id.0, repo, t.title));
                            }
                        }
                        Err(e) => {
                            tracing::warn!(repo, err = %e, "failed to list in-progress tasks");
                        }
                    }
                }
                if lines.len() == 1 {
                    lines.push("No tasks currently in progress.".to_string());
                }
                send_channel_reply(channels, &channel, &thread_id, lines.join("\n")).await;
            } else if let Some(cmd) = parse_command(&cmd_str) {
                let reply = format!(
                    "Command `{cmd}` requires a task context. \
                     Send it in a thread that is bound to a running task."
                );
                send_channel_reply(channels, &channel, &thread_id, reply).await;
            } else {
                let reply = "Available commands: /status (list tasks), /retry, /close, \
                             /block, /unblock, /review — send task-specific commands in a task thread."
                    .to_string();
                send_channel_reply(channels, &channel, &thread_id, reply).await;
            }
        }

        MessageRoute::NewTask => {
            let channel = msg.channel.clone();
            let thread_id = msg.thread_id.clone();

            // Pick the first configured project for new tasks from channels
            if let Some((repo, _, task_manager, _)) = engine_refs.first() {
                let title = if msg.body.len() > 80 {
                    format!("{}…", &msg.body[..80])
                } else {
                    msg.body.clone()
                };
                let req = CreateTaskRequest {
                    title,
                    body: msg.body.clone(),
                    task_type: TaskType::Internal,
                    labels: vec!["channel-created".to_string()],
                    source: channel.clone(),
                    source_id: thread_id.clone(),
                };
                match task_manager.create_task(req).await {
                    Ok(task) => {
                        use crate::engine::tasks::Task;
                        let task_id = match &task {
                            Task::Internal(t) => format!("internal:{}", t.id),
                            Task::External(t) => t.id.0.clone(),
                        };
                        // Bind the thread to the new task
                        transport
                            .bind(
                                &task_id,
                                &format!("orch-{repo}-{task_id}"),
                                &channel,
                                &thread_id,
                            )
                            .await;
                        // Register the session with CaptureService (graceful if no session yet)
                        capture
                            .register_session(&task_id, &format!("orch-{repo}-{task_id}"))
                            .await;
                        // Spawn output fanout for this task
                        let transport_clone = transport.clone();
                        let channels_clone = channels.clone();
                        let task_id_clone = task_id.clone();
                        tokio::spawn(async move {
                            fanout_output(task_id_clone, transport_clone, channels_clone).await;
                        });
                        let reply =
                            format!("Task created: `{task_id}` — I'll start working on it now.");
                        send_channel_reply(channels, &channel, &thread_id, reply).await;
                    }
                    Err(e) => {
                        tracing::warn!(repo, err = %e, "failed to create task from channel message");
                        let reply = format!("Failed to create task: {e}");
                        send_channel_reply(channels, &channel, &thread_id, reply).await;
                    }
                }
            } else {
                tracing::warn!("no project configured, cannot create task from channel message");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    #[test]
    fn engine_config_from_config_uses_defaults_when_no_config() {
        let config = EngineConfig::from_config();
        assert_eq!(config.tick_interval, std::time::Duration::from_secs(10));
        assert_eq!(config.sync_interval, std::time::Duration::from_secs(45));
        assert_eq!(config.max_parallel, 4);
        assert_eq!(config.stuck_timeout, 1800);
        assert_eq!(config.no_session_stuck_timeout, 600);
        assert_eq!(
            config.graceful_shutdown_timeout,
            std::time::Duration::from_secs(600)
        );
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

    #[tokio::test]
    async fn integration_channel_to_tmux_to_capture() {
        use crate::channels::capture::CaptureService;
        use crate::channels::transport::Transport;
        use std::sync::Arc;

        // Skip test if tmux is not available in the environment (CI may not have tmux)
        if tokio::process::Command::new("tmux")
            .arg("-V")
            .output()
            .await
            .is_err()
        {
            tracing::warn!("tmux not found, skipping integration_channel_to_tmux_to_capture test");
            return;
        }

        // Create transport and capture service
        let transport = Arc::new(Transport::new());
        let capture = Arc::new(CaptureService::new(transport.clone()));

        let task_id = "testtask1".to_string();
        let session_name = format!("orch-test-{}", task_id);

        // Start a detached tmux session
        let _ = tokio::process::Command::new("tmux")
            .args(["new-session", "-d", "-s", &session_name])
            .output()
            .await
            .expect("failed to start tmux session");

        // Register session with capture service and transport binding
        capture.register_session(&task_id, &session_name).await;
        transport
            .bind(&task_id, &session_name, "telegram", "12345")
            .await;

        // Spawn capture.run() which exits when session is unregistered
        let capture_clone = capture.clone();
        let capture_handle = tokio::spawn(async move { capture_clone.run().await });

        // Subscribe to transport output for this task
        let mut rx = transport
            .subscribe(&task_id)
            .await
            .expect("no subscription");

        // Send a message into the tmux session via tmux send-keys (simulate channel input)
        let send = tokio::process::Command::new("tmux")
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

        // Clean up: unregister and kill tmux session
        capture.unregister_session(&task_id).await;
        let _ = tokio::process::Command::new("tmux")
            .args(["kill-session", "-t", &session_name])
            .output()
            .await;

        // Wait for capture.run to finish
        let _ = capture_handle.await;

        assert!(got, "did not observe tmux output via capture/transport");
    }
}
