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

use crate::backends::{ExternalBackend, ExternalId, ExternalTask, Status};
use crate::channels::capture::CaptureService;
use crate::channels::transport::Transport;
use crate::channels::ChannelRegistry;
use crate::db::{Db, TaskStatus};
use crate::engine::router::{get_route_result, RouteResult};
use crate::sidecar;
use crate::tmux::TmuxManager;
use anyhow::Context;
use runner::TaskRunner;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::Semaphore;

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
    let backend: Arc<dyn ExternalBackend> =
        Arc::new(crate::backends::github::GitHubBackend::new(repo.clone()));

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
    let runner = Arc::new(TaskRunner::new(repo.clone()));

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
                    if let Err(e) = sync_tick(&backend, &tmux, &repo, &db).await {
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

        // Register session for capture
        let session_name = tmux.session_name(&task_id);
        capture.register_session(&task_id, &session_name).await;

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
