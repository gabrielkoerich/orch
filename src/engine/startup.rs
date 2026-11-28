//! Startup reconciliation, upgrade check and escalation notice

use crate::backends::ExternalId;
use crate::channels::discord_ws::DiscordGateway;
use crate::channels::routing::ProjectChannelConfig;
use crate::channels::slack::SlackChannel;
use crate::channels::telegram::TelegramChannel;
use crate::channels::{Channel, ChannelRegistry, OutgoingMessage};
use crate::cmd::CommandErrorContext;
use crate::config;
use crate::engine::cleanup::{
    local_branch_exists, remove_worktree_and_branch, remove_worktree_and_branch_for,
};
use crate::engine::git::worktree::{
    abort_worktree_rebase, list_project_worktrees, task_id_from_worktree_name,
    validate_worktree_gitdir,
};
/// Standard Orch attribution footer for issue bodies, PR bodies, and comments.
///
/// Append to every user-visible string posted to GitHub so activity is
/// clearly attributed to orch rather than appearing as a human post.
use crate::engine::progress;
use crate::store::{TaskStatus, TaskStore};
// AtomicBool/Ordering removed — shutdown is now immediate (reset tasks + break)
use super::{ProjectEngine, connect_project_engine};
use crate::backends::Status;
use crate::channels::notification;
use crate::engine::git::git_ops;
use crate::engine::git::worktree;
use crate::home;
use crate::secret_ref;
use crate::store;
use crate::store::cluster;
use crate::store::cluster::local;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio::process::Command;

pub(super) fn should_run_upgrade_check(
    last_upgrade_check: Option<Instant>,
    upgrade_check_interval: u64,
) -> bool {
    if upgrade_check_interval == 0 {
        return false;
    }

    last_upgrade_check
        .is_none_or(|last| last.elapsed() >= Duration::from_secs(upgrade_check_interval))
}

/// Initialize all project engines from config.
///
/// Returns a vector of ProjectEngine, one for each configured project.
/// The task store every project shares, with the cluster and its local write endpoint when on
pub(super) async fn open_shared_store() -> anyhow::Result<Arc<TaskStore>> {
    let store = Arc::new(TaskStore::open(&store::default_db_path().await?).await?);
    if cluster::enabled() {
        cluster::start(&store, &home::orch_home()?.join("raft.db")).await?;
        local::serve((*store).clone(), progress::handler()).await?;
    }
    Ok(store)
}

/// One engine per configured project. None is fine, the web UI adds the first one and
/// `sync_project_engines` starts it
pub(super) async fn init_project_engines(
    store: &Arc<TaskStore>,
) -> anyhow::Result<Vec<ProjectEngine>> {
    let projects = config::get_projects_with_paths()?;
    let repos: Vec<&str> = projects.iter().map(|(r, _)| r.as_str()).collect();
    tracing::info!(repos = ?repos, "loading projects from config");

    if projects.is_empty() {
        tracing::info!("no projects yet, add one with `orch ui` or `orch project add`");
    }

    let mut engines = Vec::new();
    for (repo, project_dir) in projects {
        tracing::info!(repo = %repo, "initializing project engine");
        config::watch(&project_dir.join(".orch.yml"));

        engines.push(connect_project_engine(repo, project_dir, Arc::clone(store), None).await?);
    }

    Ok(engines)
}

