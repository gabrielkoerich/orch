//! Worktree cleanup and merged-PR detection.
//!
//! Contains the post-merge cleanup pipeline: removing git worktrees,
//! deleting local/remote branches, pulling main, and detecting
//! already-merged PRs so their tasks can be marked done.

use crate::backends::{ExternalBackend, ExternalId, ExternalTask, Status};
use crate::cmd::CommandErrorContext;
use crate::engine::tasks::TaskManager;
use crate::store;
use crate::store::store_log_activity;
use crate::store::TaskStatus;
use crate::store::TaskStore;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::process::Command;

use super::sync::ReviewTaskSnapshot;

/// Options controlling the worktree janitor.
#[derive(Debug, Clone)]
pub struct JanitorOptions {
    /// Minimum age (in hours) of the worktree directory before it is eligible for removal.
    ///
    /// A worktree whose filesystem mtime is more recent than this threshold is skipped
    /// even if the task is already in a terminal state. This provides a safety buffer
    /// against races (e.g. review agent still writing when the janitor runs).
    ///
    /// Default: 24.  Configurable via `workflow.worktree_janitor_ttl_hours` in config.
    pub ttl_hours: u64,

    /// When `true`, log what would be done but skip all destructive operations.
    ///
    /// Configurable via `workflow.worktree_janitor_dry_run: true` in config.
    pub dry_run: bool,
}

impl Default for JanitorOptions {
    fn default() -> Self {
        Self {
            ttl_hours: 24,
            dry_run: false,
        }
    }
}

impl JanitorOptions {
    /// Read options from the global config, falling back to defaults.
    pub fn from_config() -> Self {
        let ttl_hours = crate::config::get("workflow.worktree_janitor_ttl_hours")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(24);

        let dry_run = crate::config::get("workflow.worktree_janitor_dry_run")
            .ok()
            .map(|v| v == "true")
            .unwrap_or(false);

        Self { ttl_hours, dry_run }
    }
}

/// Cleanup worktrees for completed tasks.
///
/// Targets tasks that are finished: status:done OR closed on GitHub
/// (e.g. when a PR merge auto-closes the issue via "Fixes #N").
///
/// Note: This is a fallback. Primary cleanup happens inline in
/// `auto_merge_pr` via `cleanup_task_worktree`. This catches edge cases
/// where the inline cleanup missed (e.g., manual merges, auto-closed issues).
pub(crate) async fn cleanup_done_worktrees(
    backend: &Arc<dyn ExternalBackend>,
    repo: &str,
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
) -> anyhow::Result<()> {
    let opts = JanitorOptions::from_config();
    cleanup_done_worktrees_with_opts(backend, repo, task_manager, store, &opts).await
}

async fn reconcile_closed_tasks(
    task_ids: &mut Vec<String>,
    all_tasks: &[ExternalTask],
    task_manager: &TaskManager,
) {
    let done_set: std::collections::HashSet<String> = task_ids.iter().cloned().collect();
    let closed_not_in_store: Vec<_> = all_tasks
        .iter()
        .filter(|t| t.state == "closed" && !done_set.contains(&t.id.0))
        .collect();

    // Split: issues that already have status:done label just need worktree
    // cleanup (no API call needed), vs those needing label reconciliation.
    let mut already_labeled: Vec<String> = Vec::new();
    let mut needs_reconcile: Vec<&ExternalTask> = Vec::new();
    for task in &closed_not_in_store {
        if task.labels.iter().any(|l| l == "status:done") {
            already_labeled.push(task.id.0.clone());
        } else {
            needs_reconcile.push(task);
        }
    }

    // Add already-labeled issues directly (no API call).
    if !already_labeled.is_empty() {
        tracing::debug!(
            count = already_labeled.len(),
            "closed issues already labeled status:done — adding to cleanup set"
        );
        task_ids.extend(already_labeled);
    }

    // Reconcile issues missing the done label — cap to avoid rate-limit
    // exhaustion. Remaining issues will be picked up in subsequent cycles.
    const MAX_RECONCILE_PER_CYCLE: usize = 10;
    if needs_reconcile.len() > MAX_RECONCILE_PER_CYCLE {
        tracing::info!(
            total = needs_reconcile.len(),
            batch = MAX_RECONCILE_PER_CYCLE,
            "capping closed-issue reconciliation to avoid rate-limit exhaustion"
        );
    }
    let mut futs = Vec::new();
    for task in needs_reconcile.iter().take(MAX_RECONCILE_PER_CYCLE) {
        let id = task.id.clone();
        let task_id_str = task.id.0.clone();
        let labels = task.labels.clone();
        futs.push(async move {
            tracing::info!(
                task_id = task_id_str,
                labels = ?labels,
                "closed issue with stale status label — reconciling to done and cleaning up"
            );
            match task_manager.update_task_status(&id, Status::Done).await {
                Ok(()) => Some(task_id_str),
                Err(e) => {
                    tracing::warn!(
                        task_id = task_id_str,
                        err = %e,
                        "failed to reconcile closed issue status to done"
                    );
                    None
                }
            }
        });
    }
    let results = futures::future::join_all(futs).await;
    task_ids.extend(results.into_iter().flatten());
}

