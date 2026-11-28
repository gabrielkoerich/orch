//! The engine service loop and config reload

/// Standard Orch attribution footer for issue bodies, PR bodies, and comments.
///
/// Append to every user-visible string posted to GitHub so activity is
/// clearly attributed to orch rather than appearing as a human post.
use crate::backends::ExternalId;
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
use crate::engine::router::{Router, RouterConfig};
use crate::engine::runner::WeightSignal;
use crate::engine::tasks::TaskManager;
use crate::engine::tasks::events::TaskEvent;
use crate::github::http::{GhHttp, rate_limit_metrics};
use crate::store::{TaskStatus, TaskStore};
use crate::tmux::TmuxManager;
use async_trait::async_trait;
use tokio::sync::broadcast;
// AtomicBool/Ordering removed — shutdown is now immediate (reset tasks + break)
use super::channel_handler;
use super::startup::{
    check_and_notify_upgrade, fetch_latest_release_version, init_project_engines,
    open_shared_store, read_project_channel_config, reconcile_startup_estimates,
    reconcile_startup_worktrees, send_startup_escalation_notification, should_run_upgrade_check,
};
use crate::backends::Status;
use crate::channels::github;
use crate::channels::github::is_transient_bind_error;
use crate::channels::github::webhook_backoff_delay;
use crate::channels::notification;
use crate::channels::notification::TaskNotification;
use crate::channels::transport;
use crate::cooldown as engine_cooldown;
use crate::engine::tasks::events;
use crate::engine::{
    EngineConfig, EngineRef, ProjectEngine, RATE_LIMITED_UNTIL_KEY, format_rate_limited_until,
    reset_orphaned_reviews, spawn_sync_pass, start_relay_client, subscribers, sync_project_engines,
    tick,
};
use crate::github::app;
use crate::home;
use crate::secret_ref;
use crate::store::cluster::local;
use crate::suspend as engine_suspend;
use crate::suspend::SuspendDetector;
use crate::webhook_status::WebhookStatus;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time as std_time;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use tokio::signal;
use tokio::signal::unix;
use tokio::signal::unix::SignalKind;
use tokio::sync::Mutex;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc::Receiver;
use tokio::sync::{Notify, RwLock, Semaphore, mpsc};
use tokio::task;
use tokio::time;

