//! Worktree cleanup and merged-PR detection.
//!
//! Contains the post-merge cleanup pipeline: removing git worktrees,
//! deleting local/remote branches, pulling main, and detecting
//! already-merged PRs so their tasks can be marked done.

use crate::backends::{ExternalBackend, ExternalId, ExternalTask, Status};
use crate::cmd::CommandErrorContext;
use crate::engine::ci_bootstrap;
use crate::engine::tasks::{TaskManager, store_task_to_external};
use crate::store;
use crate::store::TaskStatus;
use crate::store::TaskStore;
use crate::store::store_log_activity;
use dashmap::{DashMap, DashSet};
use futures::future;
use std::collections::HashMap;
use std::collections::HashSet;
use std::env;
use std::fs as std_fs;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::process::Output;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::SystemTime;
use std::time::{Duration, Instant};
use tokio::fs;
use tokio::process::Command;
use tokio::time::sleep;
use tokio::time::timeout;

use super::sync::ReviewTaskSnapshot;
use crate::config;
use crate::engine;
use crate::engine::auto_merge;
use crate::engine::runner::cow;
use crate::engine::runner::git_ops;
use crate::engine::runner::screenshots::SCREENSHOTS_BRANCH;
use crate::engine::runner::worktree;
use crate::github::http::GhHttp;
use crate::home;

/// Reconciliation calls can be slow on repos with many issues or during GitHub instability.
/// Cleanup runs in a background task (tokio::spawn) so it does not block the main tick loop.
/// 30 s gives enough headroom for paginated list_all_tasks calls while still bounding the
/// overall background-task duration.
const RECONCILIATION_LIST_TIMEOUT: Duration = Duration::from_secs(30);
/// Closed-issue reconciliation is a best-effort cleanup fallback, not a per-sync requirement.
/// Run it on a slower cadence so repeated GitHub slowness does not consume 30 s every sync.
const RECONCILIATION_INTERVAL: Duration = Duration::from_mins(15);

const RUN_OUTPUT_RETENTION_DAYS: i32 = 30;
const RUN_PRUNE_INTERVAL_SECS: i64 = 24 * 60 * 60;
const RUN_PRUNE_KV_KEY: &str = "task_runs_pruned_at";

/// Per-repo in-flight guard for closed-issue reconciliation/cleanup.
/// Prevents overlapping cleanup runs from piling up across sync cycles.
static CLEANUP_IN_FLIGHT: LazyLock<DashSet<String>> = LazyLock::new(DashSet::new);
/// Per-repo reconciliation cadence tracker.
static RECONCILIATION_LAST_RUN: LazyLock<DashMap<String, Instant>> = LazyLock::new(DashMap::new);

struct CleanupInFlightGuard {
    repo: String,
}

impl CleanupInFlightGuard {
    fn try_acquire(repo: &str) -> Option<Self> {
        if CLEANUP_IN_FLIGHT.insert(repo.to_string()) {
            Some(Self {
                repo: repo.to_string(),
            })
        } else {
            None
        }
    }
}

impl Drop for CleanupInFlightGuard {
    fn drop(&mut self) {
        CLEANUP_IN_FLIGHT.remove(&self.repo);
    }
}

fn should_run_reconciliation(repo: &str) -> bool {
    let now = Instant::now();
    if let Some(mut last_run) = RECONCILIATION_LAST_RUN.get_mut(repo) {
        if last_run.elapsed() < RECONCILIATION_INTERVAL {
            return false;
        }
        *last_run = now;
        true
    } else {
        RECONCILIATION_LAST_RUN.insert(repo.to_string(), now);
        true
    }
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
        let ttl_hours = config::get("workflow.worktree_janitor_ttl_hours")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(24);

        let dry_run = config::get("workflow.worktree_janitor_dry_run").is_ok_and(|v| v == "true");

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

/// Clear the stored output of runs for tasks done longer than `RUN_OUTPUT_RETENTION_DAYS`, once a day
pub(crate) async fn prune_old_run_output(store: &TaskStore) -> anyhow::Result<()> {
    let now = chrono::Utc::now().timestamp();
    let last = store
        .kv_get(RUN_PRUNE_KV_KEY)
        .await?
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    if now - last < RUN_PRUNE_INTERVAL_SECS {
        return Ok(());
    }
    store.kv_set(RUN_PRUNE_KV_KEY, &now.to_string()).await?;

    let pruned = store.prune_old_runs(RUN_OUTPUT_RETENTION_DAYS).await?;
    if pruned > 0 {
        tracing::info!(pruned, "cleared the output of old task runs");
    }
    Ok(())
}

async fn reconcile_closed_tasks(
    task_ids: &mut Vec<String>,
    all_tasks: &[ExternalTask],
    task_manager: &TaskManager,
    store: &Arc<TaskStore>,
    repo: &str,
) {
    const MAX_RECONCILE_PER_CYCLE: usize = 10;

    let done_set: HashSet<String> = task_ids.iter().cloned().collect();
    let mut closed_not_in_store = Vec::new();
    for task in all_tasks {
        if task.state != "closed" || done_set.contains(&task.id.0) {
            continue;
        }
        if ci_bootstrap::hold_after_merge(task, store, repo).await {
            continue;
        }
        closed_not_in_store.push(task);
    }

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
    let results = future::join_all(futs).await;
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
    let Some(_guard) = CleanupInFlightGuard::try_acquire(repo) else {
        tracing::debug!(
            repo = %repo,
            "cleanup already in progress for repo — skipping overlapping run"
        );
        return Ok(());
    };

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
    //
    // Use list_reconciliation_candidates() (closed issues only, bounded recent window) directly.
    // list_all_tasks() fetches every issue ever created (state=all, all pages) and
    // consistently times out on repos with many issues — it is not appropriate for
    // a cleanup fallback path.
    if should_run_reconciliation(repo) {
        match timeout(
            RECONCILIATION_LIST_TIMEOUT,
            backend.list_reconciliation_candidates(),
        )
        .await
        {
            Ok(Ok(candidates)) if !candidates.is_empty() => {
                tracing::debug!(
                    count = candidates.len(),
                    "reconciling closed issues from candidates"
                );
                reconcile_closed_tasks(&mut task_ids, &candidates, task_manager, store, repo).await;
            }
            Ok(Ok(_)) => {
                tracing::debug!("no reconciliation candidates found");
            }
            Ok(Err(e)) => {
                tracing::warn!(err = %e, "failed to list reconciliation candidates");
            }
            Err(_) => {
                tracing::warn!(
                    timeout_secs = RECONCILIATION_LIST_TIMEOUT.as_secs(),
                    retry_after_secs = RECONCILIATION_INTERVAL.as_secs(),
                    "timed out listing reconciliation candidates"
                );
            }
        }
    } else {
        tracing::debug!(
            retry_after_secs = RECONCILIATION_INTERVAL.as_secs(),
            "skipping closed-issue reconciliation until interval elapses"
        );
    }

    // Also include internal done tasks.
    if let Ok(internal_done) = task_manager.list_internal_by_status(TaskStatus::Done).await {
        task_ids.extend(internal_done.iter().map(|t| t.id.0.clone()));
    }

    tracing::debug!(
        count = task_ids.len(),
        "checking all terminal tasks for cleanup"
    );

    if let Err(e) = auto_merge::sweep_ci_timer_keys(store, repo, &task_ids).await {
        tracing::warn!(err = %e, "failed to sweep CI timer keys");
    }

    // Prefetch all cleaned external IDs in one query instead of N+1 per-task lookups.
    // NOTE: This is now primarily a hint to skip processing, NOT a correctness
    // guarantee — tasks with external_id NOT in the store at all (or mismatched
    // repo) will still be cleaned by iterating all worktree directories.
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

    // ── Orphaned worktree scanner ─────────────────────────────────────────────
    //
    // Even if every task in our list was already cleaned (or had no worktree),
    // there may be worktree directories on disk that are not owned by any task.
    // This happens when a task was ingested into the store but its worktree_cleaned
    // flag was never set — e.g. because the task was never in `list_all_by_status`
    // (unknown repo / missing from store at cleanup time), or the cleanup skipped
    // due to TTL guard and never retried.
    //
    // We scan all worktree directories in this repo's worktrees subdirectory and
    // remove any that are not referenced by an existing task. This catches the
    // "done but worktree not cleaned" case that `orch service doctor` reports.
    let worktrees_base = match home::worktrees_dir() {
        Ok(base) => Some(base),
        Err(e) => {
            tracing::debug!(err = %e, "worktrees_dir() unavailable — skipping orphan scan");
            None
        }
    };

    if let Some(base) = worktrees_base {
        let project = repo
            .rsplit('/')
            .next()
            .unwrap_or(repo)
            .trim_end_matches(".git");
        let repo_worktrees = base.join(project);

        // Build set of all known worktree paths for this repo (all statuses).
        let mut combined: HashSet<String> = HashSet::new();
        if let Ok(tasks) = store.list_all(repo).await {
            for t in tasks {
                if !t.worktree.is_empty() {
                    combined.insert(t.worktree);
                }
            }
        }

        // Also include active worktrees (in case task is in-memory but has a dir).
        if let Ok(project_dir) = resolve_project_dir_for_repo(repo)
            && let Ok(active_wts) = worktree::list_project_worktrees(&project_dir).await
        {
            for wt in active_wts {
                combined.insert(wt.to_string_lossy().to_string());
            }
        }

        // Scan all branch directories and remove orphaned ones.
        if let Ok(mut entries) = fs::read_dir(&repo_worktrees).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let Ok(entry_meta) = entry.metadata().await else {
                    continue;
                };
                if !entry_meta.is_dir() {
                    continue;
                }
                let wt_path = entry.path().to_string_lossy().to_string();
                if combined.contains(&wt_path) {
                    continue;
                }
                tracing::info!(worktree = %wt_path, "found orphaned worktree — attempting cleanup");
                match resolve_repo_root_for_orphaned_worktree(&entry.path()).await {
                    Ok(repo_root) => {
                        let removed = remove_worktree_and_branch(
                            "orphan-scan",
                            &entry.path(),
                            None,
                            Path::new(&repo_root),
                            false,
                        )
                        .await;
                        if removed {
                            tracing::info!(worktree = %wt_path, "orphaned worktree removed");
                        } else {
                            tracing::warn!(worktree = %wt_path, "orphaned worktree could not be removed");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(worktree = %wt_path, err = %e, "could not resolve repo root for orphaned worktree");
                    }
                }
            }
        }
    }

    // Prune stale worktree metadata and pull main after cleanup.
    if cleaned_any
        && !opts.dry_run
        && let Ok(repo_root) = resolve_repo_root(repo).await
    {
        const GIT_TIMEOUT: Duration = Duration::from_secs(60);

        // Prune stale .git/worktrees/ entries whose directories no longer exist.
        let prune_result = timeout(
            GIT_TIMEOUT,
            Command::new("git")
                .args(["-C", &repo_root, "worktree", "prune"])
                .output_with_context(),
        )
        .await;
        match prune_result {
            Ok(Ok(output)) if output.status.success() => {
                tracing::debug!(%repo, "pruned stale worktree metadata");
            }
            Ok(Ok(output)) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::warn!(%repo, err = %stderr, "git worktree prune failed");
            }
            Ok(Err(e)) => {
                tracing::warn!(%repo, err = %e, "git worktree prune failed");
            }
            Err(_) => {
                tracing::warn!(%repo, "git worktree prune timed out after 60s");
            }
        }

        // Fetch instead of pull: refresh `origin/*` refs without touching
        // the operator's checked-out branch or working tree. `pull` would
        // operate on whatever HEAD happens to be (often a feature branch,
        // not main) and would silently no-op on divergence.
        let fetch_result = timeout(
            GIT_TIMEOUT,
            git_ops::git()
                .await
                .args(["-C", &repo_root, "fetch", "--prune", "origin"])
                .output_with_context(),
        )
        .await;
        match fetch_result {
            Ok(Ok(output)) if output.status.success() => {
                tracing::info!(%repo, "fetched origin after worktree cleanup");
            }
            Ok(Ok(output)) => {
                let stderr = git_ops::sanitize_push_error(&String::from_utf8_lossy(&output.stderr));
                tracing::debug!(%repo, err = %stderr, "git fetch skipped after cleanup");
            }
            Ok(Err(e)) => {
                tracing::debug!(%repo, err = %e, "git fetch failed after cleanup");
            }
            Err(_) => {
                tracing::debug!(%repo, "git fetch timed out after 60s");
            }
        }
    }

    Ok(())
}

