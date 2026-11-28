//! Bringing a PR branch up to date and recovering a failed rebase

use crate::engine::approval;

use super::MAX_MERGE_CONFLICT_RETRIES;
use crate::backends::{ExternalTask, Status};
use crate::engine;
use crate::engine::git::{git_ops, worktree};
use crate::engine::tasks::TaskManager;
use crate::github::http::GhHttp;
use crate::github::types::GitHubPullRequest;
use crate::store::TaskStore;
use crate::store::{opt_store_get_task, store_increment, store_set_result};
use std::cmp;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio::time;

pub(super) fn is_ssh_agent_fetch_failure(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("sign_and_send_pubkey")
        || lower.contains("agent refused operation")
        || lower.contains("communication with agent failed")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum WorktreeRebaseOutcome {
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

pub(super) fn classify_fetch_failure(stderr: &str) -> WorktreeRebaseOutcome {
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
    let on_head = approval::worktree_head_sha(path).await.as_deref() == Some(pr.head.sha.as_str());
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
pub(super) async fn bring_branch_up_to_date(
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
            approval::carry_approval_after_rebase(store, gh, repo, task_id, pr, pr_number).await;
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
pub(super) enum MergeFailureClass {
    RecoverableConflict,
    Transient,
    Blocking,
}

pub(super) fn classify_merge_failure(error: &str) -> MergeFailureClass {
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

pub(super) async fn attempt_worktree_rebase_and_force_push(
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
pub(super) async fn handle_pr_rebase_recovery(
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
pub(super) async fn apply_rebase_recovery_outcome(
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
pub(super) async fn poll_mergeable_until(
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
