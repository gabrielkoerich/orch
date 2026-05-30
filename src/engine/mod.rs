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

pub mod auto_merge;
pub mod channel_handler;
pub mod cleanup;
pub mod commands;
pub mod cooldown;
pub mod dispatch_guard;
pub mod events;
pub mod jobs;
pub mod review;
pub mod review_poll;
pub mod router;
pub mod runner;
pub mod subscribers;
pub mod sync;
pub mod tasks;
pub mod tick;

/// Standard Orch attribution footer for issue bodies, PR bodies, and comments.
///
/// Append to every user-visible string posted to GitHub so activity is
/// clearly attributed to orch rather than appearing as a human post.
pub fn orch_footer() -> &'static str {
    "\n\n---\n*Posted by [Orch](https://github.com/gabrielkoerich/orch)*"
}

/// Build a standard attribution footer for GitHub comments posted by orch bots.
///
/// `verb` is "Created", "Reviewed", or "Commented" depending on context.
pub fn attribution_footer(verb: &str, agent: &str, model: Option<&str>) -> String {
    match model {
        Some(m) => format!(
            "\n\n---\n*{} by {}[bot] via [Orch](https://github.com/gabrielkoerich/orch) using `{}`*",
            verb, agent, m
        ),
        None => format!(
            "\n\n---\n*{} by {}[bot] via [Orch](https://github.com/gabrielkoerich/orch)*",
            verb, agent
        ),
    }
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
use crate::cmd::CommandErrorContext;
use crate::config;
use crate::engine::cleanup::remove_worktree_and_branch;
use crate::engine::router::{Router, RouterConfig};
use crate::engine::runner::worktree::{
    abort_worktree_rebase, list_project_worktrees, rebase_worktree_on_origin_main,
    task_id_from_worktree_name, validate_worktree_gitdir,
};
use crate::engine::tasks::TaskManager;
use crate::github::http::{rate_limit_metrics, GhHttp};
use crate::repo_context::REPO_CONTEXT;
use crate::store::{review_session_expected, set_review_session_expected};
use crate::store::{TaskStatus, TaskStore};
use crate::tmux::TmuxManager;
use runner::WeightSignal;
// AtomicBool/Ordering removed — shutdown is now immediate (reset tasks + break)
use std::sync::Arc;
use tokio::process::Command;
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
#[derive(Clone)]
pub struct ProjectEngine {
    pub repo: String,
    pub project_dir: std::path::PathBuf,
    pub backend: Arc<dyn ExternalBackend>,
    pub task_manager: Arc<TaskManager>,
    pub runner: Arc<runner::TaskRunner>,
    pub store: Arc<TaskStore>,
}

/// Return the configured agent list from top-level `agents` in config.yml,
/// falling back to [`router::config::DEFAULT_AGENTS`] when not set.
///
/// This is the canonical source of truth for "which agents does orch know
/// about" — use it instead of `DEFAULT_AGENTS` directly so that
/// Claude-compatible agents added via config (e.g. `olm`) are recognized
/// without code changes.
///
/// The key is top-level (`agents:`) rather than nested (`engine.agents:`)
/// because `config::get` serializes nested YAML arrays via `serde_norway::to_string`
/// which produces YAML block format, not JSON — breaking downstream parsing.
pub fn configured_agents() -> Vec<String> {
    if let Ok(agents_str) = crate::config::get("agents") {
        if !agents_str.is_empty() && agents_str != "[]" {
            // config::get returns serde_norway::to_string for arrays, which is
            // YAML block format ("- claude\n- codex\n..."). Parse as YAML first.
            if let Ok(agents_arr) = serde_norway::from_str::<Vec<String>>(&agents_str) {
                if !agents_arr.is_empty() {
                    return agents_arr;
                }
            }
            // Fallback: try JSON (inline arrays) or comma-separated
            if let Ok(agents_arr) = serde_json::from_str::<Vec<String>>(&agents_str) {
                if !agents_arr.is_empty() {
                    return agents_arr;
                }
            }
        }
    }
    router::config::DEFAULT_AGENTS
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Engine configuration.
#[derive(Clone)]
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
    /// Stuck task timeout for in_review tasks with no active tmux session (seconds).
    /// Longer than `no_session_stuck_timeout` because review agents exit their tmux session
    /// on normal completion before the result is delivered to the engine. Using a short
    /// no-session timeout for in_review tasks causes a race where a completed review result
    /// arrives after the stuck recovery has already reset the task to needs_review, discarding
    /// the review work.
    pub in_review_no_session_stuck_timeout: u64,
    /// Auto-create follow-up tasks when PR reviews request changes
    pub auto_create_followup_on_changes: bool,
    /// Graceful shutdown timeout — how long to wait for running agents before exiting.
    pub graceful_shutdown_timeout: std::time::Duration,
    /// Grace period before silence detection kicks in (seconds).
    /// Allows time for agent startup, model download, API handshake.
    pub silence_grace_period: u64,
    /// Cooldown duration for a model detected as silent (seconds).
    pub silence_cooldown: u64,
    /// How often to check for a newer orch release and notify channels (seconds).
    /// Set to 0 to disable. Default: 3600 (1 hour).
    pub upgrade_check_interval: u64,
    /// Automatically run `brew upgrade orch` and restart the service when a newer
    /// release is detected. Default: true. Set `engine.auto_upgrade: false` to disable.
    pub auto_upgrade: bool,
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
            in_review_no_session_stuck_timeout: 1800,
            auto_create_followup_on_changes: true,
            graceful_shutdown_timeout: std::time::Duration::from_secs(600),
            silence_grace_period: 300,
            silence_cooldown: 3600,
            upgrade_check_interval: 3600,
            auto_upgrade: true,
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
            } else {
                tracing::warn!(key = "engine.tick_interval", value = %val, default_secs = config.tick_interval.as_secs(), "invalid value for engine.tick_interval, using default");
            }
        }

        if let Ok(val) = crate::config::get("engine.sync_interval") {
            if let Ok(secs) = val.parse::<u64>() {
                config.sync_interval = std::time::Duration::from_secs(secs);
            } else {
                tracing::warn!(key = "engine.sync_interval", value = %val, default_secs = config.sync_interval.as_secs(), "invalid value for engine.sync_interval, using default");
            }
        }

        if let Ok(val) = crate::config::get("engine.max_parallel") {
            if let Ok(n) = val.parse::<usize>() {
                config.max_parallel = n;
            } else {
                tracing::warn!(key = "engine.max_parallel", value = %val, default = config.max_parallel, "invalid value for engine.max_parallel, using default");
            }
        }

        if let Ok(val) = crate::config::get("engine.stuck_timeout") {
            if let Ok(secs) = val.parse::<u64>() {
                config.stuck_timeout = secs;
            } else {
                tracing::warn!(key = "engine.stuck_timeout", value = %val, default_secs = config.stuck_timeout, "invalid value for engine.stuck_timeout, using default");
            }
        }

        if let Ok(val) = crate::config::get("engine.no_session_stuck_timeout") {
            if let Ok(secs) = val.parse::<u64>() {
                config.no_session_stuck_timeout = secs;
            } else {
                tracing::warn!(key = "engine.no_session_stuck_timeout", value = %val, default_secs = config.no_session_stuck_timeout, "invalid value for engine.no_session_stuck_timeout, using default");
            }
        }

        if let Ok(val) = crate::config::get("engine.in_review_no_session_stuck_timeout") {
            if let Ok(secs) = val.parse::<u64>() {
                config.in_review_no_session_stuck_timeout = secs;
            } else {
                tracing::warn!(key = "engine.in_review_no_session_stuck_timeout", value = %val, default_secs = config.in_review_no_session_stuck_timeout, "invalid value for engine.in_review_no_session_stuck_timeout, using default");
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
            } else {
                tracing::warn!(key = "engine.webhook_health_check_interval", value = %val, default = ?config.webhook_health_check_interval, "invalid value for engine.webhook_health_check_interval, using default");
            }
        }

        if let Ok(val) = crate::config::get("workflow.auto_create_followup_on_changes") {
            config.auto_create_followup_on_changes = !val.eq_ignore_ascii_case("false");
        }

        if let Ok(val) = crate::config::get("engine.graceful_shutdown_timeout") {
            if let Ok(secs) = val.parse::<u64>() {
                config.graceful_shutdown_timeout = std::time::Duration::from_secs(secs);
            } else {
                tracing::warn!(key = "engine.graceful_shutdown_timeout", value = %val, default_secs = config.graceful_shutdown_timeout.as_secs(), "invalid value for engine.graceful_shutdown_timeout, using default");
            }
        }

        if let Ok(val) = crate::config::get("engine.silence_grace_period") {
            if let Ok(secs) = val.parse::<u64>() {
                config.silence_grace_period = secs;
            } else {
                tracing::warn!(key = "engine.silence_grace_period", value = %val, default = config.silence_grace_period, "invalid value for engine.silence_grace_period, using default");
            }
        }

        if let Ok(val) = crate::config::get("engine.silence_cooldown") {
            if let Ok(secs) = val.parse::<u64>() {
                config.silence_cooldown = secs;
            } else {
                tracing::warn!(key = "engine.silence_cooldown", value = %val, default = config.silence_cooldown, "invalid value for engine.silence_cooldown, using default");
            }
        }

        if let Ok(val) = crate::config::get("engine.upgrade_check_interval") {
            if let Ok(secs) = val.parse::<u64>() {
                config.upgrade_check_interval = secs;
            } else {
                tracing::warn!(key = "engine.upgrade_check_interval", value = %val, default = config.upgrade_check_interval, "invalid value for engine.upgrade_check_interval, using default");
            }
        }

        if let Ok(val) = crate::config::get("engine.auto_upgrade") {
            config.auto_upgrade = !val.eq_ignore_ascii_case("false");
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

    // Initialize unified task store once — all project engines share the same SQLite file.
    // Creating one pool here avoids N separate connection pools for the same database.
    let store = Arc::new(TaskStore::open(&crate::store::default_db_path().await?).await?);

    // Load persisted model cooldowns and register store for future writes.
    // Runs once here so the global cooldown store is not overwritten per project.
    crate::engine::cooldown::init_cooldown_store(store.clone()).await;

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
            store: store.clone(),
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

async fn reconcile_startup_worktrees(project_engines: &[ProjectEngine]) -> anyhow::Result<()> {
    for engine in project_engines {
        let repo_root =
            crate::engine::runner::worktree::resolve_main_repo(&engine.project_dir).await;
        let repo_root_path = std::path::PathBuf::from(&repo_root);
        let default_branch =
            crate::engine::runner::worktree::detect_default_branch(&repo_root_path).await;
        let worktrees = list_project_worktrees(&repo_root_path).await?;

        // Fetch refs once for all worktrees in this project, so rebase below
        // operates on up-to-date origin/* refs without N sequential fetches.
        if let Err(e) = Command::new("git")
            .args(["-C", &repo_root.to_string_lossy(), "fetch", "origin"])
            .output_with_context()
            .await
        {
            tracing::warn!(repo = %engine.repo, err = %e, "startup git fetch origin failed, rebases may use stale refs");
        }

        let mut orphans_removed: u32 = 0;
        let mut invalid_removed: u32 = 0;
        let mut cleaned: u32 = 0;
        let mut valid_kept: u32 = 0;

        for worktree_dir in worktrees {
            let Some(name) = worktree_dir.file_name().and_then(|n| n.to_str()) else {
                continue;
            };

            // Validate gitdir link before doing anything else — a broken .git file
            // means git commands on this worktree will fail with "not a git repository".
            if !validate_worktree_gitdir(&worktree_dir).await {
                tracing::warn!(repo = %engine.repo, worktree = %worktree_dir.display(), "worktree has invalid or missing gitdir, removing");
                remove_worktree_and_branch(name, &worktree_dir, Some(name), &repo_root_path, false)
                    .await;
                invalid_removed += 1;
                continue;
            }

            let Some(task_id) = task_id_from_worktree_name(name) else {
                tracing::info!(repo = %engine.repo, worktree = %worktree_dir.display(), "orphan worktree without task id, removing");
                remove_worktree_and_branch(name, &worktree_dir, Some(name), &repo_root_path, false)
                    .await;
                orphans_removed += 1;
                continue;
            };

            let Some(store_id) = engine.store.resolve_task_id(&engine.repo, &task_id).await? else {
                tracing::info!(repo = %engine.repo, task_id = %task_id, worktree = %worktree_dir.display(), "worktree has no matching task, removing");
                remove_worktree_and_branch(
                    &task_id,
                    &worktree_dir,
                    Some(name),
                    &repo_root_path,
                    false,
                )
                .await;
                orphans_removed += 1;
                continue;
            };

            let task = engine.store.get(store_id).await?;
            let branch_name = if task.branch.is_empty() {
                name
            } else {
                task.branch.as_str()
            };

            match task.status {
                TaskStatus::New
                | TaskStatus::Routed
                | TaskStatus::InProgress
                | TaskStatus::NeedsReview
                | TaskStatus::InReview => {
                    tracing::info!(repo = %engine.repo, task_id = %task_id, worktree = %worktree_dir.display(), "rebasing startup worktree");
                    if let Err(e) =
                        rebase_worktree_on_origin_main(&worktree_dir, &default_branch).await
                    {
                        // If rebase failed we already made efforts to stash and
                        // restore changes safely. Log the error and reset the
                        // task as before. The worktree removal path is used
                        // to avoid leaving corrupted worktrees around.
                        tracing::warn!(repo = %engine.repo, task_id = %task_id, err = %e, "startup rebase failed, resetting task");
                        abort_worktree_rebase(&worktree_dir).await;
                        let keep_remote = task.pr_number.is_some();
                        remove_worktree_and_branch(
                            &task_id,
                            &worktree_dir,
                            Some(branch_name),
                            &repo_root_path,
                            keep_remote,
                        )
                        .await;
                        match engine.store.resolve_task_id(&engine.repo, &task_id).await {
                            Ok(Some(reset_id)) => {
                                if let Err(e) = engine.store.reset_to_new(reset_id).await {
                                    tracing::warn!(task_id, err = %e, "failed to reset task to new after rebase failure");
                                }
                            }
                            Ok(None) => {
                                tracing::warn!(
                                    task_id,
                                    "task not found in store during reconciliation"
                                );
                            }
                            Err(e) => {
                                tracing::warn!(task_id, err = %e, "failed to resolve task ID during reconciliation");
                            }
                        }
                        if let Err(e) = engine
                            .task_manager
                            .update_task_status(
                                &ExternalId(task_id.clone()),
                                crate::backends::Status::New,
                            )
                            .await
                        {
                            tracing::warn!(task_id, err = %e, "failed to update backend status during reconciliation");
                        }
                        cleaned += 1;
                    } else {
                        valid_kept += 1;
                    }
                }
                _ => {
                    let keep_remote =
                        task.status == TaskStatus::Blocked && task.pr_number.is_some();
                    tracing::info!(repo = %engine.repo, task_id = %task_id, worktree = %worktree_dir.display(), status = ?task.status, "terminal task worktree, removing");
                    remove_worktree_and_branch(
                        &task_id,
                        &worktree_dir,
                        Some(branch_name),
                        &repo_root_path,
                        keep_remote,
                    )
                    .await;
                    cleaned += 1;
                }
            }
        }

        tracing::info!(
            repo = %engine.repo,
            orphans_removed,
            invalid_removed,
            cleaned,
            valid_kept,
            "worktree reconciliation: {orphans_removed} orphans removed, {invalid_removed} invalid removed, {cleaned} cleaned, {valid_kept} valid kept"
        );

        // Prune stale .git/worktrees/ entries whose directories no longer exist.
        // This handles entries left behind by crashes or manual directory removal.
        let prune = Command::new("git")
            .args(["-C", &repo_root.to_string_lossy(), "worktree", "prune"])
            .output_with_context()
            .await;
        match prune {
            Ok(output) if output.status.success() => {
                tracing::debug!(repo = %engine.repo, "startup worktree prune completed");
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::warn!(repo = %engine.repo, err = %stderr, "startup worktree prune failed");
            }
            Err(e) => {
                tracing::warn!(repo = %engine.repo, err = %e, "startup worktree prune failed");
            }
        }
    }

    Ok(())
}

/// Sync Fibonacci estimates stored in SQLite to the GitHub Projects board for
/// all tasks that have a positive estimate.
///
/// Runs once on startup to catch tasks that were routed before the estimate
/// sync feature was available or before `project_estimate_field_id` was
/// configured. The underlying mutations are idempotent, so re-syncing an
/// already-correct estimate is harmless.
async fn reconcile_startup_estimates(project_engines: &[ProjectEngine]) {
    use crate::backends::ExternalId;

    for engine in project_engines {
        let tasks = match engine.store.list_external_tasks_with_estimates().await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(repo = %engine.repo, err = %e, "startup estimate reconciliation: store query failed");
                continue;
            }
        };

        if tasks.is_empty() {
            continue;
        }

        let mut synced: u32 = 0;
        let mut failed: u32 = 0;
        for (external_id, estimate) in &tasks {
            match engine
                .backend
                .sync_estimate_to_project(&ExternalId(external_id.clone()), *estimate)
                .await
            {
                Ok(()) => synced += 1,
                Err(e) => {
                    tracing::debug!(
                        repo = %engine.repo,
                        task_id = %external_id,
                        estimate,
                        err = %e,
                        "startup estimate reconciliation: sync failed"
                    );
                    failed += 1;
                }
            }
        }

        if synced > 0 || failed > 0 {
            tracing::info!(
                repo = %engine.repo,
                synced,
                failed,
                "startup estimate reconciliation complete"
            );
        }
    }
}

