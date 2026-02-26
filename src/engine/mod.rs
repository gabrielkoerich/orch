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

pub mod internal_tasks;
pub mod jobs;
pub mod router;
pub mod runner;
pub mod tasks;

use crate::backends::{ExternalBackend, ExternalId, ExternalTask, Status};
use crate::channels::capture::CaptureService;
use crate::channels::discord::DiscordChannel;
use crate::channels::telegram::TelegramChannel;
use crate::channels::tmux::TmuxChannel;
use crate::channels::transport::Transport;
use crate::channels::{Channel, ChannelRegistry, IncomingMessage};
use crate::config;
use crate::db::{Db, TaskStatus};
use crate::engine::router::{get_route_result, Router};
use crate::engine::tasks::TaskManager;
use crate::sidecar;
use crate::tmux::TmuxManager;
use runner::TaskRunner;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::{RwLock, Semaphore};

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
    /// Maximum parallel task executions
    pub max_parallel: usize,
    /// Stuck task timeout (seconds)
    pub stuck_timeout: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            tick_interval: std::time::Duration::from_secs(10),
            sync_interval: std::time::Duration::from_secs(120),
            max_parallel: 4,
            stuck_timeout: 1800,
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

        // Initialize task manager (placeholder db, will be replaced with shared db)
        let task_manager = Arc::new(TaskManager::new(
            Arc::new(Db::open(&crate::db::default_path()?)?),
            backend.clone(),
        ));

        // Task runner
        let runner = Arc::new(TaskRunner::new(repo.clone()));

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
            "no valid projects configured — run `orch init` or add repos to ~/.orchestrator/config.yml"
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
                // Core tick: poll tasks for all projects
                let router_guard = router.read().await;
                for engine in &project_engines {
                    if let Err(e) = tick(
                        &engine.backend,
                        &tmux,
                        &engine.runner,
                        &capture_for_tick,
                        &semaphore,
                        &config,
                        &jobs_path,
                        &db,
                        &router_guard,
                        &engine.task_manager,
                    ).await {
                        tracing::error!(repo = %engine.repo, ?e, "tick failed for project");
                    }
                }
                drop(router_guard);

                // Periodic sync (less frequent)
                if last_sync.elapsed() >= config.sync_interval {
                    for engine in &project_engines {
                        if let Err(e) = sync_tick(&engine.backend, &tmux, &engine.repo, &db).await {
                            tracing::error!(repo = %engine.repo, ?e, "sync tick failed for project");
                        }
                    }
                    last_sync = std::time::Instant::now();
                }
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
    runner: &Arc<TaskRunner>,
    capture: &Arc<CaptureService>,
    semaphore: &Arc<Semaphore>,
    config: &EngineConfig,
    jobs_path: &std::path::PathBuf,
    db: &Arc<Db>,
    router: &Router,
    task_manager: &Arc<TaskManager>,
) -> anyhow::Result<()> {
    // Phase 1: Check active tmux sessions for completions
    let session_snapshot = tmux.snapshot().await;
    for (task_id, active) in &session_snapshot {
        if !active {
            tracing::info!(task_id, "session completed, collecting results");
            // Unregister from capture service
            capture.unregister_session(task_id).await;
            // The runner handles status updates and GitHub comment posting.
            // We just clean up the session.
            let session_name = tmux.session_name(task_id);
            if let Err(e) = tmux.kill_session(&session_name).await {
                tracing::debug!(
                    task_id,
                    ?e,
                    "kill_session failed (session may already be gone)"
                );
            }
        }
    }

    // Phase 2: Recover stuck tasks
    let in_progress = task_manager
        .list_external_by_status(Status::InProgress)
        .await?;
    for task in &in_progress {
        let session_name = tmux.session_name(&task.id.0);
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

    // Phase 3a: Route new tasks
    let new_tasks = task_manager.list_external_by_status(Status::New).await?;
    let routable: Vec<&ExternalTask> = new_tasks
        .iter()
        .filter(|t| !t.labels.iter().any(|l| l == "no-agent"))
        .collect();

    for task in routable {
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

    // Phase 3b: Dispatch routed tasks.
    // Note: Routed tasks should never have no-agent (filtered during Phase 3a routing),
    // but we keep this filter as defense-in-depth.
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
        let session_name = tmux.session_name(&task.id.0);
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
        let session_name = tmux.session_name(&task_id);
        capture.register_session(&task_id, &session_name).await;

        // Dispatch task
        let runner = runner.clone();
        let backend = backend.clone();
        let tmux = tmux.clone();
        let capture = capture.clone();
        let task_id_for_cleanup = task_id.clone();
        let task_owned = task.clone();

        // Load routing result from sidecar (stored during Phase 3a)
        let route_result = get_route_result(&task_id).ok();

        tokio::spawn(async move {
            tracing::info!(task_id, "dispatching task");

            match runner
                .run_with_context(&task_owned, &backend, &tmux, route_result.as_ref())
                .await
            {
                Ok(()) => {
                    tracing::info!(task_id, "task runner completed");
                }
                Err(e) => {
                    tracing::error!(task_id, ?e, "task runner failed");
                    let _ = backend
                        .post_comment(
                            &crate::backends::ExternalId(task_id.clone()),
                            &format!(
                                "[{}] error: task runner failed: {e}",
                                chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
                            ),
                        )
                        .await;
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

    // 4. Review open PRs (optional - if enabled in config)
    if let Err(e) = review_open_prs(backend).await {
        tracing::warn!(err = %e, "PR review failed");
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
    let worktrees_base = dirs::home_dir()
        .map(|h| h.join(".orchestrator").join("worktrees"))
        .unwrap_or_else(|| std::path::PathBuf::from(".orchestrator/worktrees"));

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

/// Review open PRs (optional feature).
///
/// Lists tasks in review and triggers review agent if configured.
async fn review_open_prs(backend: &Arc<dyn ExternalBackend>) -> anyhow::Result<()> {
    // Check if review agent is enabled
    let review_enabled = crate::config::get("enable_review_agent")
        .map(|v| v == "true")
        .unwrap_or(false);

    if !review_enabled {
        tracing::debug!("review agent disabled, skipping");
        return Ok(());
    }

    tracing::info!("review agent enabled, listing in_review tasks");

    // Get tasks that are in review (have open PRs)
    let in_review_tasks = backend.list_by_status(Status::InReview).await?;

    for task in in_review_tasks {
        let task_id = &task.id.0;

        // Get branch from sidecar
        let branch = match sidecar::get(task_id, "branch") {
            Ok(b) if !b.is_empty() => b,
            _ => continue,
        };

        // TODO: Trigger review agent for each PR
        // For now, just log
        tracing::debug!(task_id, branch = %branch, "would trigger review agent");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
