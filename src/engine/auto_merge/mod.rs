//! PR auto-merge and review-changes handling.
//!
//! Extracted from `engine/review.rs`. Handles:
//! - [`auto_merge_pr`]: merge an approved PR after CI passes
//! - rebase recovery on merge conflict (prevents infinite needs_review loop)
//! - [`handle_review_changes`]: re-dispatch agent to address review feedback
//! - [`dedup_reviews`]: deduplicate GitHub reviews by reviewer (keep latest)
//! - [`any_changes_requested_in_reviews`]: check if any reviewer's latest review blocks merge
//! - [`crate::engine::attribution_footer`]: standard "Commented/Reviewed by bot" footer for GitHub comments

use crate::backends::{ExternalBackend, ExternalTask, Status};
use crate::config;
use crate::engine;
use crate::engine::ci_bootstrap;
use crate::engine::cleanup::{cleanup_merged_pr_worktree, cleanup_task_worktree};
use crate::engine::review;
use crate::engine::review_gate;
use crate::engine::review_poll;
use crate::engine::router::config::RouterConfig;
use crate::engine::runner::response::ReviewIssue;
use crate::engine::runner::{git_ops, worktree};
use crate::engine::tasks::TaskManager;
use crate::github::app;
use crate::github::http::GhHttp;
use crate::github::types::GitHubComment;
use crate::github::types::{GitHubCheckRun, GitHubPullRequest, GitHubReview, RequiredStatusCheck};
use crate::security;
use crate::store;
use crate::store::TaskStatus;
use crate::store::TaskStore;
use crate::store::{
    opt_store_get_task, store_increment, store_reset_failure_counters, store_set_result,
};
use crate::tmux::TmuxManager;
use std::cmp;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use std::time::Instant;
use tokio::sync::Semaphore;
use tokio::time;

/// Minimum interval between CI status checks for the same task.
///
/// Without this cooldown, the engine polls CI for every in_review task on every
/// tick (~10s). With 20+ tasks in CI failure loops, this creates 120+ requests/min,
/// quickly exhausting GitHub's 5000-point/hour GraphQL rate limit. At 60s minimum
/// polling interval, 20 tasks generate at most 20 requests/min — a 6x reduction.
///
/// Configurable via workflow.ci_check_cooldown_secs.
const DEFAULT_CI_CHECK_COOLDOWN_SECS: u64 = 60;

/// Maximum number of times we will attempt to rebase and retry a conflicting PR
/// before giving up and blocking the task for human intervention
pub(crate) const MAX_MERGE_CONFLICT_RETRIES: u64 = 3;

/// Maximum number of CI failure or CI timeout events in `auto_merge_pr` before
/// the task is blocked for human intervention instead of re-entering NeedsReview.
pub(crate) const MAX_CI_MERGE_FAILURES: u64 = 3;

/// Merge-time CI-failure block. The recovery sweep only acts on this stage
pub(crate) const CI_FAILURE_AUTO_MERGE_STAGE: &str = "during auto-merge";

/// Pre-review CI-failure block. A person unblocks these, recovery must not merge them
pub(crate) const CI_FAILURE_BEFORE_REVIEW_STAGE: &str = "before review";

// Global semaphore limiting concurrent CI polling loops across all tasks.
static CI_POLL_SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();

fn ci_poll_semaphore() -> &'static Arc<Semaphore> {
    CI_POLL_SEMAPHORE.get_or_init(|| Arc::new(Semaphore::new(3)))
}

/// Shared by the review gate and auto-merge so the recovery sweep can tell them apart
pub(crate) fn ci_failure_limit_reason(stage: &str) -> String {
    format!("CI failure limit ({MAX_CI_MERGE_FAILURES}) reached {stage}")
}

async fn finish_merged_pr(
    task: &ExternalTask,
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
    repo: &str,
) -> anyhow::Result<()> {
    if ci_bootstrap::hold_after_merge(task, store, repo).await {
        tracing::info!(
            task_id = task.id.0,
            "CI bootstrap merged, holding done until required checks exist"
        );
    } else {
        task_manager
            .update_task_status(&task.id, Status::Done)
            .await?;
    }
    if let Err(e) = cleanup_merged_pr_worktree(&task.id.0, repo, store).await {
        tracing::warn!(
            task_id = task.id.0,
            err = %e,
            "post-merge cleanup failed"
        );
    }
    Ok(())
}

/// Confirm a closed PR still belongs to `branch`, then mark the task done
async fn finish_already_closed_pr(
    gh: &GhHttp,
    task: &ExternalTask,
    branch: &str,
    repo: &str,
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
) -> anyhow::Result<()> {
    match gh.get_closed_pr_state(repo, branch).await {
        Ok(Some((closed_pr_number, true, _))) => {
            let pr = gh.get_pr(repo, closed_pr_number).await.inspect_err(|e| {
                tracing::warn!(
                    task_id = task.id.0,
                    branch,
                    pr_number = closed_pr_number,
                    err = %e,
                    "failed to verify PR details"
                );
            })?;
            if pr.head.ref_ != branch {
                tracing::warn!(
                    task_id = task.id.0,
                    branch,
                    pr_number = closed_pr_number,
                    "closed PR head ref does not match branch — may be wrong PR"
                );
                anyhow::bail!("no matching merged PR found for branch {branch}");
            }
            tracing::info!(
                task_id = task.id.0,
                branch,
                pr_number = closed_pr_number,
                "PR already merged — marking task done (idempotent)"
            );
            finish_merged_pr(task, task_manager, store, repo).await?;
            Ok(())
        }
        Ok(Some((closed_pr_number, false, state))) => {
            let pr = gh.get_pr(repo, closed_pr_number).await.inspect_err(|e| {
                tracing::warn!(
                    task_id = task.id.0,
                    branch,
                    pr_number = closed_pr_number,
                    err = %e,
                    "failed to verify PR details"
                );
            })?;
            if pr.head.ref_ != branch {
                tracing::warn!(
                    task_id = task.id.0,
                    branch,
                    pr_number = closed_pr_number,
                    "closed PR head ref does not match branch — may be wrong PR"
                );
                anyhow::bail!("no matching closed PR found for branch {branch}");
            }
            let approved = store::opt_store_get_task(&Some(Arc::clone(store)), repo, &task.id.0)
                .await
                .is_some_and(|t| t.status == TaskStatus::Approved);
            if approved {
                review_poll::block_closed_approved_pr(task_manager, &task.id).await;
                return Ok(());
            }

            tracing::info!(
                task_id = task.id.0,
                branch,
                pr_number = closed_pr_number,
                pr_state = %state,
                "PR was closed without merge — marking task done"
            );
            task_manager
                .update_task_status(&task.id, Status::Done)
                .await?;
            if let Err(e) = cleanup_task_worktree(&task.id.0, repo, store).await {
                tracing::warn!(
                    task_id = task.id.0,
                    err = %e,
                    "post-close cleanup failed"
                );
            }
            Ok(())
        }
        Ok(None) => {
            anyhow::bail!("no open PR found for branch {branch}");
        }
        Err(e) => {
            tracing::warn!(
                task_id = task.id.0,
                branch,
                err = %e,
                "failed to check closed PR state"
            );
            anyhow::bail!("no open PR found for branch {branch}");
        }
    }
}

/// Check if we should skip CI status checking for this task due to per-task cooldown.
///
/// Uses the KV store to track the last time CI was checked for each task.
/// Returns `true` if the cooldown has not elapsed yet.
async fn is_ci_check_in_cooldown(
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
) -> anyhow::Result<bool> {
    let cooldown_secs: u64 = config::get("workflow.ci_check_cooldown_secs")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_CI_CHECK_COOLDOWN_SECS);

    let key = ci_check_ts_key(repo, task_id);
    let last_check = store.kv_get(&key).await?;

    if let Some(last_ts) = last_check
        && let Ok(last_time) = chrono::DateTime::parse_from_rfc3339(&last_ts)
    {
        let elapsed =
            chrono::Utc::now().signed_duration_since(last_time.with_timezone(&chrono::Utc));
        if elapsed.num_seconds() < i64::try_from(cooldown_secs).unwrap_or(i64::MAX) {
            return Ok(true);
        }
    }

    Ok(false)
}