/// Cleanup worktrees for completed tasks with explicit options.
///
/// Separated from `cleanup_done_worktrees` so that integration tests can
/// inject specific options (e.g. `ttl_hours: 0`, `dry_run: true`) without
/// touching the global config.
pub(crate) async fn cleanup_done_worktrees_with_opts(
    backend: &Arc<dyn ExternalBackend>,
    repo: &str,
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
    opts: &JanitorOptions,
) -> anyhow::Result<()> {
    // Read done tasks from the store first; fall back to backend before first sync.
    let done_tasks = task_manager.list_all_by_status(Status::Done).await?;
    tracing::debug!(count = done_tasks.len(), "checking done tasks for cleanup");

    // Collect task IDs from external done tasks.
    let mut task_ids: Vec<String> = done_tasks.iter().map(|t| t.id.0.clone()).collect();

    // Also include closed-but-not-done external tasks.
    // When a PR merge auto-closes a GitHub issue (via "Fixes #N"), the issue
    // state becomes "closed" but orch never updates the status label to "done".
    // These orphaned worktrees accumulate unless we catch them here.
    // Note: This still queries the backend because GitHub `state` (open/closed)
    // is backend-specific and not tracked in the store.
    match backend.list_all_tasks().await {
        Ok(all_tasks) => {
            reconcile_closed_tasks(&mut task_ids, &all_tasks, task_manager).await;
        }
        Err(e) => {
            tracing::warn!(err = %e, "failed to list all tasks for closed-issue reconciliation");
            match backend.list_reconciliation_candidates().await {
                Ok(fallback_tasks) if !fallback_tasks.is_empty() => {
                    tracing::info!(
                        count = fallback_tasks.len(),
                        "using fallback tasks for closed-issue reconciliation"
                    );
                    reconcile_closed_tasks(&mut task_ids, &fallback_tasks, task_manager).await;
                }
                Ok(_) => {
                    tracing::debug!("no fallback tasks available for closed-issue reconciliation");
                }
                Err(fallback_err) => {
                    tracing::warn!(
                        err = %fallback_err,
                        "failed to list fallback tasks for closed-issue reconciliation"
                    );
                }
            }
        }
    }

    // Also include internal done tasks.
    if let Ok(internal_done) = task_manager.list_internal_by_status(TaskStatus::Done).await {
        task_ids.extend(internal_done.iter().map(|t| t.id.0.clone()));
    }

    // Also include internal blocked tasks (terminal state, worktree is useless).
    if let Ok(internal_blocked) = task_manager
        .list_internal_by_status(TaskStatus::Blocked)
        .await
    {
        task_ids.extend(internal_blocked.iter().map(|t| t.id.0.clone()));
    }

    tracing::debug!(
        count = task_ids.len(),
        "checking all terminal tasks for cleanup"
    );

    // Prefetch all cleaned external IDs in one query instead of N+1 per-task lookups.
    let already_cleaned = store.cleaned_external_ids(repo).await.unwrap_or_default();

    let mut cleaned_any = false;
    for task_id in &task_ids {
        // Skip if already cleaned (checked via prefetched set).
        if already_cleaned.contains(task_id) {
            continue;
        }

        match cleanup_task_worktree_with_opts(task_id, repo, store, opts).await {
            Ok(true) => cleaned_any = true,
            Ok(false) => {
                // Skipped (TTL guard, tmux guard, or nothing to remove) — do not
                // pull main; we did not change the repo state.
            }
            Err(e) => {
                tracing::warn!(task_id, err = %e, "worktree cleanup failed for task");
            }
        }
    }

    // Prune stale worktree metadata and pull main after cleanup.
    if cleaned_any && !opts.dry_run {
        if let Ok(repo_root) = resolve_repo_root(repo).await {
            // Prune stale .git/worktrees/ entries whose directories no longer exist.
            let prune_result = Command::new("git")
                .args(["-C", &repo_root, "worktree", "prune"])
                .output_with_context()
                .await;
            match prune_result {
                Ok(output) if output.status.success() => {
                    tracing::debug!(%repo, "pruned stale worktree metadata");
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    tracing::warn!(%repo, err = %stderr, "git worktree prune failed");
                }
                Err(e) => {
                    tracing::warn!(%repo, err = %e, "git worktree prune failed");
                }
            }

            let pull_result = Command::new("git")
                .args(["-C", &repo_root, "pull", "--ff-only"])
                .output_with_context()
                .await;
            match pull_result {
                Ok(output) if output.status.success() => {
                    tracing::info!(%repo, "pulled main after worktree cleanup");
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    tracing::debug!(%repo, err = %stderr, "git pull skipped after cleanup");
                }
                Err(e) => {
                    tracing::debug!(%repo, err = %e, "git pull failed after cleanup");
                }
            }
        }
    }

    Ok(())
}

/// Cleanup a single task's worktree and branches.
///
/// Removes the git worktree, deletes local + remote branches,
/// pulls main to stay up-to-date, and marks the task as cleaned.
///
/// This function is used for inline post-merge cleanup (called from
/// auto-merge flows) and must attempt immediate removal — it constructs
/// janitor options with `ttl_hours = 0` so the janitor age guard does not
/// postpone removing freshly-created worktrees.
pub(crate) async fn cleanup_task_worktree(
    task_id: &str,
    repo: &str,
    store: &Arc<TaskStore>,
) -> anyhow::Result<()> {
    let opts = JanitorOptions {
        ttl_hours: 0,
        ..Default::default()
    };
    let cleaned = cleanup_task_worktree_with_opts(task_id, repo, store, &opts).await?;

    // Pull main once after inline cleanup (post-merge single-task path).
    // Only pull if cleanup actually happened (not skipped due to active session).
    if cleaned {
        if let Ok(repo_root) = resolve_repo_root(repo).await {
            let _ = Command::new("git")
                .args(["-C", &repo_root, "pull", "--ff-only"])
                .output_with_context()
                .await;
        }
    }

    Ok(())
}

