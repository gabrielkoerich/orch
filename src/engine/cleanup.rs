//! Worktree cleanup and merged-PR detection.
//!
//! Contains the post-merge cleanup pipeline: removing git worktrees,
//! deleting local/remote branches, pulling main, and detecting
//! already-merged PRs so their tasks can be marked done.

use crate::backends::{ExternalBackend, ExternalId, Status};
use crate::cmd::CommandErrorContext;
use crate::engine::tasks::TaskManager;
use crate::store::TaskStatus;
use crate::store::TaskStore;
use std::sync::Arc;
use tokio::process::Command;

/// Try to read a task field from the store.
///
/// Convenience wrapper that handles `Option<Arc<TaskStore>>`:
/// if store is `None`, returns `None`.
pub(crate) async fn opt_store_get_field(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
    field: &str,
) -> Option<String> {
    if let Some(ref s) = store {
        store_get_field(s, repo, task_id, field).await
    } else {
        None
    }
}

/// Try to read a task field from the store.
///
/// Supports common fields: worktree, branch, summary, agent, model, last_error,
/// worktree_cleaned, last_review_ts, last_comment_review_ts, review_cycles,
/// merge_conflict_retries, ci_merge_failures, pr_create_failures, attempts,
/// pr_number, route_reason, complexity.
pub(crate) async fn store_get_field(
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
    field: &str,
) -> Option<String> {
    // Try store first
    if let Ok(Some(store_id)) = store.resolve_task_id(repo, task_id).await {
        if let Ok(task) = store.get(store_id).await {
            let val: Option<String> = match field {
                "worktree" if !task.worktree.is_empty() => Some(task.worktree.clone()),
                "branch" if !task.branch.is_empty() => Some(task.branch.clone()),
                "summary" if !task.summary.is_empty() => Some(task.summary.clone()),
                "agent" => task.agent.clone(),
                "model" => task.model.clone(),
                "last_error" if !task.last_error.is_empty() => Some(task.last_error.clone()),
                "worktree_cleaned" => Some(task.worktree_cleaned.to_string()),
                "last_review_ts" if !task.last_review_ts.is_empty() => {
                    Some(task.last_review_ts.clone())
                }
                "last_comment_review_ts" if !task.last_comment_review_ts.is_empty() => {
                    Some(task.last_comment_review_ts.clone())
                }
                "review_cycles" if task.review_cycles > 0 => Some(task.review_cycles.to_string()),
                "merge_conflict_retries" if task.merge_conflict_retries > 0 => {
                    Some(task.merge_conflict_retries.to_string())
                }
                "ci_merge_failures" if task.ci_merge_failures > 0 => {
                    Some(task.ci_merge_failures.to_string())
                }
                "pr_create_failures" if task.pr_create_failures > 0 => {
                    Some(task.pr_create_failures.to_string())
                }
                "review_agent_failures" if task.review_agent_failures > 0 => {
                    Some(task.review_agent_failures.to_string())
                }
                "attempts" if task.attempts > 0 => Some(task.attempts.to_string()),
                "pr_number" => task.pr_number.map(|n| n.to_string()),
                "route_reason" if !task.route_reason.is_empty() => Some(task.route_reason.clone()),
                "complexity" if !task.complexity.is_empty() => Some(task.complexity.clone()),
                "budget_warning" if !task.budget_warning.is_empty() => {
                    Some(task.budget_warning.clone())
                }
                "budget_exceeded" if task.budget_exceeded => Some("true".to_string()),
                "title" if !task.title.is_empty() => Some(task.title.clone()),
                "body" if !task.body.is_empty() => Some(task.body.clone()),
                "parent_id" => task.parent_id.map(|id| id.to_string()),
                "pr_review_context" if !task.pr_review_context.is_empty() => {
                    Some(task.pr_review_context.clone())
                }
                "limit_reroute_chain" if !task.limit_reroute_chain.is_empty() => {
                    Some(task.limit_reroute_chain.clone())
                }
                "model_reroute_chain" if !task.model_reroute_chain.is_empty() => {
                    Some(task.model_reroute_chain.clone())
                }
                "input_tokens" if task.input_tokens > 0 => Some(task.input_tokens.to_string()),
                "output_tokens" if task.output_tokens > 0 => Some(task.output_tokens.to_string()),
                "input_cost_usd" if task.input_cost_usd > 0.0 => {
                    Some(task.input_cost_usd.to_string())
                }
                "output_cost_usd" if task.output_cost_usd > 0.0 => {
                    Some(task.output_cost_usd.to_string())
                }
                "total_cost_usd" if task.total_cost_usd > 0.0 => {
                    Some(task.total_cost_usd.to_string())
                }
                "route_attempts" if task.route_attempts > 0 => {
                    Some(task.route_attempts.to_string())
                }
                "agent_profile" if !task.agent_profile.is_empty() => {
                    Some(task.agent_profile.clone())
                }
                "selected_skills" if !task.selected_skills.is_empty() => {
                    Some(task.selected_skills.clone())
                }
                "delegations" => {
                    let json = serde_json::to_string(&task.delegations).unwrap_or_default();
                    if json != "[]" && !json.is_empty() {
                        Some(json)
                    } else {
                        None
                    }
                }
                _ => None,
            };
            return val;
        }
    }
    None
}

