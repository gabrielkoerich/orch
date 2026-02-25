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

pub mod jobs;
pub mod router;
mod runner;
pub mod tasks;

use crate::backends::github::GitHubBackend;
use crate::backends::{ExternalBackend, ExternalId, ExternalTask, Status};
use crate::channels::capture::CaptureService;
use crate::channels::transport::Transport;
use crate::channels::ChannelRegistry;
use crate::db::Db;
use crate::engine::router::{RouteResult, Router};
use crate::tmux::TmuxManager;
use anyhow::Context;
use runner::TaskRunner;
use std::path::PathBuf;
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

    // Initialize router
    let router = Router::from_config();
    tracing::info!(
        mode = %router.config.mode,
        router_agent = %router.config.router_agent,
        fallback = %router.config.fallback_executor,
        "router initialized"
    );

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
                    &router,
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
/// 3. Route new tasks (status:new → status:routed)
/// 4. Dispatch routed tasks
#[allow(clippy::too_many_arguments)]
async fn tick(
    backend: &Arc<dyn ExternalBackend>,
    tmux: &Arc<TmuxManager>,
    runner: &Arc<TaskRunner>,
    capture: &Arc<CaptureService>,
    router: &Router,
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

    // Phase 3: Route new tasks (status:new -> status:routed)
    let new_tasks = backend.list_by_status(Status::New).await?;
    for task in &new_tasks {
        // Skip no-agent tasks
        if task.labels.iter().any(|l| l == "no-agent") {
            continue;
        }

        let task_id = task.id.0.clone();
        tracing::info!(task_id, "routing task");

        match router.route(task).await {
            Ok(route_result) => {
                // Store routing result in sidecar
                if let Err(e) = router.store_route_result(&task_id, &route_result) {
                    tracing::warn!(task_id, ?e, "failed to store route result");
                }

                // Add agent label
                let agent_label = format!("agent:{}", route_result.agent);
                if let Err(e) = backend.set_labels(&task.id, &[agent_label]).await {
                    tracing::warn!(task_id, ?e, "failed to add agent label");
                }

                // Post routing comment
                let note = if let Some(ref warning) = route_result.warning {
                    format!(
                        "routed to {} (complexity: {}) (warning: {})",
                        route_result.agent, route_result.complexity, warning
                    )
                } else {
                    format!(
                        "routed to {} (complexity: {})",
                        route_result.agent, route_result.complexity
                    )
                };

                if let Err(e) = backend.post_comment(&task.id, &note).await {
                    tracing::warn!(task_id, ?e, "failed to post routing comment");
                }

                // Mark as routed
                if let Err(e) = backend.update_status(&task.id, Status::Routed).await {
                    tracing::error!(task_id, ?e, "failed to set routed status");
                } else {
                    tracing::info!(
                        task_id,
                        agent = %route_result.agent,
                        complexity = %route_result.complexity,
                        "task routed"
                    );
                }
            }
            Err(e) => {
                tracing::error!(task_id, ?e, "routing failed");
                // Mark as needs_review since routing failed
                let _ = backend
                    .post_comment(&task.id, &format!("routing failed: {e}"))
                    .await;
                let _ = backend.update_status(&task.id, Status::NeedsReview).await;
            }
        }
    }

    // Phase 4: Dispatch routed tasks
    let routed_tasks = backend.list_by_status(Status::Routed).await?;

    // Filter out no-agent tasks
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
        // If two ticks overlap, the second tick's list_by_status("routed") won't
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
        let route_result = router::get_route_result(&task_id).ok();

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
                            &ExternalId(task_id.clone()),
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

    // Phase 5: Unblock parents (blocked tasks whose children are all done)
    let blocked = backend.list_by_status(Status::Blocked).await?;
    for task in &blocked {
        // Check if all sub-issues are done
        // TODO: query sub-issues API to check children status
        let _ = task; // placeholder
    }

    // Phase 6: Check job schedules
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
/// - Review open PRs
async fn sync_tick(
    backend: &Arc<dyn ExternalBackend>,
    _tmux: &Arc<TmuxManager>,
) -> anyhow::Result<()> {
    tracing::debug!("sync tick");

    // Get repo from config (needed for GitHub operations)
    let repo = match crate::config::get("repo") {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("no repo configured, skipping sync_tick: {}", e);
            return Ok(());
        }
    };

    let gh = crate::github::cli::GhCli::new();

    // ============================================
    // 1. Worktree Cleanup (for done tasks)
    // ============================================
    if let Err(e) = cleanup_worktrees(backend, &repo, &gh).await {
        tracing::warn!("worktree cleanup failed: {}", e);
    }

    // ============================================
    // 2. Merged PR Detection (in_review → done)
    // ============================================
    if let Err(e) = check_merged_prs(backend, &repo, &gh).await {
        tracing::warn!("merged PR check failed: {}", e);
    }

    // ============================================
    // 3. @Mention Scanning
    // ============================================
    if let Err(e) = scan_mentions(backend, &repo, &gh).await {
        tracing::warn!("mention scan failed: {}", e);
    }

    // ============================================
    // 4. PR Review
    // ============================================
    if let Err(e) = review_open_prs(backend, &repo, &gh).await {
        tracing::warn!("PR review failed: {}", e);
    }

    Ok(())
}