fn is_ssh_agent_fetch_failure(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("sign_and_send_pubkey")
        || lower.contains("agent refused operation")
        || lower.contains("communication with agent failed")
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WorktreeRebaseOutcome {
    RebasedAndPushed,
    RebaseConflict(String),
    PushFailed(String),
    SshFetchFailed(String),
    /// Fetch timed out, GitHub was unreachable, or a ref lock blocked the fetch
    Transient(String),
    Unavailable(String),
}

fn is_transient_fetch_failure(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("could not connect")
        || lower.contains("failed to connect")
        || lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("cannot lock ref")
        || git_ops::is_transient_network_error(stderr)
        || git_ops::is_transient_github_error(stderr)
}

fn classify_fetch_failure(stderr: &str) -> WorktreeRebaseOutcome {
    if is_ssh_agent_fetch_failure(stderr) {
        WorktreeRebaseOutcome::SshFetchFailed(stderr.to_string())
    } else if is_transient_fetch_failure(stderr) {
        WorktreeRebaseOutcome::Transient(format!("git fetch failed: {stderr}"))
    } else {
        WorktreeRebaseOutcome::Unavailable(format!("git fetch failed: {stderr}"))
    }
}

/// The task worktree, when it sits on the PR head and the PR base is the branch a rebase targets.
/// Otherwise a push would carry other commits under the PR
async fn worktree_matching_pr(
    task_id: &str,
    repo: &str,
    pr_number: u64,
    pr: &GitHubPullRequest,
    store: &Arc<TaskStore>,
) -> Option<String> {
    let wt = opt_store_get_task(&Some(Arc::clone(store)), repo, task_id)
        .await
        .map(|t| t.worktree)
        .filter(|wt| !wt.is_empty())?;
    let path = Path::new(&wt);
    let same_repo = pr
        .head
        .repo
        .as_ref()
        .is_some_and(|r| r.full_name.eq_ignore_ascii_case(repo));
    let on_head = review::worktree_head_sha(path).await.as_deref() == Some(pr.head.sha.as_str());
    let on_branch = git_ops::get_current_branch(path).await == pr.head.ref_;
    let same_base = worktree::detect_default_branch(path).await == pr.base.ref_;
    if same_repo && on_head && on_branch && same_base {
        return Some(wt);
    }
    tracing::info!(
        task_id,
        pr_number,
        same_repo,
        on_head,
        on_branch,
        same_base,
        "worktree does not match the PR, not rebasing it"
    );
    None
}

/// Rebase a PR that fell behind its base in the task's worktree and carry its approval to the
/// rebased head. Otherwise, or when the rebase or push fails, GitHub merges the base in
async fn bring_branch_up_to_date(
    task: &ExternalTask,
    repo: &str,
    pr_number: u64,
    pr: &GitHubPullRequest,
    gh: &GhHttp,
    store: &Arc<TaskStore>,
) {
    let task_id = task.id.0.as_str();
    let worktree = worktree_matching_pr(task_id, repo, pr_number, pr, store).await;

    match attempt_worktree_rebase_and_force_push(task_id, worktree.clone()).await {
        WorktreeRebaseOutcome::RebasedAndPushed => {
            review::carry_approval_after_rebase(store, gh, repo, task_id, pr, pr_number).await;
            tracing::info!(
                task_id,
                pr_number,
                "rebased the branch onto its base, checks run again"
            );
        }
        outcome => {
            tracing::info!(
                task_id,
                pr_number,
                ?outcome,
                "rebase not possible, merging the base in"
            );
            if let Err(e) = gh.update_pr_branch(repo, pr_number).await {
                tracing::warn!(task_id, pr_number, err = %e, "update-branch failed; retrying next tick");
            }
        }
    }
}

/// The branch check before a review (#3973): a review of a PR that conflicts with its base, or
/// that fell behind it, is wasted because the merge then rebases the branch. Returns true when the
/// review may start. A branch that is behind or conflicting is rebased in the task worktree and
/// the review waits for CI on the new head. A conflict the rebase cannot solve goes back to the
/// task agent, which has the context of the change, and the reviewer never edits the branch
pub(crate) async fn prepare_branch_for_review(
    task: &ExternalTask,
    repo: &str,
    pr_number: u64,
    gh: &GhHttp,
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
) -> bool {
    let task_id = task.id.0.as_str();
    let (pr, has_conflict) = match poll_mergeable_until(
        gh,
        repo,
        pr_number,
        Duration::from_secs(10),
    )
    .await
    {
        Ok(v) => v,
        // The merge checks the branch again, so an unknown state does not hold the review
        Err(e) => {
            tracing::warn!(task_id, pr_number, err = %e, "mergeability unknown, starting the review");
            return true;
        }
    };
    if !has_conflict && !is_pr_behind(&pr) {
        return true;
    }
    if !has_conflict {
        bring_branch_up_to_date(task, repo, pr_number, &pr, gh, store).await;
        return false;
    }

    let retries = opt_store_get_task(&Some(Arc::clone(store)), repo, task_id)
        .await
        .map_or(0, |t| u64::try_from(t.merge_conflict_retries).unwrap_or(0));
    if retries >= MAX_MERGE_CONFLICT_RETRIES {
        let fields = [(
            "block_reason",
            serde_json::json!(format!(
                "merge conflict retry limit ({MAX_MERGE_CONFLICT_RETRIES}) reached before review"
            )),
        )];
        if let Err(e) = task_manager
            .update_task_status_and_result(&task.id, Status::Blocked, &fields)
            .await
        {
            tracing::error!(task_id, err = %e, "failed to block a task that conflicts with its base");
        }
        return false;
    }

    let worktree = worktree_matching_pr(task_id, repo, pr_number, &pr, store).await;
    let context = match attempt_worktree_rebase_and_force_push(task_id, worktree).await {
        WorktreeRebaseOutcome::RebasedAndPushed => {
            tracing::info!(
                task_id,
                pr_number,
                "rebased a conflicting branch, review waits for CI on the new head"
            );
            return false;
        }
        WorktreeRebaseOutcome::Transient(reason) => {
            tracing::warn!(
                task_id,
                pr_number,
                reason = %reason,
                "transient fetch failure before review, waiting for the next tick"
            );
            return false;
        }
        WorktreeRebaseOutcome::RebaseConflict(_) => {
            "Your PR conflicts with its base branch before review. Rebasing hit a content conflict and the worktree has the rebase in progress (see `git status`). Resolve the conflict markers keeping your changes, `git add` the files and run `git rebase --continue`. Do not redo the task from scratch."
        }
        WorktreeRebaseOutcome::SshFetchFailed(_)
        | WorktreeRebaseOutcome::PushFailed(_)
        | WorktreeRebaseOutcome::Unavailable(_) => {
            "Your PR conflicts with its base branch before review and orch could not rebase it. Rebase the branch onto the default branch (check `git status` first), resolve the conflicts and make sure the checks pass."
        }
    };

    if let Err(e) = store_increment(
        &Some(Arc::clone(store)),
        repo,
        task_id,
        "merge_conflict_retries",
    )
    .await
    {
        tracing::warn!(task_id, err = %e, "failed to increment merge_conflict_retries, review waits");
        return false;
    }
    tracing::warn!(
        task_id,
        pr_number,
        "PR conflicts with its base before review, routing back to the task agent"
    );
    let fields = [
        (
            "last_error",
            serde_json::json!(format!("PR #{pr_number} conflicts with its base branch")),
        ),
        (
            "route_reason",
            serde_json::json!("re-dispatch after a merge conflict before review"),
        ),
        ("pr_review_context", serde_json::json!(context)),
    ];
    if let Err(e) = task_manager
        .update_task_status_and_result(&task.id, Status::Routed, &fields)
        .await
    {
        tracing::error!(task_id, err = %e, "failed to route back a task that conflicts with its base");
    }
    false
}

pub(crate) fn is_pr_behind(pr: &GitHubPullRequest) -> bool {
    pr.mergeable_state
        .as_deref()
        .is_some_and(|state| state.eq_ignore_ascii_case("behind"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MergeFailureClass {
    RecoverableConflict,
    Transient,
    Blocking,
}

fn classify_merge_failure(error: &str) -> MergeFailureClass {
    let err_msg = error.to_ascii_lowercase();
    // The merge carried a head sha that is no longer the PR head, rebasing now would drop the new commits
    if err_msg.contains("head branch was modified") || err_msg.contains("sha does not match") {
        return MergeFailureClass::Transient;
    }
    // Draft merges return HTTP 405, keep the task until the PR is ready
    if err_msg.contains("pull request is a draft") {
        return MergeFailureClass::Transient;
    }
    let is_conflict = err_msg.contains("merge conflict")
        || err_msg.contains("not mergeable")
        || (err_msg.contains("409") && err_msg.contains("head branch is out of date"))
        || err_msg.contains("review and try the merge again");
    let is_transient = git_ops::is_transient_github_error(error)
        || err_msg.contains("502")
        || err_msg.contains("503")
        || err_msg.contains("504")
        || err_msg.contains("server error")
        || err_msg.contains("bad gateway")
        || err_msg.contains("service unavailable");

    if is_transient {
        MergeFailureClass::Transient
    } else if is_conflict {
        MergeFailureClass::RecoverableConflict
    } else {
        MergeFailureClass::Blocking
    }
}

async fn attempt_worktree_rebase_and_force_push(
    task_id: &str,
    worktree_path: Option<String>,
) -> WorktreeRebaseOutcome {
    let Some(wt) = worktree_path else {
        return WorktreeRebaseOutcome::Unavailable("no worktree available".to_string());
    };

    let wt_path = PathBuf::from(&wt);
    if !wt_path.exists() {
        return WorktreeRebaseOutcome::Unavailable(format!("worktree missing: {wt}"));
    }

    tracing::info!(task_id, worktree = %wt, "attempting automatic rebase recovery");

    let default_branch = worktree::detect_default_branch(&wt_path).await;
    let branch = git_ops::get_current_branch(&wt_path).await;
    // Lease the feature-branch tip before fetch moves the tracking ref
    git_ops::record_push_lease(&wt_path, &branch, &default_branch).await;
    let git_timeout = Duration::from_secs(120);
    // Over HTTPS with the gh token, an SSH remote would ask the owner's SSH agent
    let fetch_result = time::timeout(
        git_timeout,
        git_ops::git()
            .await
            .args(["fetch", "origin"])
            .current_dir(&wt_path)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap_or_else(|_| {
        tracing::warn!(
            task_id,
            "git fetch timed out in rebase recovery after {}s",
            git_timeout.as_secs()
        );
        Err(io::Error::other("git fetch timed out"))
    });

    let rebase_result = match fetch_result {
        Ok(out) if out.status.success() => {
            git_ops::stash_rebase_restore(
                &wt_path,
                &format!("origin/{default_branch}"),
                git_ops::StashRebaseOpts {
                    abort_on_failure: false,
                },
            )
            .await
        }
        Ok(out) => {
            let stderr = git_ops::sanitize_push_error(String::from_utf8_lossy(&out.stderr).trim());
            let outcome = classify_fetch_failure(&stderr);
            if !matches!(outcome, WorktreeRebaseOutcome::SshFetchFailed(_)) {
                tracing::warn!(task_id, stderr = %stderr, "git fetch failed before rebase");
            }
            return outcome;
        }
        Err(err) => {
            let err = err.to_string();
            tracing::warn!(task_id, error = %err, "git fetch errored before rebase");
            return classify_fetch_failure(&err);
        }
    };

    match rebase_result {
        Ok(git_ops::RebaseOutcome::Succeeded) => {
            let push_result = time::timeout(
                git_timeout,
                git_ops::force_push_leased(&wt_path, &branch, &default_branch),
            )
            .await
            .unwrap_or_else(|_| {
                tracing::warn!(
                    task_id,
                    "git push timed out in rebase recovery after {}s",
                    git_timeout.as_secs()
                );
                Err(anyhow::anyhow!("git push timed out"))
            });

            match push_result {
                Ok(()) => WorktreeRebaseOutcome::RebasedAndPushed,
                Err(push_err) => WorktreeRebaseOutcome::PushFailed(push_err.to_string()),
            }
        }
        Ok(git_ops::RebaseOutcome::Failed(stderr)) => WorktreeRebaseOutcome::RebaseConflict(stderr),
        Ok(git_ops::RebaseOutcome::Skipped(reason)) => {
            WorktreeRebaseOutcome::Unavailable(format!("rebase skipped: {reason}"))
        }
        Err(err) => WorktreeRebaseOutcome::Unavailable(format!("rebase command failed: {err}")),
    }
}

/// Record that CI was checked for this task, updating the timestamp in KV store.
async fn record_ci_check(store: &Arc<TaskStore>, repo: &str, task_id: &str) -> anyhow::Result<()> {
    let key = ci_check_ts_key(repo, task_id);
    let now = chrono::Utc::now().to_rfc3339();
    store.kv_set(&key, &now).await
}

fn ci_check_ts_key(repo: &str, task_id: &str) -> String {
    format!("ci_check_ts:{repo}#{task_id}")
}

/// Prefix shared by the pending timers of every head of one task
fn ci_pending_since_prefix(repo: &str, task_id: &str) -> String {
    format!("ci_pending_since:{repo}#{task_id}#")
}

/// The head sha in the key means a new head starts a new pending window
fn ci_pending_since_key(repo: &str, task_id: &str, head_sha: &str) -> String {
    format!("{}{head_sha}", ci_pending_since_prefix(repo, task_id))
}

/// Prefix shared by the counted-failure keys of every head of one task
fn ci_failure_sha_prefix(repo: &str, task_id: &str) -> String {
    format!("ci_failure_sha:{repo}#{task_id}#")
}

fn ci_failure_sha_key(repo: &str, task_id: &str, head_sha: &str) -> String {
    format!("{}{head_sha}", ci_failure_sha_prefix(repo, task_id))
}

/// Increment `ci_merge_failures` once per head SHA. `None` when this SHA was already counted
pub(crate) async fn count_ci_failure_for_head(
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
    head_sha: &str,
) -> anyhow::Result<Option<u64>> {
    let key = (!head_sha.is_empty()).then(|| ci_failure_sha_key(repo, task_id, head_sha));
    if let Some(ref key) = key
        && store.kv_increment(key).await? > 1
    {
        return Ok(None);
    }
    match store_increment(&Some(Arc::clone(store)), repo, task_id, "ci_merge_failures").await {
        Ok(n) => Ok(Some(n)),
        Err(e) => {
            if let Some(ref key) = key
                && let Err(del) = store.kv_delete(key).await
            {
                tracing::warn!(
                    task_id,
                    err = %del,
                    "failed to roll back ci_failure_sha after increment error"
                );
            }
            Err(e)
        }
    }
}

/// Return how long CI has been continuously observed as `pending` for this
/// task, persisting the first-seen timestamp in the KV store.
///
/// `auto_merge_pr` runs as many short-lived invocations rather than one long
/// blocking call — `is_ci_check_in_cooldown` returns early well before
/// `max_wait` elapses, and the next sync tick spawns a fresh invocation with
/// its own local `Instant`. A local `start.elapsed()` therefore never
/// accumulates past a few seconds, so the pending-timeout escalation can
/// never fire. Persisting "first seen pending" here lets elapsed time
/// accumulate across those invocations the same way `ci_check_ts` already
/// does for the check cooldown.
async fn ci_pending_elapsed(
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
    head_sha: &str,
) -> anyhow::Result<chrono::Duration> {
    let key = ci_pending_since_key(repo, task_id, head_sha);
    let now = chrono::Utc::now();
    // `auto_merge_pr` can run as two overlapping invocations for the same task
    // (the review-agent's direct approval call in review.rs races the periodic
    // review_open_prs sweep, which is not guarded against that call site). A
    // plain "read, then write-if-absent" has a TOCTOU window where both
    // invocations observe an absent key and each writes its own "now",
    // silently discarding whichever wrote first and resetting the elapsed
    // clock. `kv_insert_if_absent` performs the check-and-set in one SQL
    // statement so only the first writer's timestamp survives.
    let stored = store.kv_insert_if_absent(&key, &now.to_rfc3339()).await?;
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&stored) {
        Ok(now.signed_duration_since(dt.with_timezone(&chrono::Utc)))
    } else {
        tracing::warn!(
            task_id,
            stored,
            "ci_pending_since value unparseable — resetting"
        );
        store.kv_set(&key, &now.to_rfc3339()).await?;
        Ok(chrono::Duration::zero())
    }
}

/// Clear the persisted pending-since timestamp once CI leaves the pending
/// state (success, failure, or after a timeout escalation fires) so the next
/// pending streak starts fresh.
///
/// `reason` identifies which branch triggered the clear (e.g. "ci_success",
/// "ci_failure", "pending_timeout_escalation") and is logged alongside the
/// value being cleared, so a future occurrence of the elapsed time silently
/// resetting (see #3609) is diagnosable from logs alone instead of requiring
/// another multi-day live reproduction.
async fn clear_ci_pending_since(
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
    reason: &str,
) -> anyhow::Result<()> {
    for (key, prior) in store
        .kv_list_prefix(&ci_pending_since_prefix(repo, task_id))
        .await?
    {
        tracing::debug!(task_id, reason, prior_since = %prior, "clearing ci_pending_since");
        store.kv_delete(&key).await?;
    }
    Ok(())
}

/// Delete the CI timers of tasks that finished, and the bare-id keys older versions wrote
pub(crate) async fn sweep_ci_timer_keys(
    store: &Arc<TaskStore>,
    repo: &str,
    finished_task_ids: &[String],
) -> anyhow::Result<()> {
    let finished: HashSet<&str> = finished_task_ids.iter().map(String::as_str).collect();
    let repo_marker = format!("{repo}#");
    for prefix in [
        "ci_check_ts:",
        "ci_pending_since:",
        "review_ci_wait_since:",
        "ci_failure_sha:",
        "ci_bootstrap_wait_since:",
        "ci_bootstrap_no_checks_comment:",
        "ci_bootstrap_never_run_comment:",
    ] {
        for (key, _) in store.kv_list_prefix(prefix).await? {
            let rest = &key[prefix.len()..];
            let stale = match rest.strip_prefix(repo_marker.as_str()) {
                // Pending keys carry a trailing `#{head_sha}`
                Some(tail) => finished.contains(tail.split('#').next().unwrap_or(tail)),
                None => !rest.contains('#'),
            };
            if stale {
                store.kv_delete(&key).await?;
            }
        }
    }
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "the arguments have no natural grouping"
)]
/// Handle a detected merge conflict by attempting a worktree rebase.
///
/// Both the `poll_mergeable_until` conflict path and the `gh.merge_pr()` failure
/// path enter here. The `merge_error` param carries the original error string for
/// PR-facing comments (optional — the poll path has no specific error).
///
/// Returns `Ok(())` in all cases; callers should NOT propagate this as an error
/// since the retry/block decision is made internally. Propagating would cause
/// `classify_review_failure` to return `ReviewOutcome::Reset`, re-spawning the
/// review agent in an infinite loop.
async fn handle_pr_rebase_recovery(
    task: &ExternalTask,
    repo: &str,
    pr_number: u64,
    gh: &GhHttp,
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
    review_agent: &str,
    review_model: &str,
    merge_error: Option<&str>,
) -> anyhow::Result<()> {
    let retries = opt_store_get_task(&Some(Arc::clone(store)), repo, &task.id.0)
        .await
        .map_or(0, |t| u64::try_from(t.merge_conflict_retries).unwrap_or(0));
    if retries >= MAX_MERGE_CONFLICT_RETRIES {
        tracing::error!(
            task_id = task.id.0,
            retries,
            "merge conflict retry limit reached"
        );
        // Persist block_reason BEFORE transitioning to Blocked to avoid
        // a race where auto_unblock sees a blocked task without a reason
        // and immediately unblocks it.
        let fields = [(
            "block_reason",
            serde_json::json!(format!(
                "merge conflict retry limit ({}) reached",
                MAX_MERGE_CONFLICT_RETRIES
            )),
        )];
        if let Err(e) = task_manager
            .update_task_status_and_result(&task.id, Status::Blocked, &fields)
            .await
        {
            tracing::error!(task_id = task.id.0, err = %e, "failed to write block_reason and set Blocked");
            return Ok(());
        }
        let comment = format!(
            "Auto-merge failed after {} rebase attempts: {}",
            retries,
            merge_error.unwrap_or("merge conflict")
        );
        let footer = engine::attribution_footer("Commented", review_agent, Some(review_model));
        if let Err(e) = gh
            .add_comment(repo, &pr_number.to_string(), &format!("{comment}{footer}"))
            .await
        {
            tracing::warn!(
                task_id = task.id.0,
                pr_number,
                err = %e,
                "handle_merge_conflict: failed to post retry limit comment on PR"
            );
        }
        return Ok(());
    }

    let worktree_path = opt_store_get_task(&Some(Arc::clone(store)), repo, &task.id.0)
        .await
        .map(|t| t.worktree)
        .filter(|wt| !wt.is_empty());
    let outcome = attempt_worktree_rebase_and_force_push(&task.id.0, worktree_path).await;
    apply_rebase_recovery_outcome(
        outcome,
        task,
        repo,
        pr_number,
        gh,
        task_manager,
        store,
        review_agent,
        review_model,
    )
    .await
}

#[expect(
    clippy::too_many_arguments,
    reason = "mirrors handle_pr_rebase_recovery so tests can inject a rebase outcome"
)]
async fn apply_rebase_recovery_outcome(
    outcome: WorktreeRebaseOutcome,
    task: &ExternalTask,
    repo: &str,
    pr_number: u64,
    gh: &GhHttp,
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
    review_agent: &str,
    review_model: &str,
) -> anyhow::Result<()> {
    match outcome {
        WorktreeRebaseOutcome::RebasedAndPushed => {
            tracing::info!(
                task_id = task.id.0,
                "rebase succeeded — task stays in InReview for CI + auto-merge"
            );
            if let Err(e) = store_increment(
                &Some(Arc::clone(store)),
                repo,
                &task.id.0,
                "merge_conflict_retries",
            )
            .await
            {
                tracing::warn!(task_id = task.id.0, err = %e, "failed to increment merge_conflict_retries — skipping dispatch to avoid bypassing retry limit");
                return Ok(());
            }
            match gh.enable_auto_merge(repo, pr_number).await {
                Ok(()) => {
                    tracing::info!(
                        task_id = task.id.0,
                        "auto-merge enabled — task stays in InReview until GitHub merges"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = task.id.0,
                        error = %e,
                        "auto-merge unavailable — task stays in InReview for sync retry"
                    );
                }
            }
            return Ok(());
        }
        WorktreeRebaseOutcome::RebaseConflict(stderr) => {
            tracing::warn!(
                task_id = task.id.0,
                stderr = %stderr,
                "rebase failed with content conflict — re-routing to agent"
            );
            // Increment counter so retry limit fires if agent cannot resolve.
            // This matches the CI failure pattern: increment before re-routing.
            if let Err(e) = store_increment(
                &Some(Arc::clone(store)),
                repo,
                &task.id.0,
                "merge_conflict_retries",
            )
            .await
            {
                tracing::warn!(task_id = task.id.0, err = %e, "failed to increment merge_conflict_retries — skipping dispatch to avoid bypassing retry limit");
                return Ok(());
            }
            if let Err(e) = store_set_result(
                &Some(Arc::clone(store)),
                repo,
                &task.id.0,
                &[
                    (
                        "last_error",
                        serde_json::json!(
                            "auto-merge rebase hit a content conflict; agent must resolve the in-progress rebase in the worktree"
                        ),
                    ),
                    (
                        "route_reason",
                        serde_json::json!("re-dispatch after auto-merge rebase conflict"),
                    ),
                    (
                        "pr_review_context",
                        serde_json::json!(
                            "Merging your PR failed: rebasing onto the default branch hit a content conflict. The worktree has the rebase in progress (see `git status`). Resolve the conflict markers keeping your changes, `git add` the files and run `git rebase --continue`. Do not redo the task from scratch."
                        ),
                    ),
                ],
            )
            .await
            {
                tracing::warn!(task_id = task.id.0, err = %e, "store write failed");
            }
            task_manager
                .update_task_status(&task.id, Status::Routed)
                .await?;
            return Ok(());
        }
        WorktreeRebaseOutcome::SshFetchFailed(stderr) => {
            let block_reason = format!(
                "auto-merge git fetch failed due to SSH agent error: {}",
                stderr.trim()
            );
            tracing::error!(
                task_id = task.id.0,
                stderr = %stderr,
                "git fetch failed before rebase due to SSH agent error — blocking for human review"
            );
            let fields = [("block_reason", serde_json::json!(block_reason.clone()))];
            if let Err(e) = task_manager
                .update_task_status_and_result(&task.id, Status::Blocked, &fields)
                .await
            {
                tracing::error!(task_id = task.id.0, err = %e, "failed to write SSH-fetch block_reason and set Blocked");
                return Ok(());
            }
            let footer = engine::attribution_footer("Commented", review_agent, Some(review_model));
            let comment = format!(
                "Auto-merge could not rebase because `git fetch origin` failed with an SSH agent error: {}",
                stderr.trim()
            );
            if let Err(e) = gh
                .add_comment(repo, &pr_number.to_string(), &format!("{comment}{footer}"))
                .await
            {
                tracing::warn!(
                    task_id = task.id.0,
                    pr_number,
                    err = %e,
                    "handle_pr_rebase_recovery: failed to post SSH-fetch failure comment on PR"
                );
            }
            return Ok(());
        }
        WorktreeRebaseOutcome::PushFailed(push_err_msg) => {
            let comment = format!(
                "Auto-merge failed (rebase succeeded but force-push failed): {push_err_msg}"
            );
            let block_reason = format!("auto-merge force-push failed after rebase: {push_err_msg}");
            let fields = [("block_reason", serde_json::json!(block_reason))];
            if let Err(e) = task_manager
                .update_task_status_and_result(&task.id, Status::Blocked, &fields)
                .await
            {
                tracing::error!(task_id = task.id.0, err = %e, "failed to write block_reason and set Blocked");
                return Ok(());
            }
            let footer = engine::attribution_footer("Commented", review_agent, Some(review_model));
            if let Err(e) = gh
                .add_comment(repo, &pr_number.to_string(), &format!("{comment}{footer}"))
                .await
            {
                tracing::warn!(
                    task_id = task.id.0,
                    pr_number,
                    err = %e,
                    "handle_pr_rebase_recovery: failed to post push failure comment on PR"
                );
            }
            return Ok(());
        }
        WorktreeRebaseOutcome::Transient(reason) => {
            tracing::warn!(
                task_id = task.id.0,
                reason = %reason,
                "merge rebase fetch failed transiently, leaving the task in place"
            );
            return Ok(());
        }
        WorktreeRebaseOutcome::Unavailable(reason) => {
            tracing::warn!(
                task_id = task.id.0,
                reason = %reason,
                "merge conflict could not be auto-rebased — re-routing to task agent"
            );
        }
    }

    // No worktree or the rebase could not start, re-route so the agent can rebase
    tracing::info!(
        task_id = task.id.0,
        pr_number,
        "merge conflict could not be auto-rebased — re-routing to task agent"
    );
    if let Err(e) = store_increment(
        &Some(Arc::clone(store)),
        repo,
        &task.id.0,
        "merge_conflict_retries",
    )
    .await
    {
        tracing::warn!(task_id = task.id.0, err = %e, "failed to increment merge_conflict_retries — skipping dispatch to avoid bypassing retry limit");
        return Ok(());
    }
    if let Err(e) = store_set_result(
        &Some(Arc::clone(store)),
        repo,
        &task.id.0,
        &[
            (
                "last_error",
                serde_json::json!(
                    "merge conflict detected but auto-rebase was not possible (no worktree or IO error); agent must rebase and resolve conflicts"
                ),
            ),
            (
                "route_reason",
                serde_json::json!("re-dispatch after merge conflict — auto-rebase unavailable"),
            ),
        ],
    )
    .await
    {
        tracing::warn!(task_id = task.id.0, err = %e, "store write failed");
    }
    task_manager
        .update_task_status(&task.id, Status::Routed)
        .await?;

    Ok(())
}

/// Poll GitHub for PR mergeability until it is computed or `max_wait` elapses.
///
/// GitHub returns `None` for `mergeable` when it hasn't computed the value yet —
/// common when the review agent approves seconds after PR creation. Polling for
/// a brief window avoids an unnecessary deferral to the next sync tick (~10-45s).
///
/// Returns `(pr, has_conflict)` where `has_conflict` is `true` when the PR has
/// merge conflicts. Callers should handle conflicts via rebase recovery
/// instead of propagating an error. This prevents the infinite needs_review loop
/// that occurred when `classify_review_failure` returned `ReviewOutcome::Reset`
/// for merge-conflict errors, re-spawning the review agent on every attempt.
async fn poll_mergeable_until(
    gh: &GhHttp,
    repo: &str,
    pr_number: u64,
    max_wait: Duration,
) -> anyhow::Result<(GitHubPullRequest, bool)> {
    let interval = Duration::from_secs(2);
    let deadline = Instant::now() + max_wait;
    loop {
        let pr = gh.get_pr(repo, pr_number).await?;
        match pr.mergeable {
            Some(false) => return Ok((pr, true)),
            Some(true) => return Ok((pr, false)),
            None => {
                if Instant::now() >= deadline {
                    anyhow::bail!("PR mergeability not yet computed — retry");
                }
                let remaining = deadline - Instant::now();
                let sleep_for = cmp::min(remaining, interval);
                time::sleep(sleep_for).await;
            }
        }
    }
}

pub(crate) fn required_checks_state(
    required_contexts: &[RequiredStatusCheck],
    check_runs: &[GitHubCheckRun],
    statuses: &[(String, String)],
) -> (String, u64, u64, u64, u64) {
    if required_contexts.is_empty() {
        return ("success".to_string(), 0, 0, 0, 0);
    }

    let mut passing = 0u64;
    let mut failing = 0u64;
    let mut pending = 0u64;

    for required in required_contexts {
        let mut matched = false;

        if let Some(run) = check_runs
            .iter()
            .find(|run| required_check_run_matches(required, run))
        {
            matched = true;
            if !run.is_finished() {
                pending += 1;
            } else if run.is_passed() {
                passing += 1;
            } else {
                failing += 1;
            }
        } else {
            for (name, state) in statuses {
                if !required_commit_status_matches(required, name) {
                    continue;
                }
                matched = true;
                match state.as_str() {
                    "success" | "neutral" | "skipped" => passing += 1,
                    "failure" | "error" => failing += 1,
                    _ => pending += 1,
                }
                break;
            }
        }

        if !matched {
            pending += 1;
        }
    }

    let total = required_contexts.len() as u64;
    let state = if failing > 0 {
        "failure".to_string()
    } else if pending > 0 {
        "pending".to_string()
    } else {
        "success".to_string()
    };

    (state, total, passing, failing, pending)
}

pub(crate) fn required_check_run_matches(
    required: &RequiredStatusCheck,
    run: &GitHubCheckRun,
) -> bool {
    if run.name != required.context {
        return false;
    }
    match required.integration_id {
        None => true,
        Some(id) => run.app.as_ref().is_some_and(|app| app.id == id),
    }
}

pub(crate) fn required_commit_status_matches(
    required: &RequiredStatusCheck,
    context: &str,
) -> bool {
    required.integration_id.is_none() && required.context == context
}

/// Deduplicate GitHub reviews by reviewer, keeping only the latest per reviewer.
///
/// GitHub returns reviews in chronological order. COMMENTED and DISMISSED reviews
/// are ignored — they carry no approval/rejection signal.
pub(crate) fn dedup_reviews<'a>(reviews: &'a [GitHubReview]) -> HashMap<String, &'a GitHubReview> {
    let mut by_reviewer: HashMap<String, &'a GitHubReview> = HashMap::new();
    for review in reviews {
        if review.state != "APPROVED" && review.state != "CHANGES_REQUESTED" {
            continue;
        }
        let is_newer_or_first = by_reviewer
            .get(&review.user.login)
            .is_none_or(|prev| review.submitted_at > prev.submitted_at);
        if is_newer_or_first {
            by_reviewer.insert(review.user.login.clone(), review);
        }
    }
    by_reviewer
}

