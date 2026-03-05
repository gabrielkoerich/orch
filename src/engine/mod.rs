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

use crate::backends::ExternalBackend;
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
use crate::github::token;
use crate::sidecar::REPO_CONTEXT;
use crate::tmux::TmuxManager;
use runner::WeightSignal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Notify, RwLock, Semaphore};

use crate::backends::Status;

/// Per-project engine state.
///
/// Each project has its own backend, task runner, and task manager,
/// but they share the global tmux manager, transport, and semaphore.
pub struct ProjectEngine {
    pub repo: String,
    pub backend: Arc<dyn ExternalBackend>,
    pub task_manager: Arc<TaskManager>,
    pub runner: Arc<runner::TaskRunner>,
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

        // Initialize task manager
        let task_manager = Arc::new(TaskManager::new(db.clone(), backend.clone()));

        // Task runner (with db for metrics)
        let runner = Arc::new(runner::TaskRunner::new(repo.clone()).with_db(db.clone()));

        engines.push(ProjectEngine {
            repo,
            backend,
            task_manager,
            runner,
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

async fn auth_available_for_webhook() -> Result<bool, String> {
    tokio::task::spawn_blocking(|| token::shared().is_auth_available())
        .await
        .map_err(|e| format!("auth probe task failed: {e}"))?
}

async fn enforce_webhook_auth_gate(webhook_enabled: bool) -> bool {
    if !webhook_enabled {
        return false;
    }

    match auth_available_for_webhook().await {
        Ok(true) => true,
        Ok(false) => {
            let metrics = token::gh_fallback_metrics();
            tracing::warn!(
                gh_fallback_attempts = metrics.attempts,
                gh_fallback_successes = metrics.successes,
                gh_fallback_failures = metrics.failures,
                "GitHub auth unavailable; disabling webhook and using polling fallback mode. \
                 Configure GH_TOKEN/GITHUB_TOKEN, gh.auth.token, or run `gh auth login`."
            );
            false
        }
        Err(e) => {
            let metrics = token::gh_fallback_metrics();
            tracing::warn!(
                error = %e,
                gh_fallback_attempts = metrics.attempts,
                gh_fallback_successes = metrics.successes,
                gh_fallback_failures = metrics.failures,
                "GitHub auth probe failed; disabling webhook and using polling fallback mode. \
                 Configure GH_TOKEN/GITHUB_TOKEN, gh.auth.token, or run `gh auth login`."
            );
            false
        }
    }
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
    let mut webhook_enabled = crate::config::get("webhook.enabled")
        .map(|v| v == "true")
        .unwrap_or(false);

    webhook_enabled = enforce_webhook_auth_gate(webhook_enabled).await;

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
        tracing::info!(port, "webhook server started");
    } else {
        webhook_port = None;
        webhook_healthy = false;
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
                        WeightSignal::None => {}
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
                    let router_guard = router.read().await;
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
                                &router_guard,
                                &router,
                                &engine.task_manager,
                                &weight_tx,
                                &transport,
                                &dispatching,
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
                                if let Err(e) = sync::sync_tick(&engine.backend, &tmux, &engine.repo, &db, &config, &router).await {
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
                            let health = crate::channels::github::check_webhook_health(port).await;
                            if health != webhook_healthy {
                                webhook_healthy = health;
                                if webhook_healthy {
                                    tracing::info!(port, "webhook health restored");
                                } else {
                                    tracing::warn!(port, "webhook health check failed");
                                }
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

                    let router_guard = router.read().await;
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
                                &router_guard,
                                &router,
                                &engine.task_manager,
                                &weight_tx,
                                &transport,
                                &dispatching,
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

#[cfg(test)]
mod tests {
    use super::*;
    use once_cell::sync::Lazy;
    use std::env;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    static ENV_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

    struct BufferWriter(Arc<Mutex<Vec<u8>>>);

    struct BufferGuard(Arc<Mutex<Vec<u8>>>);

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufferWriter {
        type Writer = BufferGuard;

        fn make_writer(&'a self) -> Self::Writer {
            BufferGuard(self.0.clone())
        }
    }

    impl Write for BufferGuard {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut locked = self.0.lock().unwrap();
            locked.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
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

    #[tokio::test(flavor = "current_thread")]
    async fn webhook_auth_gate_disables_webhook_when_gh_missing() {
        {
            let _guard = ENV_LOCK.lock().unwrap();
            env::remove_var("GH_TOKEN");
            env::remove_var("GITHUB_TOKEN");
            env::set_var("ORCH_GH_CLI_CANDIDATES", "/nonexistent/orch-gh");
        }

        let resolver = token::shared();
        resolver.clear_cache().await;

        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(BufferWriter(buffer.clone()))
            .with_ansi(false)
            .finish();
        let _default = tracing::subscriber::set_default(subscriber);

        let enabled = enforce_webhook_auth_gate(true).await;
        assert!(!enabled);

        let metrics = token::gh_fallback_metrics();
        assert!(metrics.attempts >= 1);
        assert!(metrics.failures >= 1);

        let output = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
        assert!(output.contains("polling fallback mode"));

        {
            let _guard = ENV_LOCK.lock().unwrap();
            env::remove_var("ORCH_GH_CLI_CANDIDATES");
        }
    }
}