/// Read per-project channel configuration from `.orch.yml`.
fn read_project_channel_config(project_dir: &std::path::Path) -> ProjectChannelConfig {
    let config_path = project_dir.join(".orch.yml");
    if !config_path.exists() {
        return ProjectChannelConfig::default();
    }
    let content = match std::fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                path = %config_path.display(),
                error = %e,
                "failed to read .orch.yml; channel config will use defaults"
            );
            return ProjectChannelConfig::default();
        }
    };
    let val: serde_norway::Value = match serde_norway::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                path = %config_path.display(),
                error = %e,
                "failed to parse .orch.yml; channel config will use defaults"
            );
            return ProjectChannelConfig::default();
        }
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

/// Fetch the latest orch release tag from GitHub via the `gh` CLI (async).
///
/// Returns the version string (without the leading "v"), or `None` when the
/// check fails (no network, unauthenticated, `gh` not installed, etc.).
async fn fetch_latest_release_version() -> Option<String> {
    let output = Command::new("gh")
        .args([
            "api",
            "repos/gabrielkoerich/orch/releases/latest",
            "--jq",
            ".tag_name",
        ])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let tag = String::from_utf8(output.stdout).ok()?;
    let version = tag.trim().trim_start_matches('v').to_string();
    if version.is_empty() {
        None
    } else {
        Some(version)
    }
}