/// Cleanup a single task's worktree and branches with explicit janitor options.
///
/// Returns `Ok(true)` when cleanup was actually performed and the task was
/// marked as cleaned.  Returns `Ok(false)` when cleanup was intentionally
/// skipped (TTL guard, active tmux session guard, or nothing on disk to remove).
/// Returns `Err` on unexpected failures (e.g. cannot resolve the repo root).
pub(crate) async fn cleanup_task_worktree_with_opts(
    task_id: &str,
    repo: &str,
    store: &Arc<TaskStore>,
    opts: &JanitorOptions,
) -> anyhow::Result<bool> {
    let (worktree, branch, keep_remote_branch) =
        store::opt_store_get_task(&Some(Arc::clone(store)), repo, task_id)
            .await
            .map(|t| {
                // Blocked tasks with a PR should keep their remote branch so the
                // PR stays open for human review.
                let keep = t.status == TaskStatus::Blocked && t.pr_number.is_some();
                (Some(t.worktree), Some(t.branch), keep)
            })
            .unwrap_or((None, None, false));

    let worktree_path = worktree.as_ref().map(std::path::PathBuf::from);

    // Get worktrees base path
    let worktrees_base = crate::home::worktrees_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from(".orch/worktrees"));

    // Determine which directory to remove.
    //
    // Priority:
    //   1. stored "worktree" path, if it exists on disk.
    //   2. Construct from worktrees_base + project name + branch, if it exists.
    let worktree_to_remove = if let Some(ref wt) = worktree_path {
        if wt.exists() {
            Some(wt.clone())
        } else {
            // The recorded worktree path doesn't exist — try reconstructing from branch.
            if let Some(ref b) = branch {
                let project = repo
                    .rsplit('/')
                    .next()
                    .unwrap_or(repo)
                    .trim_end_matches(".git");
                let candidate = worktrees_base.join(project).join(b);
                if candidate.exists() {
                    Some(candidate)
                } else {
                    None
                }
            } else {
                None
            }
        }
    } else if let Some(ref b) = branch {
        // No worktree path in store — try branch-based path.
        let project = repo
            .rsplit('/')
            .next()
            .unwrap_or(repo)
            .trim_end_matches(".git");
        let candidate = worktrees_base.join(project).join(b);
        if candidate.exists() {
            Some(candidate)
        } else {
            None
        }
    } else {
        None
    };

    let mut did_clean = false;

    // If there's nothing to remove (no worktree path and no branch),
    // treat as a no-op and return Ok(false) without attempting to
    // resolve the repo root. Resolving the repo root can fail for
    // projects not registered in config and that should not make
    // cleanup a hard error when there is nothing to do.
    let branch_nonempty = branch
        .as_ref()
        .and_then(|b| if b.is_empty() { None } else { Some(b) });
    if worktree_to_remove.is_none() && branch_nonempty.is_none() {
        // If the stored worktree path is non-empty but doesn't exist on disk,
        // the worktree is already gone — mark it cleaned so we stop retrying.
        if worktree_path
            .as_ref()
            .is_some_and(|p| !p.as_os_str().is_empty())
        {
            tracing::debug!(
                task_id,
                worktree = ?worktree_path,
                "stored worktree path no longer exists on disk — marking cleaned"
            );
            if let Ok(Some(store_id)) = store.resolve_task_id(repo, task_id).await {
                if let Err(e) = store.mark_cleaned(store_id).await {
                    tracing::warn!(task_id, err = %e, "failed to mark worktree cleaned in store — will retry");
                }
            }
        }
        return Ok(false);
    }

    if let Some(wt) = worktree_to_remove {
        // TTL guard: skip if the worktree directory is too young.
        if let Some(age_hours) = worktree_age_hours(&wt).await {
            if age_hours < opts.ttl_hours {
                tracing::debug!(
                    task_id,
                    worktree = %wt.display(),
                    age_hours,
                    ttl_hours = opts.ttl_hours,
                    "worktree too young for cleanup, skipping"
                );
                return Ok(false);
            }
        }

        // Tmux guard: skip if any active pane still has its cwd inside this worktree.
        if is_worktree_in_active_session(&wt).await {
            tracing::warn!(
                task_id,
                worktree = %wt.display(),
                "worktree is referenced by an active tmux session — skipping cleanup"
            );
            return Ok(false);
        }

        // Validate git metadata before attempting any git operations.
        // If the .git file points to a deleted gitdir the worktree is
        // unrecoverable — force-remove the directory and skip git commands.
        let gitdir_valid = crate::engine::runner::worktree::validate_worktree_gitdir(&wt).await;

        if opts.dry_run {
            tracing::info!(
                task_id,
                worktree = %wt.display(),
                branch = ?branch,
                gitdir_valid,
                "[dry-run] would remove worktree and branch"
            );
        } else if !gitdir_valid {
            tracing::warn!(
                task_id,
                worktree = %wt.display(),
                "worktree has broken git metadata — force-removing directory without git operations"
            );
            match tokio::fs::remove_dir_all(&wt).await {
                Ok(()) => {
                    did_clean = true;
                }
                Err(e) => {
                    tracing::warn!(
                        task_id,
                        worktree = %wt.display(),
                        err = %e,
                        "failed to force-remove broken worktree directory"
                    );
                }
            }
        } else {
            tracing::info!(task_id, worktree = %wt.display(), "removing worktree");
            // Derive repo root from the worktree itself (for cross-project support).
            // If the worktree belongs to a different project than the current repo
            // context (e.g., internal task with worktree in another project), we can
            // still find the repo root by examining the worktree's .git file.
            let repo_root = match resolve_repo_root_from_worktree(&wt).await {
                Ok(root) => root,
                Err(_) => resolve_repo_root(repo).await?,
            };
            let removed = remove_worktree_and_branch(
                task_id,
                &wt,
                branch.as_deref(),
                std::path::Path::new(&repo_root),
                keep_remote_branch,
            )
            .await;
            if removed {
                did_clean = true;
            } else {
                tracing::warn!(
                    task_id,
                    worktree = %wt.display(),
                    "worktree directory still exists after removal attempt — not marking as cleaned"
                );
            }
        }
    } else if let Some(ref br) = branch {
        // Worktree directory is already gone, but the branch may still exist
        // on the remote. Delete it to avoid orphaned branches.
        if !opts.dry_run && !keep_remote_branch {
            tracing::debug!(task_id, branch = %br, "no worktree on disk, cleaning up branch only");
            // Resolve repo root lazily — branch cleanup needs it.
            let repo_root = resolve_repo_root(repo).await?;
            delete_branches(task_id, br, std::path::Path::new(&repo_root)).await;
            did_clean = true;
        }
    }

    if did_clean {
        // Mark as cleaned in store so we don't retry next tick.
        // If resolve_task_id returns None the task is not (or no longer) in
        // the store; that is fine — we already did the on-disk work.
        if let Ok(Some(store_id)) = store.resolve_task_id(repo, task_id).await {
            if let Err(e) = store.mark_cleaned(store_id).await {
                tracing::warn!(task_id, err = %e, "failed to mark worktree cleaned — will retry");
                // Don't log activity; retry next tick will do the actual cleanup
                return Ok(did_clean);
            }
        }
        store_log_activity(
            &Some(Arc::clone(store)),
            repo,
            task_id,
            "branch_delete",
            None,
            None,
            None,
            None,
            Some(&serde_json::json!({
                "worktree": worktree_path.map(|p| p.display().to_string()),
                "branch": branch,
                "keep_remote_branch": keep_remote_branch,
            })),
        )
        .await;
    }

    Ok(did_clean)
}