pub(super) async fn reconcile_startup_worktrees(
    project_engines: &[ProjectEngine],
) -> anyhow::Result<()> {
    for engine in project_engines {
        let repo_root = worktree::resolve_main_repo(&engine.project_dir).await;
        let repo_root_path = PathBuf::from(&repo_root);
        let default_branch = worktree::detect_default_branch(&repo_root_path).await;
        let worktrees = list_project_worktrees(&repo_root_path).await?;

        // Fetch refs once for all worktrees in this project, so rebase below
        // operates on up-to-date origin/* refs without N sequential fetches.
        if let Err(e) = git_ops::git()
            .await
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
                    let keep_conflict = worktree::keep_conflicted_rebase(
                        task.merge_conflict_retries,
                        task.pr_number,
                    );
                    if keep_conflict && git_ops::rebase_in_progress(&worktree_dir).await {
                        tracing::info!(
                            repo = %engine.repo,
                            task_id = %task_id,
                            worktree = %worktree_dir.display(),
                            "keeping the conflicted rebase auto_merge left for the agent"
                        );
                        valid_kept += 1;
                        continue;
                    }
                    tracing::info!(repo = %engine.repo, task_id = %task_id, worktree = %worktree_dir.display(), "rebasing startup worktree");
                    let origin_branch = format!("origin/{default_branch}");
                    let outcome = git_ops::stash_rebase_restore(
                        &worktree_dir,
                        &origin_branch,
                        git_ops::StashRebaseOpts {
                            abort_on_failure: false,
                        },
                    )
                    .await;
                    let conflict_left = git_ops::rebase_in_progress(&worktree_dir).await;
                    let failed_err = match outcome {
                        Ok(
                            git_ops::RebaseOutcome::Succeeded | git_ops::RebaseOutcome::Skipped(_),
                        ) => None,
                        Ok(git_ops::RebaseOutcome::Failed(e)) if keep_conflict && conflict_left => {
                            tracing::info!(
                                repo = %engine.repo,
                                task_id = %task_id,
                                err = %e,
                                "startup rebase hit a content conflict, leaving it for the agent"
                            );
                            None
                        }
                        Ok(git_ops::RebaseOutcome::Failed(e)) => Some(e),
                        Err(e) => Some(e.to_string()),
                    };
                    if let Some(e) = failed_err {
                        reset_startup_task_after_failed_rebase(
                            engine,
                            &task_id,
                            &worktree_dir,
                            branch_name,
                            &repo_root_path,
                            task.pr_number.is_some(),
                            &e,
                        )
                        .await;
                        cleaned += 1;
                    } else {
                        valid_kept += 1;
                    }
                }
                // A blocked or waiting task resumes from this work, and without a PR the branch
                // may hold the only copy of it. An approved task's PR is updated on GitHub
                TaskStatus::Blocked | TaskStatus::Waiting | TaskStatus::Approved => {
                    valid_kept += 1;
                }
                TaskStatus::Done => {
                    tracing::info!(repo = %engine.repo, task_id = %task_id, worktree = %worktree_dir.display(), status = ?task.status, "terminal task worktree, removing");
                    remove_worktree_and_branch_for(
                        &task_id,
                        &worktree_dir,
                        Some(branch_name),
                        &repo_root_path,
                        false,
                        task.pr_number.is_some(),
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

async fn reset_startup_task_after_failed_rebase(
    engine: &ProjectEngine,
    task_id: &str,
    worktree_dir: &Path,
    branch_name: &str,
    repo_root: &Path,
    keep_remote: bool,
    err: &str,
) {
    tracing::warn!(repo = %engine.repo, task_id, err, "startup rebase failed, resetting task");
    abort_worktree_rebase(worktree_dir).await;
    remove_worktree_and_branch(
        task_id,
        worktree_dir,
        Some(branch_name),
        repo_root,
        keep_remote,
    )
    .await;
    let keep_branch = local_branch_exists(repo_root, branch_name).await;
    match engine.store.resolve_task_id(&engine.repo, task_id).await {
        Ok(Some(reset_id)) => {
            let result = if keep_branch {
                engine.store.reset_to_new_keep_branch(reset_id).await
            } else {
                engine.store.reset_to_new(reset_id).await
            };
            if let Err(e) = result {
                tracing::warn!(task_id, err = %e, "failed to reset task to new after rebase failure");
            }
        }
        Ok(None) => {
            tracing::warn!(task_id, "task not found in store during reconciliation");
        }
        Err(e) => {
            tracing::warn!(task_id, err = %e, "failed to resolve task ID during reconciliation");
        }
    }
    if let Err(e) = engine
        .task_manager
        .update_task_status(&ExternalId(task_id.to_string()), Status::New)
        .await
    {
        tracing::warn!(task_id, err = %e, "failed to update backend status during reconciliation");
    }
}

/// Sync Fibonacci estimates stored in SQLite to the GitHub Projects board for
/// all tasks that have a positive estimate.
///
/// Runs once on startup to catch tasks that were routed before the estimate
/// sync feature was available or before `project_estimate_field_id` was
/// configured. The underlying mutations are idempotent, so re-syncing an
/// already-correct estimate is harmless.
pub(super) async fn reconcile_startup_estimates(project_engines: &[ProjectEngine]) {
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
pub(super) fn read_project_channel_config(project_dir: &Path) -> ProjectChannelConfig {
    let config_path = project_dir.join(".orch.yml");
    if !config_path.exists() {
        return ProjectChannelConfig::default();
    }
    let content = match fs::read_to_string(&config_path) {
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
    let Some(channels) = val.get("channels") else {
        return ProjectChannelConfig::default();
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
        discord_channel_id: get_str("discord", "channel_id"),
    }
}

/// Fetch the latest orch release tag from GitHub via the `gh` CLI (async).
///
/// Returns the version string (without the leading "v"), or `None` when the
/// check fails (no network, unauthenticated, `gh` not installed, etc.).
pub(super) async fn fetch_latest_release_version() -> Option<String> {
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

/// Check if a newer orch release is available and send channel notifications
/// asking the operator to upgrade manually. Upgrading is operator-only — the
/// service never runs `brew` itself.
///
/// Uses the store's KV to throttle checks (`upgrade:last_check_at`) and
/// deduplicate notifications (`upgrade:last_notified_version`). Each distinct
/// latest version is notified about once; the notification repeats only when
/// an even newer version is released.
pub(super) async fn check_and_notify_upgrade(
    store: &Arc<TaskStore>,
    channels: &Arc<ChannelRegistry>,
    current_version: &str,
    check_interval: Duration,
) {
    let last_check_key = "upgrade:last_check_at";
    let last_notified_key = "upgrade:last_notified_version";

    // Throttle: only check once per interval.
    let now = chrono::Utc::now().timestamp();
    if let Ok(Some(last_check_str)) = store.kv_get(last_check_key).await
        && let Ok(last_check) = last_check_str.parse::<i64>()
        && now - last_check < i64::try_from(check_interval.as_secs()).unwrap_or(i64::MAX)
    {
        return;
    }

    // Record check timestamp (best-effort).
    let _ = store.kv_set(last_check_key, &now.to_string()).await;

    let Some(latest) = fetch_latest_release_version().await else {
        tracing::debug!("upgrade check: failed to fetch latest release version");
        return;
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

    // Operator-only upgrade: send channel notifications so the operator can
    // upgrade on their own schedule. The service never runs brew itself.

    // Deduplicate: don't re-notify for the same latest version.
    if let Ok(Some(last_notified)) = store.kv_get(last_notified_key).await
        && last_notified == latest
    {
        tracing::debug!(latest = %latest, "upgrade already notified");
        return;
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

/// Send a one-time escalation notification when the startup retry loop has been
/// spinning for longer than `startup_failure_escalation_secs`.
///
/// This is called *before* the channel registry is initialised (the retry loop
/// sits above it in `serve()`), so we construct channel objects directly from
/// config — mirroring the approach used during normal channel registration, but
/// without relying on `ChannelRegistry`.
pub(super) async fn send_startup_escalation_notification(elapsed_secs: u64) {
    #[expect(clippy::cast_precision_loss, reason = "a display duration")]
    let duration = notification::format_duration(elapsed_secs as f64);

    let msg_telegram = format!(
        "🚨 <b>Orch startup blocked — GitHub unreachable</b>\n\n\
         The engine has been retrying backend connections for {duration}\n\
         without success. All task processing, routing, and scheduled\n\
         jobs are paused. Check service logs with `orch log -f`."
    );

    let msg_discord = format!(
        "🚨 **Orch startup blocked — GitHub unreachable**\n\n\
         The engine has been retrying backend connections for {duration}\n\
         without success. All task processing, routing, and scheduled\n\
         jobs are paused. Check service logs with `orch log -f`."
    );

    let msg_slack = format!(
        "🚨 *Orch startup blocked — GitHub unreachable*\n\n\
         The engine has been retrying backend connections for {duration}\n\
         without success. All task processing, routing, and scheduled\n\
         jobs are paused. Check service logs with `orch log -f`."
    );

    // Telegram
    let tg_token = secret_ref::config_secret("channels.telegram.bot_token").await;
    let tg_chat_id = config::get("channels.telegram.chat_id");
    if let (Ok(token), Ok(chat_id)) = (&tg_token, &tg_chat_id)
        && !token.is_empty()
        && !chat_id.is_empty()
        && let Ok(channel) = TelegramChannel::new(token.clone(), Some(chat_id.clone()))
    {
        let msg = OutgoingMessage {
            thread_id: "startup_escalation".to_string(),
            body: msg_telegram.clone(),
            reply_to: None,
            metadata: serde_json::json!({ "preformatted_html": true }),
            topic_id: config::get("channels.telegram.general_topic_id").ok(),
        };
        match channel.send(&msg).await {
            Err(e) => {
                tracing::warn!(?e, "failed to send startup escalation via telegram");
            }
            _ => {
                tracing::info!("startup escalation notification sent to telegram");
            }
        }
    }

    // Discord
    if let Ok(token) = secret_ref::config_secret("channels.discord.bot_token").await {
        let channel_id = config::get("channels.discord.channel_id")
            .or_else(|_| config::get("channels.discord.general_channel_id"));
        if !token.is_empty()
            && let Some(cid) = channel_id.ok().filter(|s| !s.is_empty())
        {
            let shard_id = config::get("channels.discord.shard_id")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            let shard_count = config::get("channels.discord.shard_count")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(1);
            if let Ok(gateway) = DiscordGateway::new(token, Some(cid), shard_id, shard_count) {
                let msg = OutgoingMessage {
                    thread_id: "startup_escalation".to_string(),
                    body: msg_discord.clone(),
                    reply_to: None,
                    metadata: serde_json::Value::Null,
                    topic_id: None,
                };
                match gateway.send(&msg).await {
                    Err(e) => {
                        tracing::warn!(?e, "failed to send startup escalation via discord");
                    }
                    _ => {
                        tracing::info!("startup escalation notification sent to discord");
                    }
                }
            }
        }
    }

    // Slack
    if let Ok(token) = secret_ref::config_secret("channels.slack.bot_token").await {
        let channel_id = config::get("channels.slack.channel_id");
        if !token.is_empty()
            && let Ok(cid) = channel_id
            && let Ok(channel) = SlackChannel::new(token, Some(cid))
        {
            let msg = OutgoingMessage {
                thread_id: "startup_escalation".to_string(),
                body: msg_slack.clone(),
                reply_to: None,
                metadata: serde_json::Value::Null,
                topic_id: None,
            };
            match channel.send(&msg).await {
                Err(e) => {
                    tracing::warn!(?e, "failed to send startup escalation via slack");
                }
                _ => {
                    tracing::info!("startup escalation notification sent to slack");
                }
            }
        }
    }
}