/// Run `brew upgrade orch` then send SIGTERM to the current process so launchd
/// restarts the service with the new binary. The signal is sent only after the
/// brew command succeeds; on failure, nothing is restarted and the next check
/// will try again.
///
/// Returns `true` if brew succeeded and SIGTERM was sent, `false` on any failure.
async fn perform_auto_upgrade(latest: &str) -> bool {
    tracing::info!(latest = %latest, "auto-upgrading orch via brew");

    let status = Command::new("brew")
        .args(["upgrade", "orch"])
        .status()
        .await;

    match status {
        Ok(s) if s.success() => {
            tracing::info!(latest = %latest, "brew upgrade orch succeeded — restarting service");
            // Send SIGTERM to self; launchd / brew services will restart the process
            // with the newly installed binary.
            let pid = std::process::id().to_string();
            let _ = Command::new("kill").args(["-TERM", &pid]).status().await;
            true
        }
        Ok(s) => {
            tracing::warn!(exit_code = ?s.code(), "brew upgrade orch failed — will retry at next check");
            false
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to spawn brew upgrade orch");
            false
        }
    }
}

/// Check if a newer orch release is available. When `auto_upgrade` is true,
/// runs `brew upgrade orch` and restarts the service. Otherwise sends channel
/// notifications asking the operator to upgrade manually.
///
/// Uses the store's KV to throttle checks (`upgrade:last_check_at`) and
/// deduplicate notifications (`upgrade:last_notified_version`). Each distinct
/// latest version is notified about once; the notification repeats only when
/// an even newer version is released.
async fn check_and_notify_upgrade(
    store: &Arc<TaskStore>,
    channels: &Arc<ChannelRegistry>,
    current_version: &str,
    check_interval: std::time::Duration,
    auto_upgrade: bool,
) {
    let last_check_key = "upgrade:last_check_at";
    let last_notified_key = "upgrade:last_notified_version";

    // Throttle: only check once per interval.
    let now = chrono::Utc::now().timestamp();
    if let Ok(Some(last_check_str)) = store.kv_get(last_check_key).await {
        if let Ok(last_check) = last_check_str.parse::<i64>() {
            if now - last_check < check_interval.as_secs() as i64 {
                return;
            }
        }
    }

    // Record check timestamp (best-effort).
    let _ = store.kv_set(last_check_key, &now.to_string()).await;

    let latest = match fetch_latest_release_version().await {
        Some(v) => v,
        None => {
            tracing::debug!("upgrade check: failed to fetch latest release version");
            return;
        }
    };

    if latest == current_version {
        tracing::debug!(current = %current_version, "orch is up to date");
        return;
    }

    tracing::warn!(
        current_version = %current_version,
        latest_version = %latest,
        "orch upgrade available"
    );

    // When auto_upgrade is enabled, upgrade via brew and restart — no manual
    // channel notification needed since the restart is self-evident.
    if auto_upgrade {
        // Deduplicate: don't re-trigger brew upgrade for the same latest version
        // unless a previous attempt failed (in which case the key won't be set).
        if let Ok(Some(last_notified)) = store.kv_get(last_notified_key).await {
            if last_notified == latest {
                tracing::debug!(latest = %latest, "auto-upgrade already attempted for this version");
                return;
            }
        }
        // Mark before attempting so a crash/restart mid-upgrade doesn't cause a rapid
        // retry loop. On explicit failure the key is cleared below so the next hourly
        // check can retry.
        let _ = store.kv_set(last_notified_key, &latest).await;
        if !perform_auto_upgrade(&latest).await {
            // Brew failed — clear the dedup key so the next check cycle will retry.
            let _ = store.kv_delete(last_notified_key).await;
        }
        return;
    }

    // Manual upgrade path: send channel notifications.

    // Deduplicate: don't re-notify for the same latest version.
    if let Ok(Some(last_notified)) = store.kv_get(last_notified_key).await {
        if last_notified == latest {
            tracing::debug!(latest = %latest, "upgrade already notified");
            return;
        }
    }

    let msg_telegram = format!(
        "⚠️ <b>Orch Upgrade Available</b>\n\n\
         Service: <code>{current_version}</code>\n\
         Latest:  <code>{latest}</code>\n\n\
         Run: <code>brew update && brew upgrade orch && brew services restart orch</code>"
    );

    let msg_discord = format!(
        "⚠️ **Orch Upgrade Available**\n\n\
         Service: `{current_version}`\n\
         Latest:  `{latest}`\n\n\
         Run: `brew update && brew upgrade orch && brew services restart orch`"
    );

    let msg_slack = format!(
        "⚠️ *Orch Upgrade Available*\n\n\
         Service: `{current_version}`\n\
         Latest:  `{latest}`\n\n\
         Run: `brew update && brew upgrade orch && brew services restart orch`"
    );

    for channel in channels.iter() {
        let (body, metadata, topic_id) = match channel.name() {
            "telegram" => (
                msg_telegram.clone(),
                serde_json::json!({"preformatted_html": true}),
                None,
            ),
            "discord" => (msg_discord.clone(), serde_json::json!({}), None),
            "slack" => (msg_slack.clone(), serde_json::json!({}), None),
            _ => continue,
        };

        let msg = OutgoingMessage {
            thread_id: "upgrade".to_string(),
            body,
            reply_to: None,
            metadata,
            topic_id,
        };

        if let Err(e) = channel.send(&msg).await {
            tracing::warn!(
                channel = channel.name(),
                ?e,
                "failed to send upgrade notification"
            );
        }
    }

    // Record that we notified for this latest version.
    let _ = store.kv_set(last_notified_key, &latest).await;
}

