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
//! Phase 2 approach: Rust owns the loop, `run_task.sh` still handles agent
//! invocation, git workflow, and prompt building.

pub mod internal_tasks;
pub mod jobs;
pub mod router;
mod runner;
pub mod tasks;

use crate::backends::github::GitHubBackend;
use crate::backends::{ExternalBackend, ExternalTask, Status};
use crate::channels::capture::CaptureService;
use crate::channels::tmux::TmuxChannel;
use crate::channels::transport::{MessageRoute, Transport};
use crate::channels::ChannelRegistry;
use crate::channels::IncomingMessage;
use crate::db::Db;
use crate::engine::router::{get_route_result, RouteResult};
use crate::tmux::TmuxManager;
use anyhow::Context;
use runner::TaskRunner;
use std::sync::Arc;
use tokio::sync::{mpsc, Semaphore};

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

/// Start the orchestrator service.
///
/// This is the main entry point — called by `orch-core serve`.
pub async fn serve() -> anyhow::Result<()> {
    tracing::info!("orch-core engine starting");

    let config = EngineConfig::default();

    // Load config — repo is required
    let repo = crate::config::get("repo").context(
        "'repo' not set in config — run `orch-core init` or set repo in ~/.orchestrator/config.yml",
    )?;

    // Initialize backend
    let backend: Arc<dyn ExternalBackend> = Arc::new(GitHubBackend::new(repo.clone()));

    // Health check
    backend.health_check().await?;
    tracing::info!(backend = backend.name(), "backend connected");

    // Initialize internal database
    let db = Arc::new(Db::open(&crate::db::default_path()?)?);
    db.migrate().await?;
    tracing::info!("internal database ready");

    // Initialize tmux manager
    let tmux = Arc::new(TmuxManager::new());

    // Initialize transport
    let transport = Arc::new(Transport::new());

    // Initialize capture service and start background loop
    let capture = Arc::new(CaptureService::new(transport.clone()));
    let capture_for_tick = capture.clone();
    tokio::spawn(async move {
        capture.start().await;
    });

    // Initialize channel registry with tmux channel wired to transport
    let mut channels = ChannelRegistry::new();
    let tmux_channel = Box::new(TmuxChannel::with_transport(transport.clone()));
    channels.register(tmux_channel);

    // Spawn channel message handlers
    let transport_for_handler = transport.clone();
    let backend_for_handler = backend.clone();
    tokio::spawn(async move {
        if let Err(e) = channel_message_handler(transport_for_handler, backend_for_handler).await {
            tracing::error!(?e, "channel message handler failed");
        }
    });

    // Task runner (delegates to run_task.sh)
    let runner = Arc::new(TaskRunner::new(repo));

    // Jobs path
    let orch_home = dirs::home_dir().unwrap_or_default().join(".orchestrator");
    let jobs_path = orch_home.join("jobs.yml");

    // Concurrency limiter
    let semaphore = Arc::new(Semaphore::new(config.max_parallel));

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
                // Core tick: poll tasks, check sessions
                if let Err(e) = tick(
                    &backend,
                    &tmux,
                    &transport,
                    &runner,
                    &capture_for_tick,
                    &semaphore,
                    &config,
                    &jobs_path,
                    &db,
                ).await {
                    tracing::error!(?e, "tick failed");
                }

                // Periodic sync (less frequent)
                if last_sync.elapsed() >= config.sync_interval {
                    if let Err(e) = sync_tick(&backend, &tmux).await {
                        tracing::error!(?e, "sync tick failed");
                    }
                    last_sync = std::time::Instant::now();
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

    tracing::info!("orch-core engine stopped");
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
    transport: &Arc<Transport>,
    runner: &Arc<TaskRunner>,
    capture: &Arc<CaptureService>,
    semaphore: &Arc<Semaphore>,
    config: &EngineConfig,
    jobs_path: &std::path::PathBuf,
    db: &Arc<Db>,
) -> anyhow::Result<()> {
    // Phase 1: Check active tmux sessions for completions
    let session_snapshot = tmux.snapshot().await;
    for (task_id, active) in &session_snapshot {
        if !active {
            tracing::info!(task_id, "session completed, collecting results");
            // Unregister from capture service
            capture.unregister_session(task_id).await;
            // The run_task.sh process handles its own status updates
            // and GitHub comment posting. We just clean up the session.
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
    let in_progress = backend.list_by_status(Status::InProgress).await?;
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

    // Phase 3: Dispatch new/routed tasks
    let mut new_tasks = backend.list_by_status(Status::New).await?;
    let routed_tasks = backend.list_by_status(Status::Routed).await?;
    new_tasks.extend(routed_tasks);

    // Filter out no-agent tasks
    let dispatchable: Vec<&ExternalTask> = new_tasks
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
        // If two ticks overlap, the second tick's list_by_status("new") won't
        // include this task because it's already in_progress.
        let task_id = task.id.0.clone();
        if let Err(e) = backend.update_status(&task.id, Status::InProgress).await {
            tracing::error!(task_id, ?e, "failed to set in_progress, skipping dispatch");
            drop(permit);
            continue;
        }

        // Register session for capture and transport
        let session_name = tmux.session_name(&task_id);
        capture.register_session(&task_id, &session_name).await;

        // Register session with transport for output streaming
        let tmux_session = session_name.clone();
        let task_id_for_transport = task.id.0.clone();
        let transport = transport.clone();
        tokio::spawn(async move {
            // Bind the session to the transport (use "engine" as channel since this is internal)
            transport
                .bind(&task_id_for_transport, &tmux_session, "engine", "main")
                .await;
            tracing::debug!(
                task_id = task_id_for_transport,
                session = tmux_session,
                "registered session with transport"
            );
        });

        // Dispatch task
        let runner = runner.clone();
        let backend = backend.clone();
        let capture = capture.clone();
        let task_id_for_cleanup = task_id.clone();

        // Load routing result from sidecar
        let route_result = get_route_result(&task_id).ok();

        tokio::spawn(async move {
            tracing::info!(task_id, "dispatching task");

            // Pass agent/model to runner via environment variables
            let agent = route_result.as_ref().map(|r: &RouteResult| r.agent.clone());
            let model = route_result.as_ref().and_then(|r| r.model.clone());

            match runner
                .run(&task_id, agent.as_deref(), model.as_deref())
                .await
            {
                Ok(()) => {
                    tracing::info!(task_id, "task runner completed");
                }
                Err(e) => {
                    tracing::error!(task_id, ?e, "task runner failed");
                    // Post error comment
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
    let blocked = backend.list_by_status(Status::Blocked).await?;
    for task in &blocked {
        // Check if all sub-issues are done
        // TODO: query sub-issues API to check children status
        let _ = task; // placeholder
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
    _backend: &Arc<dyn ExternalBackend>,
    _tmux: &Arc<TmuxManager>,
) -> anyhow::Result<()> {
    tracing::debug!("sync tick");

    // TODO: cleanup worktrees for done tasks
    // TODO: check for merged PRs (in_review → done)
    // TODO: scan for @mentions
    // TODO: review open PRs

    Ok(())
}

/// Handle incoming messages from channels.
///
/// This runs as a background task and:
/// 1. Receives messages from connected channels
/// 2. Routes them via transport (determines task session or command)
/// 3. Executes commands or forwards to tmux sessions
async fn channel_message_handler(
    transport: Arc<Transport>,
    backend: Arc<dyn ExternalBackend>,
) -> anyhow::Result<()> {
    // Create a channel for receiving messages from all channels
    // Note: In a full implementation, channels would send messages to this handler
    let (_tx, mut rx) = mpsc::channel::<IncomingMessage>(64);

    tracing::info!("channel message handler started");

    // Note: In a full implementation, each channel would forward messages to this handler.
    // For now, this demonstrates the routing logic.

    loop {
        tokio::select! {
            Some(msg) = rx.recv() => {
                let route = transport.route(&msg).await;

                match route {
                    MessageRoute::TaskSession { task_id } => {
                        // Forward message to tmux session
                        if let Some(session) = transport.tmux_session_for(&task_id).await {
                            tracing::debug!(
                                task_id = task_id,
                                session = session,
                                "forwarding message to tmux session"
                            );
                            // Use the tmux send-keys mechanism
                            let _ = crate::channels::tmux::send_keys(&session, &msg.body).await;
                        }
                    }
                    MessageRoute::Command { raw } => {
                        // Handle orchestrator commands
                        let response = handle_command(&raw, &backend).await;
                        // Send response back to the channel that sent the message
                        tracing::debug!(command = raw, response = response);
                    }
                    MessageRoute::NewTask => {
                        // Could create a new task from this message
                        tracing::debug!(channel = msg.channel, "new task request");
                    }
                }
            }
            _ = tokio::time::sleep(tokio::time::Duration::from_secs(60)) => {
                // Periodic cleanup or health check
            }
        }
    }
}

/// Handle orchestrator commands.
async fn handle_command(raw: &str, _backend: &Arc<dyn ExternalBackend>) -> String {
    let parts: Vec<&str> = raw.split_whitespace().collect();
    let cmd = parts.first().copied().unwrap_or("");

    match cmd {
        "/status" => {
            // Return status of active tasks
            "Use `orch task status` to view task status".to_string()
        }
        "/attach" => {
            // Attach to a running session
            if parts.len() < 2 {
                "Usage: /attach <task-id>".to_string()
            } else {
                format!("Attaching to task {}...", parts[1])
            }
        }
        "orch" => {
            // Handle orch CLI commands
            let subcmd = parts.get(1).copied().unwrap_or("");
            match subcmd {
                "task" => "Use `orch task` commands".to_string(),
                _ => "Unknown orch command".to_string(),
            }
        }
        _ => format!("Unknown command: {cmd}"),
    }
}