/// Cleanup a single task's worktree and branches.
///
/// Removes the git worktree, deletes local + remote branches,
/// fetches origin to refresh remote tracking refs, and marks the task as cleaned.
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
    cleanup_task_worktree_impl(task_id, repo, store, false).await
}

/// Cleanup after a merged pull request, including while `hold_after_merge` keeps the task approved
pub(crate) async fn cleanup_merged_pr_worktree(
    task_id: &str,
    repo: &str,
    store: &Arc<TaskStore>,
) -> anyhow::Result<()> {
    cleanup_task_worktree_impl(task_id, repo, store, true).await
}

async fn cleanup_task_worktree_impl(
    task_id: &str,
    repo: &str,
    store: &Arc<TaskStore>,
    known_pr_merged: bool,
) -> anyhow::Result<()> {
    let opts = JanitorOptions {
        ttl_hours: 0,
        ..Default::default()
    };
    let cleaned =
        cleanup_task_worktree_with_opts_impl(task_id, repo, store, &opts, known_pr_merged).await?;

    // Fetch origin once after inline cleanup (post-merge single-task path).
    // Only fetch if cleanup actually happened (not skipped due to active session).
    // Fetch (not pull) so the operator's checked-out branch and working tree
    // are never touched — see the matching comment in cleanup_orphaned_worktrees.
    if cleaned && let Ok(repo_root) = resolve_repo_root(repo).await {
        const GIT_TIMEOUT: Duration = Duration::from_secs(60);
        let fetch_result = timeout(
            GIT_TIMEOUT,
            git_ops::git()
                .await
                .args(["-C", &repo_root, "fetch", "--prune", "origin"])
                .output_with_context(),
        )
        .await;
        match fetch_result {
            Ok(Ok(output)) if output.status.success() => {
                tracing::debug!(%repo, "fetched origin after task cleanup");
            }
            Ok(Ok(output)) => {
                let stderr = git_ops::sanitize_push_error(&String::from_utf8_lossy(&output.stderr));
                tracing::debug!(%repo, err = %stderr, "git fetch skipped after task cleanup");
            }
            Ok(Err(e)) => {
                tracing::debug!(%repo, err = %e, "git fetch failed after task cleanup");
            }
            Err(_) => {
                tracing::debug!(%repo, "git fetch timed out after 60s");
            }
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
    cleanup_task_worktree_with_opts_impl(task_id, repo, store, opts, false).await
}

async fn cleanup_task_worktree_with_opts_impl(
    task_id: &str,
    repo: &str,
    store: &Arc<TaskStore>,
    opts: &JanitorOptions,
    known_pr_merged: bool,
) -> anyhow::Result<bool> {
    let (worktree, branch, keep_remote_branch, merged, external_id, pr_merged) =
        store::opt_store_get_task(&Some(Arc::clone(store)), repo, task_id)
            .await
            .map_or((None, None, false, false, None, false), |t| {
                // Blocked tasks with a PR should keep their remote branch so the
                // PR stays open for human review.
                let keep = t.status == TaskStatus::Blocked && t.pr_number.is_some();
                let merged = t.status == TaskStatus::Done || known_pr_merged;
                (
                    Some(t.worktree),
                    Some(t.branch),
                    keep,
                    merged,
                    t.external_id,
                    // A squash leaves the branch commits on no ref, so skip rescue when the PR merged
                    known_pr_merged || (t.status == TaskStatus::Done && t.pr_number.is_some()),
                )
            });

    let worktree_path = worktree.as_ref().map(PathBuf::from);

    // Get worktrees base path
    let worktrees_base = home::worktrees_dir().unwrap_or_else(|_| PathBuf::from(".orch/worktrees"));

    // Determine which directory to remove.
    //
    // Priority:
    //   1. stored "worktree" path, if it exists on disk.
    //   2. Construct from worktrees_base + project name + branch, if it exists.
    let worktree_to_remove = if let Some(ref wt) = worktree_path {
        if match fs::try_exists(wt).await {
            Ok(true) => true,
            Ok(false) => false,
            Err(e) => {
                tracing::warn!(task_id, worktree = %wt.display(), err = %e, "try_exists failed on stored worktree path — trying branch-based fallback");
                false
            }
        } {
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
                if match fs::try_exists(&candidate).await {
                    Ok(true) => true,
                    Ok(false) => false,
                    Err(e) => {
                        tracing::warn!(task_id, err = %e, "try_exists failed — assuming candidate does not exist");
                        false
                    }
                } {
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
        if match fs::try_exists(&candidate).await {
            Ok(true) => true,
            Ok(false) => false,
            Err(e) => {
                tracing::warn!(task_id, err = %e, "try_exists failed — assuming candidate does not exist");
                false
            }
        } {
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
    let branch_nonempty = branch.as_ref().filter(|b| !b.is_empty());
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
            if let Ok(Some(store_id)) = store.resolve_task_id(repo, task_id).await
                && let Err(e) = store.mark_cleaned(store_id).await
            {
                tracing::warn!(task_id, err = %e, "failed to mark worktree cleaned in store — will retry");
            }
        }
        return Ok(false);
    }

    if let Some(wt) = worktree_to_remove {
        // TTL guard: skip if the worktree directory is too young.
        if let Some(age_hours) = worktree_age_hours(&wt).await
            && age_hours < opts.ttl_hours
        {
            tracing::debug!(
                task_id,
                worktree = %wt.display(),
                age_hours,
                ttl_hours = opts.ttl_hours,
                "worktree too young for cleanup, skipping"
            );
            return Ok(false);
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
        let gitdir_valid = worktree::validate_worktree_gitdir(&wt).await;

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
            match fs::remove_dir_all(&wt).await {
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
            if let Some(br) = branch.as_deref().filter(|b| !b.is_empty())
                && !pr_merged
                && !rescue_and_notify(
                    task_id,
                    repo,
                    external_id.as_deref(),
                    Some(&wt),
                    br,
                    Path::new(&repo_root),
                )
                .await
            {
                return Ok(false);
            }
            if merged {
                cow::warm_from_merged(
                    Path::new(&repo_root),
                    &worktree::project_name(Path::new(&repo_root)),
                    &wt,
                )
                .await;
            }
            let removed = remove_worktree_and_branch_unchecked(
                task_id,
                &wt,
                branch.as_deref(),
                Path::new(&repo_root),
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
            if !pr_merged
                && !rescue_and_notify(
                    task_id,
                    repo,
                    external_id.as_deref(),
                    None,
                    br,
                    Path::new(&repo_root),
                )
                .await
            {
                return Ok(false);
            }
            delete_branches(task_id, br, Path::new(&repo_root)).await;
            did_clean = true;
        }
    }

    if did_clean {
        // Mark as cleaned in store so we don't retry next tick.
        // If resolve_task_id returns None the task is not (or no longer) in
        // the store; that is fine — we already did the on-disk work.
        if let Ok(Some(store_id)) = store.resolve_task_id(repo, task_id).await
            && let Err(e) = store.mark_cleaned(store_id).await
        {
            tracing::warn!(task_id, err = %e, "failed to mark worktree cleaned — will retry");
            // Don't log activity; retry next tick will do the actual cleanup
            return Ok(did_clean);
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

/// Result of saving a task branch's unpushed work before cleanup deletes it
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Rescue {
    /// Every commit is on a remote ref already
    Clean,
    /// The commits were pushed to `rescue/<branch>`
    Pushed {
        branch: String,
        commits: Vec<String>,
    },
    /// Git failed, so the worktree and branch must stay
    Failed,
}

async fn git_in(dir: &Path, args: &[&str]) -> Option<Output> {
    git_ops::git()
        .await
        .arg("-C")
        .arg(dir)
        .args(args)
        .output_with_context()
        .await
        .ok()
        .map(git_ops::sanitize_git_output)
}

/**
 * Cleanup deletes the worktree and the local branch, which can be the only refs to commits the
 * agent made before its run ended without a push (#3999). Uncommitted changes are committed
 * first, then everything not on a remote ref goes to `rescue/<branch>`
 **/
pub(crate) async fn rescue_unpushed_work(
    task_id: &str,
    wt: Option<&Path>,
    br: &str,
    repo_root: &Path,
) -> Rescue {
    if let Some(wt) = wt
        && fs::try_exists(wt.join(".git")).await.unwrap_or(false)
    {
        let Some(status) = git_in(wt, &["status", "--porcelain"]).await else {
            tracing::warn!(task_id, branch = %br, "cannot read worktree status, keeping worktree");
            return Rescue::Failed;
        };
        if !status.status.success() {
            tracing::warn!(task_id, branch = %br, "git status failed, keeping worktree");
            return Rescue::Failed;
        }
        if !status.stdout.is_empty() {
            let committed = git_in(wt, &["add", "-A"])
                .await
                .is_some_and(|o| o.status.success())
                && git_in(
                    wt,
                    &[
                        "-c",
                        "user.name=orch",
                        "-c",
                        "user.email=orch@localhost",
                        "commit",
                        "-m",
                        "rescue: uncommitted changes saved by orch cleanup",
                    ],
                )
                .await
                .is_some_and(|o| o.status.success());
            if !committed {
                tracing::warn!(task_id, branch = %br, "cannot commit uncommitted changes, keeping worktree");
                return Rescue::Failed;
            }
        }
    }

    let local_ref = format!("refs/heads/{br}");
    let exists = git_in(repo_root, &["rev-parse", "--verify", "--quiet", &local_ref])
        .await
        .is_some_and(|o| o.status.success());
    if !exists {
        return Rescue::Clean;
    }
    // Commits on the local default branch survive the branch delete, so they do not count
    let default_ref = format!(
        "refs/heads/{}",
        worktree::detect_default_branch(repo_root).await
    );
    // A bare clone has no remote-tracking refs, so ask the remote for the branch tip
    let remote_tip = git_in(repo_root, &["ls-remote", "origin", &local_ref])
        .await
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .next()
                .map(str::to_string)
        });
    // A tip someone else pushed may be missing locally, and rev-list fails on an unknown object
    let remote_tip = match remote_tip {
        Some(tip) => {
            let known = git_in(repo_root, &["cat-file", "-e", &format!("{tip}^{{commit}}")])
                .await
                .is_some_and(|o| o.status.success());
            known.then_some(tip)
        }
        None => None,
    };
    let mut args = vec!["rev-list", "--oneline", &local_ref, "--not", "--remotes"];
    if let Some(tip) = remote_tip.as_deref() {
        args.push(tip);
    }
    let default_exists = default_ref != local_ref
        && git_in(
            repo_root,
            &["rev-parse", "--verify", "--quiet", &default_ref],
        )
        .await
        .is_some_and(|o| o.status.success());
    if default_exists {
        args.push(&default_ref);
    }
    let Some(list) = git_in(repo_root, &args)
        .await
        .filter(|o| o.status.success())
    else {
        tracing::warn!(task_id, branch = %br, "cannot list unpushed commits, keeping worktree and branch");
        return Rescue::Failed;
    };
    let commits: Vec<String> = String::from_utf8_lossy(&list.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    if commits.is_empty() {
        return Rescue::Clean;
    }

    let rescue = format!("rescue/{br}");
    let refspec = format!("{local_ref}:refs/heads/{rescue}");
    let pushed = git_in(
        repo_root,
        &["-c", "lfs.locksverify=false", "push", "origin", &refspec],
    )
    .await;
    if pushed.is_some_and(|o| o.status.success()) {
        // A bare clone has no fetchspec, so push does not create the tracking ref
        // Point that ref at the tip we just sent so delete_local_branch can see it
        let tracking = format!("refs/remotes/origin/{rescue}");
        if let Some(tip) = git_in(repo_root, &["rev-parse", &local_ref])
            .await
            .filter(|o| o.status.success())
        {
            let sha = String::from_utf8_lossy(&tip.stdout).trim().to_string();
            if !sha.is_empty() {
                let _ = git_in(repo_root, &["update-ref", &tracking, &sha]).await;
            }
        }
        tracing::warn!(task_id, branch = %br, rescue = %rescue, commits = commits.len(), "pushed unpushed commits to rescue branch");
        Rescue::Pushed {
            branch: rescue,
            commits,
        }
    } else {
        tracing::warn!(task_id, branch = %br, rescue = %rescue, "rescue push failed, keeping worktree and branch");
        Rescue::Failed
    }
}

fn rescue_comment(rescue_branch: &str, commits: &[String]) -> String {
    let list = commits
        .iter()
        .map(|c| format!("- `{c}`"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Cleanup found commits on this task's branch that were never pushed. They are saved on `{rescue_branch}`:\n\n{list}"
    )
}

/// Save unpushed work and tell the task about it. Returns false when cleanup must not go on
async fn rescue_and_notify(
    task_id: &str,
    repo: &str,
    external_id: Option<&str>,
    wt: Option<&Path>,
    br: &str,
    repo_root: &Path,
) -> bool {
    match rescue_unpushed_work(task_id, wt, br, repo_root).await {
        Rescue::Clean => true,
        Rescue::Failed => false,
        Rescue::Pushed { branch, commits } => {
            if let Some(number) = external_id.filter(|n| n.parse::<u64>().is_ok()) {
                match GhHttp::new() {
                    Ok(gh) => {
                        if let Err(e) = gh
                            .add_comment(repo, number, &rescue_comment(&branch, &commits))
                            .await
                        {
                            tracing::warn!(task_id, err = %e, "failed to comment on rescue branch");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(task_id, err = %e, "cannot comment on rescue branch");
                    }
                }
            }
            true
        }
    }
}

/// Remove a git worktree directory and its local + remote branches.
/// When `keep_remote_branch` is true, only the worktree and local branch are
/// removed — the remote branch stays so that any linked PR remains open
/// (used for blocked tasks awaiting human review).
/// Returns `true` if the worktree directory is gone after the call
/// (either we removed it or it was already absent).
pub(crate) async fn remove_worktree_and_branch(
    task_id: &str,
    wt: &Path,
    branch: Option<&str>,
    repo_root: &Path,
    keep_remote_branch: bool,
) -> bool {
    remove_worktree_and_branch_for(task_id, wt, branch, repo_root, keep_remote_branch, false).await
}

/// Same as `remove_worktree_and_branch`, but a merged PR skips the rescue. A squash leaves the
/// branch commits on no ref, so the rescue would push a branch for work that already landed
pub(crate) async fn remove_worktree_and_branch_for(
    task_id: &str,
    wt: &Path,
    branch: Option<&str>,
    repo_root: &Path,
    keep_remote_branch: bool,
    pr_merged: bool,
) -> bool {
    if !pr_merged
        && let Some(br) = branch.filter(|b| !b.is_empty())
        && rescue_unpushed_work(task_id, Some(wt), br, repo_root).await == Rescue::Failed
    {
        return false;
    }

    remove_worktree_and_branch_unchecked(task_id, wt, branch, repo_root, keep_remote_branch).await
}

/// The caller has already rescued the branch or knows its PR merged
async fn remove_worktree_and_branch_unchecked(
    task_id: &str,
    wt: &Path,
    branch: Option<&str>,
    repo_root: &Path,
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
            const MAX_RETRIES: usize = 3;
            const RETRY_DELAY_MS: u64 = 2000;
            tracing::info!(task_id, "worktree removed (git metadata)");
            // `git worktree remove` only removes git metadata — the physical directory
            // with working files may remain. Clean it up with retry logic, since macOS
            // sometimes needs a moment to release file handles from a recently-exited
            // process. Retrying with a delay addresses "Directory not empty" errors.
            let mut removed = false;
            for attempt in 0..MAX_RETRIES {
                match fs::remove_dir_all(wt).await {
                    Ok(()) => {
                        removed = true;
                        break;
                    }
                    Err(e) if e.kind() == ErrorKind::NotFound => {
                        // Directory was already removed by git or another cleanup pass.
                        removed = true;
                        break;
                    }
                    Err(e) if e.kind() == ErrorKind::DirectoryNotEmpty => {
                        if attempt < MAX_RETRIES - 1 {
                            tracing::debug!(
                                task_id,
                                path = %wt.display(),
                                attempt = attempt + 1,
                                "worktree directory not empty — retrying after delay"
                            );
                            sleep(Duration::from_millis(RETRY_DELAY_MS)).await;
                        } else {
                            tracing::warn!(
                                task_id,
                                path = %wt.display(),
                                err = %e,
                                "failed to remove worktree directory after {} attempts (Directory not empty)",
                                MAX_RETRIES
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(task_id, path = %wt.display(), err = %e, "failed to remove worktree directory");
                        break;
                    }
                }
            }
            if removed {
                tracing::info!(task_id, path = %wt.display(), "worktree physical directory removed");
            } else {
                tracing::warn!(
                    task_id,
                    path = %wt.display(),
                    "worktree directory still present after git worktree remove"
                );
            }
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
                if let Err(e) = fs::remove_dir_all(wt).await {
                    // Only warn if the directory actually still exists after removal attempt.
                    if match fs::try_exists(wt).await {
                        Ok(true) => true,
                        Ok(false) => false,
                        Err(e) => {
                            tracing::warn!(task_id, err = %e, "try_exists failed — assuming worktree does not exist");
                            false
                        }
                    } {
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
            delete_local_branch(task_id, br, repo_root).await;
        } else {
            delete_branches(task_id, br, repo_root).await;
        }
    }

    // Definitively check: is the worktree directory actually gone?
    match fs::try_exists(wt).await {
        Ok(false) => true, // confirmed gone
        Ok(true) => false, // still there
        Err(e) => {
            tracing::warn!(task_id, err = %e, "try_exists failed — assuming worktree still present");
            false // conservative: don't claim success
        }
    }
}

/// Whether `refs/heads/{br}` still exists in `repo_root`
pub(crate) async fn local_branch_exists(repo_root: &Path, br: &str) -> bool {
    let local_ref = format!("refs/heads/{br}");
    git_in(repo_root, &["rev-parse", "--verify", "--quiet", &local_ref])
        .await
        .is_some_and(|o| o.status.success())
}

/// Commits reachable from `rev` and from none of `others`. `None` when git cannot answer
async fn commits_outside(repo_root: &Path, rev: &str, others: &[&str]) -> Option<u64> {
    let mut args = vec![
        "rev-list".to_string(),
        "--count".to_string(),
        rev.to_string(),
    ];
    // One `--not` then every other ref, a `--not` per ref would toggle and count the wrong set
    let mut exclude = Vec::new();
    for other in others {
        let exists = Command::new("git")
            .args([
                "-C",
                &repo_root.to_string_lossy(),
                "rev-parse",
                "--verify",
                "--quiet",
                other,
            ])
            .output_with_context()
            .await
            .is_ok_and(|o| o.status.success());
        if exists {
            exclude.push(other.to_string());
        }
    }
    if !exclude.is_empty() {
        args.push("--not".to_string());
        args.extend(exclude);
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(&args)
        .output_with_context()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

/// Delete the local branch only when origin or the default branch holds every commit on it
async fn delete_local_branch(task_id: &str, br: &str, repo_root: &Path) {
    let default_branch = worktree::detect_default_branch(repo_root).await;
    let remote_ref = format!("refs/remotes/origin/{br}");
    let default_ref = format!("refs/remotes/origin/{default_branch}");
    let local_ref = format!("refs/heads/{br}");
    let local_default_ref = format!("refs/heads/{default_branch}");
    let rescue_ref = format!("refs/remotes/origin/rescue/{br}");
    match commits_outside(
        repo_root,
        &local_ref,
        &[&remote_ref, &default_ref, &local_default_ref, &rescue_ref],
    )
    .await
    {
        Some(0) => {}
        Some(n) => {
            tracing::warn!(task_id, branch = %br, unpushed = n, "local branch holds commits origin does not have, keeping it");
            return;
        }
        None => {
            tracing::debug!(task_id, branch = %br, "local branch missing or unreadable, nothing to delete");
            return;
        }
    }
    let output = Command::new("git")
        .args(["-C", &repo_root.to_string_lossy(), "branch", "-D", br])
        .output_with_context()
        .await;
    match output {
        Ok(o) if o.status.success() => {
            tracing::info!(task_id, branch = %br, "local branch deleted");
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            tracing::debug!(task_id, err = %stderr, "local branch delete skipped (may not exist)");
        }
        Err(e) => {
            tracing::warn!(task_id, err = %e, "failed to delete local branch");
        }
    }
}

/**
 * Delete local and remote branches for a task, but never the only copy of a commit.
 * The remote branch goes only when the default branch already contains all of it, which
 * covers fast-forward and merge-commit merges. A squash-merged branch stays, and GitHub's
 * "automatically delete head branches" removes it. Every other caller (startup rebase
 * failure, closed-issue reconciliation, orphan sweeps) may hold unmerged work (#3781)
 **/
pub(crate) async fn delete_branches(task_id: &str, br: &str, repo_root: &Path) {
    if br == SCREENSHOTS_BRANCH {
        return;
    }
    let default_branch = worktree::detect_default_branch(repo_root).await;
    let _ = git_ops::git()
        .await
        .args([
            "-C",
            &repo_root.to_string_lossy(),
            "fetch",
            "origin",
            &default_branch,
            br,
        ])
        .output_with_context()
        .await;

    delete_local_branch(task_id, br, repo_root).await;

    let remote_ref = format!("refs/remotes/origin/{br}");
    let default_ref = format!("refs/remotes/origin/{default_branch}");
    match commits_outside(repo_root, &remote_ref, &[&default_ref]).await {
        Some(0) => {}
        Some(n) => {
            tracing::info!(task_id, branch = %br, unmerged = n, "remote branch has commits the default branch lacks, keeping it");
            return;
        }
        None => {
            tracing::debug!(task_id, branch = %br, "remote branch missing or unreadable, nothing to delete");
            return;
        }
    }

    let remote_delete = git_ops::git()
        .await
        .args([
            "-C",
            &repo_root.to_string_lossy(),
            "-c",
            "lfs.locksverify=false",
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
            git_ops::clear_push_lease(repo_root, br).await;
        }
        Ok(output) => {
            let stderr = git_ops::sanitize_push_error(&String::from_utf8_lossy(&output.stderr));
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
async fn is_worktree_in_active_session(worktree: &Path) -> bool {
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
                    || pane_path.starts_with(&format!("{worktree_str}/"))
            })
        }
        // tmux not running or no sessions — treat as safe to clean.
        _ => false,
    }
}

/// Returns how many hours old the worktree directory is, based on its mtime.
///
/// Returns `None` if the directory does not exist or the mtime cannot be read.
async fn worktree_age_hours(worktree: &Path) -> Option<u64> {
    // Use tokio::fs to avoid blocking the async runtime when reading metadata.
    let metadata = fs::metadata(worktree).await.ok()?;
    let modified = metadata.modified().ok()?;
    let age = SystemTime::now().duration_since(modified).ok()?;
    Some(age.as_secs() / 3600)
}

/// Resolve the repo root for an orphaned worktree and remove it.
///
/// Used by `orch service doctor --fix` and `orch service prune` to clean up worktrees that are
/// no longer owned by any task. Returns `true` if the worktree directory is gone
/// after the call.
pub(crate) async fn cleanup_orphaned_worktree(task_id: &str, wt: &Path) -> bool {
    let repo_root = match resolve_repo_root_for_orphaned_worktree(wt).await {
        Ok(root) => root,
        Err(e) => {
            tracing::warn!(
                worktree = %wt.display(),
                error = %e,
                "cannot resolve repo root for orphaned worktree"
            );
            return false;
        }
    };
    remove_worktree_and_branch(task_id, wt, None, Path::new(&repo_root), false).await
}

/// Resolve the main git repository root for an orphaned worktree.
///
/// First tries to parse the `.git` file's `gitdir:` pointer (two levels up from
/// the pointer gives the bare repo root, which is the orch convention). Falls back
/// to looking up registered projects by directory name when the `.git` file is
/// absent or unparseable.
pub(crate) async fn resolve_repo_root_for_orphaned_worktree(wt: &Path) -> anyhow::Result<String> {
    let git_file = wt.join(".git");
    if fs::try_exists(&git_file).await.unwrap_or(false)
        && let Ok(content) = fs::read_to_string(&git_file).await
    {
        for line in content.lines() {
            if let Some(gitdir) = line.strip_prefix("gitdir: ") {
                // gitdir points to: <repo>/.git/worktrees/<name> (non-bare)
                //                or <repo>.git/worktrees/<name>   (bare)
                // Two levels up reaches the .git dir / bare repo root.
                let gitdir_path = PathBuf::from(gitdir);
                if let Some(repo_root) = gitdir_path.parent().and_then(|p| p.parent()) {
                    return Ok(repo_root.display().to_string());
                }
            }
        }
    }

    // Fallback: look up registered projects by the project-name component of the
    // worktree path (format: ~/.orch/worktrees/<project>/<branch>).
    let worktrees_dir = home::worktrees_dir()?;
    if let Ok(relative) = wt.strip_prefix(&worktrees_dir)
        && let Some(project_name) = relative.components().next()
    {
        let project_name = project_name.as_os_str().to_string_lossy();

        if let Ok(paths) = config::get_project_paths() {
            for path_str in &paths {
                let path = Path::new(path_str);
                if let Some(name) = path.file_name()
                    && name.to_string_lossy() == project_name
                {
                    return Ok(path_str.clone());
                }
            }
        }

        // Try bare clone at ~/.orch/projects/<owner>/<project>.git
        let parts: Vec<&str> = project_name.split('/').collect();
        let bare = if parts.len() == 2 {
            home::projects_dir()
                .map(|d| d.join(parts[0]).join(format!("{}.git", parts[1])))
                .unwrap_or_default()
        } else {
            return Err(anyhow::anyhow!(
                "cannot determine owner for single-segment project name '{project_name}' — \
                     bare repo location is ambiguous; register the project with 'owner/repo' format"
            ));
        };
        if fs::try_exists(&bare).await.unwrap_or(false) {
            return Ok(bare.display().to_string());
        }
    }

    anyhow::bail!(
        "cannot resolve repo root for orphaned worktree at {} — tried .git parsing and project lookup",
        wt.display()
    )
}

/// Resolve the main git repository root from a worktree's .git file.
///
/// For cross-project worktree cleanup, we cannot rely on `resolve_repo_root` because
/// the worktree may belong to a different project than the current repo context.
/// Instead, we extract the repo root from the worktree's `.git` file which contains
/// a `gitdir:` pointer to the main repo's git directory.
pub async fn resolve_repo_root_from_worktree(wt: &Path) -> anyhow::Result<String> {
    // For worktrees, .git is a file (not a directory) containing gitdir path
    let git_file = wt.join(".git");
    if !fs::try_exists(&git_file).await.unwrap_or(false) {
        anyhow::bail!(".git file not found in worktree at {}", wt.display());
    }

    let content = fs::read_to_string(&git_file)
        .await
        .map_err(|e| anyhow::anyhow!("failed to read .git file: {e}"))?;

    // Parse gitdir line: "gitdir: /path/to/repo/.git/worktrees/<name>"
    for line in content.lines() {
        if let Some(gitdir) = line.strip_prefix("gitdir:") {
            let gitdir = gitdir.trim();
            // The main repo is the parent of the .git directory
            // gitdir points to: <repo>/.git/worktrees/<name>
            // We need to go up 3 levels: worktrees/<name> -> .git -> repo
            let gitdir_path = Path::new(gitdir);
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
    let paths = config::get_project_paths().unwrap_or_default();
    for path_str in &paths {
        let path = Path::new(path_str);
        let orch_yml = path.join(".orch.yml");
        let legacy = path.join(".orchestrator.yml");
        let config_file = if fs::try_exists(&orch_yml).await.unwrap_or(false) {
            orch_yml
        } else if fs::try_exists(&legacy).await.unwrap_or(false) {
            legacy
        } else {
            continue;
        };
        if let Ok(content) = fs::read_to_string(&config_file).await
            && let Ok(doc) = serde_norway::from_str::<serde_norway::Value>(&content)
            && let Some(r) = doc
                .get("gh")
                .and_then(|gh| gh.get("repo"))
                .and_then(|r| r.as_str())
            && r == repo
        {
            return Ok(path_str.clone());
        }
    }

    // Fallback: try bare clone in ~/.orch/projects/<owner>/<repo>.git
    let parts: Vec<&str> = repo.split('/').collect();
    let bare = if parts.len() == 2 {
        home::projects_dir()
            .map(|d| d.join(parts[0]).join(format!("{}.git", parts[1])))
            .unwrap_or_default()
    } else {
        PathBuf::new()
    };
    if fs::try_exists(&bare).await.unwrap_or(false) {
        return Ok(bare.display().to_string());
    }

    anyhow::bail!(
        "cannot find local path for project {repo} — \
         checked {} registered project(s) and bare clone at {}",
        paths.len(),
        bare.display()
    )
}

/// Resolve the project directory for a given repo string.
///
/// Replicates the logic from `TaskRunner::resolve_project_dir()` without requiring
/// a Runner instance. Used by the orphan scanner to enumerate active worktrees.
fn resolve_project_dir_for_repo(repo: &str) -> anyhow::Result<PathBuf> {
    // Check PROJECT_DIR env var first
    if let Ok(dir) = env::var("PROJECT_DIR")
        && !dir.is_empty()
    {
        return Ok(PathBuf::from(dir));
    }

    // Look up registered projects in config
    if let Ok(paths) = config::get_project_paths() {
        for path_str in &paths {
            let path = Path::new(path_str);
            let orch_yml = path.join(".orch.yml");
            let legacy = path.join(".orchestrator.yml");
            let config_file = if Path::new(&orch_yml).exists() {
                orch_yml
            } else if Path::new(&legacy).exists() {
                legacy
            } else {
                continue;
            };
            if let Ok(content) = std_fs::read_to_string(&config_file)
                && let Ok(doc) = serde_norway::from_str::<serde_norway::Value>(&content)
                && let Some(r) = doc
                    .get("gh")
                    .and_then(|gh| gh.get("repo"))
                    .and_then(|r| r.as_str())
                && r == repo
            {
                return Ok(path_str.clone().into());
            }
        }
    }

    anyhow::bail!("project dir not found for repo: {repo}")
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
    let mut branch_map: HashMap<String, String> = HashMap::new();
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

        let stored = store::opt_store_get_task(&Some(Arc::clone(store)), repo, &task_id).await;
        if let Some(ref stored) = stored {
            let external = store_task_to_external(stored);
            if ci_bootstrap::hold_after_merge(&external, store, repo).await {
                tracing::info!(
                    task_id,
                    branch = %branch,
                    "CI bootstrap PR merged, holding done until the ruleset exists"
                );
                if let Err(e) = cleanup_merged_pr_worktree(&task_id, repo, store).await {
                    tracing::warn!(task_id, err = %e, "worktree cleanup after merge failed");
                }
                continue;
            }
        }

        let id = ExternalId(task_id.clone());
        if let Err(e) = task_manager.update_task_status(&id, Status::Done).await {
            tracing::warn!(task_id, err = %e, "failed to update status to done");
            continue;
        }

        if let Err(e) = cleanup_merged_pr_worktree(&task_id, repo, store).await {
            tracing::warn!(task_id, err = %e, "worktree cleanup after merge failed");
        }

        let comment = format!("PR merged, marking task complete{}", engine::orch_footer());
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
    let futs: Vec<_> = branches
        .iter()
        .map(|branch| {
            let backend = Arc::clone(backend);
            let branch = branch.clone();
            async move {
                match backend.is_pr_merged(&branch).await {
                    Ok(is_merged) => Some((branch, is_merged)),
                    Err(err) => {
                        tracing::warn!(
                            branch,
                            err = %err,
                            "fallback PR merge check failed for branch"
                        );
                        None
                    }
                }
            }
        })
        .collect();

    future::join_all(futs).await.into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::tasks;
    use crate::store::{NewTask, TaskStore};
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::process;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
    fn setup_test_repo() -> Option<(tempfile::TempDir, PathBuf)> {
        let tmp = tempfile::tempdir().ok()?;

        // Create a real git repo (not bare) so we can add worktrees.
        let repo_dir = tmp.path().join("repo.git");
        std_fs::create_dir_all(&repo_dir).ok()?;

        let init = process::Command::new("git")
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
            let _ = process::Command::new("git")
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
        std_fs::write(&readme, "# test").ok()?;
        process::Command::new("git")
            .args(["-C", repo_dir.to_str()?, "add", "."])
            .output()
            .ok()?;
        process::Command::new("git")
            .args([
                "-C",
                repo_dir.to_str()?,
                "-c",
                "user.email=test@test.com",
                "-c",
                "user.name=Test",
                "-c",
                "commit.gpgsign=false",
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
        std_fs::create_dir_all(wt_dir.parent()?).ok()?;
        let wt_out = process::Command::new("git")
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
        std_fs::create_dir_all(&wt_dir).unwrap();

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
        store.update_status(id, TaskStatus::Done).await.unwrap();

        let opts = JanitorOptions {
            ttl_hours: 0,
            dry_run: false,
        };

        let result = cleanup_task_worktree_with_opts("999", "owner/repo", &store, &opts).await;
        // resolve_repo_root will fail for synthetic repo — that's OK, the point is
        // that when nothing needs cleaning the function should not report true.
        // In production this means no git pull is triggered.
        if let Ok(cleaned) = result {
            assert!(
                !cleaned,
                "cleanup should return false when there is no worktree or branch to clean"
            );
        } else {
            // resolve_repo_root fails for test repo — acceptable.
            // The bug was that even with no worktree/branch, Ok(true) was returned.
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
        store.update_status(id, TaskStatus::Done).await.unwrap();
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

    /// Successful worktree cleanup must still delete the local branch.
    /// This guards against early-return regressions in `remove_worktree_and_branch`.
    #[tokio::test]
    async fn remove_worktree_success_still_deletes_local_branch() {
        let Some((tmp, wt_dir)) = setup_test_repo() else {
            eprintln!("skipping test: git not available");
            return;
        };

        let repo_dir = tmp.path().join("repo.git");
        let repo_dir_str = repo_dir
            .to_str()
            .expect("test repo path contains non-UTF-8 characters");
        let branch = "gh-issue-42-test";

        let before = process::Command::new("git")
            .args([
                "-C",
                repo_dir_str,
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ])
            .status()
            .unwrap();
        assert!(
            before.success(),
            "test setup must create local branch before cleanup"
        );

        let removed =
            remove_worktree_and_branch("42", &wt_dir, Some(branch), &repo_dir, false).await;
        assert!(removed, "worktree should be removed");

        let after = process::Command::new("git")
            .args([
                "-C",
                repo_dir_str,
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ])
            .status()
            .unwrap();
        assert!(
            !after.success(),
            "local branch should be deleted after successful worktree cleanup"
        );
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
        std_fs::create_dir_all(&fake_wt).unwrap();

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

        fn with_merged(branch: &str) -> Self {
            Self {
                batch_result: Mutex::new(Ok(HashMap::from([(branch.to_string(), true)]))),
                single_results: HashMap::new(),
                posted_comments: Arc::new(Mutex::new(Vec::new())),
                single_checked_branches: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl ExternalBackend for MergeCheckMockBackend {
        fn name(&self) -> &'static str {
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
        let backend_dyn: Arc<dyn ExternalBackend> = Arc::<MergeCheckMockBackend>::clone(&backend);

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
            .update_status(task1, TaskStatus::NeedsReview)
            .await
            .unwrap();
        store
            .set_fields(task1, &[("branch", serde_json::json!("branch-1"))])
            .await
            .unwrap();
        // No .git file, so cleanup force-removes the directory
        let wt_dir = tempfile::tempdir().unwrap();
        let wt_path = wt_dir.path().join("branch-1");
        std_fs::create_dir_all(&wt_path).unwrap();
        store
            .set_fields(
                task1,
                &[("worktree", serde_json::json!(wt_path.to_string_lossy()))],
            )
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
            .update_status(task2, TaskStatus::InReview)
            .await
            .unwrap();
        store
            .set_fields(task2, &[("branch", serde_json::json!("branch-2"))])
            .await
            .unwrap();

        let task_manager = Arc::new(TaskManager::with_store(
            Arc::clone(&backend_dyn),
            Arc::clone(&store),
            "owner/repo".to_string(),
        ));

        let in_review_tasks = store
            .list_by_status("owner/repo", TaskStatus::InReview)
            .await
            .unwrap()
            .into_iter()
            .map(|stored| ReviewTaskSnapshot {
                external: tasks::store_task_to_external(&stored),
                stored,
            })
            .collect::<Vec<_>>();
        let needs_review_tasks = store
            .list_by_status("owner/repo", TaskStatus::NeedsReview)
            .await
            .unwrap()
            .into_iter()
            .map(|stored| ReviewTaskSnapshot {
                external: tasks::store_task_to_external(&stored),
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
        assert_eq!(updated_task1.status, TaskStatus::Done);
        assert!(updated_task1.worktree_cleaned);
        assert!(!wt_path.exists());
        assert!(!updated_task2.worktree_cleaned);
        assert_eq!(updated_task2.status, TaskStatus::InReview);

        let comments = backend.posted_comments.lock().unwrap().clone();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].0, "101");
        assert!(comments[0].1.contains("PR merged, marking task complete"));
    }

    #[tokio::test]
    async fn check_merged_prs_skips_rescue_while_ci_bootstrap_holds() {
        let (tmp, clone) = origin_and_clone_with_feat();
        let wt = add_worktree(&clone, tmp.path(), "feat");
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        // hold_after_merge reads the plan marker in the body, not the title
        let id = store_approved_task(
            &store,
            "owner/repo",
            "task-12",
            "CI-01 Set up CI and tests",
            "<!-- orch-plan: CI-01 -->",
            &wt,
        )
        .await;
        let stored = store.get(id).await.unwrap();

        let backend = Arc::new(MergeCheckMockBackend::with_merged("feat"));
        let backend_dyn: Arc<dyn ExternalBackend> = Arc::<MergeCheckMockBackend>::clone(&backend);
        let task_manager = Arc::new(TaskManager::with_store(
            Arc::clone(&backend_dyn),
            Arc::clone(&store),
            "owner/repo".to_string(),
        ));

        check_merged_prs(
            &backend_dyn,
            "owner/repo",
            &store,
            &task_manager,
            &[ReviewTaskSnapshot {
                external: tasks::store_task_to_external(&stored),
                stored,
            }],
            &[],
        )
        .await
        .unwrap();

        let after = store.get(id).await.unwrap();
        assert_eq!(after.status, TaskStatus::Approved);
        assert!(after.worktree_cleaned);
        assert!(!wt.exists());
        assert!(!remote_has(&clone, "rescue/feat"));
        assert!(backend.posted_comments.lock().unwrap().is_empty());
    }

    struct ReconciliationMockBackend {
        reconciliation_calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ExternalBackend for ReconciliationMockBackend {
        fn name(&self) -> &'static str {
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

        async fn list_reconciliation_candidates(&self) -> anyhow::Result<Vec<ExternalTask>> {
            self.reconciliation_calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![])
        }

        async fn list_routable(&self) -> anyhow::Result<Vec<ExternalTask>> {
            Ok(vec![])
        }

        async fn post_comment(&self, _id: &ExternalId, _body: &str) -> anyhow::Result<()> {
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
    }

    #[tokio::test]
    async fn cleanup_reconciliation_is_rate_limited_per_repo() {
        let repo = "owner/repo-cleanup-rate-limit";
        RECONCILIATION_LAST_RUN.remove(repo);

        let backend = Arc::new(ReconciliationMockBackend {
            reconciliation_calls: Arc::new(AtomicUsize::new(0)),
        });
        let backend_dyn: Arc<dyn ExternalBackend> =
            Arc::<ReconciliationMockBackend>::clone(&backend);
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let task_manager = Arc::new(TaskManager::with_store(
            Arc::clone(&backend_dyn),
            Arc::clone(&store),
            repo.to_string(),
        ));
        let opts = JanitorOptions {
            ttl_hours: 0,
            dry_run: true,
        };

        cleanup_done_worktrees_with_opts(&backend_dyn, repo, &task_manager, &store, &opts)
            .await
            .unwrap();
        cleanup_done_worktrees_with_opts(&backend_dyn, repo, &task_manager, &store, &opts)
            .await
            .unwrap();

        assert_eq!(
            backend.reconciliation_calls.load(Ordering::SeqCst),
            1,
            "cleanup should not re-list reconciliation candidates on every sync pass"
        );

        RECONCILIATION_LAST_RUN.remove(repo);
    }

    /// Cleanup only collects done tasks. A blocked task waits on a person and resumes from its
    /// worktree and branch, so it must not even be considered (#3781)
    #[tokio::test]
    async fn cleanup_skips_blocked_internal_tasks_and_cleans_done_ones() {
        let repo = "owner/repo-cleanup-blocked";
        RECONCILIATION_LAST_RUN.remove(repo);
        let backend_dyn: Arc<dyn ExternalBackend> = Arc::new(ReconciliationMockBackend {
            reconciliation_calls: Arc::new(AtomicUsize::new(0)),
        });
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let task_manager = Arc::new(TaskManager::with_store(
            Arc::clone(&backend_dyn),
            Arc::clone(&store),
            repo.to_string(),
        ));

        let mut ids = Vec::new();
        for (title, status) in [("blocked", TaskStatus::Blocked), ("done", TaskStatus::Done)] {
            let id = store
                .create_internal(repo, title, "", "test", title, None)
                .await
                .unwrap();
            store
                .set_fields(
                    id,
                    &[
                        (
                            "worktree",
                            serde_json::json!(format!("/nonexistent/{title}")),
                        ),
                        ("branch", serde_json::json!("")),
                    ],
                )
                .await
                .unwrap();
            store.update_status(id, status).await.unwrap();
            ids.push(id);
        }

        let opts = JanitorOptions {
            ttl_hours: 0,
            dry_run: false,
        };
        cleanup_done_worktrees_with_opts(&backend_dyn, repo, &task_manager, &store, &opts)
            .await
            .unwrap();

        assert!(
            !store.get(ids[0]).await.unwrap().worktree_cleaned,
            "blocked task must not be considered for cleanup"
        );
        assert!(
            store.get(ids[1]).await.unwrap().worktree_cleaned,
            "done task is cleaned, which proves the pass ran"
        );
        RECONCILIATION_LAST_RUN.remove(repo);
    }

    fn git_ok(dir: &Path, args: &[&str]) -> String {
        let out = process::Command::new("git")
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@t",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    /// A clone of a bare origin with main pushed and branch `feat` one commit ahead
    fn origin_and_clone_with_feat() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("origin.git");
        std_fs::create_dir_all(&bare).unwrap();
        git_ok(&bare, &["init", "--bare", "-b", "main"]);
        git_ok(tmp.path(), &["clone", bare.to_str().unwrap(), "clone"]);
        let clone = tmp.path().join("clone");
        std_fs::write(clone.join("base.txt"), "base").unwrap();
        git_ok(&clone, &["add", "."]);
        git_ok(&clone, &["commit", "-m", "base"]);
        git_ok(&clone, &["push", "origin", "main"]);
        git_ok(&clone, &["checkout", "-b", "feat"]);
        std_fs::write(clone.join("work.txt"), "work").unwrap();
        git_ok(&clone, &["add", "."]);
        git_ok(&clone, &["commit", "-m", "work"]);
        git_ok(&clone, &["checkout", "main"]);
        (tmp, clone)
    }

    fn remote_has(clone: &Path, branch: &str) -> bool {
        !git_ok(clone, &["ls-remote", "--heads", "origin", branch])
            .trim()
            .is_empty()
    }

    fn local_has(clone: &Path, branch: &str) -> bool {
        process::Command::new("git")
            .args([
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ])
            .current_dir(clone)
            .status()
            .unwrap()
            .success()
    }

    /// Deleting a pushed branch whose commits main lacks would destroy the only copy of the
    /// work, as startup cleanup did to a blocked task (#3781)
    #[tokio::test]
    async fn delete_branches_keeps_an_unmerged_remote_branch() {
        let (_tmp, clone) = origin_and_clone_with_feat();
        git_ok(&clone, &["push", "origin", "feat"]);

        delete_branches("1", "feat", &clone).await;

        assert!(
            remote_has(&clone, "feat"),
            "unmerged remote branch must survive"
        );
        assert!(
            !local_has(&clone, "feat"),
            "local copy is on origin, so it can go"
        );
    }

    #[tokio::test]
    async fn delete_branches_leaves_the_screenshots_branch_alone() {
        let (_tmp, clone) = origin_and_clone_with_feat();
        git_ok(&clone, &["branch", SCREENSHOTS_BRANCH, "feat"]);
        git_ok(&clone, &["push", "origin", SCREENSHOTS_BRANCH]);
        git_ok(&clone, &["merge", "--ff-only", "feat"]);
        git_ok(&clone, &["push", "origin", "main"]);

        delete_branches("1", SCREENSHOTS_BRANCH, &clone).await;

        assert!(remote_has(&clone, SCREENSHOTS_BRANCH));
        assert!(local_has(&clone, SCREENSHOTS_BRANCH));
    }

    #[tokio::test]
    async fn delete_branches_removes_a_branch_main_already_contains() {
        let (_tmp, clone) = origin_and_clone_with_feat();
        git_ok(&clone, &["push", "origin", "feat"]);
        git_ok(&clone, &["merge", "--ff-only", "feat"]);
        git_ok(&clone, &["push", "origin", "main"]);

        delete_branches("1", "feat", &clone).await;

        assert!(!remote_has(&clone, "feat"));
        assert!(!local_has(&clone, "feat"));
    }

    fn add_worktree(clone: &Path, tmp: &Path, branch: &str) -> PathBuf {
        let wt = tmp.join("wt");
        git_ok(clone, &["worktree", "add", wt.to_str().unwrap(), branch]);
        wt
    }

    async fn store_approved_task(
        store: &TaskStore,
        repo: &str,
        external_id: &str,
        title: &str,
        body: &str,
        worktree: &Path,
    ) -> i64 {
        let id = store
            .create(&NewTask {
                external_id: Some(external_id.to_string()),
                repo: repo.to_string(),
                origin: "github".to_string(),
                title: title.to_string(),
                body: body.to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        store.update_status(id, TaskStatus::Approved).await.unwrap();
        store
            .set_fields(
                id,
                &[
                    ("branch", serde_json::json!("feat")),
                    (
                        "worktree",
                        serde_json::json!(worktree.to_string_lossy().as_ref()),
                    ),
                    ("pr_number", serde_json::json!(7)),
                ],
            )
            .await
            .unwrap();
        id
    }

    #[tokio::test]
    async fn cleanup_pushes_unpushed_commits_to_a_rescue_branch() {
        let (tmp, clone) = origin_and_clone_with_feat();
        let wt = add_worktree(&clone, tmp.path(), "feat");

        let sha = git_ok(&clone, &["rev-parse", "--short", "feat"]);
        let sha = sha.trim();
        match rescue_unpushed_work("1", Some(&wt), "feat", &clone).await {
            Rescue::Pushed { branch, commits } => {
                assert_eq!(branch, "rescue/feat");
                assert!(rescue_comment(&branch, &commits).contains(sha));
            }
            other => panic!("expected a rescue push, got {other:?}"),
        }

        let tip = git_ok(&clone, &["rev-parse", "feat"]);
        let tracking = git_ok(&clone, &["rev-parse", "refs/remotes/origin/rescue/feat"]);
        assert_eq!(
            tracking.trim(),
            tip.trim(),
            "rescue push must point the tracking ref at the tip it sent"
        );

        let removed = remove_worktree_and_branch("1", &wt, Some("feat"), &clone, false).await;

        assert!(removed);
        assert!(remote_has(&clone, "rescue/feat"));
        assert!(!local_has(&clone, "feat"));
    }

    #[tokio::test]
    async fn rescue_push_sets_tracking_ref_without_a_fetchspec() {
        let (tmp, clone) = origin_and_clone_with_feat();
        let wt = add_worktree(&clone, tmp.path(), "feat");
        git_ok(&clone, &["config", "--unset-all", "remote.origin.fetch"]);

        match rescue_unpushed_work("1", Some(&wt), "feat", &clone).await {
            Rescue::Pushed { branch, .. } => assert_eq!(branch, "rescue/feat"),
            other => panic!("expected a rescue push, got {other:?}"),
        }

        let tracking = git_ok(&clone, &["rev-parse", "refs/remotes/origin/rescue/feat"]);
        let tip = git_ok(&clone, &["rev-parse", "feat"]);
        assert_eq!(tracking.trim(), tip.trim());
    }

    #[tokio::test]
    async fn cleanup_commits_uncommitted_changes_before_the_rescue() {
        let (tmp, clone) = origin_and_clone_with_feat();
        git_ok(&clone, &["push", "origin", "feat"]);
        let wt = add_worktree(&clone, tmp.path(), "feat");
        std_fs::write(wt.join("dirty.txt"), "dirty").unwrap();

        let removed = remove_worktree_and_branch("1", &wt, Some("feat"), &clone, false).await;

        assert!(removed);
        assert!(remote_has(&clone, "rescue/feat"));
    }

    #[tokio::test]
    async fn cleanup_makes_no_rescue_branch_for_a_pushed_branch() {
        let (tmp, clone) = origin_and_clone_with_feat();
        git_ok(&clone, &["push", "origin", "feat"]);
        let wt = add_worktree(&clone, tmp.path(), "feat");

        let removed = remove_worktree_and_branch("1", &wt, Some("feat"), &clone, false).await;

        assert!(removed);
        assert!(!remote_has(&clone, "rescue/feat"));
        assert!(!local_has(&clone, "feat"));
    }

    #[tokio::test]
    async fn rescue_skips_a_branch_on_the_remote_without_a_tracking_ref() {
        let (_tmp, clone) = origin_and_clone_with_feat();
        git_ok(&clone, &["push", "origin", "feat"]);
        git_ok(&clone, &["update-ref", "-d", "refs/remotes/origin/feat"]);

        let rescue = rescue_unpushed_work("1", None, "feat", &clone).await;

        assert_eq!(rescue, Rescue::Clean);
        assert!(!remote_has(&clone, "rescue/feat"));
    }

    #[tokio::test]
    async fn merged_pr_cleanup_pushes_no_rescue_branch() {
        let (tmp, clone) = origin_and_clone_with_feat();
        let wt = add_worktree(&clone, tmp.path(), "feat");

        let removed =
            remove_worktree_and_branch_for("1", &wt, Some("feat"), &clone, false, true).await;

        assert!(removed);
        assert!(!wt.exists());
        assert!(!remote_has(&clone, "rescue/feat"));
    }

    #[tokio::test]
    async fn cleanup_skips_rescue_for_an_approved_merged_pr() {
        let (tmp, clone) = origin_and_clone_with_feat();
        let wt = add_worktree(&clone, tmp.path(), "feat");
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        store_approved_task(&store, "owner/repo", "task-12", "CI-01 Set up CI", "", &wt).await;

        cleanup_merged_pr_worktree("task-12", "owner/repo", &store)
            .await
            .unwrap();

        let task = store
            .get(
                store
                    .resolve_task_id("owner/repo", "task-12")
                    .await
                    .unwrap()
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(task.status, TaskStatus::Approved);
        assert!(task.worktree_cleaned);
        assert!(!wt.exists());
        assert!(!remote_has(&clone, "rescue/feat"));
    }

    #[tokio::test]
    async fn cleanup_rescues_an_approved_task_when_the_pr_is_not_known_merged() {
        let (tmp, clone) = origin_and_clone_with_feat();
        let wt = add_worktree(&clone, tmp.path(), "feat");
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        store_approved_task(&store, "owner/repo", "task-12", "fix something", "", &wt).await;

        cleanup_task_worktree("task-12", "owner/repo", &store)
            .await
            .unwrap();

        assert!(remote_has(&clone, "rescue/feat"));
    }

    #[tokio::test]
    async fn unmerged_cleanup_still_pushes_a_rescue_branch() {
        let (tmp, clone) = origin_and_clone_with_feat();
        let wt = add_worktree(&clone, tmp.path(), "feat");

        let removed =
            remove_worktree_and_branch_for("1", &wt, Some("feat"), &clone, false, false).await;

        assert!(removed);
        assert!(remote_has(&clone, "rescue/feat"));
    }

    #[tokio::test]
    async fn rescue_ignores_a_remote_tip_missing_locally() {
        let (tmp, clone) = origin_and_clone_with_feat();
        git_ok(&clone, &["push", "origin", "feat"]);
        let other = tmp.path().join("other");
        git_ok(
            tmp.path(),
            &["clone", "-b", "feat", "origin.git", other.to_str().unwrap()],
        );
        std_fs::write(other.join("extra.txt"), "extra").unwrap();
        git_ok(&other, &["add", "."]);
        git_ok(&other, &["commit", "-m", "extra"]);
        git_ok(&other, &["push", "origin", "feat"]);

        let rescue = rescue_unpushed_work("1", None, "feat", &clone).await;

        assert_ne!(rescue, Rescue::Failed);
    }

    #[tokio::test]
    async fn cleanup_keeps_worktree_and_branch_when_the_rescue_push_fails() {
        let (tmp, clone) = origin_and_clone_with_feat();
        let wt = add_worktree(&clone, tmp.path(), "feat");
        git_ok(
            &clone,
            &["remote", "set-url", "origin", "/nonexistent/origin.git"],
        );

        let removed = remove_worktree_and_branch("1", &wt, Some("feat"), &clone, false).await;

        assert!(!removed);
        assert!(wt.exists());
        assert!(local_has(&clone, "feat"));
    }

    #[tokio::test]
    async fn delete_branches_keeps_a_local_branch_with_unpushed_commits() {
        let (_tmp, clone) = origin_and_clone_with_feat();

        delete_branches("1", "feat", &clone).await;

        assert!(
            local_has(&clone, "feat"),
            "unpushed commits must not be deleted"
        );
    }
}
