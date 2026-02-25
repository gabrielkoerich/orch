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
mod runner;
pub mod tasks;

use crate::backends::github::GitHubBackend;
use crate::backends::{ExternalBackend, ExternalId, ExternalTask, Status};
use crate::channels::capture::CaptureService;
use crate::channels::transport::Transport;
use crate::channels::ChannelRegistry;
use crate::db::Db;
use crate::tmux::TmuxManager;
use anyhow::Context;
use internal_tasks::{list_internal_tasks_by_status, update_internal_task_status, InternalTask};
use runner::TaskRunner;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// Source of a unified task — either external (GitHub) or internal (SQLite).
#[derive(Debug, Clone)]
pub enum TaskSource {
    External(String), // External ID (e.g., GitHub issue number)
    Internal(i64),    // SQLite row ID
}

impl TaskSource {
    /// Returns true if this is an internal task.
    pub fn is_internal(&self) -> bool {
        matches!(self, TaskSource::Internal(_))
    }

    /// Returns the internal task ID if this is an internal task.
    pub fn internal_id(&self) -> Option<i64> {
        match self {
            TaskSource::Internal(id) => Some(*id),
            _ => None,
        }
    }

    /// Returns the external task ID if this is an external task.
    pub fn external_id(&self) -> Option<&str> {
        match self {
            TaskSource::External(id) => Some(id),
            _ => None,
        }
    }
}