/// Returns `true` if any reviewer's **latest** review is `CHANGES_REQUESTED`.
pub(crate) fn any_changes_requested_in_reviews(reviews: &[GitHubReview]) -> bool {
    dedup_reviews(reviews)
        .values()
        .any(|r| r.state == "CHANGES_REQUESTED")
}

const CI_LOG_TAIL_LINES: usize = 80;
const CI_CONTEXT_MAX_CHARS: usize = 8000;

/// The `pr_review_context` for a task routed back on red CI (#4121), empty `failed` means all
pub(crate) async fn red_ci_context(
    gh: &GhHttp,
    repo: &str,
    pr_number: u64,
    sha: &str,
    failed: &[String],
) -> String {
    let mut sections: Vec<(String, String)> = Vec::new();
    match gh.get_check_runs(repo, sha).await {
        Ok(runs) => {
            let failed_runs = runs.iter().filter(|r| {
                matches!(r.conclusion.as_deref(), Some("failure" | "timed_out"))
                    && (failed.is_empty() || failed.contains(&r.name))
            });
            for run in failed_runs {
                sections.push((run.name.clone(), failure_text(gh, repo, run).await));
            }
        }
        Err(e) => {
            tracing::debug!(err = %e, "failed to fetch check runs for the CI failure context");
        }
    }
    for name in failed {
        if !sections.iter().any(|(n, _)| n == name) {
            sections.push((name.clone(), String::new()));
        }
    }
    format_red_ci_context(pr_number, sha, &sections)
}

