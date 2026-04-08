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
/// because `config::get` serializes nested YAML arrays via `serde_yml::to_string`
/// which produces YAML block format, not JSON — breaking downstream parsing.
pub fn configured_agents() -> Vec<String> {
    if let Ok(agents_str) = crate::config::get("agents") {
        if !agents_str.is_empty() && agents_str != "[]" {
            // config::get returns serde_yml::to_string for arrays, which is
            // YAML block format ("- claude\n- codex\n..."). Parse as YAML first.
            if let Ok(agents_arr) = serde_yml::from_str::<Vec<String>>(&agents_str) {
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
    /// Grace period before silence detection kicks in (seconds).
    /// Allows time for agent startup, model download, API handshake.
    pub silence_grace_period: u64,
    /// Cooldown duration for a model detected as silent (seconds).
    pub silence_cooldown: u64,
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
            silence_grace_period: 120,
            silence_cooldown: 3600,
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
                        if let Ok(Some(reset_id)) =
                            engine.store.resolve_task_id(&engine.repo, &task_id).await
                        {
                            let _ = engine.store.reset_to_new(reset_id).await;
                        }
                        let _ = engine
                            .task_manager
                            .update_task_status(
                                &ExternalId(task_id.clone()),
                                crate::backends::Status::New,
                            )
                            .await;
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
        {
            let mut s = webhook_status.lock().await;
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
                            let mut s = status_for_spawn.lock().await;
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
                            let mut s = status_for_spawn.lock().await;
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
            let mut s = webhook_status.lock().await;
            s.configured = false;
            s.fallback_mode = true;
            s.save();
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
                        tracing::error!(?e2, "router default init also panicked in spawn_blocking");
                        Router::new(RouterConfig::default())
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
    let dispatching: Arc<dashmap::DashSet<String>> = Arc::new(dashmap::DashSet::new());

    // In-memory guard to prevent double-spawn of auto-merge background tasks.
    // Since review_open_prs runs every sync_tick (~10s) but CI polling can take
    // up to 10 minutes, we track which tasks already have an auto-merge in flight.
    let auto_merge_in_flight: Arc<dashmap::DashSet<String>> = Arc::new(dashmap::DashSet::new());

    // Subscribe to config file changes for hot reload
    let mut config_rx = crate::config::subscribe();

    // Track sync interval
    let mut last_sync = std::time::Instant::now();

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

                    // Periodic sync (less frequent)
                    if last_sync.elapsed() >= config.sync_interval {
                        for engine in &project_engines {
                            let repo = engine.repo.clone();
                            REPO_CONTEXT.scope(repo, async {
                                if let Err(e) = sync::sync_tick(&engine.backend, &tmux, &engine.repo, &config, &router, &engine.task_manager, &engine.store, &dispatching, &auto_merge_in_flight).await {
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
                                let mut s = webhook_status.lock().await;
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
                    // Reset in_review tasks — review agent sessions die on shutdown
                    if let Ok(tasks) = engine.task_manager.list_external_by_status(Status::InReview).await {
                        for task in &tasks {
                            if let Err(e) = engine.task_manager.update_task_status(&task.id, Status::NeedsReview).await {
                                tracing::warn!(task_id = task.id.0, ?e, "failed to reset in_review task on shutdown");
                            } else {
                                set_review_session_expected(&engine.store, &engine.repo, &task.id.0, false).await;
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
        // config::get returns serde_yml::to_string for arrays, which is YAML block format
        let yaml_str = "- claude\n- codex\n- opencode\n- olm\n";
        let parsed: Vec<String> = serde_yml::from_str(yaml_str).unwrap();
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

        // Clean up: unregister and kill tmux session
        capture.unregister_session("owner/repo", &task_id).await;
        let _ = tokio::process::Command::new("tmux")
            .args(["kill-session", "-t", &session_name])
            .output()
            .await;

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
        // Should silently return defaults rather than panic.
        assert!(cfg.telegram_topic_id.is_none());
        assert!(cfg.discord_channel_id.is_none());
    }
}
