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
use crate::channels::routing::{ChannelRouter, GlobalChannelConfig, ProjectChannelConfig};
use crate::channels::slack::SlackChannel;
use crate::channels::telegram::TelegramChannel;
use crate::channels::tmux::TmuxChannel;
use crate::channels::transport::Transport;
use crate::channels::{Channel, ChannelRegistry, IncomingMessage, OutgoingMessage};
use crate::config;
use crate::engine::router::Router;
use crate::engine::tasks::TaskManager;
use crate::github::http::{rate_limit_metrics, GhHttp};
use crate::repo_context::REPO_CONTEXT;
use crate::store::TaskStore;
use crate::tmux::TmuxManager;
use runner::WeightSignal;
// AtomicBool/Ordering removed — shutdown is now immediate (reset tasks + break)
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

/// A pending project-pick waiting for the user to tap a button.
struct PendingPick {
    /// Original message body (the free-text request to create a task).
    original_body: String,
    /// The topic/thread that should receive follow-up messages.
    msg_topic_id: Option<String>,
    /// When this pick was created (used to enforce 60s timeout).
    created_at: std::time::Instant,
}

/// Keyed by `"{channel}:{thread_id}"`.
type PendingPicks =
    std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, PendingPick>>>;

/// Per-project engine state.
///
/// Each project has its own backend, task runner, and task manager,
/// but they share the global tmux manager, transport, and semaphore.
pub struct ProjectEngine {
    pub repo: String,
    pub project_dir: std::path::PathBuf,
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
    let mut auth_failures = 0usize;
    let mut network_failures = 0usize;

    for (repo, project_dir) in projects {
        tracing::info!(repo = %repo, "initializing project engine");

        // Initialize backend
        let backend: Arc<dyn ExternalBackend> =
            Arc::new(crate::backends::github::GitHubBackend::new(repo.clone())?);

        // Health check — verifies network and GitHub authentication
        if let Err(e) = backend.health_check().await {
            let err_str = e.to_string();
            if err_str.contains("401")
                || err_str.contains("Bad credentials")
                || err_str.contains("authentication")
                || err_str.contains("No GitHub token")
            {
                auth_failures += 1;
                tracing::debug!(
                    repo = %repo,
                    error = %e,
                    "GitHub auth failed for {repo} — run `gh auth login`"
                );
            } else {
                network_failures += 1;
                tracing::debug!(
                    repo = %repo,
                    error = %e,
                    "GitHub unreachable for {repo} (network unavailable?)"
                );
            }
            continue;
        }
        tracing::info!(repo = %repo, backend = backend.name(), "backend connected");

        // Initialize unified task store (sqlx)
        let store = Arc::new(TaskStore::open(&crate::store::default_db_path()?).await?);

        // Initialize task manager (with unified store)
        let task_manager = Arc::new(TaskManager::with_store(
            backend.clone(),
            store.clone(),
            repo.clone(),
        ));

        // Task runner (with store for metrics)
        let runner = Arc::new(runner::TaskRunner::new(repo.clone()).with_store(store.clone()));

        engines.push(ProjectEngine {
            repo,
            project_dir,
            backend,
            task_manager,
            runner,
            store,
        });
    }

    if engines.is_empty() {
        // Projects are configured but all health checks failed — this is a connectivity
        // or auth issue, not a configuration problem.  The serve() retry loop will catch
        // this error and retry with backoff; it must NOT propagate to main() or it would
        // write "Error: ..." to brew's stderr (orch.error.log) on every restart attempt.
        if auth_failures > 0 && network_failures == 0 {
            anyhow::bail!(
                "GitHub auth failed for all configured projects ({auth_failures} project(s)) — run `gh auth login`"
            );
        } else if network_failures > 0 && auth_failures == 0 {
            anyhow::bail!(
                "GitHub unreachable for all configured projects ({network_failures} project(s)) — check network connectivity"
            );
        } else {
            anyhow::bail!(
                "all configured project backends failed health checks ({auth_failures} auth failures, {network_failures} network failures)"
            );
        }
    }

    Ok(engines)
}