/// Annotations of a failed check run, or the tail of its job log when it has none
async fn failure_text(gh: &GhHttp, repo: &str, run: &GitHubCheckRun) -> String {
    match gh.get_check_run_annotations(repo, run.id).await {
        Ok(annotations) => {
            let lines: Vec<String> = annotations
                .iter()
                .filter(|a| a.annotation_level.as_deref() != Some("notice"))
                .map(|a| {
                    let place = match (&a.path, a.start_line) {
                        (Some(path), Some(line)) => format!("{path}:{line}: "),
                        (Some(path), None) => format!("{path}: "),
                        _ => String::new(),
                    };
                    let title = a.title.as_deref().unwrap_or("");
                    let message = a.message.as_deref().unwrap_or("");
                    format!("{place}{title} {message}").trim().to_string()
                })
                .filter(|l| !l.is_empty())
                .collect();
            if !lines.is_empty() {
                return lines.join("\n");
            }
        }
        Err(e) => {
            tracing::debug!(check_run_id = run.id, err = %e, "failed to fetch check run annotations");
        }
    }
    gh.get_job_log_tail(repo, run.id, CI_LOG_TAIL_LINES)
        .await
        .unwrap_or_default()
}

pub(crate) fn format_red_ci_context(
    pr_number: u64,
    sha: &str,
    sections: &[(String, String)],
) -> String {
    let names = sections
        .iter()
        .map(|(n, _)| n.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let mut out = format!(
        "CI failed on PR #{pr_number} at head {sha}. Failed checks: {names}.\n\
         Run each failing check locally and fix it. Do not report done until it passes.\n"
    );
    for (name, text) in sections {
        let text = text.trim();
        let text = if text.is_empty() {
            "No failure text was available. Read the log of this check on the PR."
        } else {
            text
        };
        let _ = write!(out, "\n### {name}\n```\n{text}\n```\n");
    }
    let mut out = security::redact(&out);
    if out.chars().count() > CI_CONTEXT_MAX_CHARS {
        out = out.chars().take(CI_CONTEXT_MAX_CHARS).collect();
        out.push_str("\n[truncated]");
    }
    out
}

/// Counts failed check runs for `sha` that carry a GitHub Actions billing
/// failure annotation.
///
/// When an account has unpaid invoices or has hit a spending limit, GitHub
/// refuses to start jobs and records the reason ("The job was not started
/// because recent account payments have failed …") as a check-run annotation.
/// The CI state still comes back as `"failure"`, which would normally trigger
/// the agent retry loop — but no code change can fix a billing problem.
/// This function lets the caller short-circuit that loop and block the task
/// immediately with a human-readable billing message instead.
pub(crate) async fn billing_failed_runs(
    gh: &GhHttp,
    repo: &str,
    sha: &str,
    prefetched: Option<Vec<GitHubCheckRun>>,
) -> u64 {
    let check_runs = match prefetched {
        Some(runs) => runs,
        None => match gh.get_check_runs(repo, sha).await {
            Ok(runs) => runs,
            Err(e) => {
                tracing::debug!(err = %e, "failed to fetch check runs for billing failure check");
                return 0;
            }
        },
    };

    let mut count = 0;
    for run in check_runs
        .iter()
        .filter(|r| r.conclusion.as_deref() == Some("failure"))
    {
        match gh.get_check_run_annotations(repo, run.id).await {
            Ok(annotations) => {
                for annotation in &annotations {
                    let msg = annotation.message.as_deref().unwrap_or("");
                    let title = annotation.title.as_deref().unwrap_or("");
                    if msg.contains("account payments have failed")
                        || msg.contains("spending limit")
                        || title.contains("account payments have failed")
                        || title.contains("spending limit")
                    {
                        count += 1;
                        break;
                    }
                }
            }
            Err(e) => {
                tracing::debug!(
                    check_run_id = run.id,
                    err = %e,
                    "failed to fetch check run annotations"
                );
            }
        }
    }

    count
}

/// Whether a PR head may merge, given how many commits it is behind its base.
///
/// `None` means the comparison could not be read. Only a head that contains the
/// base tip has run its checks against the code the merge would produce, so an
/// unreadable comparison holds the merge too (#3780)
fn may_merge_onto_base(behind_by: Option<u64>) -> bool {
    behind_by == Some(0)
}

/// True when any check run or commit status has not finished
fn any_check_pending(check_runs: &[GitHubCheckRun], statuses: &[(String, String)]) -> bool {
    check_runs
        .iter()
        .any(|run| !review_gate::is_own_check(&run.name) && !run.is_finished())
        || statuses.iter().any(|(name, state)| {
            !review_gate::is_own_check(name)
                && !matches!(
                    state.as_str(),
                    "success" | "failure" | "error" | "neutral" | "skipped"
                )
        })
}

/// Names of completed checks that failed and are not required
fn failing_optional_check_names(
    required: &[RequiredStatusCheck],
    check_runs: &[GitHubCheckRun],
    statuses: &[(String, String)],
) -> Vec<String> {
    let mut names = Vec::new();
    for run in check_runs {
        if required.iter().any(|r| required_check_run_matches(r, run)) {
            continue;
        }
        if run.is_finished() && !run.is_passed() {
            names.push(run.name.clone());
        }
    }
    for (name, state) in statuses {
        if !matches!(state.as_str(), "failure" | "error") {
            continue;
        }
        if required
            .iter()
            .any(|r| required_commit_status_matches(r, name))
        {
            continue;
        }
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    names
}

/// `has_hooks` is `clean` on a server with pre-receive hooks
/// `unstable` is ready when required checks passed, and waits while a repo with no required checks still has running CI
fn mergeable_state_ready(
    state: &str,
    required: &[RequiredStatusCheck],
    check_runs: &[GitHubCheckRun],
    statuses: &[(String, String)],
) -> bool {
    match state {
        "clean" | "has_hooks" => true,
        "unstable" => {
            if required.is_empty() {
                !any_check_pending(check_runs, statuses)
            } else {
                required_checks_state(required, check_runs, statuses).0 == "success"
            }
        }
        _ => false,
    }
}

/// Auto-merge a PR after review approval.
///
/// Checks that the automated review comment says "approve" and that CI checks
/// are green before merging. If CI fails, sets task back to routed so the
/// agent is re-dispatched to fix the issues.
#[expect(
    clippy::too_many_arguments,
    reason = "the arguments have no natural grouping"
)]
pub(crate) async fn auto_merge_pr(
    task: &ExternalTask,
    branch: &str,
    _backend: &Arc<dyn ExternalBackend>,
    repo: &str,
    review_agent: &str,
    review_model: &str,
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
) -> anyhow::Result<()> {
    // Merge only a stored Approved task. A leftover Approve comment on an
    // in_review head must not merge while a review is still running
    if !opt_store_get_task(&Some(Arc::clone(store)), repo, &task.id.0)
        .await
        .is_some_and(|t| t.status == TaskStatus::Approved)
    {
        tracing::debug!(
            task_id = task.id.0,
            "skipping auto-merge — stored task is not approved"
        );
        return Ok(());
    }

    // 1. Get PR number from branch
    let gh = GhHttp::new()?;
    let Some(pr_number) = gh.get_pr_number(repo, branch).await? else {
        // No open PR — check if the branch was already merged/closed.
        // Treat a closed/merged PR as idempotent success rather than a
        // retryable failure so the review-agent failure counter is not
        // incremented when the work is already done.
        return finish_already_closed_pr(&gh, task, branch, repo, task_manager, store).await;
    };

    // 2. Verify the automated review comment says "approve"
    let review_status = gh.get_automated_review_status(repo, pr_number).await?;
    match review_status.as_deref() {
        Some("approve") => {
            tracing::info!(task_id = task.id.0, pr_number, "automated review approved");
        }
        Some("changes_requested") => {
            tracing::warn!(
                task_id = task.id.0,
                pr_number,
                "automated review says changes_requested — approve comment was not posted, returning error"
            );
            anyhow::bail!(
                "approve comment was not posted (latest review is changes_requested); task should be re-queued"
            );
        }
        _ => {
            tracing::info!(
                task_id = task.id.0,
                pr_number,
                "no automated review comment found — proceeding with merge"
            );
        }
    }

    // 3. Rerun the failed push-triggered review gate check so the commit status flips green
    if let Err(e) = gh
        .rerun_failed_workflow(repo, "Orch Review Gate", branch)
        .await
    {
        tracing::debug!(
            task_id = task.id.0,
            error = %e,
            "failed to rerun orch-review workflow (may not have a failed run)"
        );
    }

    // 4. Wait for CI checks to pass (poll with exponential backoff, up to max_wait)
    // Configurable via workflow.ci_poll_max_wait_secs and workflow.ci_poll_interval_secs
    let max_wait_secs: u64 = config::get("workflow.ci_poll_max_wait_secs")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(600);
    let base_interval_secs: u64 = config::get("workflow.ci_poll_interval_secs")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);
    let max_wait = Duration::from_secs(max_wait_secs);
    let mut poll_count: u32 = 0;

    // Poll for mergeability to avoid deferring to the next sync tick (~10-45s)
    // when GitHub returns None because it hasn't computed mergeability yet.
    // This is common when the review agent approves quickly after PR creation.
    let mergeability_poll_secs: u64 = config::get("workflow.mergeability_poll_secs")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    // Poll for mergeability. When conflicts are detected, handle them via the
    // rebase path instead of propagating an error — this avoids the infinite
    // needs_review loop caused by classify_review_failure returning Reset.
    let (pr, has_conflict) = poll_mergeable_until(
        &gh,
        repo,
        pr_number,
        Duration::from_secs(mergeability_poll_secs),
    )
    .await?;
    if has_conflict {
        handle_pr_rebase_recovery(
            task,
            repo,
            pr_number,
            &gh,
            task_manager,
            store,
            review_agent,
            review_model,
            None,
        )
        .await?;
        return Ok(());
    }
    let head_sha = pr.head.sha.clone();
    let base_branch = pr.base.ref_.clone();
    review::carry_approval_across_base_update(store, &gh, repo, &task.id.0, &pr, pr_number).await;
    let required_contexts = gh
        .get_required_status_check_contexts(repo, &base_branch)
        .await?;
    // Determine whether the repository has any GitHub Actions workflows so that
    // `get_combined_status` can distinguish "no CI configured" (legitimately
    // empty check-run list → success) from "CI not started yet" (empty list
    // because workflows exist but haven't queued yet → pending).
    //
    // IMPORTANT: transient lookup failures (rate-limit, 5xx, token scope) must
    // NOT silently collapse to `false`.  If we assumed `false` on an error,
    // `combined_status_state` would see `total == 0 && has_workflows == false`
    // and return "success", letting the PR merge without any CI verification.
    // Instead we propagate the error so the caller retries on the next sync tick.
    let repo_has_workflows = if required_contexts.is_empty() {
        gh.has_workflows(repo).await?
    } else {
        true
    };

    loop {
        // Check per-task CI check cooldown before making GitHub API calls.
        // This prevents rate limit exhaustion when many tasks are in CI loops.
        if is_ci_check_in_cooldown(store, repo, &task.id.0).await? {
            tracing::debug!(
                task_id = task.id.0,
                pr_number,
                "CI check skipped due to per-task cooldown"
            );
            return Ok(());
        }

        // Acquire a global permit only for the HTTP polling batch.
        // The inner block ensures the permit drops before the match/sleep.
        // When required_contexts is non-empty we also cache the raw check runs so
        // that is_billing_failure can reuse them without a redundant API call.
        let mut prefetched_check_runs: Option<Vec<GitHubCheckRun>> = None;
        let (state, total, passing, failing, pending) = {
            let _permit = Arc::clone(ci_poll_semaphore()).acquire_owned().await;
            if required_contexts.is_empty() {
                gh.get_combined_status(
                    repo,
                    &head_sha,
                    repo_has_workflows,
                    pr.mergeable_state.as_deref(),
                )
                .await?
            } else {
                let check_runs = gh.get_check_runs(repo, &head_sha).await?;
                let statuses = gh.get_commit_status_contexts(repo, &head_sha).await?;
                let result = required_checks_state(&required_contexts, &check_runs, &statuses);
                prefetched_check_runs = Some(check_runs);
                result
            }
        }; // _permit dropped here — released before logging, match, and sleep.

        tracing::info!(
            task_id = task.id.0,
            pr_number,
            state = %state,
            total,
            passing,
            failing,
            pending,
            "CI status check"
        );

        // Record that we checked CI for this task (before breaking on success or
        // returning on failure/pending timeout). This enables per-task cooldown.
        if let Err(e) = record_ci_check(store, repo, &task.id.0).await {
            tracing::warn!(task_id = task.id.0, err = %e, "failed to record CI check timestamp");
        }

        // A check that never started because of an Actions billing or spending limit
        // is "not run", not a failure. If every failed check is of that kind and the
        // other checks passed, the PR is green. Only without required contexts, where
        // `failing` counts exactly the failed check runs.
        let mut state = state;
        if state == "failure" && passing > 0 && required_contexts.is_empty() {
            let unstarted =
                billing_failed_runs(&gh, repo, &head_sha, prefetched_check_runs.clone()).await;
            if unstarted > 0 && unstarted == failing {
                tracing::warn!(
                    task_id = task.id.0,
                    pr_number,
                    unstarted,
                    passing,
                    "checks did not start because of a GitHub Actions billing limit — treating as not run"
                );
                state = "success".to_string();
            }
        }

        match state.as_str() {
            "success" => {
                if let Err(e) = clear_ci_pending_since(store, repo, &task.id.0, "ci_success").await
                {
                    tracing::warn!(task_id = task.id.0, err = %e, "failed to clear ci_pending_since after CI success");
                }
                break;
            }
            "failure" => {
                if let Err(e) = clear_ci_pending_since(store, repo, &task.id.0, "ci_failure").await
                {
                    tracing::warn!(task_id = task.id.0, err = %e, "failed to clear ci_pending_since after CI failure");
                }
                match gh.get_pr(repo, pr_number).await {
                    Ok(current_pr) if is_pr_behind(&current_pr) => {
                        tracing::info!(
                            task_id = task.id.0,
                            pr_number,
                            mergeable_state = current_pr.mergeable_state.as_deref().unwrap_or(""),
                            "CI failed while PR is behind base branch — attempting automatic rebase recovery"
                        );

                        let worktree_path =
                            opt_store_get_task(&Some(Arc::clone(store)), repo, &task.id.0)
                                .await
                                .map(|t| t.worktree)
                                .filter(|wt| !wt.is_empty());

                        match attempt_worktree_rebase_and_force_push(&task.id.0, worktree_path)
                            .await
                        {
                            WorktreeRebaseOutcome::RebasedAndPushed => {
                                if let Err(e) = store_set_result(
                                    &Some(Arc::clone(store)),
                                    repo,
                                    &task.id.0,
                                    &[("ci_merge_failures", serde_json::json!(0))],
                                )
                                .await
                                {
                                    tracing::warn!(task_id = task.id.0, err = %e, "failed to reset ci_merge_failures after behind-branch rebase");
                                }

                                match gh.enable_auto_merge(repo, pr_number).await {
                                    Ok(()) => {
                                        tracing::info!(
                                            task_id = task.id.0,
                                            "behind-branch rebase succeeded — auto-merge re-enabled"
                                        );
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            task_id = task.id.0,
                                            error = %e,
                                            "behind-branch rebase succeeded but auto-merge could not be re-enabled"
                                        );
                                    }
                                }

                                return Ok(());
                            }
                            WorktreeRebaseOutcome::RebaseConflict(stderr) => {
                                tracing::warn!(
                                    task_id = task.id.0,
                                    stderr = %stderr,
                                    "behind-branch rebase hit a content conflict — re-routing to agent"
                                );
                                if let Err(e) = store_increment(
                                    &Some(Arc::clone(store)),
                                    repo,
                                    &task.id.0,
                                    "merge_conflict_retries",
                                )
                                .await
                                {
                                    tracing::warn!(task_id = task.id.0, err = %e, "failed to increment merge_conflict_retries after behind-branch rebase conflict");
                                    return Ok(());
                                }
                                if let Err(e) = store_set_result(
                                    &Some(Arc::clone(store)),
                                    repo,
                                    &task.id.0,
                                    &[
                                        (
                                            "ci_merge_failures",
                                            serde_json::json!(0),
                                        ),
                                        (
                                            "last_error",
                                            serde_json::json!(
                                                "PR fell behind the base branch and auto-rebase hit a content conflict; agent must resolve the in-progress rebase in the worktree"
                                            ),
                                        ),
                                        (
                                            "route_reason",
                                            serde_json::json!("re-dispatch after behind-branch auto-rebase conflict"),
                                        ),
                                    ],
                                )
                                .await
                                {
                                    tracing::warn!(task_id = task.id.0, err = %e, "store write failed after behind-branch rebase conflict");
                                }
                                task_manager
                                    .update_task_status(&task.id, Status::Routed)
                                    .await?;
                                return Ok(());
                            }
                            WorktreeRebaseOutcome::SshFetchFailed(stderr) => {
                                let block_reason = format!(
                                    "auto-merge git fetch failed due to SSH agent error: {}",
                                    stderr.trim()
                                );
                                let fields =
                                    [("block_reason", serde_json::json!(block_reason.clone()))];
                                if let Err(e) = task_manager
                                    .update_task_status_and_result(
                                        &task.id,
                                        Status::Blocked,
                                        &fields,
                                    )
                                    .await
                                {
                                    tracing::error!(task_id = task.id.0, err = %e, "failed to write SSH-fetch block_reason and set Blocked after behind-branch CI failure");
                                    return Ok(());
                                }
                                let footer = engine::attribution_footer(
                                    "Commented",
                                    review_agent,
                                    Some(review_model),
                                );
                                let comment = format!(
                                    "Auto-merge could not rebase the behind branch because `git fetch origin` failed with an SSH agent error: {}",
                                    stderr.trim()
                                );
                                if let Err(e) = gh
                                    .add_comment(
                                        repo,
                                        &pr_number.to_string(),
                                        &format!("{comment}{footer}"),
                                    )
                                    .await
                                {
                                    tracing::warn!(
                                        task_id = task.id.0,
                                        pr_number,
                                        err = %e,
                                        "auto_merge: failed to post behind-branch SSH-fetch failure comment on PR"
                                    );
                                }
                                return Ok(());
                            }
                            WorktreeRebaseOutcome::PushFailed(push_err_msg) => {
                                let comment = format!(
                                    "Auto-merge failed (behind-branch rebase succeeded but force-push failed): {push_err_msg}"
                                );
                                let block_reason = format!(
                                    "auto-merge force-push failed after behind-branch rebase: {push_err_msg}"
                                );
                                let fields = [("block_reason", serde_json::json!(block_reason))];
                                if let Err(e) = task_manager
                                    .update_task_status_and_result(
                                        &task.id,
                                        Status::Blocked,
                                        &fields,
                                    )
                                    .await
                                {
                                    tracing::error!(task_id = task.id.0, err = %e, "failed to write block_reason and set Blocked after behind-branch rebase push failure");
                                    return Ok(());
                                }
                                let footer = engine::attribution_footer(
                                    "Commented",
                                    review_agent,
                                    Some(review_model),
                                );
                                if let Err(e) = gh
                                    .add_comment(
                                        repo,
                                        &pr_number.to_string(),
                                        &format!("{comment}{footer}"),
                                    )
                                    .await
                                {
                                    tracing::warn!(
                                        task_id = task.id.0,
                                        pr_number,
                                        err = %e,
                                        "auto_merge: failed to post behind-branch push failure comment on PR"
                                    );
                                }
                                return Ok(());
                            }
                            WorktreeRebaseOutcome::Transient(reason) => {
                                tracing::warn!(
                                    task_id = task.id.0,
                                    reason = %reason,
                                    "behind-branch fetch failed transiently, leaving the task in place"
                                );
                                return Ok(());
                            }
                            WorktreeRebaseOutcome::Unavailable(reason) => {
                                tracing::warn!(
                                    task_id = task.id.0,
                                    reason = %reason,
                                    "behind-branch auto-rebase unavailable — re-routing to agent instead of counting a CI failure"
                                );
                                if let Err(e) = store_set_result(
                                    &Some(Arc::clone(store)),
                                    repo,
                                    &task.id.0,
                                    &[
                                        (
                                            "ci_merge_failures",
                                            serde_json::json!(0),
                                        ),
                                        (
                                            "last_error",
                                            serde_json::json!(format!(
                                                "PR fell behind the base branch and auto-rebase was unavailable: {reason}"
                                            )),
                                        ),
                                        (
                                            "route_reason",
                                            serde_json::json!("re-dispatch after behind-branch CI failure — auto-rebase unavailable"),
                                        ),
                                    ],
                                )
                                .await
                                {
                                    tracing::warn!(task_id = task.id.0, err = %e, "store write failed after unavailable behind-branch recovery");
                                }
                                task_manager
                                    .update_task_status(&task.id, Status::Routed)
                                    .await?;
                                return Ok(());
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(
                            task_id = task.id.0,
                            pr_number,
                            err = %e,
                            "failed to refresh PR state before CI-failure handling"
                        );
                    }
                }

                // Check for billing failures before incrementing the code-quality
                // failure counter.  Billing failures are infrastructure problems —
                // no agent can fix them — so we block the task immediately and skip
                // the re-route loop entirely.
                if billing_failed_runs(&gh, repo, &head_sha, prefetched_check_runs.take()).await > 0
                {
                    tracing::error!(
                        task_id = task.id.0,
                        pr_number,
                        "GitHub Actions billing failure detected — blocking for human intervention"
                    );
                    let fields = [(
                        "block_reason",
                        serde_json::json!(
                            "GitHub Actions billing failure — check Billing & plans settings \
                             (jobs are not starting due to payment failure or spending limit)"
                        ),
                    )];
                    if let Err(e) = task_manager
                        .update_task_status_and_result(&task.id, Status::Blocked, &fields)
                        .await
                    {
                        tracing::error!(task_id = task.id.0, err = %e, "failed to write billing block_reason and set Blocked");
                    }
                    return Ok(());
                }

                let ci_failures = match count_ci_failure_for_head(
                    store, repo, &task.id.0, &head_sha,
                )
                .await
                {
                    Ok(Some(v)) => v,
                    Ok(None) => {
                        tracing::info!(
                            task_id = task.id.0,
                            pr_number,
                            sha = %head_sha,
                            "CI still failed on the same head, waiting"
                        );
                        return Ok(());
                    }
                    Err(e) => {
                        tracing::warn!(task_id = task.id.0, err = %e, "failed to increment ci_merge_failures — skipping CI-failure based block this tick");
                        // Skip escalation this tick and let the sync retry later
                        return Ok(());
                    }
                };
                if ci_failures >= MAX_CI_MERGE_FAILURES {
                    tracing::error!(
                        task_id = task.id.0,
                        pr_number,
                        ci_failures,
                        "CI failure limit reached — blocking for human intervention"
                    );
                    // Persist block_reason BEFORE transitioning to Blocked to avoid
                    // a race where auto_unblock sees a blocked task without a reason
                    // and immediately unblocks it.
                    let fields = [(
                        "block_reason",
                        serde_json::json!(ci_failure_limit_reason(CI_FAILURE_AUTO_MERGE_STAGE)),
                    )];
                    if let Err(e) = task_manager
                        .update_task_status_and_result(&task.id, Status::Blocked, &fields)
                        .await
                    {
                        tracing::error!(task_id = task.id.0, err = %e, "failed to write block_reason and set Blocked");
                        return Ok(());
                    }
                    // Immediately attempt to clean up the task's worktree when blocking
                    // due to CI failures to avoid orphaned worktrees that will never be
                    // re-used until human intervention.
                    if let Err(e) = cleanup_task_worktree(&task.id.0, repo, store).await {
                        tracing::warn!(task_id = task.id.0, err = %e, "cleanup after CI-failure block failed");
                    }
                } else {
                    tracing::warn!(
                        task_id = task.id.0,
                        pr_number,
                        failing,
                        ci_failures,
                        "CI failed — re-routing to agent to fix"
                    );
                    let context = red_ci_context(&gh, repo, pr_number, &head_sha, &[]).await;
                    let fields = [
                        (
                            "last_error",
                            serde_json::json!(format!("CI failed on PR #{pr_number}")),
                        ),
                        ("pr_review_context", serde_json::json!(context)),
                    ];
                    task_manager
                        .update_task_status_and_result(&task.id, Status::Routed, &fields)
                        .await?;
                    store_reset_failure_counters(&Some(Arc::clone(store)), repo, &task.id.0).await;
                }
                return Ok(());
            }
            _ => {
                // pending — wait up to max_wait, tracked via a persisted
                // "first seen pending" timestamp rather than the local
                // `start` Instant (see `ci_pending_elapsed` doc comment).
                let pending_elapsed = match ci_pending_elapsed(store, repo, &task.id.0, &head_sha)
                    .await
                {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!(task_id = task.id.0, err = %e, "failed to read/persist ci_pending_since — skipping timeout check this tick");
                        chrono::Duration::zero()
                    }
                };
                if pending_elapsed
                    >= chrono::Duration::from_std(max_wait).unwrap_or(chrono::Duration::MAX)
                {
                    if let Err(e) = clear_ci_pending_since(
                        store,
                        repo,
                        &task.id.0,
                        "pending_timeout_escalation",
                    )
                    .await
                    {
                        tracing::warn!(task_id = task.id.0, err = %e, "failed to clear ci_pending_since before timeout escalation");
                    }
                    let ci_failures = match count_ci_failure_for_head(
                        store, repo, &task.id.0, &head_sha,
                    )
                    .await
                    {
                        Ok(Some(v)) => v,
                        Ok(None) => {
                            tracing::info!(
                                task_id = task.id.0,
                                pr_number,
                                sha = %head_sha,
                                "CI still pending on the same head after timeout, waiting"
                            );
                            return Ok(());
                        }
                        Err(e) => {
                            tracing::warn!(task_id = task.id.0, err = %e, "failed to increment ci_merge_failures — skipping CI-timeout based block this tick");
                            return Ok(());
                        }
                    };
                    if ci_failures >= MAX_CI_MERGE_FAILURES {
                        tracing::error!(
                            task_id = task.id.0,
                            ci_failures,
                            "CI timeout limit reached — blocking for human intervention"
                        );
                        // Persist block_reason BEFORE transitioning to Blocked to avoid
                        // a race where auto_unblock sees a blocked task without a reason
                        // and immediately unblocks it.
                        let fields = [(
                            "block_reason",
                            serde_json::json!(format!(
                                "CI checks timed out after {} auto-merge failures",
                                MAX_CI_MERGE_FAILURES
                            )),
                        )];
                        if let Err(e) = task_manager
                            .update_task_status_and_result(&task.id, Status::Blocked, &fields)
                            .await
                        {
                            tracing::error!(task_id = task.id.0, err = %e, "failed to write block_reason and set Blocked");
                            return Ok(());
                        }
                        // Attempt cleanup for CI-timeout based blocks as well.
                        if let Err(e) = cleanup_task_worktree(&task.id.0, repo, store).await {
                            tracing::warn!(task_id = task.id.0, err = %e, "cleanup after CI-timeout block failed");
                        }
                    } else {
                        tracing::warn!(
                            task_id = task.id.0,
                            ci_failures,
                            "CI checks still pending after timeout — re-routing to agent"
                        );
                        task_manager
                            .update_task_status(&task.id, Status::Routed)
                            .await?;
                        store_reset_failure_counters(&Some(Arc::clone(store)), repo, &task.id.0)
                            .await;
                    }
                    return Ok(());
                }
            }
        }

        // Exponential backoff: double the interval each iteration, capped at 4x base
        poll_count += 1;
        let multiplier = (2_u64).pow(poll_count.saturating_sub(1)).min(4);
        let sleep_secs = base_interval_secs.saturating_mul(multiplier);
        time::sleep(Duration::from_secs(sleep_secs)).await;
    }

    // 5. Re-verify no reviewer has requested changes since the CI wait started.
    let post_ci_reviews = app::reviews_for_gate(
        &gh.get_pr_reviews(repo, pr_number).await?,
        &head_sha,
        app::bot_login().await.as_deref(),
    );
    if any_changes_requested_in_reviews(&post_ci_reviews) {
        tracing::warn!(
            task_id = task.id.0,
            pr_number,
            "a reviewer requested changes during CI wait — aborting merge"
        );
        anyhow::bail!("a reviewer requested changes during CI wait — aborting merge");
    }

    // 5b. Refuse to merge a head that does not contain the current base tip: its
    // checks ran against older code, so two PRs that each pass can still break
    // the base together (#3780). Merge the base in and re-check next tick.
    let behind_by = gh
        .compare_behind_by(repo, &base_branch, &head_sha)
        .await
        .ok();
    if !may_merge_onto_base(behind_by) {
        tracing::warn!(
            task_id = task.id.0,
            pr_number,
            ?behind_by,
            base_branch = %base_branch,
            "PR head does not contain the base tip — updating branch and re-running checks before merge"
        );
        if behind_by.is_some() {
            bring_branch_up_to_date(task, repo, pr_number, &pr, &gh, store).await;
        }
        return Ok(());
    }

    // Confirm every required check passed on this head SHA so an admin token
    // cannot merge through a ruleset bypass while a required check is missing
    let mut check_runs = Vec::new();
    let mut statuses = Vec::new();
    if !required_contexts.is_empty() {
        check_runs = gh.get_check_runs(repo, &head_sha).await?;
        statuses = gh.get_commit_status_contexts(repo, &head_sha).await?;
        let (state, ..) = required_checks_state(&required_contexts, &check_runs, &statuses);
        if state != "success" {
            tracing::info!(
                task_id = task.id.0,
                pr_number,
                state = %state,
                "required checks have not all passed on the head SHA, holding merge"
            );
            return Ok(());
        }
    }

    // GitHub's own verdict must read ready too, so an admin bypass cannot merge
    // a PR that is behind, blocked, or uncomputed, `unstable` merges when required checks passed (#3839, #3847)
    let fresh_pr = gh.get_pr(repo, pr_number).await?;
    let mergeable_state = fresh_pr.mergeable_state.as_deref().unwrap_or("unknown");
    if mergeable_state == "unstable" && required_contexts.is_empty() {
        check_runs = gh.get_check_runs(repo, &head_sha).await?;
        statuses = gh.get_commit_status_contexts(repo, &head_sha).await?;
    }
    if !mergeable_state_ready(mergeable_state, &required_contexts, &check_runs, &statuses) {
        tracing::info!(
            task_id = task.id.0,
            pr_number,
            mergeable_state,
            "PR is not ready to merge on GitHub, holding merge"
        );
        if mergeable_state == "behind" {
            bring_branch_up_to_date(task, repo, pr_number, &pr, &gh, store).await;
        }

        return Ok(());
    }

    if mergeable_state == "unstable" {
        let failing_optional =
            failing_optional_check_names(&required_contexts, &check_runs, &statuses);
        if !failing_optional.is_empty() {
            tracing::info!(
                task_id = task.id.0,
                pr_number,
                failing_optional = %failing_optional.join(", "),
                "merging with failing non-required checks"
            );
        }
    }

    tracing::info!(
        task_id = task.id.0,
        pr_number,
        branch = %branch,
        "merging PR"
    );

    // 6. Merge via gh CLI
    if let Err(e) = gh.merge_pr(repo, pr_number, true, &head_sha).await {
        let merge_error = e.to_string();
        match classify_merge_failure(&merge_error) {
            MergeFailureClass::Transient => {
                tracing::warn!(
                    task_id = task.id.0,
                    pr_number,
                    error = %e,
                    "transient GitHub error on merge — will retry next sync"
                );
                // Don't change status — task stays in InReview, sync tick retries
                return Ok(());
            }
            MergeFailureClass::RecoverableConflict => {
                // Delegate to the shared helper — same logic as the poll_mergeable_until path.
                handle_pr_rebase_recovery(
                    task,
                    repo,
                    pr_number,
                    &gh,
                    task_manager,
                    store,
                    review_agent,
                    review_model,
                    Some(&merge_error),
                )
                .await?;
                return Ok(());
            }
            MergeFailureClass::Blocking => {}
        }

        // Non-conflict merge failure (permissions, branch protection, etc.)
        tracing::error!(task_id = task.id.0, error = %e, "merge failed — blocking for human review");
        // Persist block_reason BEFORE transitioning to Blocked to avoid
        // a race where auto_unblock sees a blocked task without a reason
        // and immediately unblocks it.
        let fields = [(
            "block_reason",
            serde_json::json!(format!("auto-merge failed: {}", e)),
        )];
        if let Err(e) = task_manager
            .update_task_status_and_result(&task.id, Status::Blocked, &fields)
            .await
        {
            tracing::error!(task_id = task.id.0, err = %e, "failed to write block_reason and set Blocked");
            return Ok(());
        }
        let comment = format!("Auto-merge failed: {e}");
        let footer = engine::attribution_footer("Commented", review_agent, Some(review_model));
        if let Err(e) = gh
            .add_comment(repo, &pr_number.to_string(), &format!("{comment}{footer}"))
            .await
        {
            tracing::warn!(
                task_id = task.id.0,
                pr_number,
                err = %e,
                "auto-merge: failed to post merge failure comment on PR"
            );
        }
        return Ok(());
    }

    // 6. Mark done, or hold a CI bootstrap task until the ruleset exists
    if let Err(e) = store_set_result(
        &Some(Arc::clone(store)),
        repo,
        &task.id.0,
        &[("ci_merge_failures", serde_json::json!(0))],
    )
    .await
    {
        tracing::warn!(task_id = task.id.0, err = %e, "store write failed");
    }
    finish_merged_pr(task, task_manager, store, repo).await?;

    // Kill any orphaned review session that might be racing us
    let tmux = TmuxManager::new();
    let stale_session = tmux.session_name(repo, &format!("{}-review", task.id.0));
    if let Err(e) = tmux.kill_session(&stale_session).await {
        tracing::debug!(task_id = task.id.0, error = %e, "failed to kill stale review session after auto-merge");
    }

    // 8. Post final comment on the PR
    let comment = "✅ PR reviewed, approved, and merged.";
    let footer = engine::attribution_footer("Reviewed", review_agent, Some(review_model));
    if let Err(e) = gh
        .add_comment(repo, &pr_number.to_string(), &format!("{comment}{footer}"))
        .await
    {
        tracing::warn!(
            task_id = task.id.0,
            pr_number,
            err = %e,
            "auto-merge: failed to post success comment on PR"
        );
    }

    tracing::info!(task_id = task.id.0, "auto-merge completed");

    Ok(())
}