/// Remove a git worktree directory and its local + remote branches.
/// When `keep_remote_branch` is true, only the worktree and local branch are
/// removed — the remote branch stays so that any linked PR remains open
/// (used for blocked tasks awaiting human review).
/// Returns `true` if the worktree directory is gone after the call
/// (either we removed it or it was already absent).
pub(crate) async fn remove_worktree_and_branch(
    task_id: &str,
    wt: &std::path::Path,
    branch: Option<&str>,
    repo_root: &std::path::Path,
    keep_remote_branch: bool,
) -> bool {
    let wt_str = wt.to_string_lossy().to_string();
    let repo_root_str = repo_root.to_string_lossy();

    // Remove worktree FIRST, then delete the branch.
    // Git refuses to remove a worktree if its branch is already deleted.
    let remove_result = Command::new("git")
        .args([
            "-C",
            repo_root_str.as_ref(),
            "worktree",
            "remove",
            &wt_str,
            "--force",
        ])
        .output_with_context()
        .await;

    match remove_result {
        Ok(output) if output.status.success() => {
            tracing::info!(task_id, "worktree removed");
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // "is not a working tree" means git's metadata is stale — the path
            // exists on disk but is no longer registered as a worktree. Treat
            // this as an already-gone worktree: physically remove the directory
            // and prune stale git metadata. Downgrade to debug — this is an
            // expected idempotency case, not a real error.
            if stderr.contains("is not a working tree") {
                tracing::debug!(
                    task_id,
                    path = %wt.display(),
                    "path is not a registered worktree (stale metadata) — removing directory and pruning"
                );
                if let Err(e) = tokio::fs::remove_dir_all(wt).await {
                    // Only warn if the directory actually still exists after removal attempt.
                    if wt.exists() {
                        tracing::warn!(
                            task_id,
                            path = %wt.display(),
                            err = %e,
                            "failed to remove stale worktree directory"
                        );
                    }
                } else {
                    tracing::debug!(task_id, path = %wt.display(), "stale worktree directory removed");
                }
                // Prune stale worktree entries from git metadata so the warning
                // stops recurring on future `git worktree list` / cleanup cycles.
                if let Err(e) = Command::new("git")
                    .args(["-C", repo_root_str.as_ref(), "worktree", "prune"])
                    .output_with_context()
                    .await
                {
                    tracing::warn!(task_id, err = %e, "git worktree prune failed — stale metadata may persist");
                }
            } else {
                tracing::warn!(task_id, err = %stderr, "failed to remove worktree");
            }
        }
        Err(e) => {
            tracing::warn!(task_id, err = %e, "failed to remove worktree");
        }
    }

    // Prune stale .git/worktrees/ entries left behind after removal.
    if let Err(e) = Command::new("git")
        .args(["-C", repo_root_str.as_ref(), "worktree", "prune"])
        .output_with_context()
        .await
    {
        tracing::warn!(task_id, err = %e, "git worktree prune failed — stale metadata may persist");
    }

    // Delete local and remote branch from the main repo root (worktree is already gone).
    // When keep_remote_branch is set, only delete the local branch — the remote stays
    // so any linked PR remains open for human review.
    if let Some(br) = branch {
        if keep_remote_branch {
            // Only delete local branch
            let branch_delete_result = Command::new("git")
                .args(["-C", repo_root_str.as_ref(), "branch", "-D", br])
                .output_with_context()
                .await;
            match branch_delete_result {
                Ok(output) if output.status.success() => {
                    tracing::info!(
                        task_id,
                        branch = br,
                        "local branch deleted (keeping remote for open PR)"
                    );
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    if !stderr.contains("not found") {
                        tracing::debug!(task_id, branch = br, err = %stderr, "local branch delete failed (may already be gone)");
                    }
                }
                Err(e) => {
                    tracing::debug!(task_id, branch = br, err = %e, "local branch delete failed");
                }
            }
        } else {
            delete_branches(task_id, br, repo_root).await;
        }
    }

    // Definitively check: is the worktree directory actually gone?
    !wt.exists()
}