/// Read per-project channel configuration from `.orch.yml`.
fn read_project_channel_config(project_dir: &std::path::Path) -> ProjectChannelConfig {
    let config_path = project_dir.join(".orch.yml");
    if !config_path.exists() {
        return ProjectChannelConfig::default();
    }
    let content = match std::fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(_) => return ProjectChannelConfig::default(),
    };
    let val: serde_yml::Value = match serde_yml::from_str(&content) {
        Ok(v) => v,
        Err(_) => return ProjectChannelConfig::default(),
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

/// Start the orchestrator service.
///
/// This is the main entry point — called by `orch serve`.
pub async fn serve() -> anyhow::Result<()> {
    tracing::info!("orch engine starting");

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
    let mut project_engines = {
        let mut delay_secs = 5u64;
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            match init_project_engines().await {
                Ok(engines) => break engines,
                Err(e) => {
                    // Demote all retries to debug — brew routes stderr to
                    // orch.error.log, so any warn!/error! here would appear as
                    // spurious noise even when projects are configured correctly
                    // and the service will succeed on the next attempt.
                    tracing::debug!(
                        delay_secs,
                        attempt,
                        "project backends unavailable, retrying: {e}"
                    );
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

    // Re-create task managers with shared store
    for engine in &mut project_engines {
        engine.task_manager = Arc::new(TaskManager::with_store(
            engine.backend.clone(),
            engine.store.clone(),
            engine.repo.clone(),
        ));
    }

    // Build ChannelRouter from global config + per-project configs
    let global_channel_config = GlobalChannelConfig {
        telegram_general_topic_id: crate::config::get("channels.telegram.general_topic_id").ok(),
        discord_general_channel_id: crate::config::get("channels.discord.general_channel_id").ok(),
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
    let router_for_messages = channel_router.clone();
    for mut rx in channel_receivers {
        let transport = transport_for_messages.clone();
        let tmux = tmux_for_messages.clone();
        let capture = capture_for_messages.clone();
        let channels = channels_for_messages.clone();
        let engine_refs = engine_refs.clone();
        let ch_router = router_for_messages.clone();
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                tracing::debug!(channel = %msg.channel, thread = %msg.thread_id, "received message from channel");
                handle_channel_message(
                    msg,
                    &transport,
                    &tmux,
                    &capture,
                    &channels,
                    &engine_refs,
                    &ch_router,
                )
                .await;
            }
        });
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

                        if let Some(repo) = notification.repo.as_deref() {
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
                                    let msg = OutgoingMessage {
                                        thread_id: notification.task_id.clone(),
                                        body,
                                        reply_to: None,
                                        metadata: serde_json::json!({}),
                                        topic_id: Some(topic_id.to_string()),
                                    };
                                    if let Err(e) = channel.send(&msg).await {
                                        tracing::warn!(
                                            channel = ch_name,
                                            task_id = %notification.task_id,
                                            ?e,
                                            "failed to send to dedicated channel"
                                        );
                                    } else {
                                        routed = true;
                                    }
                                }
                            }

                            // 2. Subscribed channels for this repo.
                            if let Some(store) = &notif_store {
                                match store.list_subscribers_for_repo(repo).await {
                                    Ok(subscribers) => {
                                        for (ch_name, thread_id) in subscribers {
                                            let channel =
                                                channels.iter().find(|c| c.name() == ch_name);
                                            let Some(channel) = channel else {
                                                continue;
                                            };
                                            let body = notification.format_with_project(&ch_name);
                                            let msg = OutgoingMessage {
                                                thread_id: thread_id.clone(),
                                                body,
                                                reply_to: None,
                                                metadata: serde_json::json!({}),
                                                topic_id: None,
                                            };
                                            if let Err(e) = channel.send(&msg).await {
                                                tracing::warn!(
                                                    channel = %ch_name,
                                                    task_id = %notification.task_id,
                                                    ?e,
                                                    "failed to send to subscribed channel"
                                                );
                                            } else {
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
            let mut s = webhook_status.lock().unwrap_or_else(|e| e.into_inner());
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
                            let mut s = status_for_spawn.lock().unwrap_or_else(|e| e.into_inner());
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
                            let mut s = status_for_spawn.lock().unwrap_or_else(|e| e.into_inner());
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
            let mut s = webhook_status.lock().unwrap_or_else(|e| e.into_inner());
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
        use crate::store::TaskStatus as DbStatus;
        if let Ok(internal_in_review) = engine
            .task_manager
            .list_internal_by_status(DbStatus::InReview)
            .await
        {
            for task in &internal_in_review {
                let task_id = task.id.0.clone();
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
                        WeightSignal::Blocked | WeightSignal::None => {}
                    }
                }

                // Tick weight recovery for rate-limited agents
                {
                    let mut rw = router.write().await;
                    rw.tick_weight_recovery();
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
                        let project_jobs_path = engine.project_dir.join(".orch.yml");
                        REPO_CONTEXT.scope(repo, async {
                            if let Err(e) = tick::tick(
                                &engine.backend,
                                &tmux,
                                &engine.repo,
                                &engine.runner,
                                &capture_for_tick,
                                &semaphore,
                                &config,
                                &project_jobs_path,
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
                                if let Err(e) = sync::sync_tick(&engine.backend, &tmux, &engine.repo, &config, &semaphore, &router, &engine.task_manager, &engine.store, &dispatching).await {
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
                                let mut s = webhook_status.lock().unwrap_or_else(|e| e.into_inner());
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
            _ = webhook_notify.notified() => {
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
                        let project_jobs_path = engine.project_dir.join(".orch.yml");
                        REPO_CONTEXT.scope(repo, async {
                            if let Err(e) = tick::tick(
                                &engine.backend,
                                &tmux,
                                &engine.repo,
                                &engine.runner,
                                &capture_for_tick,
                                &semaphore,
                                &config,
                                &project_jobs_path,
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
                tracing::info!(signal = signal_name, "beginning graceful shutdown");

                // Reset in_progress tasks to routed so they re-dispatch after restart.
                // The tmux sessions will be killed when the process exits.
                let mut reset_count = 0u32;
                for engine in &project_engines {
                    if let Ok(tasks) = engine.task_manager.list_external_by_status(Status::InProgress).await {
                        for task in &tasks {
                            if let Err(e) = engine.task_manager.update_task_status(&task.id, Status::Routed).await {
                                tracing::warn!(task_id = task.id.0, ?e, "failed to reset task on shutdown");
                            } else {
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
                                reset_count += 1;
                            }
                        }
                    }
                }
                if reset_count > 0 {
                    tracing::info!(reset_count, "reset in_progress tasks to routed for re-dispatch");
                }
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
/// Optionally sends to a specific topic (e.g. Telegram forum topic).
async fn send_channel_reply(
    channels: &Arc<ChannelRegistry>,
    channel_name: &str,
    thread_id: &str,
    body: String,
    topic_id: Option<&str>,
) {
    for ch in channels.iter() {
        if ch.name() == channel_name {
            let msg = OutgoingMessage {
                thread_id: thread_id.to_string(),
                body,
                reply_to: None,
                metadata: serde_json::json!({}),
                topic_id: topic_id.map(String::from),
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

/// Send an inline keyboard to a specific channel.
/// Returns the message ID of the keyboard message, or an empty string on failure.
async fn send_channel_keyboard(
    channels: &Arc<ChannelRegistry>,
    channel_name: &str,
    thread_id: &str,
    topic_id: Option<&str>,
    text: &str,
    buttons: &[(String, String)],
) -> String {
    for ch in channels.iter() {
        if ch.name() == channel_name {
            match ch.send_keyboard(thread_id, topic_id, text, buttons).await {
                Ok(msg_id) => return msg_id,
                Err(e) => {
                    tracing::warn!(
                        channel = channel_name,
                        ?e,
                        "failed to send project picker keyboard"
                    );
                    return String::new();
                }
            }
        }
    }
    tracing::debug!(channel = channel_name, "channel not found for keyboard send");
    String::new()
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
/// - `Command`: global commands like `/status`, `/stats`, `/subscribe`, `/unsubscribe`, `/stream`
/// - `NewTask`: create an internal task, bind thread, start output fanout
///
/// The `channel_router` resolves which project a message targets based on
/// its topic/channel ID. This enables per-project task creation and status views.
async fn handle_channel_message(
    msg: IncomingMessage,
    transport: &Arc<Transport>,
    _tmux: &Arc<TmuxManager>,
    capture: &Arc<CaptureService>,
    channels: &Arc<ChannelRegistry>,
    engine_refs: &[EngineRef],
    channel_router: &Arc<ChannelRouter>,
) {
    use crate::backends::ExternalId;
    use crate::channels::stream::fanout_output;
    use crate::channels::transport::MessageRoute;
    use crate::engine::commands::{execute_command, parse_command};
    use crate::engine::tasks::{CreateTaskRequest, TaskType};

    // Resolve project from incoming message topic/channel
    let topic_id = msg.topic_id.as_deref().unwrap_or(&msg.thread_id);
    let resolved_repo = channel_router.resolve_project(&msg.channel, topic_id);
    let is_general = channel_router.is_general(&msg.channel, topic_id);
    let msg_topic_id = msg.topic_id.clone();

    match transport.route(&msg).await {
        MessageRoute::TaskSession { task_id } => {
            let body = msg.body.trim().to_string();
            let channel = msg.channel.clone();
            let thread_id = msg.thread_id.clone();

            if body.starts_with('/') {
                // Parse slash command and execute it on the bound task
                if let Some(cmd) = parse_command(&body) {
                    if let Some((repo, backend, task_manager, store)) = engine_refs.first() {
                        let gh = match GhHttp::new() {
                            Ok(gh) => gh,
                            Err(e) => {
                                tracing::warn!(error = %e, "failed to build HTTP client for command execution");
                                send_channel_reply(
                                    channels,
                                    &channel,
                                    &thread_id,
                                    format!("Command `{cmd}` failed: {e}"),
                                    msg_topic_id.as_deref(),
                                )
                                .await;
                                return;
                            }
                        };
                        let ext_id = ExternalId(task_id.clone());
                        let result =
                            execute_command(backend, &gh, repo, &ext_id, &cmd, store, task_manager)
                                .await;
                        let reply = match result {
                            Ok(r) => r,
                            Err(e) => format!("Command `{cmd}` failed: {e}"),
                        };
                        send_channel_reply(
                            channels,
                            &channel,
                            &thread_id,
                            reply,
                            msg_topic_id.as_deref(),
                        )
                        .await;
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

            if cmd_str == "/status" || cmd_str.starts_with("/status ") {
                // /status — project-aware: show tasks for resolved project or all
                let reply = handle_status_command(engine_refs, resolved_repo).await;
                send_channel_reply(
                    channels,
                    &channel,
                    &thread_id,
                    reply,
                    msg_topic_id.as_deref(),
                )
                .await;
            } else if cmd_str == "/stats" || cmd_str.starts_with("/stats ") {
                let reply = handle_stats_command(engine_refs, resolved_repo, is_general).await;
                send_channel_reply(
                    channels,
                    &channel,
                    &thread_id,
                    reply,
                    msg_topic_id.as_deref(),
                )
                .await;
            } else if cmd_str.starts_with("/subscribe") {
                let reply = handle_subscribe_command(
                    &cmd_str,
                    &channel,
                    &thread_id,
                    engine_refs,
                    channel_router,
                )
                .await;
                send_channel_reply(
                    channels,
                    &channel,
                    &thread_id,
                    reply,
                    msg_topic_id.as_deref(),
                )
                .await;
            } else if cmd_str.starts_with("/unsubscribe") {
                let reply = handle_unsubscribe_command(
                    &cmd_str,
                    &channel,
                    &thread_id,
                    engine_refs,
                    channel_router,
                )
                .await;
                send_channel_reply(
                    channels,
                    &channel,
                    &thread_id,
                    reply,
                    msg_topic_id.as_deref(),
                )
                .await;
            } else if cmd_str.starts_with("/stream") {
                let reply = handle_stream_command(&cmd_str, &channel, &thread_id, transport).await;
                send_channel_reply(
                    channels,
                    &channel,
                    &thread_id,
                    reply,
                    msg_topic_id.as_deref(),
                )
                .await;
            } else if let Some(cmd) = parse_command(&cmd_str) {
                let reply = format!(
                    "Command `{cmd}` requires a task context. \
                     Send it in a thread that is bound to a running task."
                );
                send_channel_reply(
                    channels,
                    &channel,
                    &thread_id,
                    reply,
                    msg_topic_id.as_deref(),
                )
                .await;
            } else {
                let reply = "Available commands: /status, /stats, /subscribe, /unsubscribe, \
                             /stream, /retry, /close, /block, /unblock, /review — \
                             send task-specific commands in a task thread."
                    .to_string();
                send_channel_reply(
                    channels,
                    &channel,
                    &thread_id,
                    reply,
                    msg_topic_id.as_deref(),
                )
                .await;
            }
        }

        MessageRoute::NewTask => {
            let channel = msg.channel.clone();
            let thread_id = msg.thread_id.clone();

            // Resolve target project: resolved_repo → specific project, else first
            let target_engine_ref = if let Some(repo) = resolved_repo {
                engine_refs.iter().find(|(r, _, _, _)| r == repo)
            } else {
                engine_refs.first()
            };

            if let Some((repo, _, task_manager, _)) = target_engine_ref {
                let title = if msg.body.chars().count() > 80 {
                    let truncated: String = msg.body.chars().take(80).collect();
                    format!("{}…", truncated)
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
                        let project_label = if resolved_repo.is_some() {
                            String::new()
                        } else {
                            format!(" in [{repo}]")
                        };
                        let reply = format!(
                            "Task created: `{task_id}`{project_label} — I'll start working on it now."
                        );
                        send_channel_reply(
                            channels,
                            &channel,
                            &thread_id,
                            reply,
                            msg_topic_id.as_deref(),
                        )
                        .await;
                    }
                    Err(e) => {
                        tracing::warn!(repo, err = %e, "failed to create task from channel message");
                        let reply = format!("Failed to create task: {e}");
                        send_channel_reply(
                            channels,
                            &channel,
                            &thread_id,
                            reply,
                            msg_topic_id.as_deref(),
                        )
                        .await;
                    }
                }
            } else {
                tracing::warn!("no project configured, cannot create task from channel message");
            }
        }
    }
}

/// Handle `/status` command with project-aware filtering.
async fn handle_status_command(engine_refs: &[EngineRef], resolved_repo: Option<&str>) -> String {
    let mut lines = vec!["**Active tasks:**".to_string()];
    for (repo, _, task_manager, _) in engine_refs {
        // If a specific project was resolved, only show that project
        if let Some(target) = resolved_repo {
            if repo != target {
                continue;
            }
        }
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
    lines.join("\n")
}

/// Handle `/stats` command — show 24h metrics, optionally per-project.
async fn handle_stats_command(
    engine_refs: &[EngineRef],
    resolved_repo: Option<&str>,
    is_general: bool,
) -> String {
    let mut lines = vec!["**Stats (24h)**".to_string(), String::new()];

    // Determine which repos to query
    let repos_to_query: Vec<&str> = if let Some(repo) = resolved_repo {
        vec![repo]
    } else {
        engine_refs.iter().map(|(r, _, _, _)| r.as_str()).collect()
    };

    for repo in &repos_to_query {
        // Find the store for this repo
        let store = engine_refs
            .iter()
            .find(|(r, _, _, _)| r == repo)
            .and_then(|(_, _, _, s)| s.as_ref());

        let store = match store {
            Some(s) => s,
            None => continue,
        };

        match store.get_metrics_summary_24h_by_repo(repo).await {
            Ok(summary) => {
                let total = summary.tasks_completed_24h + summary.tasks_failed_24h;
                let rate = if total > 0 {
                    (summary.tasks_completed_24h as f64 / total as f64) * 100.0
                } else {
                    0.0
                };
                // Show repo name only if showing multiple (general or no resolution)
                if is_general || resolved_repo.is_none() {
                    lines.push(format!(
                        "**{}**: {} done, {} failed ({:.1}%)",
                        repo, summary.tasks_completed_24h, summary.tasks_failed_24h, rate
                    ));
                } else {
                    lines.push(format!(
                        "{} done, {} failed ({:.1}%)",
                        summary.tasks_completed_24h, summary.tasks_failed_24h, rate
                    ));
                }
                // Per-agent breakdown
                for agent in &summary.agent_stats {
                    lines.push(format!(
                        "  {}: {} runs ({:.0}%)",
                        agent.agent, agent.total_runs, agent.success_rate
                    ));
                }
                if !summary.agent_stats.is_empty() {
                    lines.push(String::new());
                }
            }
            Err(e) => {
                tracing::warn!(repo, err = %e, "failed to query metrics");
                lines.push(format!("**{}**: error fetching metrics", repo));
            }
        }
    }

    if lines.len() <= 2 {
        lines.push("No metrics data available.".to_string());
    }
    lines.join("\n")
}

/// Handle `/subscribe <project>` command.
async fn handle_subscribe_command(
    cmd_str: &str,
    channel: &str,
    thread_id: &str,
    engine_refs: &[EngineRef],
    channel_router: &Arc<ChannelRouter>,
) -> String {
    let parts: Vec<&str> = cmd_str.splitn(2, ' ').collect();
    if parts.len() < 2 || parts[1].trim().is_empty() {
        let projects: Vec<&str> = channel_router
            .projects()
            .iter()
            .map(|s| s.as_str())
            .collect();
        return format!(
            "Usage: `/subscribe <project>`\nAvailable projects: {}",
            projects.join(", ")
        );
    }
    let project = parts[1].trim();

    // Validate project exists
    if !channel_router.projects().iter().any(|p| p == project) {
        return format!(
            "Unknown project `{project}`. Available: {}",
            channel_router
                .projects()
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // Find a store to use for subscription
    if let Some((_, _, _, Some(store))) = engine_refs.first() {
        match store.subscribe_channel(channel, thread_id, project).await {
            Ok(()) => format!("Subscribed to notifications from `{project}`."),
            Err(e) => format!("Failed to subscribe: {e}"),
        }
    } else {
        "No store available for subscription management.".to_string()
    }
}

/// Handle `/unsubscribe <project>` command.
async fn handle_unsubscribe_command(
    cmd_str: &str,
    channel: &str,
    thread_id: &str,
    engine_refs: &[EngineRef],
    channel_router: &Arc<ChannelRouter>,
) -> String {
    let parts: Vec<&str> = cmd_str.splitn(2, ' ').collect();
    if parts.len() < 2 || parts[1].trim().is_empty() {
        return "Usage: `/unsubscribe <project>`".to_string();
    }
    let project = parts[1].trim();

    // Validate project exists
    if !channel_router.projects().iter().any(|p| p == project) {
        return format!("Unknown project `{project}`.");
    }

    if let Some((_, _, _, Some(store))) = engine_refs.first() {
        match store.unsubscribe_channel(channel, thread_id, project).await {
            Ok(()) => format!("Unsubscribed from `{project}` notifications."),
            Err(e) => format!("Failed to unsubscribe: {e}"),
        }
    } else {
        "No store available for subscription management.".to_string()
    }
}

/// Handle `/stream <task_id>` command — bind channel to task output stream.
async fn handle_stream_command(
    cmd_str: &str,
    channel: &str,
    thread_id: &str,
    transport: &Arc<Transport>,
) -> String {
    let parts: Vec<&str> = cmd_str.splitn(2, ' ').collect();
    if parts.len() < 2 || parts[1].trim().is_empty() {
        return "Usage: `/stream <task_id>`".to_string();
    }
    let task_id = parts[1].trim();

    // Check if the task has an active binding (i.e. is running)
    if let Some(binding) = transport.get_binding(task_id).await {
        // Bind this channel/thread as an additional output target
        // The session name is retrieved from the existing binding
        transport
            .bind(task_id, &binding.tmux_session, channel, thread_id)
            .await;
        format!("Streaming output from task `{task_id}` to this channel.")
    } else {
        format!("Task `{task_id}` is not currently running or has no active session.")
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
        use std::sync::Arc;

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
