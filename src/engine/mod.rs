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
use crate::channels::transport::Transport;
use crate::channels::ChannelRegistry;
use crate::db::Db;
use crate::engine::router::{get_route_result, RouteResult};
use crate::github::cli::GhCli;
use crate::tmux::TmuxManager;
use anyhow::Context;
use runner::TaskRunner;
use std::collections::HashMap;
use std::sync::Arc;
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

/// Persistent state for @mention deduplication.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct MentionState {
    /// ISO-8601 timestamp — only fetch comments updated after this.
    since: String,
    /// Map from comment ID (string) → created task issue number.
    #[serde(default)]
    processed: HashMap<String, u64>,
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
                    if let Err(e) = sync_tick(&backend, &tmux, &repo).await {
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

        // Load routing result from sidecar
        let route_result = get_route_result(&task_id).ok();

        // Dispatch task
        let runner = runner.clone();
        let backend = backend.clone();
        let capture = capture.clone();
        let task_id_for_cleanup = task_id.clone();

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
/// Handles less-frequent maintenance operations:
/// - Cleanup finished worktrees (done/needs_review tasks older than 24h)
/// - Check for merged PRs and transition in_review → done
/// - Scan issue comments for @mentions and create response tasks
/// - Trigger the PR review agent for open PRs (if enabled)
async fn sync_tick(
    backend: &Arc<dyn ExternalBackend>,
    _tmux: &Arc<TmuxManager>,
    repo: &str,
) -> anyhow::Result<()> {
    tracing::debug!("sync tick");

    // 1. Cleanup worktrees for done/needs_review tasks older than 24h
    if let Err(e) = cleanup_worktrees(backend, repo).await {
        tracing::warn!(?e, "worktree cleanup failed");
    }

    // 2. Check for merged PRs (in_review → done)
    if let Err(e) = check_merged_prs(backend, repo).await {
        tracing::warn!(?e, "merged PR check failed");
    }

    // 3. Scan for @orchestrator mentions → create response tasks
    if let Err(e) = scan_mentions(backend, repo).await {
        tracing::warn!(?e, "mention scan failed");
    }

    // 4. Trigger PR review agent (if workflow.enable_review_agent: true)
    if let Err(e) = trigger_pr_reviews(repo).await {
        tracing::warn!(?e, "PR review trigger failed");
    }

    Ok(())
}

// ─── sync helpers ────────────────────────────────────────────────────────────

/// Resolve the bare-clone git directory for a repo.
///
/// Checks `~/.orchestrator/projects/{owner}/{repo}.git` first, then falls back
/// to the current directory (used when running from a non-bare project tree).
fn repo_git_dir(orch_home: &std::path::Path, repo: &str) -> std::path::PathBuf {
    let parts: Vec<&str> = repo.split('/').collect();
    if parts.len() == 2 {
        let bare = orch_home
            .join("projects")
            .join(parts[0])
            .join(format!("{}.git", parts[1]));
        if bare.exists() {
            return bare;
        }
    }
    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
}

/// Cleanup worktrees for tasks that are done or needs_review and older than 24h.
///
/// Only removes a worktree after its associated PR has been merged. For
/// `needs_review` tasks (no merged PR), removes after 24h regardless.
/// Also deletes the local branch from the bare clone and prunes git metadata.
async fn cleanup_worktrees(backend: &Arc<dyn ExternalBackend>, repo: &str) -> anyhow::Result<()> {
    let orch_home = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join(".orchestrator");
    let project_name = repo.split('/').next_back().unwrap_or(repo);
    let worktrees_base = orch_home.join("worktrees").join(project_name);

    if !worktrees_base.exists() {
        return Ok(());
    }

    let cutoff = chrono::Utc::now() - chrono::Duration::hours(24);

    // Collect done + needs_review tasks
    let mut tasks = backend.list_by_status(Status::Done).await?;
    tasks.extend(backend.list_by_status(Status::NeedsReview).await?);

    if tasks.is_empty() {
        return Ok(());
    }

    let gh = GhCli::new();
    let git_dir = repo_git_dir(&orch_home, repo);
    let mut cleaned = 0u32;

    for task in &tasks {
        // Skip already-cleaned tasks
        if let Ok(flag) = crate::sidecar::get(&task.id.0, "worktree_cleaned") {
            if flag == "1" || flag == "true" {
                continue;
            }
        }

        // Only clean tasks older than 24h
        let updated = match chrono::DateTime::parse_from_rfc3339(&task.updated_at) {
            Ok(dt) => dt.with_timezone(&chrono::Utc),
            Err(_) => continue,
        };
        if updated >= cutoff {
            continue;
        }

        // Get branch from sidecar (set by run_task.sh when task is dispatched)
        let branch = match crate::sidecar::get(&task.id.0, "branch") {
            Ok(b) if !b.is_empty() => b,
            _ => continue, // no branch recorded, skip
        };

        let worktree_path = worktrees_base.join(&branch);
        if !worktree_path.exists() {
            // Already gone — mark cleaned so we skip next time
            let _ = crate::sidecar::set(&task.id.0, &["worktree_cleaned=1".to_string()]);
            continue;
        }

        // For done tasks, only clean after the PR is confirmed merged.
        // For needs_review tasks, clean unconditionally after 24h.
        let is_done = task.labels.iter().any(|l| l == "status:done");
        if is_done {
            let merged = gh.is_pr_merged(repo, &branch).await.unwrap_or(false);
            if !merged {
                tracing::debug!(
                    task_id = task.id.0,
                    branch,
                    "PR not yet merged, keeping worktree"
                );
                continue;
            }
        }

        tracing::info!(
            task_id = task.id.0,
            branch,
            path = %worktree_path.display(),
            "removing completed worktree"
        );

        match std::fs::remove_dir_all(&worktree_path) {
            Ok(()) => {
                let _ = crate::sidecar::set(&task.id.0, &["worktree_cleaned=1".to_string()]);
                cleaned += 1;

                // Delete local branch from the bare clone
                if git_dir.exists() {
                    if let Err(e) = gh.delete_local_branch(&git_dir, &branch).await {
                        tracing::debug!(
                            branch,
                            ?e,
                            "could not delete local branch (may not exist)"
                        );
                    }
                    // Prune stale worktree metadata
                    let _ = tokio::process::Command::new("git")
                        .args(["--git-dir", &git_dir.to_string_lossy(), "worktree", "prune"])
                        .output()
                        .await;
                }
            }
            Err(e) => {
                tracing::warn!(
                    task_id = task.id.0,
                    branch,
                    ?e,
                    "failed to remove worktree dir"
                );
            }
        }
    }

    if cleaned > 0 {
        tracing::info!(cleaned, "worktree cleanup complete");
    }

    Ok(())
}

/// Check for merged PRs and transition in_review tasks → done.
///
/// For each `in_review` task with a known branch (from sidecar), calls
/// `gh pr list --state merged` to detect the merge.  On detection:
/// - Updates status to done
/// - Posts a timestamped comment
/// - Removes the worktree immediately
/// - Closes the GitHub issue if `workflow.auto_close: true`
async fn check_merged_prs(backend: &Arc<dyn ExternalBackend>, repo: &str) -> anyhow::Result<()> {
    let in_review = backend.list_by_status(Status::InReview).await?;

    if in_review.is_empty() {
        return Ok(());
    }

    let gh = GhCli::new();
    let auto_close =
        crate::config::get("workflow.auto_close").unwrap_or_else(|_| "true".to_string()) == "true";

    let orch_home = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join(".orchestrator");
    let project_name = repo.split('/').next_back().unwrap_or(repo);
    let worktrees_base = orch_home.join("worktrees").join(project_name);
    let git_dir = repo_git_dir(&orch_home, repo);

    for task in &in_review {
        // Get branch from sidecar
        let branch = match crate::sidecar::get(&task.id.0, "branch") {
            Ok(b) if !b.is_empty() => b,
            _ => {
                tracing::debug!(
                    task_id = task.id.0,
                    "no branch in sidecar, skipping PR check"
                );
                continue;
            }
        };

        let merged = match gh.is_pr_merged(repo, &branch).await {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(task_id = task.id.0, branch, ?e, "PR merge check failed");
                continue;
            }
        };

        if !merged {
            continue;
        }

        tracing::info!(task_id = task.id.0, branch, "PR merged → marking task done");

        // Update status to done
        if let Err(e) = backend.update_status(&task.id, Status::Done).await {
            tracing::warn!(task_id = task.id.0, ?e, "failed to update status to done");
            continue;
        }

        // Post comment
        let comment = format!(
            "[{}] PR merged — task automatically marked as done",
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ")
        );
        let _ = backend.post_comment(&task.id, &comment).await;

        // Remove worktree immediately (no need to wait for cleanup_worktrees)
        let worktree_path = worktrees_base.join(&branch);
        if worktree_path.exists() {
            if let Ok(()) = std::fs::remove_dir_all(&worktree_path) {
                let _ = crate::sidecar::set(&task.id.0, &["worktree_cleaned=1".to_string()]);
                if git_dir.exists() {
                    if let Err(e) = gh.delete_local_branch(&git_dir, &branch).await {
                        tracing::debug!(branch, ?e, "could not delete local branch");
                    }
                    let _ = tokio::process::Command::new("git")
                        .args(["--git-dir", &git_dir.to_string_lossy(), "worktree", "prune"])
                        .output()
                        .await;
                }
            }
        }

        // Auto-close the GitHub issue
        if auto_close {
            if let Err(e) = gh.close_issue(repo, &task.id.0).await {
                tracing::warn!(task_id = task.id.0, ?e, "failed to close issue");
            }
        }
    }

    Ok(())
}