/// Start the orch service.
///
/// This is the main entry point — called by `orch serve`.
pub async fn serve() -> anyhow::Result<()> {
    tracing::info!("orch engine starting");

    // Pre-create standard directories with async I/O so subsequent synchronous
    // callers (state_dir, orch_home, etc.) find them already present and their
    // create_dir_all calls become fast no-ops.
    if let Err(e) = crate::home::init_dirs().await {
        tracing::warn!("failed to initialize orch directories: {e}");
    }

    // Pre-warm the config cache with async I/O so get() / get_list() calls in
    // hot async paths never block a Tokio thread on std::fs::read_to_string.
    if let Err(e) = crate::config::warm_cache().await {
        tracing::warn!("failed to warm config cache: {e}");
    }

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
                    tracing::warn!(
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

    // Create the event bus for task status transitions
    let event_bus = events::EventBus::new(256);
    if let Err(e) = event_bus.start_ws_server().await {
        tracing::warn!(
            ?e,
            "failed to start event websocket server, continuing without it"
        );
    }

    // Check for a newer orch release in the background and warn if the service is behind.
    // Channels are not yet registered at this point, so this only logs. Channel notifications
    // are sent by the periodic check in the main loop once channels are up.
    {
        let current = env!("ORCH_VERSION").to_string();
        tokio::spawn(async move {
            if let Some(latest) = fetch_latest_release_version().await {
                if latest != current {
                    tracing::warn!(
                        current_version = %current,
                        latest_version = %latest,
                        "orch service is behind the latest release — run: brew update && brew upgrade orch && brew services restart orch"
                    );
                }
            }
        });
    }

    // Re-create task managers with shared store and event bus
    for engine in &mut project_engines {
        engine.task_manager = Arc::new(TaskManager::with_events(
            engine.backend.clone(),
            engine.store.clone(),
            engine.repo.clone(),
            event_bus.sender(),
        ));
    }

    if let Err(e) = reconcile_startup_worktrees(&project_engines).await {
        tracing::warn!(err = %e, "startup worktree reconciliation failed");
    }

    // Clear incremental ingest cursors so the first sync after startup re-scans the last 24h.
    // This catches issues created during engine downtime that would otherwise be permanently
    // skipped (GitHub's `since` filter uses updated_at, not created_at).
    for engine in &project_engines {
        crate::engine::sync::clear_issues_last_ingested(&engine.store, &engine.repo).await;
    }

    {
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
        telegram_general_topic_id: crate::config::get("channels.telegram.general_topic_id").ok(),
        discord_general_channel_id: crate::config::get("channels.discord.general_channel_id").ok(),
        control_telegram_topic_id: crate::config::get("control.channels.telegram.topic_id").ok(),
        control_discord_channel_id: crate::config::get("control.channels.discord.channel_id").ok(),
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
            match TelegramChannel::new(token, chat_id) {
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
            match DiscordGateway::new(token, channel_id, shard_id, shard_count) {
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
    }

    // Try to initialize Slack channel
    if let Ok(token) = crate::config::get("channels.slack.bot_token") {
        if !token.is_empty() {
            let channel_id = crate::config::get("channels.slack.channel_id").ok();
            match SlackChannel::new(token, channel_id) {
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
    // Shared in-memory map for pending project picks (General channel multi-project picker).
    let pending_picks: channel_handler::PendingPicks =
        std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
    let router_for_messages = channel_router.clone();
    for mut rx in channel_receivers {
        let transport = transport_for_messages.clone();
        let tmux = tmux_for_messages.clone();
        let capture = capture_for_messages.clone();
        let channels = channels_for_messages.clone();
        let engine_refs = engine_refs.clone();
        let ch_router = router_for_messages.clone();
        let picks = pending_picks.clone();
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
                                if let Err(e) = channel.send(&msg).await {
                                    tracing::warn!(
                                        task_id = %notification.task_id,
                                        target,
                                        ?e,
                                        "failed to send to notify_target"
                                    );
                                } else {
                                    routed = true;
                                }
                            }
                        }

                        if routed {
                            // notify_target handled — skip further routing
                        } else if let Some(repo) = notification.repo.as_deref() {
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
                                    let msg = OutgoingMessage {
                                        thread_id: notification.task_id.clone(),
                                        body,
                                        reply_to: None,
                                        metadata,
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
                                        for (ch_name, thread_id, topic_id) in subscribers {
                                            let channel =
                                                channels.iter().find(|c| c.name() == ch_name);
                                            let Some(channel) = channel else {
                                                continue;
                                            };
                                            let body = notification.format_with_project(&ch_name);
                                            let metadata = if ch_name == "telegram" {
                                                serde_json::json!({"preformatted_html": true})
                                            } else {
                                                serde_json::json!({})
                                            };
                                            let resolved_topic = if topic_id.is_empty() {
                                                None
                                            } else {
                                                Some(topic_id.clone())
                                            };
                                            let msg = OutgoingMessage {
                                                thread_id: thread_id.clone(),
                                                body,
                                                reply_to: None,
                                                metadata,
                                                topic_id: resolved_topic,
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
    let webhook_status = Arc::new(tokio::sync::Mutex::new(
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
        let to_save = {
            let mut s = webhook_status.lock().await;
            s.configured = true;
            s.port = Some(port);
            s.healthy = false;
            s.fallback_mode = false;
            s.clone()
        };
        to_save.save().await;

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

    // Agent router (selects agent + model per task) - shared across projects.
    // Router::from_config() may run `opencode models` (a blocking subprocess) to
    // discover free models. Wrapping in spawn_blocking keeps the Tokio runtime
    // thread free during that I/O.
    // NOTE: Router::new() also calls prime_free_model_cache() (another blocking
    // subprocess), so the fallback must also run inside spawn_blocking.
    let router = Arc::new(RwLock::new(
        match tokio::task::spawn_blocking(Router::from_config).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(?e, "router init panicked in spawn_blocking, using default");
                tokio::task::spawn_blocking(|| Router::new(RouterConfig::default()))
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

    // Concurrency limiter (shared across all projects)
    let semaphore = Arc::new(Semaphore::new(config.max_parallel));

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
    let mut config_rx = crate::config::subscribe();

    // Track sync interval
    let mut last_sync = std::time::Instant::now();

    // Track upgrade check interval
    let mut last_upgrade_check = std::time::Instant::now();

    // Channel for weight signals from task runners back to the router
    let (weight_tx, mut weight_rx) = mpsc::channel::<WeightSignal>(64);

    // Reset InReview tasks on startup. A task is reset to NeedsReview if:
    // 1. It was expecting a live review session (review_session_expected=true), OR
    // 2. It has been in InReview for >10 minutes (catches cases where the flag
    //    was never set due to crash-loops or lost events).
    // Tasks with a recent review comment on their PR are left alone.
    for engine in &project_engines {
        // Read from SQLite (source of truth) — GitHub labels may be out of sync.
        let in_review_from_store = engine
            .store
            .list_by_status(&engine.repo, crate::store::TaskStatus::InReview)
            .await
            .unwrap_or_default()
            .iter()
            .map(crate::engine::tasks::store_task_to_external)
            .collect::<Vec<_>>();
        if !in_review_from_store.is_empty() {
            let in_review = &in_review_from_store;
            for task in in_review {
                let session_expected =
                    review_session_expected(&engine.store, &engine.repo, &task.id.0).await;
                let age_minutes = chrono::DateTime::parse_from_rfc3339(&task.updated_at)
                    .map(|dt| (chrono::Utc::now() - dt.with_timezone(&chrono::Utc)).num_minutes())
                    .unwrap_or(i64::MAX);
                if !session_expected && age_minutes < 10 {
                    continue;
                }
                if let Err(e) = engine
                    .task_manager
                    .update_task_status(&task.id, Status::NeedsReview)
                    .await
                {
                    tracing::warn!(
                        task_id = task.id.0,
                        err = %e,
                        "failed to reset stale InReview task on startup"
                    );
                } else {
                    set_review_session_expected(&engine.store, &engine.repo, &task.id.0, false)
                        .await;
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
        // Note: the block above already covers internal tasks from the store,
        // but this second pass catches any that were only in the old internal_tasks table.
        use crate::store::TaskStatus as DbStatus;
        if let Ok(internal_in_review) = engine
            .task_manager
            .list_internal_by_status(DbStatus::InReview)
            .await
        {
            for task in &internal_in_review {
                let task_id = task.id.0.clone();
                let session_expected =
                    review_session_expected(&engine.store, &engine.repo, &task_id).await;
                let age_minutes = chrono::DateTime::parse_from_rfc3339(&task.updated_at)
                    .map(|dt| (chrono::Utc::now() - dt.with_timezone(&chrono::Utc)).num_minutes())
                    .unwrap_or(i64::MAX);
                if !session_expected && age_minutes < 10 {
                    continue;
                }
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
                } else {
                    set_review_session_expected(&engine.store, &engine.repo, &task_id, false).await;
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

    // Spawn dispatch subscribers — react to Routed events immediately
    for engine in &project_engines {
        subscribers::dispatch::spawn(
            event_bus.subscribe(),
            engine.backend.clone(),
            tmux.clone(),
            engine.runner.clone(),
            capture_for_tick.clone(),
            semaphore.clone(),
            engine.task_manager.clone(),
            weight_tx.clone(),
            router.clone(),
            dispatching.clone(),
            engine.store.clone(),
            engine.repo.clone(),
        );
    }
    tracing::info!("dispatch subscriber started");

    // Spawn review subscribers — react to NeedsReview events immediately
    for engine in &project_engines {
        subscribers::review::spawn(
            event_bus.subscribe(),
            engine.backend.clone(),
            tmux.clone(),
            semaphore.clone(),
            engine.task_manager.clone(),
            router.clone(),
            dispatching.clone(),
            engine.store.clone(),
            engine.repo.clone(),
        );
    }
    tracing::info!("review subscriber started");

    // Spawn unblock subscribers — react to Done events, unblock parent tasks immediately
    for engine in &project_engines {
        subscribers::unblock::spawn(
            event_bus.subscribe(),
            engine.backend.clone(),
            engine.task_manager.clone(),
            engine.store.clone(),
            engine.repo.clone(),
        );
    }
    tracing::info!("unblock subscriber started");

    // Spawn notify subscriber — reacts to ALL events, pushes to transport.
    // Spawned once (not per-project) since the transport handles all repos.
    subscribers::notify::spawn(event_bus.subscribe(), transport.clone());
    tracing::info!("notify subscriber started");

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

    // Guard to prevent concurrent sync_tick runs (sync is spawned as a background task).
    let sync_in_progress = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Tick watchdog: detects when the main loop hasn't completed a tick in > 60s.
    // Updates an atomic timestamp at the end of each tick iteration.
    let last_tick_epoch = Arc::new(std::sync::atomic::AtomicU64::new(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    ));
    {
        let last_tick_epoch = Arc::clone(&last_tick_epoch);
        let tick_interval_secs = config.tick_interval.as_secs();
        tokio::spawn(async move {
            // Check every 30s whether the main loop is still ticking.
            let mut watchdog_interval = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                watchdog_interval.tick().await;
                let last = last_tick_epoch.load(std::sync::atomic::Ordering::Relaxed);
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let stale_secs = now.saturating_sub(last);
                // Warn if the tick loop hasn't completed in > 6× the tick interval (at least 60s).
                let threshold = (tick_interval_secs * 6).max(60);
                if stale_secs > threshold {
                    tracing::error!(
                        stale_secs,
                        threshold,
                        "WATCHDOG: tick loop has not completed a tick in {}s (threshold {}s) — possible stall",
                        stale_secs,
                        threshold,
                    );
                }
            }
        });
    }

    loop {
        tokio::select! {
            _ = interval.tick() => {
                let tick_start = std::time::Instant::now();

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
                        five_xx_hits = m.five_xx_hits,
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
                                &engine.task_manager,
                                &weight_tx,
                                &dispatching,
                                &engine.store,
                                Some(&transport),
                            ).await {
                                tracing::error!(repo = %engine.repo, ?e, "tick failed for project");
                            }
                        }).await;
                    }
                    drop(router_guard);

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
                            WeightSignal::Blocked | WeightSignal::None => {}
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
                            std::sync::atomic::Ordering::AcqRel,
                            std::sync::atomic::Ordering::Relaxed,
                        ).is_ok() {
                            // Clone everything sync_tick needs — it runs in a spawned task.
                            let sync_engines: Vec<_> = project_engines.iter().map(|e| {
                                (e.backend.clone(), e.repo.clone(),
                                 e.task_manager.clone(), e.store.clone())
                            }).collect();
                            let sync_tmux = Arc::clone(&tmux);
                            let sync_config = config.clone();
                            let sync_router = Arc::clone(&router);
                            let sync_dispatching = Arc::clone(&dispatching);
                            let sync_auto_merge = Arc::clone(&auto_merge_in_flight);
                            let sync_guard = Arc::clone(&sync_in_progress);
                            tokio::spawn(async move {
                                let sync_start = std::time::Instant::now();
                                for (backend, repo, task_manager, store) in &sync_engines {
                                    let repo = repo.clone();
                                    REPO_CONTEXT.scope(repo.clone(), async {
                                        if let Err(e) = sync::sync_tick(
                                            backend, &sync_tmux, &repo, &sync_config,
                                            &sync_router, task_manager, store,
                                            &sync_dispatching, &sync_auto_merge,
                                        ).await {
                                            tracing::error!(repo = %repo, ?e, "sync tick failed for project");
                                        }
                                    }).await;
                                }
                                // Emit degraded-agents metric/log once per sync cycle.
                                // Clone the needed data before releasing the read guard so
                                // the lock is not held across the async store writes.
                                if let Some((_, _, _, store)) = sync_engines.first() {
                                    let (available_agents, config) = {
                                        let r = sync_router.read().await;
                                        (r.available_agents.clone(), r.config.clone())
                                    };
                                    sync::emit_degraded_agents_if_needed(
                                        &available_agents,
                                        &config,
                                        Some(store),
                                    )
                                    .await;
                                }
                                let elapsed = sync_start.elapsed();
                                tracing::info!(elapsed_ms = elapsed.as_millis() as u64, "sync tick complete");
                                sync_guard.store(false, std::sync::atomic::Ordering::Release);
                            });
                        } else {
                            tracing::debug!("sync tick still in progress, skipping");
                        }
                        // Records sync *schedule* time (not completion). This prevents
                        // re-entering the sync branch every tick while one is in flight.
                        // The sync_in_progress guard handles actual concurrency control.
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
                        last_webhook_health_check = std::time::Instant::now();
                    }
                }

                // Periodic upgrade check — notify channels when a newer release is available.
                if config.upgrade_check_interval > 0
                    && last_upgrade_check.elapsed()
                        >= std::time::Duration::from_secs(config.upgrade_check_interval)
                {
                    if let Some(store) = project_engines.first().map(|e| e.store.clone()) {
                        let channels = channel_registry.clone();
                        let current = env!("ORCH_VERSION").to_string();
                        let check_interval =
                            std::time::Duration::from_secs(config.upgrade_check_interval);
                        let auto_upgrade = config.auto_upgrade;
                        tokio::spawn(async move {
                            check_and_notify_upgrade(
                                &store,
                                &channels,
                                &current,
                                check_interval,
                                auto_upgrade,
                            )
                            .await;
                        });
                    }
                    last_upgrade_check = std::time::Instant::now();
                }

                // Update watchdog timestamp + log tick duration.
                let tick_elapsed = tick_start.elapsed();
                if tick_elapsed.as_secs() > 30 {
                    tracing::warn!(elapsed_ms = tick_elapsed.as_millis() as u64, "slow tick");
                } else {
                    tracing::debug!(elapsed_ms = tick_elapsed.as_millis() as u64, "tick complete");
                }
                last_tick_epoch.store(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    std::sync::atomic::Ordering::Relaxed,
                );
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
                                &engine.task_manager,
                                &weight_tx,
                                &dispatching,
                                &engine.store,
                                Some(&transport),
                            ).await {
                                tracing::error!(repo = %engine.repo, ?e, "webhook-triggered tick failed");
                            }
                        }).await;
                    }
                    drop(router_guard);

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
                        WeightSignal::Blocked | WeightSignal::None => {}
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
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Relaxed,
                ).is_ok() {
                    // Clone everything sync_tick needs — it runs in a spawned task.
                    let sync_engines: Vec<_> = project_engines.iter().map(|e| {
                        (e.backend.clone(), e.repo.clone(), e.task_manager.clone(), e.store.clone())
                    }).collect();
                    let sync_tmux = Arc::clone(&tmux);
                    let sync_config = config.clone();
                    let sync_router = Arc::clone(&router);
                    let sync_dispatching = Arc::clone(&dispatching);
                    let sync_auto_merge = Arc::clone(&auto_merge_in_flight);
                    let sync_guard = Arc::clone(&sync_in_progress);
                    tokio::spawn(async move {
                        let sync_start = std::time::Instant::now();
                        for (backend, repo, task_manager, store) in &sync_engines {
                            let repo = repo.clone();
                            REPO_CONTEXT.scope(repo.clone(), async {
                                if let Err(e) = sync::sync_tick(
                                    backend, &sync_tmux, &repo, &sync_config,
                                    &sync_router, task_manager, store,
                                    &sync_dispatching, &sync_auto_merge,
                                ).await {
                                    tracing::error!(repo = %repo, ?e, "webhook immediate sync tick failed for project");
                                }
                            }).await;
                        }
                        // Emit degraded-agents metric/log once per sync cycle.
                        if let Some((_, _, _, store)) = sync_engines.first() {
                            let (available_agents, config) = {
                                let r = sync_router.read().await;
                                (r.available_agents.clone(), r.config.clone())
                            };
                            sync::emit_degraded_agents_if_needed(
                                &available_agents,
                                &config,
                                Some(store),
                            ).await;
                        }
                        let elapsed = sync_start.elapsed();
                        tracing::info!(elapsed_ms = elapsed.as_millis() as u64, "webhook immediate sync tick complete");
                        sync_guard.store(false, std::sync::atomic::Ordering::Release);
                    });
                    // Record schedule time so periodic sync doesn't immediately re-run.
                    last_sync = std::time::Instant::now();
                } else {
                    tracing::debug!("sync tick still in progress, skipping webhook-triggered immediate sync");
                }
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

                        // Reload router config (async-safe: use reload_async to avoid blocking the runtime)
                        {
                            let mut router_guard = router.write().await;
                            router_guard.reload_async().await;
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
                            router_guard.reload_async().await;
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

                // Wrap the entire shutdown sequence in a timeout to enforce the configured limit.
                // This prevents indefinite blocking if status updates or tmux operations hang.
                let shutdown_result = tokio::time::timeout(
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
                                        {
                                            if let Err(e) = engine
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
                                        }
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
                                        set_review_session_expected(&engine.store, &engine.repo, &task.id.0, false).await;
                                        if let Ok(Some(store_id)) =
                                            engine.store.resolve_task_id(&engine.repo, &task.id.0).await
                                        {
                                            if let Err(e) = engine
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
                                        }
                                        review_reset_count += 1;
                                    }
                                }
                            }
                            // Also reset internal in_review tasks
                            if let Ok(tasks) = engine.store.list_internal_by_status(&engine.repo, crate::store::TaskStatus::InReview).await {
                                for task in &tasks {
                                    let task_id = format!("internal:{}", task.id);
                                    if let Err(e) = engine.task_manager.update_task_status(
                                        &crate::backends::ExternalId(task_id.clone()),
                                        Status::NeedsReview,
                                    ).await {
                                        tracing::warn!(task_id, ?e, "failed to reset internal in_review task on shutdown");
                                    } else {
                                        set_review_session_expected(&engine.store, &engine.repo, &task_id, false).await;
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
                                    if let Err(e) = tmux.kill_session(&session.name).await {
                                        tracing::warn!(session = %session.name, error = %e, "failed to kill tmux session");
                                    } else {
                                        killed_sessions.push(session.name.clone());
                                    }
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
                                    std::time::Duration::from_millis(100),
                                    std::time::Duration::from_secs(5),
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
                        timeout_secs = config.graceful_shutdown_timeout.as_secs(),
                        "graceful shutdown timed out, forcing exit"
                    );
                }

                break;
            }
        }
    }

    // Clean up event bus port file and service version file
    events::cleanup_port_file().await;
    events::cleanup_version_file();

    // transport and channels drop here at end of scope
    let _ = transport;
    tracing::info!("orch engine stopped");
    Ok(())
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
        assert_eq!(config.upgrade_check_interval, 3600);
        assert!(config.auto_upgrade, "auto_upgrade default must be true");
    }

    #[test]
    fn engine_config_from_config_uses_defaults_when_no_config() {
        let config = EngineConfig::from_config();
        assert_eq!(config.tick_interval, std::time::Duration::from_secs(10));
        assert_eq!(config.sync_interval, std::time::Duration::from_secs(45));
        // max_parallel may be overridden by user config; just check it's reasonable
        assert!(config.max_parallel >= 1 && config.max_parallel <= 64);
        assert_eq!(config.stuck_timeout, 1800);
        assert_eq!(config.no_session_stuck_timeout, 600);
        assert_eq!(
            config.graceful_shutdown_timeout,
            std::time::Duration::from_secs(600)
        );
        assert_eq!(config.upgrade_check_interval, 3600);
        // auto_upgrade may be overridden by user config; default is true
        // (user config may disable it, so only check default struct)
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

    #[test]
    fn configured_agents_parses_yaml_block_array() {
        // config::get returns serde_norway::to_string for arrays, which is YAML block format
        let yaml_str = "- claude\n- codex\n- opencode\n- olm\n";
        let parsed: Vec<String> = serde_norway::from_str(yaml_str).unwrap();
        assert_eq!(parsed, vec!["claude", "codex", "opencode", "olm"]);
    }

    #[test]
    fn configured_agents_parses_json_inline_array() {
        let json_str = r#"["claude","codex","olm"]"#;
        let parsed: Vec<String> = serde_json::from_str(json_str).unwrap();
        assert_eq!(parsed, vec!["claude", "codex", "olm"]);
    }

    #[test]
    fn configured_agents_fallback_returns_default_agents() {
        // When no config is present, configured_agents() returns DEFAULT_AGENTS
        let defaults = configured_agents();
        assert!(defaults.contains(&"claude".to_string()));
        assert!(defaults.contains(&"codex".to_string()));
        assert!(defaults.contains(&"opencode".to_string()));
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

        // Guard ensures session cleanup even on panic
        struct SessionGuard(String);
        impl Drop for SessionGuard {
            fn drop(&mut self) {
                let _ = std::process::Command::new("tmux")
                    .args(["kill-session", "-t", &self.0])
                    .output();
            }
        }
        let _guard = SessionGuard(session_name.clone());

        // Register session with capture service and transport binding
        capture
            .register_session("owner/repo", &task_id, &session_name)
            .await;
        transport
            .bind(
                "owner/repo",
                &task_id,
                &session_name,
                "telegram",
                "12345",
                None,
            )
            .await;

        // Spawn capture.run() which exits when session is unregistered
        let capture_clone = capture.clone();
        let capture_handle = tokio::spawn(async move { capture_clone.run().await });

        // Subscribe to transport output for this task
        let mut rx = transport
            .subscribe("owner/repo", &task_id)
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

        // Clean up: unregister session (tmux kill handled by SessionGuard on drop)
        capture.unregister_session("owner/repo", &task_id).await;

        // Wait for capture.run to finish
        let _ = capture_handle.await;

        assert!(got, "did not observe tmux output via capture/transport");
    }

    // ── read_project_channel_config tests ─────────────────────────────────

    #[test]
    fn read_project_channel_config_returns_defaults_when_no_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = read_project_channel_config(dir.path());
        assert!(cfg.telegram_topic_id.is_none());
        assert!(cfg.discord_channel_id.is_none());
    }

    #[test]
    fn read_project_channel_config_parses_telegram_and_discord() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orch_yml = dir.path().join(".orch.yml");
        std::fs::write(
            &orch_yml,
            "channels:\n  telegram:\n    topic_id: \"42\"\n  discord:\n    channel_id: \"9999\"\n",
        )
        .expect("write .orch.yml");
        let cfg = read_project_channel_config(dir.path());
        assert_eq!(cfg.telegram_topic_id.as_deref(), Some("42"));
        assert_eq!(cfg.discord_channel_id.as_deref(), Some("9999"));
    }

    #[test]
    fn read_project_channel_config_handles_partial_channels() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orch_yml = dir.path().join(".orch.yml");
        std::fs::write(&orch_yml, "channels:\n  telegram:\n    topic_id: \"7\"\n")
            .expect("write .orch.yml");
        let cfg = read_project_channel_config(dir.path());
        assert_eq!(cfg.telegram_topic_id.as_deref(), Some("7"));
        assert!(cfg.discord_channel_id.is_none());
    }

    #[test]
    fn read_project_channel_config_handles_malformed_yaml() {
        let dir = tempfile::tempdir().expect("tempdir");
        let orch_yml = dir.path().join(".orch.yml");
        std::fs::write(&orch_yml, "channels: [\nbad yaml").expect("write .orch.yml");
        let cfg = read_project_channel_config(dir.path());
        // Should return defaults (not panic) and emit a warn! log with the parse error.
        assert!(cfg.telegram_topic_id.is_none());
        assert!(cfg.discord_channel_id.is_none());
    }

    #[test]
    #[cfg(unix)]
    fn read_project_channel_config_handles_unreadable_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let orch_yml = dir.path().join(".orch.yml");
        std::fs::write(&orch_yml, "channels:\n  telegram:\n    topic_id: \"1\"\n")
            .expect("write .orch.yml");
        // Make the file unreadable.
        std::fs::set_permissions(&orch_yml, std::fs::Permissions::from_mode(0o000))
            .expect("set permissions");
        let cfg = read_project_channel_config(dir.path());
        // Should return defaults (not panic) and emit a warn! log with the read error.
        assert!(cfg.telegram_topic_id.is_none());
        assert!(cfg.discord_channel_id.is_none());
        // Restore permissions so tempdir cleanup succeeds.
        std::fs::set_permissions(&orch_yml, std::fs::Permissions::from_mode(0o644))
            .expect("restore permissions");
    }
}
