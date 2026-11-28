//! PR auto-merge and review-changes handling.
//!
//! Extracted from `engine/review.rs`. Handles:
//! - [`auto_merge_pr`]: merge an approved PR after CI passes
//! - rebase recovery on merge conflict (prevents infinite needs_review loop)
//! - [`handle_review_changes`]: re-dispatch agent to address review feedback
//! - [`dedup_reviews`]: deduplicate GitHub reviews by reviewer (keep latest)
//! - [`any_changes_requested_in_reviews`]: check if any reviewer's latest review blocks merge
//! - [`crate::engine::attribution_footer`]: standard "Commented/Reviewed by bot" footer for GitHub comments

mod rebase;

use crate::engine::review::approval;

use crate::backends::ExternalId;
use crate::backends::{ExternalBackend, ExternalTask, Status};
use crate::config;
use crate::engine;
use crate::engine::ci::bootstrap;
pub(crate) use crate::engine::ci::{
    CI_FAILURE_AUTO_MERGE_STAGE, MAX_CI_MERGE_FAILURES, ci_failure_limit_reason,
};
pub(crate) use crate::engine::ci::{
    billing_failed_runs, count_ci_failure_for_head, red_ci_context, required_checks_state,
};
use crate::engine::ci::{
    ci_check_ts_key, ci_pending_elapsed, clear_ci_pending_since, failing_optional_check_names,
    may_merge_onto_base, mergeable_state_ready, record_ci_check, required_ci_checks_pass,
};
use crate::engine::git::cleanup::{cleanup_merged_pr_worktree, cleanup_task_worktree};
use crate::engine::router::config::RouterConfig;
use crate::engine::tasks::TaskManager;
use crate::github::app;
use crate::github::http::GhHttp;
use crate::github::types::GitHubComment;
use crate::github::types::ReviewIssue;
use crate::github::types::{GitHubCheckRun, GitHubReview};
use crate::store;
use crate::store::TaskStatus;
use crate::store::TaskStore;
use crate::store::{
    opt_store_get_task, store_increment, store_reset_failure_counters, store_set_result,
};
use crate::tmux::TmuxManager;
use rebase::{
    MergeFailureClass, WorktreeRebaseOutcome, attempt_worktree_rebase_and_force_push,
    bring_branch_up_to_date, classify_merge_failure, handle_pr_rebase_recovery,
    poll_mergeable_until,
};
pub(crate) use rebase::{is_pr_behind, prepare_branch_for_review};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
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

// Global semaphore limiting concurrent CI polling loops across all tasks.
static CI_POLL_SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();

fn ci_poll_semaphore() -> &'static Arc<Semaphore> {
    CI_POLL_SEMAPHORE.get_or_init(|| Arc::new(Semaphore::new(3)))
}

async fn finish_merged_pr(
    task: &ExternalTask,
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
    repo: &str,
) -> anyhow::Result<()> {
    if bootstrap::hold_after_merge(task, store, repo).await {
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
                block_closed_approved_pr(task_manager, &task.id).await;
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
    approval::carry_approval_across_base_update(store, &gh, repo, &task.id.0, &pr, pr_number).await;
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

/// Someone closed an approved PR without merging it, so a person decides what happens next
pub(crate) async fn block_closed_approved_pr(task_manager: &TaskManager, id: &ExternalId) {
    tracing::warn!(
        task_id = id.0,
        "approved PR was closed without merge, blocking task"
    );
    let fields = [(
        "block_reason",
        serde_json::json!("approved PR was closed without merge"),
    )];
    if let Err(e) = task_manager
        .update_task_status_and_result(id, Status::Blocked, &fields)
        .await
    {
        tracing::warn!(task_id = id.0, err = %e, "failed to block a task whose approved PR was closed");
    }
}

#[cfg(test)]
mod tests;