/// Announce a GitHub rate-limit window on every operator surface: the log (`orch log`),
/// a KV flag (`orch task status`) and the channels (Telegram, Discord).
async fn report_rate_limited(
    reset_at: i64,
    store: Option<&Arc<TaskStore>>,
    transport: Option<&Arc<Transport>>,
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

    if let Some(store) = store
        && let Err(e) = store
            .kv_set(RATE_LIMITED_UNTIL_KEY, &reset_at.to_string())
            .await
    {
        tracing::warn!(err = %e, "failed to persist GitHub rate-limit flag");
    }

    if let Some(transport) = transport {
        transport.push_notification(TaskNotification {
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
pub(crate) fn resize_dispatch_pool(semaphore: &Semaphore, in_force: usize, target: usize) -> usize {
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
pub(crate) const RESTART_ONLY_KEYS: [&str; 3] = ["channels", "webhook", "relay"];

/// Read the restart-only subtrees so a later reload can tell they changed.
fn restart_only_snapshot() -> Vec<Option<String>> {
    RESTART_ONLY_KEYS
        .iter()
        .map(|key| config::get(key).ok())
        .collect()
}

/// Report the restart-only keys an edit changed. Only the key is logged, never
/// the value, because `channels.*` holds bot tokens
pub(crate) fn log_restart_only_changes(
    old: &[Option<String>],
    new: &[Option<String>],
) -> Vec<&'static str> {
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

/// Start the orch service.
///
/// This is the main entry point — called by `orch serve`.
/// The web UI the daemon starts once its store and event bus exist, and stops at shutdown
#[async_trait]
pub trait WebUi: Send + Sync {
    async fn start(
        &self,
        store: Arc<TaskStore>,
        events: broadcast::Sender<TaskEvent>,
    ) -> anyhow::Result<()>;

    async fn stop(&self);
}

pub async fn serve(web: &dyn WebUi) -> anyhow::Result<()> {
    let killed = home::kill_other_orch_serve_processes(Duration::from_secs(30));
    if killed > 0 {
        tracing::info!(count = killed, "terminated stale orch serve processes");
    }

    tracing::info!("orch engine starting");

    // Pre-create standard directories with async I/O so subsequent synchronous
    // callers (state_dir, orch_home, etc.) find them already present and their
    // create_dir_all calls become fast no-ops.
    if let Err(e) = home::init_dirs().await {
        tracing::warn!("failed to initialize orch directories: {e}");
    }

    // Pre-warm the config cache with async I/O so get() / get_list() calls in
    // hot async paths never block a Tokio thread on std::fs::read_to_string.
    if let Err(e) = config::warm_cache().await {
        tracing::warn!("failed to warm config cache: {e}");
    }

    let mut config = EngineConfig::from_config();

    // One store for every project, opened before them so the web UI can run with none. A failure
    // here retries like a project failure did, so launchd does not restart the service in a loop
    let store = {
        let mut delay_secs = 5u64;
        let mut escalated = false;
        let start = Instant::now();
        loop {
            match open_shared_store().await {
                Ok(store) => break store,
                Err(e) => {
                    let elapsed = start.elapsed().as_secs();
                    let escalation = config.startup_failure_escalation_secs;
                    // Escalates once like a project that will not start, then keeps retrying
                    if escalation > 0 && !escalated && elapsed >= escalation {
                        escalated = true;
                        tracing::error!(
                            elapsed_secs = elapsed,
                            "the store or the cluster has not started, escalating: {e:#}"
                        );
                        send_startup_escalation_notification(elapsed).await;
                    } else {
                        tracing::warn!(
                            delay_secs,
                            "could not open the store or start the cluster, retrying: {e:#}"
                        );
                    }

                    time::sleep(Duration::from_secs(delay_secs)).await;
                    delay_secs = (delay_secs * 2).min(120);
                }
            }
        }
    };

    // Load persisted model cooldowns and register store for future writes.
    // Runs once here so the global cooldown store is not overwritten per project.
    engine_cooldown::init_cooldown_store(Arc::clone(&store)).await;
    tracing::info!("internal database ready");

    // Create the event bus for task status transitions
    let event_bus = events::EventBus::new(256);
    if let Err(e) = event_bus.start_ws_server().await {
        tracing::warn!(
            ?e,
            "failed to start event websocket server, continuing without it"
        );
    }

    // Before the projects, so a first run with none can add one from the UI
    if let Err(e) = web.start(Arc::clone(&store), event_bus.sender()).await {
        tracing::warn!(?e, "failed to start the web UI, continuing without it");
    }

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
        let start = Instant::now();
        loop {
            attempt += 1;
            match init_project_engines(&store).await {
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
                        #[expect(clippy::cast_precision_loss, reason = "a display duration")]
                        let outage = notification::format_duration(elapsed as f64);
                        tracing::error!(
                            delay_secs,
                            attempt,
                            elapsed_secs = elapsed,
                            threshold_secs = startup_escalation_secs,
                            "project backends unavailable for {outage} — escalating startup failure: {e}",
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

                    time::sleep(Duration::from_secs(delay_secs)).await;
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
    let mut active_repos: HashSet<String> =
        project_engines.iter().map(|e| e.repo.clone()).collect();

    // Check for a newer orch release in the background and log if the service is behind.
    // The periodic checker sends channel notifications after channels are initialized;
    // upgrading is operator-only and never runs automatically.
    {
        let current = env!("ORCH_VERSION").to_string();
        tokio::spawn(async move {
            if let Some(latest) = fetch_latest_release_version().await
                && latest != current
            {
                tracing::warn!(
                    current_version = %current,
                    latest_version = %latest,
                    "orch service is behind the latest release — run: brew update && brew upgrade orch && brew services restart orch"
                );
            }
        });
    }

    // Re-create task managers with shared store and event bus
    for engine in &mut project_engines {
        engine.task_manager = Arc::new(TaskManager::with_events(
            Arc::clone(&engine.backend),
            Arc::clone(&engine.store),
            engine.repo.clone(),
            event_bus.sender(),
        ));
    }

    // Startup reconciliation decides task status, so only the leader runs it
    let leads = project_engines.first().is_none_or(|e| e.store.is_leader());
    if leads && let Err(e) = reconcile_startup_worktrees(&project_engines).await {
        tracing::warn!(err = %e, "startup worktree reconciliation failed");
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
        telegram_general_topic_id: config::get("channels.telegram.general_topic_id").ok(),
        discord_general_channel_id: config::get("channels.discord.general_channel_id").ok(),
        control_telegram_topic_id: config::get("control.channels.telegram.topic_id").ok(),
        control_discord_channel_id: config::get("control.channels.discord.channel_id").ok(),
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
    let capture = Arc::new(CaptureService::new(Arc::clone(&transport)));
    let capture_for_tick = Arc::clone(&capture);
    tokio::spawn(async move {
        capture.start().await;
    });

    // Initialize channel registry
    let mut channel_registry = ChannelRegistry::new();

    // Try to initialize Telegram channel
    if let Ok(token) = secret_ref::config_secret("channels.telegram.bot_token").await
        && !token.is_empty()
    {
        let chat_id = config::get("channels.telegram.chat_id").ok();
        let allowed_users =
            config::get_list("channels.telegram.allowed_user_ids").unwrap_or_default();
        match TelegramChannel::new(token, chat_id).map(|t| t.with_allowed_users(allowed_users)) {
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

    // Try to initialize Discord Gateway channel (websocket)
    if let Ok(token) = secret_ref::config_secret("channels.discord.bot_token").await
        && !token.is_empty()
    {
        let channel_id = config::get("channels.discord.channel_id").ok();
        let shard_id = config::get("channels.discord.shard_id")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        let shard_count = config::get("channels.discord.shard_count")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(1);
        let allowed_users =
            config::get_list("channels.discord.allowed_user_ids").unwrap_or_default();
        match DiscordGateway::new(token, channel_id, shard_id, shard_count)
            .map(|d| d.with_access(channel_router.discord_channel_ids(), allowed_users))
        {
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

    // Try to initialize Slack channel
    if let Ok(token) = secret_ref::config_secret("channels.slack.bot_token").await
        && !token.is_empty()
    {
        let channel_id = config::get("channels.slack.channel_id").ok();
        let allowed_users = config::get_list("channels.slack.allowed_user_ids").unwrap_or_default();
        match SlackChannel::new(token, channel_id)
            .map(|s| s.with_access(channel_router.slack_channel_ids(), allowed_users))
        {
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

    // Initialize tmux channel with transport for output streaming
    let tmux_channel = TmuxChannel::with_transport(Arc::clone(&transport));
    channel_registry.register(Box::new(tmux_channel));
    tracing::info!("tmux channel registered");

    // Start all channels and collect their message receivers
    let mut channel_receivers: Vec<Receiver<IncomingMessage>> = Vec::new();
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
    let transport_for_messages = Arc::clone(&transport);
    let tmux_for_messages = Arc::clone(&tmux);
    let capture_for_messages = Arc::clone(&capture_for_tick);
    let channels_for_messages = Arc::clone(&channel_registry);
    // Lightweight engine references for command execution and task creation
    let engine_refs: Vec<EngineRef> = project_engines
        .iter()
        .map(|e| {
            (
                e.repo.clone(),
                Arc::clone(&e.backend),
                Arc::clone(&e.task_manager),
                Some(Arc::clone(&e.store)),
            )
        })
        .collect();
    // Shared in-memory map for pending project picks (General channel multi-project picker).
    let pending_picks: channel_handler::PendingPicks = Arc::new(Mutex::new(HashMap::new()));
    let router_for_messages = Arc::clone(&channel_router);
    for mut rx in channel_receivers {
        let transport = Arc::clone(&transport_for_messages);
        let tmux = Arc::clone(&tmux_for_messages);
        let capture = Arc::clone(&capture_for_messages);
        let channels = Arc::clone(&channels_for_messages);
        let engine_refs = engine_refs.clone();
        let ch_router = Arc::clone(&router_for_messages);
        let picks = Arc::clone(&pending_picks);
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
        let restart_channels = Arc::clone(&channel_registry);
        let restart_store = Some(Arc::clone(&store));
        if let Some(store) = restart_store {
            tokio::spawn(async move {
                time::sleep(Duration::from_secs(5)).await;
                match store.kv_get("control:restart_pending").await {
                    Ok(Some(conv_key)) => {
                        if let Some((ch_name, thread_id, topic_id)) =
                            transport::parse_conversation_key(&conv_key)
                        {
                            for ch in restart_channels.iter() {
                                if ch.name() == ch_name {
                                    let msg = OutgoingMessage {
                                        thread_id: thread_id.to_string(),
                                        body: "orch restarted successfully.".to_string(),
                                        reply_to: None,
                                        metadata: serde_json::json!({}),
                                        topic_id: topic_id.map(ToString::to_string),
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
        let channels = Arc::clone(&channel_registry);
        let notif_router = Arc::clone(&channel_router);
        // Grab a store reference for subscription lookups.
        let notif_store: Option<Arc<TaskStore>> = Some(Arc::clone(&store));
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
                                match channel.send(&msg).await {
                                    Err(e) => {
                                        tracing::warn!(
                                            task_id = %notification.task_id,
                                            target,
                                            ?e,
                                            "failed to send to notify_target"
                                        );
                                    }
                                    _ => {
                                        routed = true;
                                    }
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
                            let mut sent: HashSet<(String, Option<String>)> = HashSet::new();

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
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(missed = n, "notification receiver lagged");
                    }
                    Err(RecvError::Closed) => {
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

    // `WebhookStatus::fallback_mode` is only shown by `orch webhook status`, the sync
    // tick always runs every `engine.sync_interval` whether or not webhooks work
    let webhook_port: Option<u16>;
    let mut webhook_healthy: bool;
    let mut last_webhook_health_check = Instant::now();
    // Start webhook server if configured
    let webhook_enabled = config::get_global("webhook.enabled").is_ok_and(|v| v == "true");
    // No readable secret keeps the webhook server off, it never accepts unsigned requests
    let webhook_secret = if webhook_enabled {
        secret_ref::config_secret("webhook.secret")
            .await
            .ok()
            .filter(|s| !s.trim().is_empty())
    } else {
        None
    };
    if webhook_enabled && webhook_secret.is_none() {
        tracing::error!(
            "webhook.enabled needs a readable webhook.secret, the webhook server stays off"
        );
    }
    let webhook_enabled = webhook_enabled && webhook_secret.is_some();
    let webhook_secret = webhook_secret.unwrap_or_default();

    // Shared status updated by the server-spawn task and the health-check loop.
    let webhook_status = Arc::new(Mutex::new(WebhookStatus::default()));

    if webhook_enabled {
        // Spawn the HTTP server with exponential-backoff retry on transient
        // bind errors (e.g. EADDRINUSE).  After MAX_ATTEMPTS the engine falls
        // back to polling and updates the shared status accordingly.
        const WEBHOOK_MAX_ATTEMPTS: u32 = 5;
        let port: u16 = config::get("webhook.port")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8080);
        webhook_port = Some(port);
        let secret = webhook_secret.clone();
        let webhook_repo = project_engines
            .first()
            .map(|e| e.repo.clone())
            .unwrap_or_default();

        let (tx, mut rx) = mpsc::channel::<IncomingMessage>(64);

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

        let status_for_spawn = Arc::clone(&webhook_status);
        tokio::spawn(async move {
            let mut attempt = 0u32;
            loop {
                attempt += 1;
                tracing::info!(attempt, port, "attempting to start webhook server");
                match start_webhook_server(port, secret.clone(), webhook_repo.clone(), tx.clone())
                    .await
                {
                    Ok(()) => {
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
                        time::sleep(delay).await;
                    }
                }
            }
        });

        // Spawn the message forwarding task (reads from webhook channel)
        let notify = Arc::clone(&webhook_notify);
        let transport_for_webhook = Arc::clone(&transport);
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

    start_relay_client(&store, &webhook_notify);
    app::start_loader();

    // Agent router (selects agent + model per task) - shared across projects.
    // Router::from_config() may run `opencode models` when the config uses opencode.
    // spawn_blocking keeps the Tokio runtime thread free during that I/O
    let router = Arc::new(RwLock::new(
        match task::spawn_blocking(Router::from_config).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(?e, "router init panicked in spawn_blocking, using default");
                task::spawn_blocking(|| Router::new(RouterConfig::default()))
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
    // Reviews get their own pool, one reviewer per worker, so a busy worker pool never
    // starves a review (#3971)
    let review_semaphore = Arc::new(Semaphore::new(config.max_parallel));
    let mut review_in_force = config.max_parallel;

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
    let mut config_rx = config::subscribe();

    // Track sync interval
    let mut last_sync = Instant::now();

    // Track upgrade check interval
    let mut last_upgrade_check: Option<Instant> = None;

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
            Arc::clone(&engine.backend),
            Arc::clone(&tmux),
            Arc::clone(&engine.runner),
            Arc::clone(&capture_for_tick),
            Arc::clone(&semaphore),
            Arc::clone(&engine.task_manager),
            weight_tx.clone(),
            Arc::clone(&router),
            Arc::clone(&dispatching),
            Arc::clone(&engine.store),
            engine.repo.clone(),
        );
    }
    tracing::info!("dispatch subscriber started");

    // Spawn review subscribers — react to NeedsReview events immediately
    for engine in &project_engines {
        subscribers::review::spawn(
            event_bus.subscribe(),
            Arc::clone(&engine.backend),
            Arc::clone(&tmux),
            Arc::clone(&review_semaphore),
            Arc::clone(&engine.task_manager),
            Arc::clone(&router),
            Arc::clone(&dispatching),
            Arc::clone(&engine.store),
            engine.repo.clone(),
        );
    }
    tracing::info!("review subscriber started");

    // Spawn unblock subscribers — react to Done events, unblock parent tasks immediately
    for engine in &project_engines {
        subscribers::unblock::spawn(
            event_bus.subscribe(),
            Arc::clone(&engine.backend),
            Arc::clone(&engine.task_manager),
            Arc::clone(&engine.store),
            engine.repo.clone(),
        );
    }
    tracing::info!("unblock subscriber started");

    // Spawn notify subscriber — reacts to ALL events, pushes to transport.
    // Spawned once (not per-project) since the transport handles all repos.
    subscribers::notify::spawn(event_bus.subscribe(), Arc::clone(&transport));
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

    let mut interval = time::interval(config.tick_interval);

    // Signal handlers (launchd/systemd send SIGTERM to stop services, SIGHUP for reload/restart)
    let mut sigterm = unix::signal(SignalKind::terminate())?;
    let mut sighup = unix::signal(SignalKind::hangup())?;

    // Guard to prevent concurrent sync_tick runs (sync is spawned as a background task).
    let sync_in_progress = Arc::new(AtomicBool::new(false));

    // Tick watchdog: detects when the main loop hasn't completed a tick in > 60s.
    // Updates an atomic timestamp at the end of each tick iteration.
    let last_tick_epoch = Arc::new(AtomicU64::new(
        SystemTime::now()
            .duration_since(std_time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    ));
    {
        let last_tick_epoch = Arc::clone(&last_tick_epoch);
        let tick_interval_secs = config.tick_interval_secs();
        tokio::spawn(async move {
            // Check every 30s whether the main loop is still ticking.
            let mut watchdog_interval = time::interval(Duration::from_secs(30));
            // Own suspend baseline, independent of the main loop's checkpoint(). The
            // watchdog's interval timer also freezes during a host suspend, so this
            // detects the gap from its own wake-up instead of racing the main loop's
            // checkpoint() for the same GAP_LOG entry (see #3544).
            let mut suspend_detector = SuspendDetector::new();
            loop {
                watchdog_interval.tick().await;
                let own_gap = suspend_detector.check();
                let last = last_tick_epoch.load(Ordering::Relaxed);
                let now = SystemTime::now()
                    .duration_since(std_time::UNIX_EPOCH)
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
                    let shared_gap =
                        engine_suspend::gap_detected_within(Duration::from_secs(stale_secs));
                    let gap = match (own_gap, shared_gap) {
                        (Some(a), Some(b)) => Some(a.max(b)),
                        (Some(a), None) => Some(a),
                        (None, Some(b)) => Some(b),
                        (None, None) => None,
                    };
                    if let Some(gap) = gap {
                        let gap_secs = u64::try_from(gap.num_seconds()).unwrap_or(0);
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
                let tick_start = Instant::now();

                // Detect a host suspend/resume gap once per iteration, in one place.
                // The watchdog and stuck-task recovery both read from this instead of
                // re-deriving suspend information from their own wall-clock deltas.
                if let Some(gap) = engine_suspend::checkpoint() {
                    tracing::info!(
                        gap_secs = gap.num_seconds(),
                        "host suspend/resume gap detected, discounting from stuck-task age checks"
                    );
                }

                // Finish a pending shrink of the dispatch pool as running tasks
                // release their permits. Only logs when a permit was actually
                // taken back, so a long-running task does not log every tick.
                if review_in_force != config.max_parallel {
                    review_in_force =
                        resize_dispatch_pool(&review_semaphore, review_in_force, config.max_parallel);
                }
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
                        report_rate_limited(reset_at, Some(&store), Some(&transport)).await;
                    }
                } else if let Some(prev) = rate_limit_reported.take() {
                    tracing::info!(
                        limited_until = prev,
                        "GitHub API rate limit cleared"
                    );
                    let _ = store.kv_delete(RATE_LIMITED_UNTIL_KEY).await;
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
                        WeightSignal::Failed { .. }
                        | WeightSignal::Rerouted
                        | WeightSignal::Blocked
                        | WeightSignal::None => {}
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
                        Ordering::AcqRel,
                        Ordering::Relaxed,
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
                    last_sync = Instant::now();
                }

                // Periodic webhook health check
                if webhook_enabled {
                    let health_check_interval = config.webhook_health_check_interval();

                    if last_webhook_health_check.elapsed() >= health_check_interval {
                        if let Some(port) = webhook_port {
                            let (health, failure_reason) =
                                github::check_webhook_health(port).await;
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
                        last_webhook_health_check = Instant::now();
                    }
                }

                // Periodic upgrade check — notify channels when a newer release is available.
                if should_run_upgrade_check(last_upgrade_check, config.upgrade_check_interval) {
                    let store = Arc::clone(&store);
                    let channels = Arc::clone(&channel_registry);
                    let current = env!("ORCH_VERSION").to_string();
                    let check_interval = Duration::from_secs(config.upgrade_check_interval);
                    tokio::spawn(async move {
                        check_and_notify_upgrade(&store, &channels, &current, check_interval)
                            .await;
                    });
                    last_upgrade_check = Some(Instant::now());
                }

                // Update watchdog timestamp + log tick duration.
                let tick_elapsed = tick_start.elapsed();
                if tick_elapsed.as_secs() > 30 {
                    tracing::warn!(
                        elapsed_ms = u64::try_from(tick_elapsed.as_millis()).unwrap_or(u64::MAX),
                        "slow tick"
                    );
                } else {
                    tracing::debug!(
                        elapsed_ms = u64::try_from(tick_elapsed.as_millis()).unwrap_or(u64::MAX),
                        "tick complete"
                    );
                }
                last_tick_epoch.store(
                    SystemTime::now()
                        .duration_since(std_time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    Ordering::Relaxed,
                );
            }
            // Webhook events trigger an immediate tick (bypass polling interval)
            () = webhook_notify.notified() => {
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
                        WeightSignal::Failed { .. }
                        | WeightSignal::Rerouted
                        | WeightSignal::Blocked
                        | WeightSignal::None => {}
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
                    Ordering::AcqRel,
                    Ordering::Relaxed,
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
                    last_sync = Instant::now();
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
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(missed = n, "config change receiver lagged, reloading");
                        true
                    }
                    Err(RecvError::Closed) => {
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
                        interval = time::interval(config.tick_interval);
                        tracing::info!(tick = ?config.tick_interval, "tick interval updated");
                    }

                    // Resize the dispatch pool. A shrink only takes the permits that are
                    // free right now; the rest are taken as running tasks finish.
                    if review_in_force != config.max_parallel {
                        review_in_force = resize_dispatch_pool(
                            &review_semaphore,
                            review_in_force,
                            config.max_parallel,
                        );
                    }
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
                        &store,
                        &mut project_engines,
                        &mut active_repos,
                        &event_bus,
                        &tmux,
                        &capture_for_tick,
                        &semaphore,
                        &review_semaphore,
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
                    _ = signal::ctrl_c() => "SIGINT",
                    _ = sigterm.recv() => "SIGTERM",
                    _ = sighup.recv() => "SIGHUP",
                }
            } => {
                tracing::info!(signal = signal_name, "beginning graceful shutdown");

                // Wrap the entire shutdown sequence in a timeout to enforce the configured limit.
                // This prevents indefinite blocking if status updates or tmux operations hang.
                let shutdown_result = time::timeout(
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
                                            && let Err(e) = engine
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
                                        reset_count += 1;
                                    }
                                }
                            }
                            // Also reset internal in_progress tasks
                            if let Ok(tasks) = engine.store.list_internal_by_status(&engine.repo, TaskStatus::InProgress).await {
                                for task in &tasks {
                                    let task_id = format!("internal:{}", task.id);
                                    if let Err(e) = engine.task_manager.update_task_status(
                                        &ExternalId(task_id.clone()),
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
                                            && let Err(e) = engine
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
                                        review_reset_count += 1;
                                    }
                                }
                            }
                            // Also reset internal in_review tasks
                            if let Ok(tasks) = engine.store.list_internal_by_status(&engine.repo, TaskStatus::InReview).await {
                                for task in &tasks {
                                    let task_id = format!("internal:{}", task.id);
                                    if let Err(e) = engine.task_manager.update_task_status(
                                        &ExternalId(task_id.clone()),
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
                                    match tmux.kill_session(&session.name).await { Err(e) => {
                                        tracing::warn!(session = %session.name, error = %e, "failed to kill tmux session");
                                    } _ => {
                                        killed_sessions.push(session.name.clone());
                                    }}
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
                                    Duration::from_millis(100),
                                    Duration::from_secs(5),
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
                        timeout_secs = config.graceful_shutdown_secs(),
                        "graceful shutdown timed out, forcing exit"
                    );
                }

                break;
            }
        }
    }

    // Clean up event bus port file and service version file
    events::cleanup_port_file().await;
    local::cleanup().await;
    web.stop().await;
    events::cleanup_version_file();

    // transport and channels drop here at end of scope
    let _ = transport;
    tracing::info!("orch engine stopped");
    Ok(())
}