/// Returns `true` when the branch's latest commit is newer than the most
/// recent automated review comment on the PR.
///
/// This is used to detect reviews that ran against stale code: if the agent
/// pushed a fix *after* the review comment was posted, the review's feedback
/// is no longer valid and we should not escalate based on it.
///
/// Returns `false` on any API error so that we always fall back to normal
/// escalation behaviour rather than getting stuck in a retry loop.
async fn branch_newer_than_last_review(
    gh: &GhHttp,
    repo: &str,
    pr_number: u64,
    pr_num_str: &str,
) -> bool {
    // 1. Get the current branch HEAD SHA.
    let head_sha = match gh.get_pr(repo, pr_number).await {
        Ok(pr) => pr.head.sha,
        Err(e) => {
            tracing::debug!(pr_number, err = %e, "stale-review check: failed to get PR");
            return false;
        }
    };

    // 2. Get the committer date of the HEAD commit.
    let commit_date = match gh.get_commit_timestamp(repo, &head_sha).await {
        Ok(d) => d,
        Err(e) => {
            tracing::debug!(pr_number, sha = %head_sha, err = %e, "stale-review check: failed to get commit date");
            return false;
        }
    };

    // 3. Find the latest automated review comment on the PR.
    let comments = match gh.list_comments(repo, pr_num_str).await {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(pr_number, err = %e, "stale-review check: failed to list comments");
            return false;
        }
    };

    // Defensive: find the latest automated review comment by created_at rather
    // than relying on the iteration order returned by the API.
    let mut latest: Option<&GitHubComment> = None;
    for c in &comments {
        if !c.body.starts_with("## Automated Review") {
            continue;
        }
        match latest {
            None => latest = Some(c),
            Some(prev) => {
                if c.created_at > prev.created_at {
                    latest = Some(c);
                }
            }
        }
    }
    let last_review_date = latest.map(|c| c.created_at.as_str());

    match last_review_date {
        Some(review_date) => {
            let newer = commit_date.as_str() > review_date;
            if newer {
                tracing::info!(
                    pr_number,
                    commit_date = %commit_date,
                    review_date = %review_date,
                    "stale-review check: branch has commits newer than last review"
                );
            }
            newer
        }
        // No automated review comment found — cannot determine staleness, don't skip escalation.
        None => false,
    }
}