/// Cleanup worktrees for done tasks that have merged PRs.
async fn cleanup_worktrees(
    backend: &Arc<dyn ExternalBackend>,
    repo: &str,
    gh: &crate::github::cli::GhCli,
) -> anyhow::Result<()> {
    let done_tasks = backend.list_by_status(Status::Done).await?;
    if done_tasks.is_empty() {
        return Ok(());
    }

    tracing::info!(count = done_tasks.len(), "checking worktrees for cleanup");

    for task in &done_tasks {
        let task_id = &task.id.0;

        // Check if already cleaned
        match crate::sidecar::get(task_id, "worktree_cleaned") {
            Ok(cleaned) if cleaned == "true" || cleaned == "1" => continue,
            _ => {}
        }

        // Get worktree and branch from sidecar
        let worktree = crate::sidecar::get(task_id, "worktree").ok();
        let branch = crate::sidecar::get(task_id, "branch").ok();
        let project_dir = crate::sidecar::get(task_id, "dir").ok();

        if worktree.is_none() || project_dir.is_none() {
            continue;
        }
        let worktree = worktree.unwrap();
        let project_dir = project_dir.unwrap();
        let worktree_path = PathBuf::from(&worktree);

        // Check if issue has a merged PR
        let has_merged = gh.find_merged_pr_for_issue(repo, task_id).await?.is_some();
        if !has_merged {
            continue;
        }

        tracing::info!(task_id, "cleaning up worktree for done task");

        let mut cleanup_ok = true;

        // Remove worktree directory
        if worktree_path.exists() {
            // Use git worktree remove if possible
            let git_remove = Command::new("git")
                .args([
                    "-C",
                    &project_dir,
                    "worktree",
                    "remove",
                    &worktree,
                    "--force",
                ])
                .output()
                .await;

            match git_remove {
                Ok(output) if output.status.success() => {
                    tracing::debug!(task_id, "removed worktree via git");
                }
                _ => {
                    // Fallback: just remove the directory
                    if let Err(e) = tokio::fs::remove_dir_all(&worktree_path).await {
                        tracing::warn!(task_id, "failed to remove worktree: {}", e);
                        cleanup_ok = false;
                    }
                }
            }
        }

        // Delete branch
        if let Some(ref branch_name) = branch {
            let branch_exists = Command::new("git")
                .args([
                    "-C",
                    &project_dir,
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{}", branch_name),
                ])
                .output()
                .await
                .map(|o| o.status.success())
                .unwrap_or(false);

            if branch_exists {
                let delete_result = Command::new("git")
                    .args(["-C", &project_dir, "branch", "-D", branch_name])
                    .output()
                    .await;

                match delete_result {
                    Ok(output) if output.status.success() => {
                        tracing::debug!(task_id, branch = branch_name, "deleted branch");
                    }
                    _ => {
                        tracing::warn!(task_id, branch = branch_name, "failed to delete branch");
                        cleanup_ok = false;
                    }
                }
            }
        }

        // Mark as cleaned
        if cleanup_ok {
            let _ = crate::sidecar::set(task_id, &["worktree_cleaned=true".to_string()]);
            tracing::info!(task_id, "marked worktree as cleaned");
        }
    }

    Ok(())
}