/// Scan for `@orchestrator` mentions in issue comments and create response tasks.
///
/// Maintains a state file at `~/.orchestrator/.orchestrator/mentions_{repo_key}.json`
/// that tracks the `since` timestamp and processed comment IDs to prevent
/// duplicate task creation.
///
/// Skips:
/// - Comments already in `processed`
/// - Comments generated by the orchestrator itself (`<!-- orch:` or
///   "via [Orchestrator]")
/// - Comments without `@orchestrator` in the body
async fn scan_mentions(backend: &Arc<dyn ExternalBackend>, repo: &str) -> anyhow::Result<()> {
    let state_dir = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join(".orchestrator")
        .join(".orchestrator");
    std::fs::create_dir_all(&state_dir)?;

    let repo_key = repo.replace('/', "_");
    let state_path = state_dir.join(format!("mentions_{repo_key}.json"));

    // Load (or initialise) mention state
    let mut state: MentionState = if state_path.exists() {
        let content = std::fs::read_to_string(&state_path).unwrap_or_default();
        serde_json::from_str(&content).unwrap_or_default()
    } else {
        MentionState::default()
    };

    if state.since.is_empty() {
        // Default to 7 days ago on first run to catch recent mentions
        state.since = (chrono::Utc::now() - chrono::Duration::days(7))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
    }

    let gh = GhCli::new();
    let comments = match gh.list_recent_comments(repo, &state.since).await {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(?e, "failed to fetch issue comments for mention scan");
            return Ok(());
        }
    };

    if comments.is_empty() {
        return Ok(());
    }

    let mut max_seen = state.since.clone();

    for comment in &comments {
        let comment_id = comment.id.to_string();

        // Advance the high-water mark
        let comment_time = comment.updated_at.as_deref().unwrap_or(&comment.created_at);
        if comment_time > max_seen.as_str() {
            max_seen = comment_time.to_string();
        }

        // Skip already-processed comments
        if state.processed.contains_key(&comment_id) {
            continue;
        }

        // Must contain @orchestrator
        if !comment.body.to_lowercase().contains("@orchestrator") {
            continue;
        }

        // Skip orchestrator-generated comments (avoid infinite loops)
        if comment.body.contains("<!-- orch:") || comment.body.contains("via [Orchestrator]") {
            continue;
        }

        // Extract issue number from issue_url
        // Format: https://api.github.com/repos/owner/repo/issues/123
        let issue_number = match &comment.issue_url {
            Some(url) => match url
                .split('/')
                .next_back()
                .and_then(|n| n.parse::<u64>().ok())
            {
                Some(n) => n.to_string(),
                None => {
                    tracing::debug!(comment_id, "cannot parse issue number from issue_url");
                    continue;
                }
            },
            None => {
                tracing::debug!(comment_id, "no issue_url on comment, skipping");
                continue;
            }
        };

        tracing::info!(
            comment_id,
            issue_number,
            author = comment.user.login,
            "found @orchestrator mention, creating task"
        );

        let title = format!("Respond to @orchestrator mention in #{issue_number}");
        let body = format!(
            "This task was created from an @orchestrator mention.\n\n\
            - **Repo:** `{repo}`\n\
            - **Target:** #{issue_number}\n\
            - **Author:** @{author}\n\
            - **Comment:** {url}\n\
            - **Created:** {created_at}\n\n\
            ### Mention Body\n\n\
            ```markdown\n{mention_body}\n```\n\n\
            ### Instructions\n\n\
            Respond back on #{issue_number} with your results and next steps.\n\n\
            **IMPORTANT:** Do NOT use @orchestrator in your response comments — \
            it will trigger the mention handler again and create an infinite loop. \
            Write \"orchestrator\" without the @ prefix.",
            author = comment.user.login,
            url = comment.html_url.as_deref().unwrap_or(""),
            created_at = comment.created_at,
            mention_body = comment.body,
        );

        let task_id = match backend
            .create_task(&title, &body, &["mention".to_string()])
            .await
        {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(comment_id, ?e, "failed to create mention task");
                continue;
            }
        };

        tracing::info!(
            comment_id,
            issue_number,
            task_id = task_id.0,
            "created mention task"
        );

        // Post acknowledgment comment on the source issue
        let ack = format!(
            "<!-- orch:mention -->\n\nAcknowledged. Created task #{task_number} from this @orchestrator mention.\n\n---\n*via [Orchestrator](https://github.com/gabrielkoerich/orchestrator)*",
            task_number = task_id.0,
        );
        if let Err(e) = gh.add_comment(repo, &issue_number, &ack).await {
            tracing::warn!(comment_id, ?e, "failed to post acknowledgment comment");
        }

        // Record in state to prevent reprocessing
        if let Ok(n) = task_id.0.parse::<u64>() {
            state.processed.insert(comment_id, n);
        }
    }

    // Advance the since pointer
    if max_seen > state.since {
        state.since = max_seen;
    }

    // Persist state
    let content = serde_json::to_string_pretty(&state)?;
    std::fs::write(&state_path, content)?;

    Ok(())
}

