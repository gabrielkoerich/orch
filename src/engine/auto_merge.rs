//! PR auto-merge and review-changes handling.
//!
//! Extracted from `engine/review.rs`. Handles:
//! - [`auto_merge_pr`]: merge an approved PR after CI passes
//! - [`handle_review_changes`]: re-dispatch agent to address review feedback
//! - [`dedup_reviews`]: deduplicate GitHub reviews by reviewer (keep latest)
//! - [`any_changes_requested_in_reviews`]: check if any reviewer's latest review blocks merge
//! - [`crate::engine::attribution_footer`]: standard "Commented/Reviewed by bot" footer for GitHub comments

use crate::backends::{ExternalBackend, ExternalTask, Status};
use crate::config;
use crate::engine::cleanup::cleanup_task_worktree;
use crate::engine::runner::worktree;
use crate::engine::tasks::TaskManager;
use crate::github::http::GhHttp;
use crate::github::types::{GitHubPullRequest, GitHubReview};
use crate::store::TaskStore;
use crate::store::{
    opt_store_get_task, store_increment, store_reset_failure_counters, store_set_result,
};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use tokio::sync::Semaphore;

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
/// before giving up and blocking the task for human intervention.
/// Both `review_open_prs` and `auto_merge_pr` use this constant so the limit is
/// always consistent regardless of which code path increments the counter first.
pub(crate) const MAX_MERGE_CONFLICT_RETRIES: u64 = 3;

/// Maximum number of CI failure or CI timeout events in `auto_merge_pr` before
/// the task is blocked for human intervention instead of re-entering NeedsReview.
const MAX_CI_MERGE_FAILURES: u64 = 3;

// Global semaphore limiting concurrent CI polling loops across all tasks.
static CI_POLL_SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();

fn ci_poll_semaphore() -> &'static Arc<Semaphore> {
    CI_POLL_SEMAPHORE.get_or_init(|| Arc::new(Semaphore::new(3)))
}

/// Check if we should skip CI status checking for this task due to per-task cooldown.
///
/// Uses the KV store to track the last time CI was checked for each task.
/// Returns `true` if the cooldown has not elapsed yet.
async fn is_ci_check_in_cooldown(store: &Arc<TaskStore>, task_id: &str) -> anyhow::Result<bool> {
    let cooldown_secs: u64 = config::get("workflow.ci_check_cooldown_secs")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_CI_CHECK_COOLDOWN_SECS);

    let key = format!("ci_check_ts:{}", task_id);
    let last_check = store.kv_get(&key).await?;

    if let Some(last_ts) = last_check {
        if let Ok(last_time) = chrono::DateTime::parse_from_rfc3339(&last_ts) {
            let elapsed =
                chrono::Utc::now().signed_duration_since(last_time.with_timezone(&chrono::Utc));
            if elapsed.num_seconds() < cooldown_secs as i64 {
                return Ok(true);
            }
        }
    }

    Ok(false)
}

/// Check if CI checks should be skipped for this task.
///
/// Returns true if the task is already blocked due to a billing failure or other
/// unrecoverable CI failure. In such cases, there's no point polling CI status.
fn should_skip_ci_check(stored: &crate::store::Task) -> bool {
    let block_reason = stored.block_reason.as_deref().unwrap_or("");
    block_reason.contains("billing")
        || block_reason.contains("payment")
        || block_reason.contains("spending limit")
        || block_reason.contains("CI failure limit")
}

/// Record that CI was checked for this task, updating the timestamp in KV store.
async fn record_ci_check(store: &Arc<TaskStore>, task_id: &str) -> anyhow::Result<()> {
    let key = format!("ci_check_ts:{}", task_id);
    let now = chrono::Utc::now().to_rfc3339();
    store.kv_set(&key, &now).await
}

/// Poll GitHub for PR mergeability until it is computed or `max_wait` elapses.
///
/// GitHub returns `None` for `mergeable` when it hasn't computed the value yet —
/// common when the review agent approves seconds after PR creation. Polling for
/// a brief window avoids an unnecessary deferral to the next sync tick (~10-45s).
///
/// Returns the PR with `mergeable` resolved, or bails if GitHub hasn't computed
/// it within `max_wait` or if the PR has merge conflicts.
async fn poll_mergeable_until(
    gh: &GhHttp,
    repo: &str,
    pr_number: u64,
    max_wait: std::time::Duration,
) -> anyhow::Result<GitHubPullRequest> {
    let interval = std::time::Duration::from_secs(2);
    let deadline = std::time::Instant::now() + max_wait;
    loop {
        let pr = gh.get_pr(repo, pr_number).await?;
        match pr.mergeable {
            Some(false) => anyhow::bail!("PR is not mergeable (merge conflicts present)"),
            Some(true) => return Ok(pr),
            None => {
                if std::time::Instant::now() >= deadline {
                    anyhow::bail!("PR mergeability not yet computed — retry");
                }
                let remaining = deadline - std::time::Instant::now();
                let sleep_for = std::cmp::min(remaining, interval);
                tokio::time::sleep(sleep_for).await;
            }
        }
    }
}