/// Check for merged PRs and mark in_review tasks as done.
async fn check_merged_prs(
    backend: &Arc<dyn ExternalBackend>,
    repo: &str,
    gh: &crate::github::cli::GhCli,
) -> anyhow::Result<()> {
    let in_review = backend.list_by_status(Status::InReview).await?;
    if in_review.is_empty() {
        return Ok(());
    }

    tracing::info!(count = in_review.len(), "checking for merged PRs");

    for task in &in_review {
        let task_id = &task.id.0;

        // Get branch from sidecar
        let branch = match crate::sidecar::get(task_id, "branch") {
            Ok(b) if !b.is_empty() => b,
            _ => continue,
        };

        // Search for PR with this head branch
        let prs = gh.list_pulls(repo, "all").await?;
        let matching_pr = prs
            .iter()
            .find(|pr| pr.head.ref_field == branch && pr.merged == Some(true));

        if let Some(pr) = matching_pr {
            tracing::info!(
                task_id,
                pr_number = pr.number,
                "PR merged, marking task as done"
            );

            // Update task status
            backend.update_status(&task.id, Status::Done).await?;

            // Post comment
            let comment = format!(
                "[{}] PR #{} merged — marking task as `done`",
                chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
                pr.number
            );
            backend.post_comment(&task.id, &comment).await?;
        }
    }

    Ok(())
}