/// Trigger the PR review agent for open PRs (if enabled in config).
///
/// Delegates to the existing `review_prs.sh` script which handles review
/// scheduling, deduplication, and auto-merge logic.  Skips silently if the
/// script is not found or if `workflow.enable_review_agent` is not `true`.
async fn trigger_pr_reviews(repo: &str) -> anyhow::Result<()> {
    // Check if review agent is enabled in config
    let enabled = crate::config::get("workflow.enable_review_agent")
        .unwrap_or_else(|_| "false".to_string())
        == "true";

    if !enabled {
        return Ok(());
    }

    // Resolve scripts directory (same logic as TaskRunner)
    let orch_home = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join(".orchestrator");
    let scripts_dir = std::env::var("ORCH_SCRIPTS_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let brew_prefix =
                std::env::var("HOMEBREW_PREFIX").unwrap_or_else(|_| "/opt/homebrew".to_string());
            let brew_libexec = std::path::PathBuf::from(brew_prefix)
                .join("opt")
                .join("orch")
                .join("libexec")
                .join("scripts");
            if brew_libexec.exists() {
                return brew_libexec;
            }
            orch_home.join("scripts")
        });

    let script = scripts_dir.join("review_prs.sh");
    if !script.exists() {
        tracing::debug!(
            path = %script.display(),
            "review_prs.sh not found, skipping PR review trigger"
        );
        return Ok(());
    }

    tracing::info!("triggering PR review agent via review_prs.sh");

    let output = tokio::process::Command::new("bash")
        .arg(&script)
        .env("GH_REPO", repo)
        .env("ENABLE_REVIEW_AGENT", "true")
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!(stderr = %stderr, "review_prs.sh exited with non-zero status");
    }

    Ok(())
}