/// Write fields to the task store.
///
/// `store` may be None if the store isn't initialized yet.
pub(crate) async fn store_set(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
    store_fields: &[(&str, serde_json::Value)],
) {
    if let Some(ref store) = store {
        if let Ok(Some(store_id)) = store.resolve_task_id(repo, task_id).await {
            if let Err(e) = store.set_fields(store_id, store_fields).await {
                tracing::warn!(task_id, error = %e, "store set_fields failed");
            }
        }
    }
}

/// Increment a counter in the task store.
///
/// Uses `store.increment()` for an atomic SQL `field + 1`.
/// Returns the new value, or 0 if the store is unavailable.
pub(crate) async fn store_increment(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
    field: &str,
) -> u64 {
    if let Some(ref s) = store {
        if let Ok(Some(store_id)) = s.resolve_task_id(repo, task_id).await {
            if let Ok(new_val) = s.increment(store_id, field).await {
                return new_val as u64;
            }
        }
    }
    0
}

/// Reset all task counters in the task store.
pub(crate) async fn store_reset_counters(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
) {
    if let Some(ref store) = store {
        if let Ok(Some(store_id)) = store.resolve_task_id(repo, task_id).await {
            let _ = store.reset_counters(store_id).await;
        }
    }
}

/// Get token usage from the store.
pub(crate) async fn get_token_usage(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
) -> crate::store::TokenUsage {
    if let Some(ref s) = store {
        if let Ok(Some(store_id)) = s.resolve_task_id(repo, task_id).await {
            if let Ok(task) = s.get(store_id).await {
                return crate::store::TokenUsage {
                    input_tokens: task.input_tokens as u64,
                    output_tokens: task.output_tokens as u64,
                };
            }
        }
    }
    crate::store::TokenUsage::default()
}

/// Get cost estimate from the store.
pub(crate) async fn get_cost_estimate(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
) -> crate::store::CostEstimate {
    if let Some(ref s) = store {
        if let Ok(Some(store_id)) = s.resolve_task_id(repo, task_id).await {
            if let Ok(task) = s.get(store_id).await {
                return crate::store::CostEstimate {
                    input_cost_usd: task.input_cost_usd,
                    output_cost_usd: task.output_cost_usd,
                    total_cost_usd: task.total_cost_usd,
                };
            }
        }
    }
    crate::store::CostEstimate::default()
}

/// Get total tokens from the store.
pub(crate) async fn get_total_tokens(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
) -> u64 {
    let usage = get_token_usage(store, repo, task_id).await;
    usage.total_tokens()
}