/// Scan for @mentions and create tasks.
async fn scan_mentions(
    backend: &Arc<dyn ExternalBackend>,
    repo: &str,
    gh: &crate::github::cli::GhCli,
) -> anyhow::Result<()> {
    // Load or initialize mentions tracking
    let mentions_path = dirs::home_dir()
        .map(|h| h.join(".orchestrator/.orchestrator/mentions.json"))
        .context("cannot determine home directory")?;

    std::fs::create_dir_all(mentions_path.parent().unwrap())?;

    let since = if mentions_path.exists() {
        std::fs::read_to_string(&mentions_path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v["since"].as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string())
    } else {
        "1970-01-01T00:00:00Z".to_string()
    };

    // Fetch comments since last scan
    let comments = gh.list_issue_comments(repo, &since).await?;
    if comments.is_empty() {
        return Ok(());
    }

    tracing::info!(count = comments.len(), "scanning for @mentions");

    let mention_marker = "<!-- orch:mention -->";
    let mut max_seen = since;
    let mut new_tasks = 0;

    for comment in &comments {
        // Track max timestamp seen
        if comment.updated_at.as_str() > max_seen.as_str() {
            max_seen = comment.updated_at.clone();
        } else if comment.created_at.as_str() > max_seen.as_str() {
            max_seen = comment.created_at.clone();
        }

        // Check if comment contains @orchestrator
        let body_lower = comment.body.to_lowercase();
        if !body_lower.contains("@orchestrator") {
            continue;
        }

        // Skip orchestrator-generated comments
        if comment.body.contains("via [Orchestrator]") || comment.body.contains("<!-- orch:") {
            continue;
        }

        // Skip inline code blocks and quotes
        let mut in_fence = false;
        let mut actionable = false;
        for line in comment.body.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                in_fence = !in_fence;
                continue;
            }
            if in_fence || trimmed.starts_with(">") {
                continue;
            }
            if line.to_lowercase().contains("@orchestrator") {
                actionable = true;
                break;
            }
        }

        if !actionable {
            continue;
        }

        // Extract issue number from issue_url
        let issue_number = comment
            .issue_url
            .as_ref()
            .and_then(|url| url.split('/').next_back())
            .and_then(|s| s.parse::<u64>().ok());

        let issue_num_str = match issue_number {
            Some(n) => n.to_string(),
            None => continue,
        };

        // Create task for mention
        let title = format!("Respond to @orchestrator mention in #{}", issue_num_str);
        let body = format!(
            "This task was created from an @orchestrator mention.\n\n\
             - **Repo:** `{}`\n\
             - **Target:** #{}\n\
             - **Author:** @{}\n\
             - **Comment:** {}\n\
             - **Created:** {}\n\n\
             ### Mention Body\n\n\
             ```markdown\n{}\n```\n\n\
             ### Instructions\n\n\
             Respond back on #{} with your results and next steps.\n\n\
             **IMPORTANT:** Do NOT use @orchestrator in your response comments — it will trigger the mention handler again and create an infinite loop. Write \"orchestrator\" without the @ prefix.",
            repo,
            issue_num_str,
            comment.user.login,
            comment.html_url.as_deref().unwrap_or("unknown"),
            comment.created_at,
            comment.body,
            issue_num_str
        );

        // Create the task
        let labels = vec!["mention".to_string()];
        match backend.create_task(&title, &body, &labels).await {
            Ok(new_id) => {
                new_tasks += 1;
                tracing::info!(task_id = new_id.0, "created task from @mention");

                // Post acknowledgment
                let ack_body = format!(
                    "{}\n\nAcknowledged. Created task #{} from this @orchestrator mention.\n\n---\n*via [Orchestrator](https://github.com/gabrielkoerich/orchestrator)*",
                    mention_marker,
                    new_id.0
                );

                let ack_endpoint = format!("repos/{}/issues/{}/comments", repo, issue_num_str);
                let _ = Command::new("gh")
                    .args(["api", &ack_endpoint, "-f", &format!("body={}", ack_body)])
                    .output()
                    .await;
            }
            Err(e) => {
                tracing::warn!("failed to create task from mention: {}", e);
            }
        }
    }

    // Update mentions tracking
    let tracking = serde_json::json!({
        "since": max_seen,
        "last_scan": chrono::Utc::now().to_rfc3339(),
    });
    let _ = std::fs::write(&mentions_path, tracking.to_string());

    if new_tasks > 0 {
        tracing::info!(count = new_tasks, "created tasks from @mentions");
    }

    Ok(())
}