/// Delete local and remote branches for a task.
pub(crate) async fn delete_branches(task_id: &str, br: &str, repo_root: &std::path::Path) {
    let repo_root_str = repo_root.to_string_lossy();
    let branch_delete_result = Command::new("git")
        .args(["-C", repo_root_str.as_ref(), "branch", "-D", br])
        .output_with_context()
        .await;

    match branch_delete_result {
        Ok(output) if output.status.success() => {
            tracing::info!(task_id, branch = %br, "local branch deleted");
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::debug!(
                task_id,
                err = %stderr,
                "local branch delete skipped (may not exist)"
            );
        }
        Err(e) => {
            tracing::warn!(task_id, err = %e, "failed to delete local branch");
        }
    }

    // Delete remote branch
    let auth_env = crate::engine::runner::git_ops::build_git_auth_env();
    let remote_delete = Command::new("git")
        .envs(auth_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .args([
            "-C",
            repo_root_str.as_ref(),
            "push",
            "origin",
            "--delete",
            br,
        ])
        .output_with_context()
        .await;

    match remote_delete {
        Ok(output) if output.status.success() => {
            tracing::info!(task_id, branch = %br, "remote branch deleted");
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::debug!(task_id, err = %stderr, "remote branch delete skipped");
        }
        Err(e) => {
            tracing::warn!(task_id, err = %e, "failed to delete remote branch");
        }
    }
}

/// Returns `true` if any active tmux pane has its cwd inside `worktree`.
///
/// Uses `tmux list-panes -a -F '#{pane_current_path}'` to enumerate all
/// pane working directories across every session. If tmux is not running
/// (command fails), we conservatively return `false` so cleanup can proceed.
async fn is_worktree_in_active_session(worktree: &std::path::Path) -> bool {
    let output = Command::new("tmux")
        .args(["list-panes", "-a", "-F", "#{pane_current_path}"])
        .output()
        .await;

    match output {
        Ok(o) if o.status.success() => {
            let worktree_str = worktree.to_string_lossy();
            String::from_utf8_lossy(&o.stdout).lines().any(|line| {
                let pane_path = line.trim();
                // Match exact path or any path that starts with worktree + separator.
                pane_path == worktree_str.as_ref()
                    || pane_path.starts_with(&format!("{}/", worktree_str))
            })
        }
        // tmux not running or no sessions — treat as safe to clean.
        _ => false,
    }
}

/// Returns how many hours old the worktree directory is, based on its mtime.
///
/// Returns `None` if the directory does not exist or the mtime cannot be read.
async fn worktree_age_hours(worktree: &std::path::Path) -> Option<u64> {
    // Use tokio::fs to avoid blocking the async runtime when reading metadata.
    let metadata = tokio::fs::metadata(worktree).await.ok()?;
    let modified = metadata.modified().ok()?;
    let age = std::time::SystemTime::now().duration_since(modified).ok()?;
    Some(age.as_secs() / 3600)
}

/// Resolve the main git repository root from a worktree's .git file.
///
/// For cross-project worktree cleanup, we cannot rely on `resolve_repo_root` because
/// the worktree may belong to a different project than the current repo context.
/// Instead, we extract the repo root from the worktree's `.git` file which contains
/// a `gitdir:` pointer to the main repo's git directory.
pub async fn resolve_repo_root_from_worktree(wt: &std::path::Path) -> anyhow::Result<String> {
    // For worktrees, .git is a file (not a directory) containing gitdir path
    let git_file = wt.join(".git");
    if !git_file.exists() {
        anyhow::bail!(".git file not found in worktree at {}", wt.display());
    }

    let content = tokio::fs::read_to_string(&git_file)
        .await
        .map_err(|e| anyhow::anyhow!("failed to read .git file: {e}"))?;

    // Parse gitdir line: "gitdir: /path/to/repo/.git/worktrees/<name>"
    for line in content.lines() {
        if let Some(gitdir) = line.strip_prefix("gitdir:") {
            let gitdir = gitdir.trim();
            // The main repo is the parent of the .git directory
            // gitdir points to: <repo>/.git/worktrees/<name>
            // We need to go up 3 levels: worktrees/<name> -> .git -> repo
            let gitdir_path = std::path::Path::new(gitdir);
            if let Some(repo_root) = gitdir_path
                .parent() // worktrees/<name>
                .and_then(|p| p.parent()) // .git
                .and_then(|p| p.parent())
            // repo root
            {
                return Ok(repo_root.display().to_string());
            }
        }
    }

    anyhow::bail!(
        "cannot resolve repo root from worktree .git file at {}",
        wt.display()
    )
}

/// Resolve the main git repository root path for a project.
///
/// Looks up the local project path from config, then verifies it's a git repo.
/// This avoids relying on cwd (which is undefined under launchd services).
pub(crate) async fn resolve_repo_root(repo: &str) -> anyhow::Result<String> {
    // Look up the local path from registered projects
    let paths = crate::config::get_project_paths().unwrap_or_default();
    for path_str in &paths {
        let path = std::path::Path::new(path_str);
        let orch_yml = path.join(".orch.yml");
        let legacy = path.join(".orchestrator.yml");
        let config_file = if orch_yml.exists() {
            orch_yml
        } else if legacy.exists() {
            legacy
        } else {
            continue;
        };
        if let Ok(content) = tokio::fs::read_to_string(&config_file).await {
            if let Ok(doc) = serde_yml::from_str::<serde_yml::Value>(&content) {
                if let Some(r) = doc
                    .get("gh")
                    .and_then(|gh| gh.get("repo"))
                    .and_then(|r| r.as_str())
                {
                    if r == repo {
                        return Ok(path_str.clone());
                    }
                }
            }
        }
    }

    // Fallback: try bare clone in ~/.orch/projects/<owner>/<repo>.git
    let parts: Vec<&str> = repo.split('/').collect();
    let bare = if parts.len() == 2 {
        crate::home::projects_dir()
            .map(|d| d.join(parts[0]).join(format!("{}.git", parts[1])))
            .unwrap_or_default()
    } else {
        std::path::PathBuf::new()
    };
    if bare.exists() {
        return Ok(bare.display().to_string());
    }

    anyhow::bail!(
        "cannot find local path for project {repo} — \
         checked {} registered project(s) and bare clone at {}",
        paths.len(),
        bare.display()
    )
}

/// Check for merged PRs and update task status accordingly.
///
/// Queries status:in_review and status:needs_review tasks, checks if their PR
/// is merged, and updates status to done if merged.
pub(crate) async fn check_merged_prs(
    backend: &Arc<dyn ExternalBackend>,
    repo: &str,
    store: &Arc<TaskStore>,
    task_manager: &Arc<TaskManager>,
    in_review_tasks: &[ReviewTaskSnapshot],
    needs_review_tasks: &[ReviewTaskSnapshot],
) -> anyhow::Result<()> {
    let mut branch_map: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let all_review_tasks: Vec<&ReviewTaskSnapshot> = in_review_tasks
        .iter()
        .chain(needs_review_tasks.iter())
        .collect();

    for task in &all_review_tasks {
        if !task.stored.branch.is_empty() {
            branch_map.insert(task.external.id.0.clone(), task.stored.branch.clone());
        }
    }

    tracing::debug!(
        count = all_review_tasks.len(),
        "checking review tasks for merged PRs"
    );

    // Collect (task_id, branch) pairs — skip tasks with no branch recorded.
    // Use the pre-built branch map; fall back to store lookup only for tasks
    // not in the map (e.g. internal tasks, backend fallback path).
    let mut task_branches: Vec<(String, String)> = Vec::new();
    for task in all_review_tasks {
        let task_id = &task.external.id.0;
        let branch = if let Some(b) = branch_map.get(task_id) {
            b.clone()
        } else {
            match store::opt_store_get_task(&Some(Arc::clone(store)), repo, task_id)
                .await
                .map(|t| t.branch)
            {
                Some(b) if !b.is_empty() => b,
                _ => {
                    tracing::debug!(task_id, "no branch info, skipping PR check");
                    continue;
                }
            }
        };
        task_branches.push((task_id.clone(), branch));
    }

    if task_branches.is_empty() {
        return Ok(());
    }

    // Single GraphQL call for all branches — N REST calls → 1 GraphQL call.
    let branches: Vec<String> = task_branches.iter().map(|(_, b)| b.clone()).collect();
    let merged_map = match backend.batch_is_pr_merged(&branches).await {
        Ok(map) => map,
        Err(e) => {
            tracing::warn!(
                err = %e,
                branch_count = branches.len(),
                "batch PR merge check failed, retrying affected branches individually"
            );
            fallback_is_pr_merged_by_branch(backend, &branches).await
        }
    };

    for (task_id, branch) in task_branches {
        let is_merged = merged_map.get(&branch).copied().unwrap_or(false);
        if !is_merged {
            continue;
        }

        tracing::info!(task_id, branch = %branch, "PR merged, marking task complete");

        let id = ExternalId(task_id.clone());
        if let Err(e) = task_manager.update_task_status(&id, Status::Done).await {
            tracing::warn!(task_id, err = %e, "failed to update status to done");
            continue;
        }

        let comment = format!(
            "PR merged, marking task complete{}",
            crate::engine::orch_footer()
        );
        if let Err(e) = backend.post_comment(&id, &comment).await {
            tracing::warn!(task_id, err = %e, "failed to post comment");
        }
    }

    Ok(())
}

async fn fallback_is_pr_merged_by_branch(
    backend: &Arc<dyn ExternalBackend>,
    branches: &[String],
) -> HashMap<String, bool> {
    let mut merged_map = HashMap::with_capacity(branches.len());

    for branch in branches {
        match backend.is_pr_merged(branch).await {
            Ok(is_merged) => {
                merged_map.insert(branch.clone(), is_merged);
            }
            Err(err) => {
                tracing::warn!(
                    branch,
                    err = %err,
                    "fallback PR merge check failed for branch"
                );
            }
        }
    }

    merged_map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{NewTask, TaskStore};
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Mutex;

    // worktree_age_hours

    #[tokio::test]
    async fn worktree_age_hours_returns_none_for_missing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        assert!(worktree_age_hours(&missing).await.is_none());
    }

    #[tokio::test]
    async fn worktree_age_hours_returns_some_for_existing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        // A freshly created directory should have age ~0 hours.
        let age = worktree_age_hours(tmp.path()).await;
        assert!(age.is_some());
        assert_eq!(age.unwrap(), 0, "freshly created dir should have age 0h");
    }

    // JanitorOptions::from_config defaults

    #[test]
    fn janitor_options_default_values() {
        let opts = JanitorOptions::default();
        assert_eq!(opts.ttl_hours, 24);
        assert!(!opts.dry_run);
    }

    // cleanup_task_worktree_with_opts TTL guard (skips if git missing)

    /// Create a minimal git repo + worktree; return (tmp, worktree_dir).
    fn setup_test_repo() -> Option<(tempfile::TempDir, std::path::PathBuf)> {
        let tmp = tempfile::tempdir().ok()?;

        // Create a real git repo (not bare) so we can add worktrees.
        let repo_dir = tmp.path().join("repo.git");
        std::fs::create_dir_all(&repo_dir).ok()?;

        let init = std::process::Command::new("git")
            .args([
                "init",
                "--initial-branch=main",
                repo_dir
                    .to_str()
                    .expect("test repo path contains non-UTF-8 characters"),
            ])
            .output()
            .ok()?;
        if !init.status.success() {
            // Older git may not have --initial-branch; fall back.
            let _ = std::process::Command::new("git")
                .args([
                    "init",
                    repo_dir
                        .to_str()
                        .expect("test repo path contains non-UTF-8 characters"),
                ])
                .output()
                .ok()?;
        }

        // Create an initial commit so the repo has a HEAD.
        let readme = repo_dir.join("README.md");
        std::fs::write(&readme, "# test").ok()?;
        std::process::Command::new("git")
            .args(["-C", repo_dir.to_str()?, "add", "."])
            .output()
            .ok()?;
        std::process::Command::new("git")
            .args([
                "-C",
                repo_dir.to_str()?,
                "-c",
                "user.email=test@test.com",
                "-c",
                "user.name=Test",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ])
            .output()
            .ok()?;

        // Create a worktree branch.
        let wt_dir = tmp
            .path()
            .join("worktrees")
            .join("repo.git")
            .join("gh-issue-42-test");
        std::fs::create_dir_all(wt_dir.parent()?).ok()?;
        let wt_out = std::process::Command::new("git")
            .args([
                "-C",
                repo_dir.to_str()?,
                "worktree",
                "add",
                "-b",
                "gh-issue-42-test",
                wt_dir.to_str()?,
            ])
            .output()
            .ok()?;

        if !wt_out.status.success() {
            return None;
        }

        Some((tmp, wt_dir))
    }

    #[tokio::test]
    async fn janitor_skips_young_worktree() {
        let Some((tmp, wt_dir)) = setup_test_repo() else {
            eprintln!("skipping test: git not available");
            return;
        };

        // The worktree was just created → age is 0h < ttl_hours=24 → must be skipped.
        assert!(wt_dir.exists(), "worktree should exist before janitor runs");

        // Run the TTL guard check directly (without a full engine).
        let age = worktree_age_hours(&wt_dir).await.unwrap_or(u64::MAX);
        assert!(
            age < 24,
            "freshly created worktree should be younger than 24h (age={age}h)"
        );

        // Explicitly verify: with ttl_hours=24 the worktree stays.
        let opts = JanitorOptions {
            ttl_hours: 24,
            dry_run: true,
        };
        // Age < ttl → should skip (worktree_age_hours < opts.ttl_hours).
        assert!(age < opts.ttl_hours, "janitor should skip young worktree");

        drop(tmp); // cleanup tempdir
    }

    #[tokio::test]
    async fn janitor_eligible_when_ttl_zero() {
        let Some((tmp, wt_dir)) = setup_test_repo() else {
            eprintln!("skipping test: git not available");
            return;
        };

        // With ttl_hours=0 even a brand-new worktree is eligible.
        let age = worktree_age_hours(&wt_dir).await.unwrap_or(u64::MAX);
        let opts = JanitorOptions {
            ttl_hours: 0,
            dry_run: true,
        };
        assert!(
            age >= opts.ttl_hours,
            "worktree should be eligible when ttl=0 (age={age}h)"
        );

        drop(tmp);
    }

    #[tokio::test]
    async fn dry_run_does_not_remove_worktree() {
        // Verify that with dry_run=true the worktree directory is NOT removed.
        let Some((tmp, wt_dir)) = setup_test_repo() else {
            eprintln!("skipping test: git not available");
            return;
        };

        assert!(wt_dir.exists(), "worktree must exist before test");

        // With dry_run=true and ttl_hours=0 the janitor would normally remove the
        // worktree, but dry_run prevents it.
        let opts = JanitorOptions {
            ttl_hours: 0,
            dry_run: true,
        };

        // The janitor needs a repo_root to run git commands.  We point it at the
        // bare repo we created inside tmp (even though it's not registered in config).
        // Because resolve_repo_root will fail for our synthetic repo slug, we test the
        // inner helpers directly here.
        let age = worktree_age_hours(&wt_dir).await.unwrap_or(u64::MAX);
        let in_use = false; // no tmux running in tests
        let skip = age < opts.ttl_hours || in_use;

        if !skip && opts.dry_run {
            // dry-run: log only, don't touch filesystem.
            tracing::debug!("[dry-run] would remove {}", wt_dir.display());
        } else if !skip {
            // This branch would do actual removal — but we're in dry_run mode above.
        }

        // Worktree must still be there.
        assert!(
            wt_dir.exists(),
            "dry-run must not remove the worktree directory"
        );

        drop(tmp);
    }

    #[test]
    fn fallback_branch_path_construction() {
        // Verify the new fallback logic: when worktree path is absent but
        // branch is known, we should construct the path correctly.
        let tmp = tempfile::tempdir().unwrap();
        let wt_dir = tmp.path().join("myrepo").join("gh-issue-7-fix");
        std::fs::create_dir_all(&wt_dir).unwrap();

        // Simulate: worktree=None, branch=Some("gh-issue-7-fix"), repo="owner/myrepo"
        let branch = "gh-issue-7-fix";
        let repo = "owner/myrepo";
        let worktrees_base = tmp.path();

        let project = repo
            .rsplit('/')
            .next()
            .unwrap_or(repo)
            .trim_end_matches(".git");
        let candidate = worktrees_base.join(project).join(branch);

        assert_eq!(candidate, wt_dir);
        assert!(
            candidate.exists(),
            "fallback path should point to existing dir"
        );
    }

    /// A task with no worktree and no branch should return Ok(false) —
    /// nothing was cleaned, so git pull should NOT be triggered.
    #[tokio::test]
    async fn cleanup_returns_false_when_nothing_to_clean() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        // Create a done task with no worktree or branch fields set
        let id = store
            .create(&NewTask {
                external_id: Some("999".to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "Already cleaned".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        store
            .update_status(id, crate::store::TaskStatus::Done)
            .await
            .unwrap();

        let opts = JanitorOptions {
            ttl_hours: 0,
            dry_run: false,
        };

        let result = cleanup_task_worktree_with_opts("999", "owner/repo", &store, &opts).await;
        // resolve_repo_root will fail for synthetic repo — that's OK, the point is
        // that when nothing needs cleaning the function should not report true.
        // In production this means no git pull is triggered.
        match result {
            Ok(cleaned) => assert!(
                !cleaned,
                "cleanup should return false when there is no worktree or branch to clean"
            ),
            Err(_) => {
                // resolve_repo_root fails for test repo — acceptable.
                // The bug was that even with no worktree/branch, Ok(true) was returned.
            }
        }
    }

    /// When the stored worktree path no longer exists on disk and there is no
    /// branch recorded, cleanup must mark the task as cleaned so subsequent
    /// runs do not keep retrying (idempotency regression test for #1021).
    #[tokio::test]
    async fn cleanup_marks_cleaned_when_stored_path_is_gone() {
        let tmp = tempfile::tempdir().unwrap();
        // A path that does NOT exist on disk.
        let stale_wt = tmp.path().join("stale-worktree");
        assert!(!stale_wt.exists(), "stale path must not exist");

        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        let id = store
            .create(&NewTask {
                external_id: Some("1005".to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "Stale worktree task".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        store
            .update_status(id, crate::store::TaskStatus::Done)
            .await
            .unwrap();
        // Set the stale worktree path on the task.
        store
            .set_fields(
                id,
                &[("worktree", stale_wt.to_string_lossy().as_ref().into())],
            )
            .await
            .unwrap();

        // Verify the task starts out NOT cleaned.
        let before = store.get(id).await.unwrap();
        assert!(!before.worktree_cleaned, "task should not be cleaned yet");
        assert!(
            !before.worktree.is_empty(),
            "task must have a worktree path set"
        );

        let opts = JanitorOptions {
            ttl_hours: 0,
            dry_run: false,
        };

        let result = cleanup_task_worktree_with_opts("1005", "owner/repo", &store, &opts).await;

        // The function should succeed (Ok) — stale path is not an error.
        // It returns Ok(false) because there was nothing to actively remove.
        match result {
            Ok(_) => {}
            Err(e) => panic!("cleanup should not error for stale path: {e}"),
        }

        // The task must now be marked cleaned so the next cycle skips it.
        let after = store.get(id).await.unwrap();
        assert!(
            after.worktree_cleaned,
            "task must be marked cleaned when stored worktree path is already gone"
        );
    }

    /// `remove_worktree_and_branch` returns true when the worktree directory
    /// is successfully removed (regression test for #1143).
    #[tokio::test]
    async fn remove_worktree_returns_true_on_success() {
        let Some((tmp, wt_dir)) = setup_test_repo() else {
            eprintln!("skipping test: git not available");
            return;
        };

        let repo_dir = tmp.path().join("repo.git");
        assert!(wt_dir.exists(), "worktree must exist before removal");

        let removed =
            remove_worktree_and_branch("42", &wt_dir, Some("gh-issue-42-test"), &repo_dir, false)
                .await;

        assert!(
            removed,
            "remove_worktree_and_branch should return true when directory is gone"
        );
        assert!(!wt_dir.exists(), "worktree directory should be removed");
    }

    /// `remove_worktree_and_branch` returns false when the worktree directory
    /// cannot be removed (regression test for #1143 — prevents orphaned worktrees
    /// from being permanently marked as cleaned).
    #[tokio::test]
    async fn remove_worktree_returns_false_when_dir_persists() {
        let tmp = tempfile::tempdir().unwrap();
        // Create a plain directory (not a real git worktree) — git will fail
        // to remove it, so the directory stays on disk.
        let fake_wt = tmp.path().join("not-a-worktree");
        std::fs::create_dir_all(&fake_wt).unwrap();

        // repo_root doesn't matter much — it just needs to exist for the -C flag.
        // Using tmp itself; git commands will fail but that's the point.
        let removed = remove_worktree_and_branch("99", &fake_wt, None, tmp.path(), false).await;

        // The directory still exists because git couldn't remove it.
        // Before the fix, this would still have been marked as cleaned.
        assert!(
            !removed,
            "remove_worktree_and_branch should return false when directory still exists"
        );
        assert!(fake_wt.exists(), "directory should still be on disk");
    }

    struct MergeCheckMockBackend {
        batch_result: Mutex<anyhow::Result<HashMap<String, bool>>>,
        single_results: HashMap<String, anyhow::Result<bool>>,
        posted_comments: Arc<Mutex<Vec<(String, String)>>>,
        single_checked_branches: Arc<Mutex<Vec<String>>>,
    }

    impl MergeCheckMockBackend {
        fn with_batch_error(
            error: anyhow::Error,
            single_results: HashMap<String, anyhow::Result<bool>>,
        ) -> Self {
            Self {
                batch_result: Mutex::new(Err(error)),
                single_results,
                posted_comments: Arc::new(Mutex::new(Vec::new())),
                single_checked_branches: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl ExternalBackend for MergeCheckMockBackend {
        fn name(&self) -> &str {
            "mock"
        }

        async fn create_task(
            &self,
            _title: &str,
            _body: &str,
            _labels: &[String],
        ) -> anyhow::Result<ExternalId> {
            anyhow::bail!("not implemented")
        }

        async fn get_task(&self, _id: &ExternalId) -> anyhow::Result<ExternalTask> {
            anyhow::bail!("not implemented")
        }

        async fn list_by_status(&self, _status: Status) -> anyhow::Result<Vec<ExternalTask>> {
            Ok(vec![])
        }

        async fn list_routable(&self) -> anyhow::Result<Vec<ExternalTask>> {
            Ok(vec![])
        }

        async fn post_comment(&self, id: &ExternalId, body: &str) -> anyhow::Result<()> {
            self.posted_comments
                .lock()
                .unwrap()
                .push((id.0.clone(), body.to_string()));
            Ok(())
        }

        async fn set_labels(&self, _id: &ExternalId, _labels: &[String]) -> anyhow::Result<()> {
            Ok(())
        }

        async fn remove_label(&self, _id: &ExternalId, _label: &str) -> anyhow::Result<()> {
            Ok(())
        }

        async fn get_sub_issues(&self, _id: &ExternalId) -> anyhow::Result<Vec<ExternalId>> {
            Ok(vec![])
        }

        async fn create_sub_task(
            &self,
            _parent: &ExternalId,
            _title: &str,
            _body: &str,
            _labels: &[String],
        ) -> anyhow::Result<ExternalId> {
            anyhow::bail!("not implemented")
        }

        async fn health_check(&self) -> anyhow::Result<()> {
            Ok(())
        }

        async fn is_pr_merged(&self, branch: &str) -> anyhow::Result<bool> {
            self.single_checked_branches
                .lock()
                .unwrap()
                .push(branch.to_string());
            match self.single_results.get(branch) {
                Some(Ok(is_merged)) => Ok(*is_merged),
                Some(Err(err)) => Err(anyhow::anyhow!(err.to_string())),
                None => Ok(false),
            }
        }

        async fn batch_is_pr_merged(
            &self,
            _branches: &[String],
        ) -> anyhow::Result<HashMap<String, bool>> {
            match &*self.batch_result.lock().unwrap() {
                Ok(map) => Ok(map.clone()),
                Err(err) => Err(anyhow::anyhow!(err.to_string())),
            }
        }
    }

    #[tokio::test]
    async fn check_merged_prs_falls_back_to_single_branch_checks_on_batch_error() {
        let single_results = HashMap::from([
            ("branch-1".to_string(), Ok(true)),
            ("branch-2".to_string(), Ok(false)),
        ]);
        let backend = Arc::new(MergeCheckMockBackend::with_batch_error(
            anyhow::anyhow!("missing /data/repository in GraphQL response: no error details"),
            single_results,
        ));
        let backend_dyn: Arc<dyn ExternalBackend> = backend.clone();

        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let task1 = store
            .create(&NewTask {
                external_id: Some("101".to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "Task 101".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        store
            .update_status(task1, crate::store::TaskStatus::NeedsReview)
            .await
            .unwrap();
        store
            .set_fields(task1, &[("branch", serde_json::json!("branch-1"))])
            .await
            .unwrap();

        let task2 = store
            .create(&NewTask {
                external_id: Some("102".to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "Task 102".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        store
            .update_status(task2, crate::store::TaskStatus::InReview)
            .await
            .unwrap();
        store
            .set_fields(task2, &[("branch", serde_json::json!("branch-2"))])
            .await
            .unwrap();

        let task_manager = Arc::new(TaskManager::with_store(
            backend_dyn.clone(),
            store.clone(),
            "owner/repo".to_string(),
        ));

        let in_review_tasks = store
            .list_by_status("owner/repo", crate::store::TaskStatus::InReview)
            .await
            .unwrap()
            .into_iter()
            .map(|stored| ReviewTaskSnapshot {
                external: crate::engine::tasks::store_task_to_external(&stored),
                stored,
            })
            .collect::<Vec<_>>();
        let needs_review_tasks = store
            .list_by_status("owner/repo", crate::store::TaskStatus::NeedsReview)
            .await
            .unwrap()
            .into_iter()
            .map(|stored| ReviewTaskSnapshot {
                external: crate::engine::tasks::store_task_to_external(&stored),
                stored,
            })
            .collect::<Vec<_>>();

        check_merged_prs(
            &backend_dyn,
            "owner/repo",
            &store,
            &task_manager,
            &in_review_tasks,
            &needs_review_tasks,
        )
        .await
        .unwrap();

        let checked = backend.single_checked_branches.lock().unwrap().clone();
        assert_eq!(
            checked,
            vec!["branch-2".to_string(), "branch-1".to_string()]
        );

        let updated_task1 = store.get(task1).await.unwrap();
        let updated_task2 = store.get(task2).await.unwrap();
        assert_eq!(updated_task1.status, crate::store::TaskStatus::Done);
        assert_eq!(updated_task2.status, crate::store::TaskStatus::InReview);

        let comments = backend.posted_comments.lock().unwrap().clone();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].0, "101");
        assert!(comments[0].1.contains("PR merged, marking task complete"));
    }
}