/// Check whether all *required* CI checks pass for the given PR.
///
/// Returns `Some(true)` when all required contexts report success, `Some(false)` when
/// at least one required check is failing or pending, and `None` on any API error (so
/// the caller can fall through to the default escalation path).
async fn required_ci_checks_pass(gh: &GhHttp, repo: &str, pr_number: u64) -> Option<bool> {
    let pr = match gh.get_pr(repo, pr_number).await {
        Ok(pr) => pr,
        Err(e) => {
            tracing::warn!(pr_number, err = %e, "required_ci_checks_pass: failed to fetch PR");
            return None;
        }
    };

    let head_sha = pr.head.sha.clone();
    let base_branch = pr.base.ref_.clone();

    let required_contexts = match gh
        .get_required_status_check_contexts(repo, &base_branch)
        .await
    {
        Ok(ctx) => ctx,
        Err(e) => {
            tracing::warn!(
                pr_number,
                err = %e,
                "required_ci_checks_pass: failed to fetch required contexts"
            );
            return None;
        }
    };

    // No required contexts configured — treat as passing.
    if required_contexts.is_empty() {
        return Some(true);
    }

    let check_runs = match gh.get_check_runs(repo, &head_sha).await {
        Ok(runs) => runs,
        Err(e) => {
            tracing::warn!(
                pr_number,
                err = %e,
                "required_ci_checks_pass: failed to fetch check runs"
            );
            return None;
        }
    };

    let statuses = match gh.get_commit_status_contexts(repo, &head_sha).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                pr_number,
                err = %e,
                "required_ci_checks_pass: failed to fetch commit statuses"
            );
            return None;
        }
    };

    let (state, ..) = required_checks_state(&required_contexts, &check_runs, &statuses);
    Some(state == "success")
}