/// Review open PRs using the review agent.
async fn review_open_prs(
    backend: &Arc<dyn ExternalBackend>,
    repo: &str,
    gh: &crate::github::cli::GhCli,
) -> anyhow::Result<()> {
    // Check if review agent is enabled
    let review_enabled = crate::config::get("workflow.enable_review_agent")
        .map(|v| v == "true")
        .unwrap_or(false);

    if !review_enabled {
        tracing::debug!("review agent disabled, skipping PR review");
        return Ok(());
    }

    let review_agent =
        crate::config::get("workflow.review_agent").unwrap_or_else(|_| "claude".to_string());

    // Load review state
    let state_path = dirs::home_dir()
        .map(|h| {
            h.join(format!(
                ".orchestrator/.orchestrator/pr_reviews_{}.json",
                repo.replace('/', "_")
            ))
        })
        .context("cannot determine home directory")?;

    let mut reviewed_shas: std::collections::HashSet<String> = if state_path.exists() {
        std::fs::read_to_string(&state_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    } else {
        std::collections::HashSet::new()
    };

    // List open PRs
    let prs = gh.list_pulls(repo, "open").await?;
    if prs.is_empty() {
        return Ok(());
    }

    tracing::info!(count = prs.len(), "checking PRs for review");

    let review_drafts = crate::config::get("workflow.review_drafts")
        .map(|v| v == "true")
        .unwrap_or(false);

    // Load the review prompt template
    let review_prompt_path = std::path::PathBuf::from("prompts/pr_review.md");
    let review_prompt_template = if review_prompt_path.exists() {
        tokio::fs::read_to_string(&review_prompt_path)
            .await
            .unwrap_or_default()
    } else {
        String::new()
    };

    for pr in &prs {
        // Skip drafts unless configured
        if pr.draft == Some(true) && !review_drafts {
            continue;
        }

        let sha = &pr.head.sha;

        // Skip if already reviewed at this SHA
        if reviewed_shas.contains(sha) {
            continue;
        }

        tracing::info!(pr_number = pr.number, "reviewing PR");

        // Get PR diff
        let diff_output = Command::new("gh")
            .args(["pr", "diff", &pr.number.to_string(), "--repo", repo])
            .output()
            .await;

        let diff = match diff_output {
            Ok(output) if output.status.success() => {
                let text = String::from_utf8_lossy(&output.stdout);
                // Limit diff size
                let limit = crate::config::get("workflow.review_diff_limit")
                    .ok()
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(5000);
                text.chars().take(limit).collect::<String>()
            }
            _ => {
                tracing::warn!(pr_number = pr.number, "failed to get PR diff");
                continue;
            }
        };

        if diff.is_empty() {
            continue;
        }

        // Build review prompt from template or default
        let review_prompt = if review_prompt_template.is_empty() {
            format!(
                "Review this pull request:\n\n\
                 **Title:** {}\n\
                 **Author:** @{}\n\
                 **Body:** {}\n\n\
                 **Diff:**\n```diff\n{}\n```\n\n\
                 Provide your review as JSON with 'decision' (approve or request_changes) and 'notes' fields.",
                pr.title,
                pr.user.login,
                pr.body.as_deref().unwrap_or("(no body)"),
                diff
            )
        } else {
            review_prompt_template
                .replace("{{PR_NUMBER}}", &pr.number.to_string())
                .replace("{{PR_TITLE}}", &pr.title)
                .replace("{{PR_AUTHOR}}", &pr.user.login)
                .replace("{{PR_BODY}}", pr.body.as_deref().unwrap_or("(no body)"))
                .replace("{{DIFF}}", &diff)
        };

        // Run review agent
        let review_result = run_review_agent(&review_agent, &review_prompt).await;

        match review_result {
            Ok((decision, notes)) => {
                // Post the review
                let event = if decision == "approve" {
                    "APPROVE"
                } else {
                    "REQUEST_CHANGES"
                };
                let badge = match review_agent.as_str() {
                    "claude" => "🟣",
                    "codex" => "🟢",
                    "opencode" => "🔵",
                    _ => "🔍",
                };
                let decision_text = if decision == "approve" {
                    "Approve"
                } else {
                    "Changes Requested"
                };

                let comment_body = format!(
                    "## {} Automated Review — {}\n\n{}\n\n---\n*By {}[bot] via [Orchestrator](https://github.com/gabrielkoerich/orchestrator)*",
                    badge,
                    decision_text,
                    notes,
                    review_agent
                );

                // Try to post formal PR review, fall back to comment
                if let Err(e) = gh
                    .post_pr_review(repo, pr.number, event, &comment_body)
                    .await
                {
                    tracing::warn!(pr_number = pr.number, error = %e, "failed to post PR review, trying comment");
                    if let Err(e2) = gh
                        .add_comment(repo, &pr.number.to_string(), &comment_body)
                        .await
                    {
                        tracing::warn!(pr_number = pr.number, error = %e2, "failed to post comment");
                    }
                }

                // Update linked task status
                if let Ok(task_id) = find_task_by_branch(backend, &pr.head.ref_field).await {
                    let new_status = if decision == "approve" {
                        Status::Done
                    } else {
                        Status::NeedsReview
                    };

                    if let Err(e) = backend
                        .update_status(&crate::backends::ExternalId(task_id.clone()), new_status)
                        .await
                    {
                        tracing::warn!(task_id, error = %e, "failed to update task status after review");
                    }

                    let status_note = if decision == "approve" {
                        "approved by review agent"
                    } else {
                        "changes requested by review agent"
                    };
                    let _ = backend
                        .post_comment(
                            &crate::backends::ExternalId(task_id),
                            &format!(
                                "[{}] PR #{} {}",
                                chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
                                pr.number,
                                status_note
                            ),
                        )
                        .await;
                }

                tracing::info!(pr_number = pr.number, decision, "review completed");
            }
            Err(e) => {
                tracing::warn!(pr_number = pr.number, error = %e, "review agent failed");
            }
        }

        // Mark as reviewed
        reviewed_shas.insert(sha.clone());
    }

    // Save review state
    let state = serde_json::to_string(&reviewed_shas)?;
    std::fs::write(&state_path, state)?;

    Ok(())
}

/// Run the review agent and parse the response.
async fn run_review_agent(agent: &str, prompt: &str) -> anyhow::Result<(String, String)> {
    let output = match agent {
        "claude" => {
            Command::new("claude")
                .args(["--print", "--model", "sonnet"])
                .arg(prompt)
                .output()
                .await
        }
        "codex" => {
            Command::new("codex")
                .args(["--print", "--model", "gpt-5.2"])
                .arg(prompt)
                .output()
                .await
        }
        "opencode" => {
            Command::new("opencode")
                .args(["run", "--print"])
                .arg(prompt)
                .output()
                .await
        }
        _ => {
            // Default to claude
            Command::new("claude")
                .args(["--print", "--model", "sonnet"])
                .arg(prompt)
                .output()
                .await
        }
    };

    let output = output.context("failed to run review agent")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("review agent failed: {}", stderr);
    }

    let response = String::from_utf8_lossy(&output.stdout);

    // Parse JSON response
    let json_str = extract_json_from_response(&response);
    let parsed: serde_json::Value =
        serde_json::from_str(&json_str).context("failed to parse review agent response as JSON")?;

    let decision = parsed["decision"]
        .as_str()
        .unwrap_or("request_changes")
        .to_string();

    let notes = parsed["notes"]
        .as_str()
        .unwrap_or("No notes provided")
        .to_string();

    Ok((decision, notes))
}