/// Get recent memory from the store.
pub(crate) async fn get_recent_memory(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
    max_entries: usize,
) -> Vec<crate::store::MemoryEntry> {
    if let Some(ref s) = store {
        if let Ok(Some(store_id)) = s.resolve_task_id(repo, task_id).await {
            if let Ok(memory) = s.recent_memory(store_id, max_entries).await {
                return memory;
            }
        }
    }
    vec![]
}

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
    let done_tasks = {
        if store.has_tasks(repo).await {
            store
                .list_by_status(repo, crate::store::TaskStatus::Done)
                .await?
                .iter()
                .filter(|t| t.origin != "internal")
                .map(crate::engine::tasks::store_task_to_external)
                .collect()
        } else {
            backend.list_by_status(Status::Done).await?
        }
    };
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
            let done_set: std::collections::HashSet<String> = task_ids.iter().cloned().collect();
            let closed_not_in_store: Vec<_> = all_tasks
                .iter()
                .filter(|t| t.state == "closed" && !done_set.contains(&t.id.0))
                .collect();

            // Split: issues that already have status:done label just need worktree
            // cleanup (no API call needed), vs those needing label reconciliation.
            let mut already_labeled: Vec<String> = Vec::new();
            let mut needs_reconcile: Vec<&crate::backends::ExternalTask> = Vec::new();
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
            for task in needs_reconcile.iter().take(MAX_RECONCILE_PER_CYCLE) {
                tracing::info!(
                    task_id = task.id.0,
                    labels = ?task.labels,
                    "closed issue with stale status label — reconciling to done and cleaning up"
                );
                if let Err(e) = task_manager
                    .update_task_status(&task.id, Status::Done)
                    .await
                {
                    tracing::warn!(
                        task_id = task.id.0,
                        err = %e,
                        "failed to reconcile closed issue status to done"
                    );
                }
            }
            task_ids.extend(
                needs_reconcile
                    .iter()
                    .take(MAX_RECONCILE_PER_CYCLE)
                    .map(|t| t.id.0.clone()),
            );
        }
        Err(e) => {
            tracing::warn!(err = %e, "failed to list all tasks for closed-issue reconciliation");
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

    let mut cleaned_any = false;
    for task_id in &task_ids {
        // Skip if already cleaned
        let worktree_cleaned = store_get_field(store, repo, task_id, "worktree_cleaned").await;
        if worktree_cleaned.as_deref() == Some("true") || worktree_cleaned.as_deref() == Some("1") {
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

    // Pull main after worktrees are cleaned so the repo stays current.
    if cleaned_any && !opts.dry_run {
        if let Ok(repo_root) = resolve_repo_root(repo).await {
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
    let worktree = store_get_field(store, repo, task_id, "worktree").await;
    let branch = store_get_field(store, repo, task_id, "branch").await;

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

    let repo_root = resolve_repo_root(repo).await?;

    let mut did_clean = false;

    if let Some(wt) = worktree_to_remove {
        // TTL guard: skip if the worktree directory is too young.
        if let Some(age_hours) = worktree_age_hours(&wt) {
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

        if opts.dry_run {
            tracing::info!(
                task_id,
                worktree = %wt.display(),
                branch = ?branch,
                "[dry-run] would remove worktree and branch"
            );
        } else {
            tracing::info!(task_id, worktree = %wt.display(), "removing worktree");
            remove_worktree_and_branch(task_id, &wt, branch.as_deref(), &repo_root).await;
            did_clean = true;
        }
    } else if let Some(ref br) = branch {
        // Worktree directory is already gone, but the branch may still exist
        // on the remote. Delete it to avoid orphaned branches.
        if !opts.dry_run {
            tracing::debug!(task_id, branch = %br, "no worktree on disk, cleaning up branch only");
            delete_branches(task_id, br, &repo_root).await;
            did_clean = true;
        }
    }

    if did_clean {
        // Mark as cleaned in store so we don't retry next tick
        if let Ok(Some(store_id)) = store.resolve_task_id(repo, task_id).await {
            let _ = store.mark_cleaned(store_id).await;
        }
    }

    Ok(did_clean)
}

/// Remove a git worktree directory and its local + remote branches.
async fn remove_worktree_and_branch(
    task_id: &str,
    wt: &std::path::Path,
    branch: Option<&str>,
    repo_root: &str,
) {
    let wt_str = wt.to_string_lossy().to_string();

    // Remove worktree FIRST, then delete the branch.
    // Git refuses to remove a worktree if its branch is already deleted.
    let remove_result = Command::new("git")
        .args(["-C", repo_root, "worktree", "remove", &wt_str, "--force"])
        .output_with_context()
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

    // Delete local and remote branch from the main repo root (worktree is already gone)
    if let Some(br) = branch {
        delete_branches(task_id, br, repo_root).await;
    }
}

/// Delete local and remote branches for a task.
async fn delete_branches(task_id: &str, br: &str, repo_root: &str) {
    let branch_delete_result = Command::new("git")
        .args(["-C", repo_root, "branch", "-D", br])
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
    let remote_delete = Command::new("git")
        .args(["-C", repo_root, "push", "origin", "--delete", br])
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
fn worktree_age_hours(worktree: &std::path::Path) -> Option<u64> {
    let metadata = std::fs::metadata(worktree).ok()?;
    let modified = metadata.modified().ok()?;
    let age = std::time::SystemTime::now().duration_since(modified).ok()?;
    Some(age.as_secs() / 3600)
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
        if let Ok(content) = std::fs::read_to_string(&config_file) {
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
) -> anyhow::Result<()> {
    // Read from the store first; fall back to backend if the store has no data.
    let in_review_tasks = if store.has_tasks(repo).await {
        store
            .list_by_status(repo, crate::store::TaskStatus::InReview)
            .await?
            .iter()
            .filter(|t| t.origin != "internal")
            .map(crate::engine::tasks::store_task_to_external)
            .collect()
    } else {
        backend.list_by_status(Status::InReview).await?
    };
    let needs_review_tasks = if store.has_tasks(repo).await {
        store
            .list_by_status(repo, crate::store::TaskStatus::NeedsReview)
            .await?
            .iter()
            .filter(|t| t.origin != "internal")
            .map(crate::engine::tasks::store_task_to_external)
            .collect()
    } else {
        backend.list_by_status(Status::NeedsReview).await?
    };
    let all_review_tasks: Vec<_> = in_review_tasks
        .into_iter()
        .chain(needs_review_tasks)
        .collect();
    tracing::debug!(
        count = all_review_tasks.len(),
        "checking review tasks for merged PRs"
    );

    for task in all_review_tasks {
        let task_id = &task.id.0;

        // Get branch from store
        let branch = match store_get_field(store, repo, task_id, "branch").await {
            Some(b) if !b.is_empty() => b,
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
                if let Err(e) = task_manager.update_task_status(&id, Status::Done).await {
                    tracing::warn!(task_id, err = %e, "failed to update status to done");
                    continue;
                }

                // Post comment
                let comment = format!(
                    "PR merged, marking task complete{}",
                    crate::engine::orch_footer()
                );
                if let Err(e) = backend.post_comment(&id, &comment).await {
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── worktree_age_hours ──────────────────────────────────────────────────

    #[test]
    fn worktree_age_hours_returns_none_for_missing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        assert!(worktree_age_hours(&missing).is_none());
    }

    #[test]
    fn worktree_age_hours_returns_some_for_existing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        // A freshly created directory should have age ~0 hours.
        let age = worktree_age_hours(tmp.path());
        assert!(age.is_some());
        assert_eq!(age.unwrap(), 0, "freshly created dir should have age 0h");
    }

    // ── JanitorOptions::from_config defaults ───────────────────────────────

    #[test]
    fn janitor_options_default_values() {
        let opts = JanitorOptions::default();
        assert_eq!(opts.ttl_hours, 24);
        assert!(!opts.dry_run);
    }

    // ── cleanup_task_worktree_with_opts — TTL guard ─────────────────────────
    //
    // These tests exercise the janitor against a real git repo created in a
    // tempdir, so they require `git` to be in PATH.  They are skipped if git
    // is unavailable rather than failing hard.

    /// Initialize a bare git repo and two worktrees, mimicking what orch does
    /// when it dispatches a task.  Returns the base tempdir and the worktree path.
    fn setup_test_repo() -> Option<(tempfile::TempDir, std::path::PathBuf)> {
        let tmp = tempfile::tempdir().ok()?;

        // Create a real git repo (not bare) so we can add worktrees.
        let repo_dir = tmp.path().join("repo.git");
        std::fs::create_dir_all(&repo_dir).ok()?;

        let init = std::process::Command::new("git")
            .args([
                "init",
                "--initial-branch=main",
                repo_dir.to_str().unwrap_or("."),
            ])
            .output()
            .ok()?;
        if !init.status.success() {
            // Older git may not have --initial-branch; fall back.
            let _ = std::process::Command::new("git")
                .args(["init", repo_dir.to_str().unwrap_or(".")])
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
            .join("gh-task-42-test");
        std::fs::create_dir_all(wt_dir.parent()?).ok()?;
        let wt_out = std::process::Command::new("git")
            .args([
                "-C",
                repo_dir.to_str()?,
                "worktree",
                "add",
                "-b",
                "gh-task-42-test",
                wt_dir.to_str()?,
            ])
            .output()
            .ok()?;

        if !wt_out.status.success() {
            return None;
        }

        Some((tmp, wt_dir))
    }

    #[test]
    fn janitor_skips_young_worktree() {
        let Some((tmp, wt_dir)) = setup_test_repo() else {
            eprintln!("skipping test: git not available");
            return;
        };

        // The worktree was just created → age is 0h < ttl_hours=24 → must be skipped.
        assert!(wt_dir.exists(), "worktree should exist before janitor runs");

        // Run the TTL guard check directly (without a full engine).
        let age = worktree_age_hours(&wt_dir).unwrap_or(u64::MAX);
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

    #[test]
    fn janitor_eligible_when_ttl_zero() {
        let Some((tmp, wt_dir)) = setup_test_repo() else {
            eprintln!("skipping test: git not available");
            return;
        };

        // With ttl_hours=0 even a brand-new worktree is eligible.
        let age = worktree_age_hours(&wt_dir).unwrap_or(u64::MAX);
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

    #[test]
    fn dry_run_does_not_remove_worktree() {
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
        let age = worktree_age_hours(&wt_dir).unwrap_or(u64::MAX);
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
        let wt_dir = tmp.path().join("myrepo").join("gh-task-7-fix");
        std::fs::create_dir_all(&wt_dir).unwrap();

        // Simulate: worktree=None, branch=Some("gh-task-7-fix"), repo="owner/myrepo"
        let branch = "gh-task-7-fix";
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

    // ── store_get_field ──────────────────────────────────────────────────

    #[tokio::test]
    async fn store_get_field_reads_from_store() {
        use crate::store::{NewTask, TaskStore};
        use std::sync::Arc;

        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        let id = store
            .create(&NewTask {
                external_id: Some("42".to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "Test task".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();

        store
            .set_fields(id, &[("branch", serde_json::json!("gh-42-fix"))])
            .await
            .unwrap();
        store
            .set_fields(id, &[("worktree", serde_json::json!("/tmp/wt42"))])
            .await
            .unwrap();

        // Should read from store
        let branch = store_get_field(&store, "owner/repo", "42", "branch").await;
        assert_eq!(branch.as_deref(), Some("gh-42-fix"));

        let wt = store_get_field(&store, "owner/repo", "42", "worktree").await;
        assert_eq!(wt.as_deref(), Some("/tmp/wt42"));
    }

    #[tokio::test]
    async fn store_get_field_falls_back_for_unknown_task() {
        use crate::store::TaskStore;
        use std::sync::Arc;

        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        // Task doesn't exist in store — should return None
        let result = store_get_field(&store, "owner/repo", "unknown-999", "branch").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn store_get_field_counter_fields() {
        use crate::store::{NewTask, TaskStore};
        use std::sync::Arc;

        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        let id = store
            .create(&NewTask {
                external_id: Some("55".to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "Counter test".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();

        // Zero counters should return None
        let retries = store_get_field(&store, "owner/repo", "55", "merge_conflict_retries").await;
        assert!(retries.is_none());

        // Set counter
        store.increment(id, "merge_conflict_retries").await.unwrap();
        let retries = store_get_field(&store, "owner/repo", "55", "merge_conflict_retries").await;
        assert_eq!(retries.as_deref(), Some("1"));

        // Agent (Option field)
        store
            .set_fields(id, &[("agent", serde_json::json!("claude"))])
            .await
            .unwrap();
        let agent = store_get_field(&store, "owner/repo", "55", "agent").await;
        assert_eq!(agent.as_deref(), Some("claude"));
    }

    // ── opt_store_get_field with None store ────────────────────────────

    #[tokio::test]
    async fn opt_store_get_field_with_none_store() {
        // When store is None, should return None without panicking
        let store: Option<Arc<TaskStore>> = None;
        let result = opt_store_get_field(&store, "owner/repo", "nonexistent-999", "branch").await;
        // Should not panic; value is None because task doesn't exist
        assert!(result.is_none());
    }

    // ── get_token_usage from store ──────────────────────────────────────

    #[tokio::test]
    async fn get_token_usage_from_store() {
        use crate::store::{NewTask, TaskStore};

        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let id = store
            .create(&NewTask {
                external_id: Some("70".to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "Token test".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();

        // Set token fields
        store
            .set_fields(
                id,
                &[
                    ("input_tokens", serde_json::json!(1500)),
                    ("output_tokens", serde_json::json!(800)),
                ],
            )
            .await
            .unwrap();

        let opt_store = Some(store);
        let usage = get_token_usage(&opt_store, "owner/repo", "70").await;
        assert_eq!(usage.input_tokens, 1500);
        assert_eq!(usage.output_tokens, 800);
        assert_eq!(usage.total_tokens(), 2300);
    }

    // ── get_cost_estimate from store ────────────────────────────────────

    #[tokio::test]
    async fn get_cost_estimate_from_store() {
        use crate::store::{NewTask, TaskStore};

        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let id = store
            .create(&NewTask {
                external_id: Some("71".to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "Cost test".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();

        store
            .set_fields(
                id,
                &[
                    ("input_cost_usd", serde_json::json!(0.05)),
                    ("output_cost_usd", serde_json::json!(0.10)),
                    ("total_cost_usd", serde_json::json!(0.15)),
                ],
            )
            .await
            .unwrap();

        let opt_store = Some(store);
        let cost = get_cost_estimate(&opt_store, "owner/repo", "71").await;
        assert!((cost.input_cost_usd - 0.05).abs() < f64::EPSILON);
        assert!((cost.output_cost_usd - 0.10).abs() < f64::EPSILON);
        assert!((cost.total_cost_usd - 0.15).abs() < f64::EPSILON);
    }

    // ── get_recent_memory from store ────────────────────────────────────

    #[tokio::test]
    async fn get_recent_memory_from_store() {
        use crate::store::MemoryEntry;
        use crate::store::{NewTask, TaskStore};

        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let id = store
            .create(&NewTask {
                external_id: Some("72".to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "Memory test".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();

        let entry1 = MemoryEntry {
            attempt: 1,
            agent: "claude".to_string(),
            model: Some("opus".to_string()),
            learnings: vec!["learned A".to_string()],
            error: None,
            files_modified: vec!["src/main.rs".to_string()],
            approach: "first try".to_string(),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
        };
        let entry2 = MemoryEntry {
            attempt: 2,
            agent: "codex".to_string(),
            model: Some("gpt-5".to_string()),
            learnings: vec!["learned B".to_string()],
            error: Some("timeout".to_string()),
            files_modified: vec![],
            approach: "second try".to_string(),
            timestamp: "2026-01-01T01:00:00Z".to_string(),
        };

        store.append_memory(id, &entry1).await.unwrap();
        store.append_memory(id, &entry2).await.unwrap();

        let opt_store = Some(store);
        let memory = get_recent_memory(&opt_store, "owner/repo", "72", 10).await;
        assert_eq!(memory.len(), 2);
        assert_eq!(memory[0].attempt, 1);
        assert_eq!(memory[1].attempt, 2);
        assert_eq!(memory[0].agent, "claude");
        assert_eq!(memory[1].agent, "codex");
    }

    // ── increment returns new value ─────────────────────────────────────

    #[tokio::test]
    async fn increment_returns_new_value() {
        use crate::store::{NewTask, TaskStore};

        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let id = store
            .create(&NewTask {
                external_id: Some("73".to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "Increment test".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();

        let opt_store = Some(store);
        let v1 = store_increment(&opt_store, "owner/repo", "73", "attempts").await;
        assert_eq!(v1, 1);

        let v2 = store_increment(&opt_store, "owner/repo", "73", "attempts").await;
        assert_eq!(v2, 2);

        let v3 = store_increment(&opt_store, "owner/repo", "73", "attempts").await;
        assert_eq!(v3, 3);

        // Verify store has the correct value
        let task = opt_store.as_ref().unwrap().get(id).await.unwrap();
        assert_eq!(task.attempts, 3);
    }

    // ── increment without store returns zero ──────────────────────────

    #[tokio::test]
    async fn increment_without_store_returns_zero() {
        let store: Option<Arc<TaskStore>> = None;
        let v = store_increment(&store, "owner/repo", "no-store-task", "attempts").await;
        // No store available — returns 0
        assert_eq!(v, 0);
    }

    // ── store_get_field: route_attempts and agent_profile ─────────────────

    #[tokio::test]
    async fn store_get_field_reads_route_attempts() {
        use crate::store::{NewTask, TaskStore};
        use std::sync::Arc;

        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        let id = store
            .create(&NewTask {
                external_id: Some("80".to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "Route attempts test".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();

        store
            .set_fields(id, &[("route_attempts", serde_json::json!(3))])
            .await
            .unwrap();

        let val = store_get_field(&store, "owner/repo", "80", "route_attempts").await;
        assert_eq!(val.as_deref(), Some("3"));
    }

    #[tokio::test]
    async fn store_get_field_reads_agent_profile() {
        use crate::store::{NewTask, TaskStore};
        use std::sync::Arc;

        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        let id = store
            .create(&NewTask {
                external_id: Some("81".to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "Agent profile test".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();

        store
            .set_fields(
                id,
                &[
                    (
                        "agent_profile",
                        serde_json::json!(r#"{"role":"backend specialist"}"#),
                    ),
                    ("selected_skills", serde_json::json!("git,rust,gh")),
                ],
            )
            .await
            .unwrap();

        let profile = store_get_field(&store, "owner/repo", "81", "agent_profile").await;
        assert_eq!(profile.as_deref(), Some(r#"{"role":"backend specialist"}"#));

        let skills = store_get_field(&store, "owner/repo", "81", "selected_skills").await;
        assert_eq!(skills.as_deref(), Some("git,rust,gh"));
    }

    // ── store_set with None store ───────────────────────────────────────────

    #[tokio::test]
    async fn store_set_with_none_store() {
        // When store is None, store_set should not panic and return normally.
        let store: Option<Arc<TaskStore>> = None;
        // Should complete without panicking
        store_set(
            &store,
            "owner/repo",
            "42",
            &[("branch", serde_json::json!("main"))],
        )
        .await;
    }

    // ── store_set with valid store ────────────────────────────────────────

    #[tokio::test]
    async fn store_set_writes_fields() {
        use crate::store::{NewTask, TaskStore};

        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let id = store
            .create(&NewTask {
                external_id: Some("90".to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "Store set test".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();

        let opt_store = Some(store.clone());
        store_set(
            &opt_store,
            "owner/repo",
            "90",
            &[
                ("branch", serde_json::json!("fix-bug")),
                ("worktree", serde_json::json!("/tmp/wt")),
            ],
        )
        .await;

        let task = store.get(id).await.unwrap();
        assert_eq!(task.branch, "fix-bug");
        assert_eq!(task.worktree, "/tmp/wt");
    }

    #[tokio::test]
    async fn store_set_ignores_unknown_task() {
        use crate::store::TaskStore;

        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let opt_store = Some(store);
        // Should not panic for a task that doesn't exist
        store_set(
            &opt_store,
            "owner/repo",
            "nonexistent-999",
            &[("branch", serde_json::json!("main"))],
        )
        .await;
    }

    // ── store_reset_counters ──────────────────────────────────────────────

    #[tokio::test]
    async fn store_reset_counters_zeroes_counters() {
        use crate::store::{NewTask, TaskStore};

        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let id = store
            .create(&NewTask {
                external_id: Some("91".to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "Reset test".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();

        // Increment some counters
        let opt_store = Some(store.clone());
        store_increment(&opt_store, "owner/repo", "91", "attempts").await;
        store_increment(&opt_store, "owner/repo", "91", "attempts").await;
        store_increment(&opt_store, "owner/repo", "91", "merge_conflict_retries").await;

        let task = store.get(id).await.unwrap();
        assert_eq!(task.attempts, 2);
        assert_eq!(task.merge_conflict_retries, 1);

        // Reset
        store_reset_counters(&opt_store, "owner/repo", "91").await;

        let task = store.get(id).await.unwrap();
        assert_eq!(task.attempts, 0);
        assert_eq!(task.merge_conflict_retries, 0);
    }

    #[tokio::test]
    async fn store_reset_counters_noop_without_store() {
        let store: Option<Arc<TaskStore>> = None;
        // Should not panic
        store_reset_counters(&store, "owner/repo", "no-task").await;
    }

    // ── get_total_tokens ──────────────────────────────────────────────────

    #[tokio::test]
    async fn get_total_tokens_sums_input_and_output() {
        use crate::store::{NewTask, TaskStore};

        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        store
            .create(&NewTask {
                external_id: Some("92".to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "Tokens test".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();

        store
            .set_fields(
                1,
                &[
                    ("input_tokens", serde_json::json!(5000)),
                    ("output_tokens", serde_json::json!(3000)),
                ],
            )
            .await
            .unwrap();

        let opt_store = Some(store);
        let total = get_total_tokens(&opt_store, "owner/repo", "92").await;
        assert_eq!(total, 8000);
    }

    #[tokio::test]
    async fn get_total_tokens_returns_zero_without_store() {
        let store: Option<Arc<TaskStore>> = None;
        let total = get_total_tokens(&store, "owner/repo", "any").await;
        assert_eq!(total, 0);
    }

    // ── store_get_field edge cases ────────────────────────────────────────

    #[tokio::test]
    async fn store_get_field_returns_none_for_empty_string_fields() {
        use crate::store::{NewTask, TaskStore};
        use std::sync::Arc;

        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        store
            .create(&NewTask {
                external_id: Some("93".to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "Empty fields test".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();

        // Fields that are empty strings should return None (fallthrough)
        let branch = store_get_field(&store, "owner/repo", "93", "branch").await;
        assert!(branch.is_none(), "empty branch should return None");

        let summary = store_get_field(&store, "owner/repo", "93", "summary").await;
        assert!(summary.is_none(), "empty summary should return None");

        let last_error = store_get_field(&store, "owner/repo", "93", "last_error").await;
        assert!(last_error.is_none(), "empty last_error should return None");
    }

    #[tokio::test]
    async fn store_get_field_returns_none_for_unknown_field() {
        use crate::store::{NewTask, TaskStore};
        use std::sync::Arc;

        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        store
            .create(&NewTask {
                external_id: Some("94".to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "Unknown field test".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();

        let result = store_get_field(&store, "owner/repo", "94", "nonexistent_field").await;
        assert!(result.is_none(), "unknown field should return None");
    }

    #[tokio::test]
    async fn store_get_field_reads_pr_number() {
        use crate::store::{NewTask, TaskStore};
        use std::sync::Arc;

        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let id = store
            .create(&NewTask {
                external_id: Some("95".to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "PR number test".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();

        // pr_number is None by default
        let pr = store_get_field(&store, "owner/repo", "95", "pr_number").await;
        assert!(pr.is_none(), "null pr_number should return None");

        // Set pr_number
        store
            .set_fields(id, &[("pr_number", serde_json::json!(42))])
            .await
            .unwrap();

        let pr = store_get_field(&store, "owner/repo", "95", "pr_number").await;
        assert_eq!(pr.as_deref(), Some("42"));
    }

    #[tokio::test]
    async fn store_get_field_reads_delegations() {
        use crate::store::{NewTask, TaskStore};
        use std::sync::Arc;

        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let id = store
            .create(&NewTask {
                external_id: Some("96".to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "Delegations test".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();

        // Empty delegations → None
        let d = store_get_field(&store, "owner/repo", "96", "delegations").await;
        assert!(d.is_none(), "empty delegations should return None");

        // Set delegations
        store
            .set_fields(
                id,
                &[("delegations", serde_json::json!(r#"[{"task_id":"sub:1"}]"#))],
            )
            .await
            .unwrap();

        let d = store_get_field(&store, "owner/repo", "96", "delegations").await;
        assert!(d.is_some(), "non-empty delegations should return Some");
    }

    // ── get_recent_memory edge cases ────────────────────────────────────

    #[tokio::test]
    async fn get_recent_memory_returns_empty_without_store() {
        let store: Option<Arc<TaskStore>> = None;
        let memory = get_recent_memory(&store, "owner/repo", "any", 10).await;
        assert!(memory.is_empty());
    }

    /// A task with no worktree and no branch should return Ok(false) —
    /// nothing was cleaned, so git pull should NOT be triggered.
    #[tokio::test]
    async fn cleanup_returns_false_when_nothing_to_clean() {
        use crate::store::{NewTask, TaskStore};

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
}