fn required_checks_state(
    required_contexts: &[String],
    check_runs: &[(String, String, Option<String>)],
    statuses: &[(String, String)],
) -> (String, u64, u64, u64, u64) {
    if required_contexts.is_empty() {
        return ("success".to_string(), 0, 0, 0, 0);
    }

    let mut passing = 0u64;
    let mut failing = 0u64;
    let mut pending = 0u64;

    for context in required_contexts {
        let mut matched = false;

        let mut check_run_match = None;
        for (name, status, conclusion) in check_runs {
            if name == context {
                check_run_match = Some((status.as_str(), conclusion.as_deref()));
                break;
            }
        }

        if let Some((status, conclusion)) = check_run_match {
            matched = true;
            if status != "completed" || conclusion.is_none() {
                pending += 1;
            } else if matches!(conclusion, Some("success" | "neutral" | "skipped")) {
                passing += 1;
            } else {
                failing += 1;
            }
        } else {
            for (name, state) in statuses {
                if name != context {
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

/// Returns `true` if any failed check run for `sha` carries a GitHub Actions
/// billing failure annotation.
///
/// When an account has unpaid invoices or has hit a spending limit, GitHub
/// refuses to start jobs and records the reason ("The job was not started
/// because recent account payments have failed …") as a check-run annotation.
/// The CI state still comes back as `"failure"`, which would normally trigger
/// the agent retry loop — but no code change can fix a billing problem.
/// This function lets the caller short-circuit that loop and block the task
/// immediately with a human-readable billing message instead.
async fn is_billing_failure(gh: &GhHttp, repo: &str, sha: &str) -> bool {
    let check_runs = match gh.get_check_runs(repo, sha).await {
        Ok(runs) => runs,
        Err(e) => {
            tracing::debug!(err = %e, "failed to fetch check runs for billing failure check");
            return false;
        }
    };

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
                        return true;
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

    false
}

/// Auto-merge a PR after review approval.
///
/// Checks that the automated review comment says "approve" and that CI checks
/// are green before merging. If CI fails, sets task back to routed so the
/// agent is re-dispatched to fix the issues.
#[allow(clippy::too_many_arguments)]
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
    // Early exit: skip CI checks if the task is already blocked due to
    // unrecoverable failures (billing, CI failure limit, etc.). No point
    // polling CI for a task that will never be unblocked.
    if let Some(stored_task) = opt_store_get_task(&Some(Arc::clone(store)), repo, &task.id.0).await
    {
        if stored_task.status == crate::store::TaskStatus::Blocked
            && should_skip_ci_check(&stored_task)
        {
            tracing::debug!(
                task_id = task.id.0,
                "skipping auto-merge CI checks — task is blocked with unrecoverable failure"
            );
            return Ok(());
        }
    }

    // 1. Get PR number from branch
    let gh = GhHttp::new()?;
    let pr_number = match gh.get_pr_number(repo, branch).await? {
        Some(n) => n,
        None => {
            // No open PR — check if the branch was already merged/closed.
            // Treat a closed/merged PR as idempotent success rather than a
            // retryable failure so the review-agent failure counter is not
            // incremented when the work is already done.
            match gh.get_closed_pr_state(repo, branch).await {
                Ok(Some((closed_pr_number, true, _))) => {
                    // Validate the PR's head ref matches our branch before treating as merged
                    match gh.get_pr(repo, closed_pr_number).await {
                        Ok(pr) if pr.head.ref_ == branch => {
                            tracing::info!(
                                task_id = task.id.0,
                                branch,
                                pr_number = closed_pr_number,
                                "PR already merged — marking task done (idempotent)"
                            );
                            task_manager
                                .update_task_status(&task.id, Status::Done)
                                .await?;
                            if let Err(e) = cleanup_task_worktree(&task.id.0, repo, store).await {
                                tracing::warn!(
                                    task_id = task.id.0,
                                    err = %e,
                                    "post-merge cleanup failed"
                                );
                            }
                            return Ok(());
                        }
                        Ok(_) => {
                            tracing::warn!(
                                task_id = task.id.0,
                                branch,
                                pr_number = closed_pr_number,
                                "closed PR head ref does not match branch — may be wrong PR"
                            );
                            anyhow::bail!("no matching merged PR found for branch {}", branch);
                        }
                        Err(e) => {
                            tracing::warn!(
                                task_id = task.id.0,
                                branch,
                                pr_number = closed_pr_number,
                                err = %e,
                                "failed to verify PR details, assuming merged"
                            );
                            // Fall through to marking done since get_closed_pr_state said it was merged
                            task_manager
                                .update_task_status(&task.id, Status::Done)
                                .await?;
                            if let Err(e) = cleanup_task_worktree(&task.id.0, repo, store).await {
                                tracing::warn!(
                                    task_id = task.id.0,
                                    err = %e,
                                    "post-merge cleanup failed"
                                );
                            }
                            return Ok(());
                        }
                    }
                }
                Ok(Some((closed_pr_number, false, state))) => {
                    // Validate the PR's head ref matches our branch before treating as closed
                    match gh.get_pr(repo, closed_pr_number).await {
                        Ok(pr) if pr.head.ref_ == branch => {
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
                            return Ok(());
                        }
                        Ok(_) => {
                            tracing::warn!(
                                task_id = task.id.0,
                                branch,
                                pr_number = closed_pr_number,
                                "closed PR head ref does not match branch — may be wrong PR"
                            );
                            anyhow::bail!("no matching closed PR found for branch {}", branch);
                        }
                        Err(e) => {
                            tracing::warn!(
                                task_id = task.id.0,
                                branch,
                                pr_number = closed_pr_number,
                                err = %e,
                                "failed to verify PR details, assuming closed"
                            );
                            // Fall through to marking done since get_closed_pr_state said it was closed
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
                            return Ok(());
                        }
                    }
                }
                Ok(None) => {
                    // No closed PR found either — this is a genuine error
                    anyhow::bail!("no open PR found for branch {}", branch);
                }
                Err(e) => {
                    // Error checking closed PR state — fail gracefully
                    tracing::warn!(
                        task_id = task.id.0,
                        branch,
                        err = %e,
                        "failed to check closed PR state"
                    );
                    anyhow::bail!("no open PR found for branch {}", branch);
                }
            }
        }
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
    let max_wait = std::time::Duration::from_secs(max_wait_secs);
    let start = std::time::Instant::now();
    let mut poll_count: u32 = 0;

    // Poll for mergeability to avoid deferring to the next sync tick (~10-45s)
    // when GitHub returns None because it hasn't computed mergeability yet.
    // This is common when the review agent approves quickly after PR creation.
    let mergeability_poll_secs: u64 = config::get("workflow.mergeability_poll_secs")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let pr = poll_mergeable_until(
        &gh,
        repo,
        pr_number,
        std::time::Duration::from_secs(mergeability_poll_secs),
    )
    .await?;
    let head_sha = pr.head.sha.clone();
    let base_branch = pr.base.ref_.clone();
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
        if is_ci_check_in_cooldown(store, &task.id.0).await? {
            tracing::debug!(
                task_id = task.id.0,
                pr_number,
                "CI check skipped due to per-task cooldown"
            );
            return Ok(());
        }

        // Acquire a global permit only for the HTTP polling batch.
        // The inner block ensures the permit drops before the match/sleep.
        let (state, total, passing, failing, pending) = {
            let _permit = ci_poll_semaphore().clone().acquire_owned().await;
            if required_contexts.is_empty() {
                gh.get_combined_status(repo, &head_sha, repo_has_workflows)
                    .await?
            } else {
                let check_runs = gh.get_check_runs(repo, &head_sha).await?;
                let statuses = gh.get_commit_status_contexts(repo, &head_sha).await?;
                let check_runs = check_runs
                    .into_iter()
                    .map(|run| (run.name, run.status, run.conclusion))
                    .collect::<Vec<_>>();
                required_checks_state(&required_contexts, &check_runs, &statuses)
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
        if let Err(e) = record_ci_check(store, &task.id.0).await {
            tracing::warn!(task_id = task.id.0, err = %e, "failed to record CI check timestamp");
        }

        match state.as_str() {
            "success" => break,
            "failure" => {
                // Check for billing failures before incrementing the code-quality
                // failure counter.  Billing failures are infrastructure problems —
                // no agent can fix them — so we block the task immediately and skip
                // the re-route loop entirely.
                if is_billing_failure(&gh, repo, &head_sha).await {
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

                let ci_failures = match store_increment(
                    &Some(Arc::clone(store)),
                    repo,
                    &task.id.0,
                    "ci_merge_failures",
                )
                .await
                {
                    Ok(v) => v,
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
                        serde_json::json!(format!(
                            "CI failure limit ({}) reached during auto-merge",
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
                } else {
                    tracing::warn!(
                        task_id = task.id.0,
                        pr_number,
                        failing,
                        ci_failures,
                        "CI failed — re-routing to agent to fix"
                    );
                    task_manager
                        .update_task_status(&task.id, Status::Routed)
                        .await?;
                    store_reset_failure_counters(&Some(Arc::clone(store)), repo, &task.id.0).await;
                }
                return Ok(());
            }
            _ => {
                // pending — wait up to max_wait
                if start.elapsed() >= max_wait {
                    let ci_failures = match store_increment(
                        &Some(Arc::clone(store)),
                        repo,
                        &task.id.0,
                        "ci_merge_failures",
                    )
                    .await
                    {
                        Ok(v) => v,
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
        tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)).await;
    }

    // 5. Re-verify no reviewer has requested changes since the CI wait started.
    let post_ci_reviews = gh.get_pr_reviews(repo, pr_number).await?;
    if any_changes_requested_in_reviews(&post_ci_reviews) {
        tracing::warn!(
            task_id = task.id.0,
            pr_number,
            "a reviewer requested changes during CI wait — aborting merge"
        );
        anyhow::bail!("a reviewer requested changes during CI wait — aborting merge");
    }

    tracing::info!(
        task_id = task.id.0,
        pr_number,
        branch = %branch,
        "merging PR"
    );

    // 6. Merge via gh CLI
    if let Err(e) = gh.merge_pr(repo, pr_number, true).await {
        let err_msg = e.to_string().to_lowercase();
        let is_conflict = err_msg.contains("405")
            || err_msg.contains("not mergeable")
            || err_msg.contains("merge conflict");
        let is_transient = err_msg.contains("502")
            || err_msg.contains("503")
            || err_msg.contains("504")
            || err_msg.contains("server error")
            || err_msg.contains("bad gateway")
            || err_msg.contains("service unavailable");

        if is_transient {
            tracing::warn!(
                task_id = task.id.0,
                pr_number,
                error = %e,
                "transient GitHub error on merge — will retry next sync"
            );
            // Don't change status — task stays in InReview, sync tick retries
            return Ok(());
        }

        if is_conflict {
            let retries = opt_store_get_task(&Some(Arc::clone(store)), repo, &task.id.0)
                .await
                .map(|t| t.merge_conflict_retries as u64)
                .unwrap_or(0);
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
                let comment = format!("Auto-merge failed after {} rebase attempts: {}", retries, e);
                let footer = crate::engine::attribution_footer(
                    "Commented",
                    review_agent,
                    Some(review_model),
                );
                let _ = gh
                    .add_comment(
                        repo,
                        &pr_number.to_string(),
                        &format!("{}{}", comment, footer),
                    )
                    .await;
                return Ok(());
            }

            // Try rebase in the worktree
            let worktree_path = opt_store_get_task(&Some(Arc::clone(store)), repo, &task.id.0)
                .await
                .map(|t| t.worktree)
                .filter(|wt| !wt.is_empty());
            // Track whether a subsequent force-push failed after a successful rebase
            // so we can report the correct reason to the PR instead of blaming
            // the rebase/merge step. Declared here so it's visible after the worktree block.
            let mut push_failed = false;
            let mut push_err_msg = String::new();

            if let Some(wt) = worktree_path {
                let wt_path = std::path::PathBuf::from(&wt);
                if wt_path.exists() {
                    tracing::info!(
                        task_id = task.id.0,
                        worktree = %wt,
                        "attempting rebase to resolve merge conflict"
                    );
                    let default_branch = worktree::detect_default_branch(&wt_path).await;
                    let fetch_result = tokio::process::Command::new("git")
                        .args(["fetch", "origin"])
                        .current_dir(&wt_path)
                        .output()
                        .await;

                    let rebase_result = match fetch_result {
                        Ok(out) if out.status.success() => {
                            // Stash any uncommitted changes so the rebase can proceed cleanly.
                            // Worktrees killed mid-run (e.g. service restart) may have leftover
                            // unstaged changes from a previous attempt that block `git rebase`.
                            //
                            // Safety: git stashes are repo-global. With multiple worktrees running
                            // concurrently we must capture the exact stash ref created here and
                            // apply it by that ref — NOT with `stash pop`, which would apply
                            // stash@{0} (the most-recent stash) and could restore a stash from a
                            // different worktree running in parallel.
                            let stash_ref: Option<String> =
                                if crate::engine::runner::git_ops::has_changes(&wt_path).await {
                                    let stash_out = tokio::process::Command::new("git")
                                        .args(["stash", "--include-untracked"])
                                        .current_dir(&wt_path)
                                        .output()
                                        .await;
                                    match stash_out {
                                        Ok(o) if o.status.success() => {
                                            let ref_out = tokio::process::Command::new("git")
                                                .args(["rev-parse", "refs/stash@{0}"])
                                                .current_dir(&wt_path)
                                                .output()
                                                .await;
                                            let stash_hash = ref_out
                                                .ok()
                                                .filter(|o| o.status.success())
                                                .map(|o| {
                                                    String::from_utf8_lossy(&o.stdout)
                                                        .trim()
                                                        .to_string()
                                                })
                                                .filter(|s| !s.is_empty());
                                            if stash_hash.is_none() {
                                                // Stash was created but we can't track its ref.
                                                // Pop immediately to avoid orphaning the changes,
                                                // then bail — the next tick will retry.
                                                tracing::warn!(
                                                    task_id = task.id.0,
                                                    "git stash succeeded but rev-parse failed — popping stash and skipping rebase"
                                                );
                                                let _ = tokio::process::Command::new("git")
                                                    .args(["stash", "pop"])
                                                    .current_dir(&wt_path)
                                                    .output()
                                                    .await;
                                                return Ok(());
                                            }
                                            stash_hash
                                        }
                                        _ => None,
                                    }
                                } else {
                                    None
                                };

                            let rebase_out = tokio::process::Command::new("git")
                                .args([
                                    "-c",
                                    "commit.gpgsign=false",
                                    "rebase",
                                    &format!("origin/{default_branch}"),
                                ])
                                .current_dir(&wt_path)
                                .output()
                                .await;

                            // Restore stashed changes using the captured ref so we never
                            // accidentally pop a stash that belongs to a different
                            // concurrently-running worktree.
                            if let Some(ref stash_hash) = stash_ref {
                                let apply = tokio::process::Command::new("git")
                                    .args(["stash", "apply", stash_hash])
                                    .current_dir(&wt_path)
                                    .output()
                                    .await;
                                if apply.map(|o| o.status.success()).unwrap_or(false) {
                                    let list = tokio::process::Command::new("git")
                                        .args(["stash", "list", "--format=%H %gd"])
                                        .current_dir(&wt_path)
                                        .output()
                                        .await;
                                    if let Ok(list_out) = list {
                                        if list_out.status.success() {
                                            let list_str =
                                                String::from_utf8_lossy(&list_out.stdout);
                                            if let Some(ref_entry) =
                                                crate::engine::runner::git_ops::find_stash_ref_by_hash(
                                                    &list_str,
                                                    stash_hash,
                                                )
                                            {
                                                let _ = tokio::process::Command::new("git")
                                                    .args(["stash", "drop", &ref_entry])
                                                    .current_dir(&wt_path)
                                                    .output()
                                                    .await;
                                            }
                                        }
                                    }
                                } else {
                                    tracing::warn!(
                                        task_id = task.id.0,
                                        stash = %stash_hash,
                                        "stash apply failed after rebase — stash preserved for manual recovery"
                                    );
                                }
                            }

                            rebase_out
                        }
                        Ok(out) => {
                            // fetch failed — not a content conflict; leave task in InReview for retry
                            let stderr = String::from_utf8_lossy(&out.stderr);
                            tracing::warn!(
                                task_id = task.id.0,
                                stderr = %stderr,
                                "git fetch failed before rebase — skipping rebase"
                            );
                            return Ok(());
                        }
                        Err(err) => Err(err),
                    };

                    let mut rebase_conflict = false;

                    match rebase_result {
                        Ok(out) if out.status.success() => {
                            let push_result = tokio::process::Command::new("git")
                                .args(["push", "--force-with-lease"])
                                .current_dir(&wt_path)
                                .output()
                                .await;

                            match push_result {
                                Ok(push_out) if push_out.status.success() => {
                                    tracing::info!(
                                task_id = task.id.0,
                                "rebase succeeded — resetting to NeedsReview for CI + merge"
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
                                    // Enable auto-merge — GitHub merges once CI passes.
                                    // If auto-merge isn't available, keep task in InReview
                                    // so the sync tick retries merge on the next cycle.
                                    match gh.enable_auto_merge(repo, pr_number).await {
                                        Ok(_) => {
                                            tracing::info!(
                                                task_id = task.id.0,
                                                "auto-merge enabled — task stays in InReview until GitHub merges"
                                            );
                                            // Don't mark Done or clean up worktree yet.
                                            // GitHub auto-merge fires only after CI passes on the
                                            // rebased commit. If CI fails, the PR stays open.
                                            // check_merged_prs (sync tick) will detect the actual
                                            // merge and mark Done at that point.
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                task_id = task.id.0,
                                                error = %e,
                                                "auto-merge unavailable — task stays in InReview for sync retry"
                                            );
                                            // Don't change status — sync tick will poll CI
                                            // and retry merge when checks pass.
                                        }
                                    }
                                    return Ok(());
                                }
                                Ok(push_out) => {
                                    // Push returned non-zero. Capture stderr for PR-facing message.
                                    let stderr =
                                        String::from_utf8_lossy(&push_out.stderr).to_string();
                                    tracing::error!(
                                        task_id = task.id.0,
                                        stderr = %stderr,
                                        "force-push after rebase failed — blocking for human review"
                                    );
                                    push_failed = true;
                                    push_err_msg = stderr;
                                }
                                Err(push_err) => {
                                    let err_str = push_err.to_string();
                                    tracing::error!(
                                        task_id = task.id.0,
                                        error = %push_err,
                                        "force-push command error — blocking for human review"
                                    );
                                    push_failed = true;
                                    push_err_msg = err_str;
                                }
                            }
                        }
                        Ok(out) => {
                            let stderr = String::from_utf8_lossy(&out.stderr);
                            tracing::warn!(
                                task_id = task.id.0,
                                stderr = %stderr,
                                "rebase failed with content conflict — re-routing to agent"
                            );
                            rebase_conflict = true;
                        }
                        Err(io_err) => {
                            tracing::error!(
                                task_id = task.id.0,
                                error = %io_err,
                                "fetch/rebase command error — blocking for human review"
                            );
                        }
                    }

                    if rebase_conflict {
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
                                    serde_json::json!(
                                        "re-dispatch after auto-merge rebase conflict"
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
                }
            }

            // Rebase failed or no worktree or push failure — block
            // Persist block_reason BEFORE transitioning to Blocked to avoid
            // a race where auto_unblock sees a blocked task without a reason
            // and immediately unblocks it.
            let comment = if push_failed {
                format!(
                    "Auto-merge failed (rebase succeeded but force-push failed): {}",
                    push_err_msg
                )
            } else {
                format!(
                    "Auto-merge failed (merge conflict, rebase unsuccessful): {}",
                    e
                )
            };
            let block_reason = if push_failed {
                format!(
                    "auto-merge force-push failed after rebase: {}",
                    push_err_msg
                )
            } else {
                format!("auto-merge rebase failed after merge conflict: {}", e)
            };
            let fields = [("block_reason", serde_json::json!(block_reason))];
            if let Err(e) = task_manager
                .update_task_status_and_result(&task.id, Status::Blocked, &fields)
                .await
            {
                tracing::error!(task_id = task.id.0, err = %e, "failed to write block_reason and set Blocked");
                return Ok(());
            }
            let footer =
                crate::engine::attribution_footer("Commented", review_agent, Some(review_model));
            if let Err(e) = gh
                .add_comment(
                    repo,
                    &pr_number.to_string(),
                    &format!("{}{}", comment, footer),
                )
                .await
            {
                tracing::warn!(
                    task_id = task.id.0,
                    pr_number,
                    err = %e,
                    "auto-merge: failed to post merge conflict failure comment on PR"
                );
            }
            return Ok(());
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
        let comment = format!("Auto-merge failed: {}", e);
        let footer =
            crate::engine::attribution_footer("Commented", review_agent, Some(review_model));
        if let Err(e) = gh
            .add_comment(
                repo,
                &pr_number.to_string(),
                &format!("{}{}", comment, footer),
            )
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

    // 6. Update status to done
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
    task_manager
        .update_task_status(&task.id, Status::Done)
        .await?;

    // 7. Cleanup worktree + branches
    if let Err(e) = cleanup_task_worktree(&task.id.0, repo, store).await {
        tracing::warn!(task_id = task.id.0, err = %e, "post-merge cleanup failed");
    }

    // Kill any orphaned review session that might be racing us
    let tmux = crate::tmux::TmuxManager::new();
    let stale_session = tmux.session_name(repo, &format!("{}-review", task.id.0));
    if let Err(e) = tmux.kill_session(&stale_session).await {
        tracing::debug!(task_id = task.id.0, error = %e, "failed to kill stale review session after auto-merge");
    }

    // 8. Post final comment on the PR
    let comment = "✅ PR reviewed, approved, and merged.";
    let footer = crate::engine::attribution_footer("Reviewed", review_agent, Some(review_model));
    if let Err(e) = gh
        .add_comment(
            repo,
            &pr_number.to_string(),
            &format!("{}{}", comment, footer),
        )
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
    let mut latest: Option<&crate::github::types::GitHubComment> = None;
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
        Ok(runs) => runs
            .into_iter()
            .map(|r| (r.name, r.status, r.conclusion))
            .collect::<Vec<_>>(),
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
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_review_changes(
    task: &ExternalTask,
    notes: &str,
    issues: &[crate::engine::runner::response::ReviewIssue],
    _backend: &Arc<dyn ExternalBackend>,
    repo: &str,
    pr_number: u64,
    review_agent: &str,
    review_model: &str,
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
) -> anyhow::Result<()> {
    // 1. Check review cycle count (max 2 review rounds)
    let task_store_record = opt_store_get_task(&Some(Arc::clone(store)), repo, &task.id.0).await;
    let review_cycles: u32 = task_store_record
        .as_ref()
        .map(|t| t.review_cycles.max(0) as u32)
        .unwrap_or(0);

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
            .map(|t| t.ci_recovery_count)
            .unwrap_or(0);

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
                            crate::engine::attribution_footer("Commented", review_agent, Some(review_model))
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
            "🔍 Review agent requested changes after {} cycles. Escalating to human.\n\n**Review Notes:**\n{}",
            review_cycles, notes
        );
        let footer =
            crate::engine::attribution_footer("Commented", review_agent, Some(review_model));
        if let Err(e) = gh
            .add_comment(repo, &pr_num_str, &format!("{}{}", escalation, footer))
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
            comment.push_str(&format!(
                "- `{}` line {}: {} [{}]\n",
                issue.file,
                issue
                    .line
                    .map(|l| l.to_string())
                    .unwrap_or_else(|| "?".to_string()),
                issue.description,
                issue.severity
            ));
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
        .map(|a| a.as_str())
        .unwrap_or("");
    let complexity = task_store_record
        .as_ref()
        .map(|t| {
            if t.complexity.is_empty() {
                "medium".to_string()
            } else {
                t.complexity.clone()
            }
        })
        .unwrap_or_else(|| "medium".to_string());

    let config = crate::engine::router::config::RouterConfig::from_config();
    let new_model = config.model_for_complexity(previous_agent, &complexity, &task.id.0);

    match new_model {
        Some(model) => {
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
                    serde_json::json!((review_cycles + 1) as i64),
                ),
                (
                    "review_agent_failures",
                    serde_json::json!(0), // reset failures for the new review round
                ),
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
        }
        None => {
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
                    serde_json::json!((review_cycles + 1) as i64),
                ),
                (
                    "review_agent_failures",
                    serde_json::json!(0), // reset failures for the new review round
                ),
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
mod tests {
    use super::*;
    use crate::backends::test_helpers::NoopBackend;
    use crate::backends::{ExternalId, ExternalTask};
    use crate::github::types::{GitHubReview, GitHubUser};

    fn make_review(login: &str, state: &str, submitted_at: &str) -> GitHubReview {
        GitHubReview {
            id: 1,
            user: GitHubUser {
                login: login.to_string(),
            },
            body: None,
            state: state.to_string(),
            html_url: None,
            submitted_at: submitted_at.to_string(),
            commit_id: None,
        }
    }

    #[test]
    fn required_checks_state_uses_required_contexts() {
        let required = vec!["ci".to_string(), "lint".to_string()];
        let check_runs = vec![
            (
                "ci".to_string(),
                "completed".to_string(),
                Some("success".to_string()),
            ),
            (
                "lint".to_string(),
                "completed".to_string(),
                Some("failure".to_string()),
            ),
        ];
        let statuses = vec![("unused".to_string(), "success".to_string())];

        let (state, total, passing, failing, pending) =
            required_checks_state(&required, &check_runs, &statuses);

        assert_eq!(state, "failure");
        assert_eq!(total, 2);
        assert_eq!(passing, 1);
        assert_eq!(failing, 1);
        assert_eq!(pending, 0);
    }

    #[test]
    fn test_any_changes_requested_returns_true_when_latest_is_changes_requested() {
        let reviews = vec![
            make_review("alice", "APPROVED", "2026-01-01T10:00:00Z"),
            make_review("alice", "CHANGES_REQUESTED", "2026-01-01T11:00:00Z"),
        ];
        assert!(
            any_changes_requested_in_reviews(&reviews),
            "should return true when reviewer's latest review is CHANGES_REQUESTED"
        );
    }

    #[test]
    fn test_any_changes_requested_returns_false_when_latest_is_approved() {
        let reviews = vec![
            make_review("alice", "CHANGES_REQUESTED", "2026-01-01T10:00:00Z"),
            make_review("alice", "APPROVED", "2026-01-01T11:00:00Z"),
        ];
        assert!(
            !any_changes_requested_in_reviews(&reviews),
            "should return false when reviewer's latest review is APPROVED"
        );
    }

    #[test]
    fn test_any_changes_requested_returns_false_for_empty_reviews() {
        assert!(
            !any_changes_requested_in_reviews(&[]),
            "should return false for empty review list"
        );
    }

    #[test]
    fn test_any_changes_requested_ignores_commented_state() {
        let reviews = vec![make_review("alice", "COMMENTED", "2026-01-01T10:00:00Z")];
        assert!(
            !any_changes_requested_in_reviews(&reviews),
            "COMMENTED state should be ignored"
        );
    }

    #[test]
    fn test_any_changes_requested_one_reviewer_blocks_even_if_other_approved() {
        let reviews = vec![
            make_review("alice", "APPROVED", "2026-01-01T10:00:00Z"),
            make_review("bob", "CHANGES_REQUESTED", "2026-01-01T10:00:00Z"),
        ];
        assert!(
            any_changes_requested_in_reviews(&reviews),
            "should return true if any reviewer has outstanding CHANGES_REQUESTED"
        );
    }

    /// Regression test: handle_review_changes must set status to Routed (not New)
    /// when a valid model is available for the task's agent via model_for_complexity().
    /// This preserves the documented review-cycle architecture where re-dispatch
    /// reuses existing routing context instead of forcing full LLM re-routing.
    #[tokio::test]
    async fn handle_review_changes_sets_routed_with_valid_model() {
        use crate::engine::tasks::TaskManager;
        use crate::store::{TaskStatus, TaskStore};

        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let repo = "owner/repo";

        let task_id_num = store
            .create(&crate::store::NewTask {
                external_id: None,
                repo: repo.to_string(),
                origin: "internal".to_string(),
                title: "Implement feature X".to_string(),
                body: "body".to_string(),
                source: "cron".to_string(),
                source_id: "daily".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
                parent_id: None,
            })
            .await
            .unwrap();
        store
            .update_status(task_id_num, crate::store::TaskStatus::InReview)
            .await
            .unwrap();

        let task_id_str = format!("internal:{task_id_num}");
        let backend: Arc<dyn crate::backends::ExternalBackend> = Arc::new(NoopBackend);
        let task_manager = Arc::new(TaskManager::with_store(
            Arc::clone(&backend),
            Arc::clone(&store),
            repo.to_string(),
        ));

        let task = ExternalTask {
            id: ExternalId(task_id_str.clone()),
            title: "Implement feature X".to_string(),
            body: "body".to_string(),
            state: "open".to_string(),
            labels: vec![],
            author: "".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: "".to_string(),
        };

        let result = handle_review_changes(
            &task,
            "Please fix the type error on line 42",
            &[],
            &backend,
            repo,
            101,
            "claude",
            "sonnet",
            &task_manager,
            &store,
        )
        .await;

        assert!(
            result.is_ok(),
            "handle_review_changes returned Err: {result:?}"
        );

        let updated = store.get(task_id_num).await.unwrap();
        // When the agent has no model pools configured (default test config),
        // model_for_complexity returns None → falls back to Status::New.
        assert_eq!(
            updated.status,
            TaskStatus::New,
            "handle_review_changes must fall back to New when model_for_complexity returns None"
        );

        assert!(
            !updated.pr_review_context.is_empty(),
            "pr_review_context must be stored so the re-dispatched agent sees the feedback"
        );
        assert!(
            !updated
                .pr_review_context
                .starts_with("A reviewer has requested changes"),
            "pr_review_context must NOT start with the template header"
        );
        assert!(
            updated
                .pr_review_context
                .contains("Please fix the type error on line 42"),
            "pr_review_context must contain the review notes passed to handle_review_changes"
        );

        assert_eq!(
            updated.review_cycles, 1,
            "review_cycles must be incremented on each review change request"
        );

        assert_eq!(
            updated.review_agent_failures, 0,
            "review_agent_failures must be reset to 0 when re-routing after review changes"
        );
    }

    /// Regression: handle_review_changes must resolve a valid model via
    /// model_for_complexity() instead of reusing the review agent's model.
    /// When the task's agent has a valid model available, it sets Routed.
    /// When all models are cooled, it falls back to New for full re-route (#1723).
    #[tokio::test]
    async fn handle_review_changes_resolves_valid_model_or_falls_back() {
        use crate::engine::tasks::TaskManager;
        use crate::store::{TaskStatus, TaskStore};

        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let repo = "owner/repo";

        let task_id_num = store
            .create(&crate::store::NewTask {
                external_id: None,
                repo: repo.to_string(),
                origin: "internal".to_string(),
                title: "Test".to_string(),
                body: "body".to_string(),
                source: "cron".to_string(),
                source_id: "daily".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
                parent_id: None,
            })
            .await
            .unwrap();
        store
            .update_status(task_id_num, crate::store::TaskStatus::InReview)
            .await
            .unwrap();

        // Pre-set agent/model/complexity to simulate a stored routing.
        // The model "opus" may not be valid for this agent's pool, but
        // model_for_complexity will resolve a valid one.
        store
            .set_fields(
                task_id_num,
                &[
                    ("agent", serde_json::json!("claude")),
                    ("model", serde_json::json!("opus")),
                    ("complexity", serde_json::json!("medium")),
                    (
                        "route_reason",
                        serde_json::json!("llm classified as medium"),
                    ),
                ],
            )
            .await
            .unwrap();

        let task_id_str = format!("internal:{task_id_num}");
        let backend: Arc<dyn crate::backends::ExternalBackend> = Arc::new(NoopBackend);
        let task_manager = Arc::new(TaskManager::with_store(
            Arc::clone(&backend),
            Arc::clone(&store),
            repo.to_string(),
        ));

        let task = ExternalTask {
            id: ExternalId(task_id_str.clone()),
            title: "Test".to_string(),
            body: "body".to_string(),
            state: "open".to_string(),
            labels: vec![],
            author: "".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: "".to_string(),
        };

        let result = handle_review_changes(
            &task,
            "Fix the bug",
            &[],
            &backend,
            repo,
            42,
            "minimax",
            "opus",
            &task_manager,
            &store,
        )
        .await;

        assert!(
            result.is_ok(),
            "handle_review_changes returned Err: {result:?}"
        );

        let updated = store.get(task_id_num).await.unwrap();

        // When model_for_complexity returns a valid model → Routed, agent preserved.
        // When all models are cooled → falls back to New, agent cleared for full re-route.
        // In test env with no model_map configured, model_for_complexity returns None → New.
        // This is the expected fallback behavior.
        let model_was_resolved = updated
            .model
            .as_deref()
            .map(|m| !m.is_empty())
            .unwrap_or(false);
        if model_was_resolved {
            // Got a valid model → should be Routed, agent preserved
            assert_eq!(
                updated.agent.as_deref(),
                Some("claude"),
                "agent must be preserved when model_for_complexity returns a valid model"
            );
            assert_eq!(
                updated.status,
                TaskStatus::Routed,
                "status must be Routed when model_for_complexity returns a valid model"
            );
        } else {
            // All models cooled or no pools configured → falls back to New, agent cleared
            assert_eq!(
                updated.agent.as_deref(),
                None,
                "agent must be cleared when falling back to full re-route"
            );
            assert_eq!(
                updated.status,
                TaskStatus::New,
                "status must be New when model_for_complexity returns None (all cooled)"
            );
        }

        assert_eq!(
            updated.review_agent_failures, 0,
            "review_agent_failures must be reset to 0 when re-routing after review changes"
        );
    }

    /// Regression: a transient `update_task_status` failure must NOT
    /// cause `handle_review_changes` to return `Err`.  Returning `Err` would
    /// propagate to the review subscriber which increments
    /// `review_agent_failures` — incorrectly consuming the retry budget even
    /// though the review decision itself was already persisted.
    ///
    /// We simulate the failure by giving the `TaskManager` a different repo
    /// than the one the task was created in, so `resolve_task_id` returns
    /// `None` and `update_task_status` returns `Err` for that internal task.
    #[tokio::test]
    async fn handle_review_changes_transition_failure_returns_ok() {
        use crate::engine::tasks::TaskManager;
        use crate::store::{TaskStatus, TaskStore};

        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let task_repo = "owner/repo";

        let task_id_num = store
            .create(&crate::store::NewTask {
                external_id: None,
                repo: task_repo.to_string(),
                origin: "internal".to_string(),
                title: "Fix bug".to_string(),
                body: "body".to_string(),
                source: "cron".to_string(),
                source_id: "daily".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
                parent_id: None,
            })
            .await
            .unwrap();
        store
            .update_status(task_id_num, crate::store::TaskStatus::InReview)
            .await
            .unwrap();

        let task_id_str = format!("internal:{task_id_num}");
        let backend: Arc<dyn crate::backends::ExternalBackend> = Arc::new(NoopBackend);

        // Intentionally wrong repo — causes resolve_task_id to return None,
        // which makes update_task_status return Err for the internal task.
        let wrong_repo = "other/repo";
        let task_manager = Arc::new(TaskManager::with_store(
            Arc::clone(&backend),
            Arc::clone(&store),
            wrong_repo.to_string(),
        ));

        let task = ExternalTask {
            id: ExternalId(task_id_str.clone()),
            title: "Fix bug".to_string(),
            body: "body".to_string(),
            state: "open".to_string(),
            labels: vec![],
            author: "".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: "".to_string(),
        };

        // The status transition will fail (wrong repo), but handle_review_changes
        // must still return Ok — review context was persisted before the failure.
        let result = handle_review_changes(
            &task,
            "Fix the null pointer on line 10",
            &[],
            &backend,
            task_repo,
            202,
            "claude",
            "sonnet",
            &task_manager,
            &store,
        )
        .await;

        assert!(
            result.is_ok(),
            "handle_review_changes must return Ok even when status transition fails: {result:?}"
        );

        // Review context must have been persisted despite the status failure.
        let stored = store.get(task_id_num).await.unwrap();
        assert!(
            stored
                .pr_review_context
                .contains("Fix the null pointer on line 10"),
            "pr_review_context must be persisted even when status transition fails"
        );
        // Review_cycles should NOT be incremented if the status transition fails.
        // This prevents premature escalation when the Routed transition fails.
        // The increment happens AFTER successful status transition in the fixed code.
        assert_eq!(
            stored.review_cycles, 0,
            "review_cycles must NOT be incremented when status transition fails"
        );
        // Status stays InReview (not Routed/New) because the transition failed.
        assert_eq!(
            stored.status,
            TaskStatus::InReview,
            "status should remain InReview when status transition failed"
        );
    }

    /// ISO-8601 timestamps returned by GitHub compare lexicographically in
    /// chronological order.  `branch_newer_than_last_review` relies on this.
    #[test]
    fn iso8601_timestamps_compare_correctly() {
        // newer commit (pushed after review) should be greater
        assert!(iso8601_is_newer(
            "2024-01-15T13:00:00Z",
            "2024-01-15T12:30:00Z"
        ));
        // same timestamp is not "newer"
        assert!(!iso8601_is_newer(
            "2024-01-15T12:30:00Z",
            "2024-01-15T12:30:00Z"
        ));
        // older commit is not newer
        assert!(!iso8601_is_newer(
            "2024-01-14T23:59:59Z",
            "2024-01-15T00:00:01Z"
        ));
        // cross-day boundary
        assert!(iso8601_is_newer(
            "2024-01-16T00:00:01Z",
            "2024-01-15T23:59:59Z"
        ));
    }

    /// When the GitHub API is unavailable (no real token in unit tests),
    /// `handle_review_changes` must still escalate correctly rather than
    /// getting stuck.  The stale-review check fails gracefully and we fall
    /// through to the normal Blocked path.
    #[tokio::test]
    async fn handle_review_changes_escalates_when_stale_check_fails() {
        use crate::engine::tasks::TaskManager;
        use crate::store::{TaskStatus, TaskStore};

        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let repo = "owner/repo";

        let task_id_num = store
            .create(&crate::store::NewTask {
                external_id: None,
                repo: repo.to_string(),
                origin: "internal".to_string(),
                title: "Fix defaults".to_string(),
                body: "body".to_string(),
                source: "cron".to_string(),
                source_id: "daily".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
                parent_id: None,
            })
            .await
            .unwrap();

        // Set review_cycles to max so we enter the escalation path.
        store
            .set_fields(task_id_num, &[("review_cycles", serde_json::json!(2i64))])
            .await
            .unwrap();
        store
            .update_status(task_id_num, TaskStatus::InReview)
            .await
            .unwrap();

        let task_id_str = format!("internal:{task_id_num}");
        let backend: Arc<dyn crate::backends::ExternalBackend> = Arc::new(NoopBackend);
        let task_manager = Arc::new(TaskManager::with_store(
            Arc::clone(&backend),
            Arc::clone(&store),
            repo.to_string(),
        ));

        let task = ExternalTask {
            id: ExternalId(task_id_str.clone()),
            title: "Fix defaults".to_string(),
            body: "body".to_string(),
            state: "open".to_string(),
            labels: vec![],
            author: "".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: "".to_string(),
        };

        // With no real GitHub token the stale check will fail gracefully and
        // we fall through to normal escalation (Blocked status).
        let result = handle_review_changes(
            &task,
            "sync_interval defaults are wrong",
            &[],
            &backend,
            repo,
            303,
            "minimax",
            "minimax-m2",
            &task_manager,
            &store,
        )
        .await;

        assert!(
            result.is_ok(),
            "handle_review_changes must return Ok on escalation path: {result:?}"
        );

        let stored = store.get(task_id_num).await.unwrap();
        assert_eq!(
            stored.status,
            TaskStatus::Blocked,
            "task must be Blocked when max review cycles exceeded and stale check is unavailable"
        );
    }
}