/// Extract JSON from agent response (may be wrapped in markdown code blocks).
fn extract_json_from_response(raw: &str) -> String {
    // Try ```json ... ``` blocks first
    if let Some(start) = raw.find("```json") {
        if let Some(end) = raw[start + 7..].find("```") {
            return raw[start + 7..start + 7 + end].trim().to_string();
        }
    }

    // Try any ``` ... ``` block
    if let Some(start) = raw.find("```") {
        if let Some(end) = raw[start + 3..].find("```") {
            let content = raw[start + 3..start + 3 + end].trim();
            if content.starts_with('{') {
                return content.to_string();
            }
        }
    }

    // Try to find JSON object directly
    if let Some(start) = raw.find('{') {
        if let Some(end) = raw.rfind('}') {
            if end > start {
                return raw[start..=end].to_string();
            }
        }
    }

    raw.to_string()
}

/// Find a task by its branch name.
async fn find_task_by_branch(
    backend: &Arc<dyn ExternalBackend>,
    branch: &str,
) -> anyhow::Result<String> {
    // Search through in_progress and in_review tasks
    let statuses = vec![Status::InProgress, Status::InReview];

    for status in statuses {
        let tasks = backend.list_by_status(status).await?;
        for task in tasks {
            if let Ok(task_branch) = crate::sidecar::get(&task.id.0, "branch") {
                if task_branch == branch {
                    return Ok(task.id.0.clone());
                }
            }
        }
    }

    anyhow::bail!("no task found for branch: {}", branch)
}