/// Handle review changes request — re-dispatch the original agent.
#[expect(
    clippy::too_many_arguments,
    reason = "the arguments have no natural grouping"
)]
pub(crate) async fn handle_review_changes(
    task: &ExternalTask,
    notes: &str,
    issues: &[ReviewIssue],
    _backend: &Arc<dyn ExternalBackend>,
    repo: &str,
    pr_number: u64,
    review_agent: &str,
    review_model: &str,
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
    router_config: &RouterConfig,
) -> anyhow::Result<()> {
    // 1. Check review cycle count (max 2 review rounds)
    let task_store_record = opt_store_get_task(&Some(Arc::clone(store)), repo, &task.id.0).await;
    let review_cycles: u32 = task_store_record
        .as_ref()
        .map_or(0, |t| u32::try_from(t.review_cycles).unwrap_or(0));

    let max_cycles: u32 = config::get("workflow.max_review_cycles")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);

    let gh = GhHttp::new()?;
    let pr_num_str = pr_number.to_string();

    if review_cycles >= max_cycles {
        // Before escalating, check whether the branch has been updated after the
        // last automated review comment was posted.  If it has, the review that
        // triggered this escalation ran against stale code — the agent already
        // pushed a fix.  In that case, trigger one fresh review instead of
        // blocking the PR with outdated feedback.
        if branch_newer_than_last_review(&gh, repo, pr_number, &pr_num_str).await {
            tracing::info!(
                task_id = task.id.0,
                review_cycles,
                "branch updated since last review — triggering fresh review instead of escalating"
            );
            if let Err(e) = task_manager
                .update_task_status(&task.id, Status::NeedsReview)
                .await
            {
                tracing::warn!(
                    task_id = task.id.0,
                    err = %e,
                    "failed to set NeedsReview for fresh review — will escalate instead"
                );
                // Fall through to escalation below.
            } else {
                // Reset review_cycles so the fresh review against new code starts
                // from a clean count. Without this reset, the next review would
                // immediately see review_cycles >= max_cycles and escalate again.
                if let Err(e) = store_set_result(
                    &Some(Arc::clone(store)),
                    repo,
                    &task.id.0,
                    &[("review_cycles", serde_json::json!(0))],
                )
                .await
                {
                    tracing::warn!(task_id = task.id.0, err = %e, "store write failed");
                }
                return Ok(());
            }
        }

        // Before blocking, check whether all *required* CI checks pass.
        // If they do, the review agent may have been triggered by a non-required
        // check failure (e.g. `review-gate`).  Allow one auto-recovery to reset
        // the review cycle counter and re-trigger the review agent.
        let ci_recovery_count: i32 = task_store_record
            .as_ref()
            .map_or(0, |t| t.ci_recovery_count);

        if ci_recovery_count < 1 {
            match required_ci_checks_pass(&gh, repo, pr_number).await {
                Some(true) => {
                    tracing::info!(
                        task_id = task.id.0,
                        review_cycles,
                        "required CI checks pass — auto-recovering from non-required check failure"
                    );
                    if let Err(e) = store_set_result(
                        &Some(Arc::clone(store)),
                        repo,
                        &task.id.0,
                        &[("review_cycles", serde_json::json!(0))],
                    )
                    .await
                    {
                        tracing::warn!(task_id = task.id.0, err = %e, "store write failed");
                    }
                    if let Err(e) = store_increment(
                        &Some(Arc::clone(store)),
                        repo,
                        &task.id.0,
                        "ci_recovery_count",
                    )
                    .await
                    {
                        tracing::warn!(task_id = task.id.0, err = %e, "failed to increment ci_recovery_count — recovery count may be inaccurate");
                    }
                    if let Err(e) = task_manager
                        .update_task_status(&task.id, Status::NeedsReview)
                        .await
                    {
                        tracing::warn!(
                            task_id = task.id.0,
                            err = %e,
                            "auto-recovery: failed to set NeedsReview — will escalate instead"
                        );
                        // Fall through to escalation below.
                    } else {
                        let recovery_comment = format!(
                            "🔄 Auto-recovery: required CI checks pass. \
                        The review agent may have been blocked by a non-required check failure. \
                        Resetting review cycles and re-triggering review.{}",
                            engine::attribution_footer(
                                "Commented",
                                review_agent,
                                Some(review_model)
                            )
                        );
                        if let Err(e) = gh.add_comment(repo, &pr_num_str, &recovery_comment).await {
                            tracing::warn!(
                                task_id = task.id.0,
                                pr_number,
                                err = %e,
                                "auto-recovery: failed to post recovery comment on PR"
                            );
                        }
                        return Ok(());
                    }
                }
                Some(false) => {
                    tracing::info!(
                        task_id = task.id.0,
                        review_cycles,
                        "required CI checks failing — escalating to Blocked"
                    );
                }
                None => {
                    tracing::warn!(
                        task_id = task.id.0,
                        review_cycles,
                        "could not determine required CI check state — escalating to Blocked"
                    );
                }
            }
        }

        tracing::warn!(
            task_id = task.id.0,
            review_cycles,
            max_cycles,
            "max review cycles exceeded, blocking for human review"
        );
        // Persist block_reason BEFORE transitioning to Blocked to avoid
        // a race where auto_unblock sees a blocked task without a reason
        // and immediately unblocks it.
        let fields = [
            (
                "block_reason",
                serde_json::json!(format!("max review cycles ({}) exceeded", max_cycles)),
            ),
            (
                "last_error",
                serde_json::json!(format!(
                    "review agent requested changes {} times — escalated to human review",
                    review_cycles
                )),
            ),
        ];
        // A transient store failure here must not be counted as a review-agent
        // crash. Log and return Ok — the next tick will re-check the task and
        // can retry the status transition.
        if let Err(e) = task_manager
            .update_task_status_and_result(&task.id, Status::Blocked, &fields)
            .await
        {
            tracing::warn!(
                task_id = task.id.0,
                err = %e,
                "failed to set Blocked after max review cycles — will retry on next tick"
            );
            return Ok(());
        }
        let escalation = format!(
            "🔍 Review agent requested changes after {review_cycles} cycles. Escalating to human.\n\n**Review Notes:**\n{notes}"
        );
        let footer = engine::attribution_footer("Commented", review_agent, Some(review_model));
        if let Err(e) = gh
            .add_comment(repo, &pr_num_str, &format!("{escalation}{footer}"))
            .await
        {
            tracing::warn!(task_id = task.id.0, pr_number, err = %e, "failed to post escalation comment on PR");
        }
        return Ok(());
    }

    // 2. Build review context for the re-dispatched agent's prompt.
    let mut comment = format!(
        "🔍 Review agent requested changes (cycle {} of {}):\n\n{}",
        review_cycles + 1,
        max_cycles,
        notes
    );

    if !issues.is_empty() {
        comment.push_str("\n\n**Issues Found:**\n");
        for issue in issues {
            let _ = writeln!(
                comment,
                "- `{}` line {}: {} [{}]",
                issue.file,
                issue
                    .line
                    .map_or_else(|| "?".to_string(), |l| l.to_string()),
                issue.description,
                issue.severity
            );
        }
    }

    // 3. Store review context (but NOT review_cycles — increment only after
    // successful status transition to avoid premature escalation on failure).
    if let Err(e) = store_set_result(
        &Some(Arc::clone(store)),
        repo,
        &task.id.0,
        &[("pr_review_context", serde_json::json!(comment.clone()))],
    )
    .await
    {
        tracing::warn!(task_id = task.id.0, err = %e, "store write failed");
    }

    // 3b. Re-assign a valid model for the task's agent using
    // model_for_complexity().  The previous approach of reusing the stored
    // model directly produced invalid combos after failover (e.g. opencode:opus
    // when "opus" was the review agent's model, not a valid opencode model).
    //
    // We read the task's existing agent and complexity from the store record
    // fetched at line 1087, then resolve a fresh model from the agent's
    // configured pool.  If all models for that agent are cooled, we fall back
    // to Status::New for a full re-route.
    let previous_agent = task_store_record
        .as_ref()
        .and_then(|t| t.agent.as_ref())
        .map_or("", String::as_str);
    let complexity = task_store_record.as_ref().map_or_else(
        || "medium".to_string(),
        |t| {
            if t.complexity.is_empty() {
                "medium".to_string()
            } else {
                t.complexity.clone()
            }
        },
    );

    let new_model = router_config.model_for_complexity(previous_agent, &complexity, &task.id.0);

    if let Some(model) = new_model {
        // Got a valid model for this agent — update the store and set Routed.
        if let Err(e) = store_set_result(
            &Some(Arc::clone(store)),
            repo,
            &task.id.0,
            &[
                ("model", serde_json::json!(model)),
                (
                    "route_reason",
                    serde_json::json!("re-dispatch after review changes"),
                ),
            ],
        )
        .await
        {
            tracing::warn!(task_id = task.id.0, err = %e, "model update failed — will retry on next tick");
            return Ok(());
        }

        let fields = [
            (
                "review_cycles",
                serde_json::json!(i64::from(review_cycles + 1)),
            ),
            (
                "review_agent_failures",
                serde_json::json!(0), // reset failures for the new review round
            ),
            ("review_resets", serde_json::json!(0)),
        ];
        if let Err(e) = task_manager
            .update_task_status_and_result(&task.id, Status::Routed, &fields)
            .await
        {
            tracing::warn!(
                task_id = task.id.0,
                err = %e,
                "Routed transition failed — will retry on next tick"
            );
            return Ok(());
        }

        tracing::info!(
            task_id = task.id.0,
            review_cycles = review_cycles + 1,
            pr_number,
            agent = previous_agent,
            model = %model,
            complexity = %complexity,
            "re-dispatching task (new model via model_for_complexity) to address review feedback on same PR"
        );
    } else {
        // All models for this agent are cooled — fall back to full re-route.
        // Clear agent/model/route_reason so Phase 3a picks the best available combo.
        if let Err(e) = store_set_result(
            &Some(Arc::clone(store)),
            repo,
            &task.id.0,
            &[
                ("agent", serde_json::json!(null)),
                ("model", serde_json::json!(null)),
                ("route_reason", serde_json::json!("")),
            ],
        )
        .await
        {
            tracing::warn!(task_id = task.id.0, err = %e, "store write failed");
        }

        let fields = [
            (
                "review_cycles",
                serde_json::json!(i64::from(review_cycles + 1)),
            ),
            (
                "review_agent_failures",
                serde_json::json!(0), // reset failures for the new review round
            ),
            ("review_resets", serde_json::json!(0)),
        ];
        if let Err(e) = task_manager
            .update_task_status_and_result(&task.id, Status::New, &fields)
            .await
        {
            tracing::warn!(
                task_id = task.id.0,
                err = %e,
                "all models cooled for agent {previous_agent} — New transition failed, will retry on next tick"
            );
            return Ok(());
        }

        tracing::info!(
            task_id = task.id.0,
            review_cycles = review_cycles + 1,
            pr_number,
            agent = previous_agent,
            "all models cooled for agent — falling back to full re-route (Status::New)"
        );
    }

    Ok(())
}

/// ISO-8601 timestamp comparison sanity check used by `branch_newer_than_last_review`.
///
/// GitHub timestamps are RFC 3339 / ISO-8601 strings (e.g. `"2024-01-15T12:30:00Z"`).
/// Lexicographic string ordering is equivalent to chronological ordering for this
/// format, which is what we rely on in `branch_newer_than_last_review`.
#[cfg(test)]
fn iso8601_is_newer(newer: &str, older: &str) -> bool {
    newer > older
}

#[cfg(test)]
mod tests;