/// A unified task that can come from either external (GitHub) or internal (SQLite) sources.
#[derive(Debug, Clone)]
pub struct UnifiedTask {
    pub id: String,
    pub title: String,
    pub body: String,
    pub status: Status,
    pub source: TaskSource,
    pub labels: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl UnifiedTask {
    /// Create a unified task from an external task.
    pub fn from_external(task: ExternalTask) -> Self {
        let status = task
            .labels
            .iter()
            .find(|l| l.starts_with("status:"))
            .and_then(|l| l.strip_prefix("status:"))
            .and_then(|s| match s {
                "new" => Some(Status::New),
                "routed" => Some(Status::Routed),
                "in_progress" => Some(Status::InProgress),
                "done" => Some(Status::Done),
                "blocked" => Some(Status::Blocked),
                "in_review" => Some(Status::InReview),
                "needs_review" => Some(Status::NeedsReview),
                _ => None,
            })
            .unwrap_or(Status::New);

        Self {
            id: task.id.0.clone(),
            title: task.title,
            body: task.body,
            status,
            source: TaskSource::External(task.id.0.clone()),
            labels: task.labels,
            created_at: task.created_at,
            updated_at: task.updated_at,
        }
    }

    /// Create a unified task from an internal task.
    pub fn from_internal(task: InternalTask) -> Self {
        let status = internal_tasks::string_to_status(&task.status).unwrap_or(Status::New);
        let id = format!("internal:{}", task.id);

        let mut labels = vec!["internal".to_string()];
        labels.push(format!("source:{}", task.source));
        if !task.source_id.is_empty() {
            labels.push(format!("source_id:{}", task.source_id));
        }

        Self {
            id,
            title: task.title,
            body: task.body,
            status,
            source: TaskSource::Internal(task.id),
            labels,
            created_at: task.created_at,
            updated_at: task.updated_at,
        }
    }

    /// Returns true if this is an internal task.
    pub fn is_internal(&self) -> bool {
        self.source.is_internal()
    }
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

    // Initialize channel registry
    let _channels = ChannelRegistry::new();

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
                    if let Err(e) = sync_tick(&backend, &db, &tmux).await {
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

    // transport and channels drop here at end of scope
    let _ = transport;
    tracing::info!("orch-core engine stopped");
    Ok(())
}

/// Core tick — runs every 10s.
///
/// Phases (matching v0 poll.sh):
/// 1. Monitor active tmux sessions (detect completions)
/// 2. Recover stuck in_progress tasks
/// 3. Dispatch new/routed tasks (both external and internal)
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

    // Phase 3: Dispatch new/routed tasks (both external and internal)
    let mut unified_tasks: Vec<UnifiedTask> = Vec::new();

    // Query external (GitHub) tasks
    let external_new = backend.list_by_status(Status::New).await?;
    let external_routed = backend.list_by_status(Status::Routed).await?;
    for task in external_new.into_iter().chain(external_routed.into_iter()) {
        unified_tasks.push(UnifiedTask::from_external(task));
    }

    // Query internal (SQLite) tasks
    match list_internal_tasks_by_status(db, "new").await {
        Ok(internal_new) => {
            for task in internal_new {
                unified_tasks.push(UnifiedTask::from_internal(task));
            }
        }
        Err(e) => {
            tracing::warn!(?e, "failed to list new internal tasks");
        }
    }
    match list_internal_tasks_by_status(db, "routed").await {
        Ok(internal_routed) => {
            for task in internal_routed {
                unified_tasks.push(UnifiedTask::from_internal(task));
            }
        }
        Err(e) => {
            tracing::warn!(?e, "failed to list routed internal tasks");
        }
    }

    // Filter out no-agent tasks
    let dispatchable: Vec<UnifiedTask> = unified_tasks
        .into_iter()
        .filter(|t| !t.labels.iter().any(|l| l == "no-agent"))
        .collect();

    if !dispatchable.is_empty() {
        tracing::info!(count = dispatchable.len(), "dispatchable tasks found");
    }

    for task in dispatchable {
        // Check if already running (has active session)
        let session_name = tmux.session_name(&task.id);
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
        let task_id = task.id.clone();
        let is_internal = task.is_internal();
        let db_for_update = db.clone();
        let backend_for_update = backend.clone();

        let update_result = if is_internal {
            // Update internal task status
            if let Some(internal_id) = task.source.internal_id() {
                update_internal_task_status(&db_for_update, internal_id, "in_progress")
                    .await
                    .map_err(|e| anyhow::anyhow!(e))
            } else {
                Err(anyhow::anyhow!("internal task missing ID"))
            }
        } else {
            // Update external task status
            backend_for_update
                .update_status(&ExternalId(task_id.clone()), Status::InProgress)
                .await
        };

        if let Err(e) = update_result {
            tracing::error!(task_id, ?e, "failed to set in_progress, skipping dispatch");
            drop(permit);
            continue;
        }

        // Register session for capture
        if !is_internal {
            let session_name = tmux.session_name(&task_id);
            capture.register_session(&task_id, &session_name).await;
        }

        // Dispatch task
        let runner = runner.clone();
        let backend = backend.clone();
        let db = db.clone();
        let capture = capture.clone();
        let task_id_for_cleanup = task_id.clone();

        tokio::spawn(async move {
            tracing::info!(task_id, is_internal, "dispatching task");

            match runner.run(&task_id).await {
                Ok(()) => {
                    tracing::info!(task_id, "task runner completed");
                }
                Err(e) => {
                    tracing::error!(task_id, ?e, "task runner failed");

                    // Handle error reporting based on task type
                    if is_internal {
                        // Store error in SQLite for internal tasks
                        if let Some(internal_id) = task.source.internal_id() {
                            let _ = internal_tasks::store_internal_task_result(
                                &db,
                                internal_id,
                                "Task execution failed",
                                "error",
                                &format!("Task runner failed: {e}"),
                                &[],
                            )
                            .await;
                        }
                    } else {
                        // Post error comment for external tasks
                        let _ = backend
                            .post_comment(
                                &ExternalId(task_id.clone()),
                                &format!(
                                    "[{}] error: task runner failed: {e}",
                                    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
                                ),
                            )
                            .await;
                    }
                }
            }

            // Unregister session from capture (no-op for internal tasks)
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

    // Phase 5: Check job schedules (pass db for internal task creation)
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
/// - Cleanup old internal task results
async fn sync_tick(
    _backend: &Arc<dyn ExternalBackend>,
    _db: &Arc<Db>,
    _tmux: &Arc<TmuxManager>,
) -> anyhow::Result<()> {
    tracing::debug!("sync tick");

    // TODO: cleanup worktrees for done tasks
    // TODO: check for merged PRs (in_review → done)
    // TODO: scan for @mentions
    // TODO: review open PRs
    // TODO: archive old internal task results (keep last 90 days)

    Ok(())
}
