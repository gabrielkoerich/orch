//! PR review pipeline.
//!
//! Handles the full review lifecycle: running the review agent on completed
//! tasks, parsing its decision, posting automated review comments, merging
//! approved PRs, and re-dispatching agents when changes are requested.

/// Maximum number of times we will attempt to rebase and retry a conflicting PR
/// before giving up and blocking the task for human intervention.
/// Both `review_open_prs` and `auto_merge_pr` use this constant so the limit is
/// always consistent regardless of which code path increments the counter first.
const MAX_MERGE_CONFLICT_RETRIES: u64 = 3;

/// Maximum number of CI failure or CI timeout events in `auto_merge_pr` before
/// the task is blocked for human intervention instead of re-entering NeedsReview.
const MAX_CI_MERGE_FAILURES: u64 = 3;

/// Maximum number of consecutive review agent failures before the task is blocked
/// for human intervention. Exported so `tick` and `sync` use the same threshold
/// without duplicating the constant.
pub(crate) const MAX_REVIEW_AGENT_FAILURES: u64 = 3;

use crate::backends::{ExternalBackend, ExternalTask, Status};
use crate::config;
use crate::engine::runner;
use crate::github::http::GhHttp;
use crate::github::types::{GitHubReviewComment, PullRequestReview};
use crate::tmux::TmuxManager;
use std::sync::Arc;
use tokio::sync::RwLock;

use super::cleanup::{cleanup_task_worktree, store_increment, store_reset_counters, store_set};
use super::router::Router;
use super::tasks::TaskManager;
use super::EngineConfig;
use crate::store::TaskStore;

/// Maximum consecutive PR-creation failures before blocking the task.
const MAX_PR_CREATE_FAILURES: u64 = 3;

/// Review agent decision result.
#[derive(Debug, Clone)]
pub(crate) enum ReviewDecision {
    /// Review approved, PR can be merged.
    Approve,
    /// Changes requested, PR needs fixes.
    RequestChanges {
        notes: String,
        issues: Vec<crate::engine::runner::response::ReviewIssue>,
    },
    /// Review agent failed or crashed (reason stored for logging).
    Failed(String),
    /// Unrecoverable failure — task should be blocked for human intervention.
    Blocked(String),
    /// No PR exists — nothing to review, task marked done directly.
    Skipped,
}

/// Review open PRs - re-dispatch agent to address review feedback.
///
/// Lists tasks in review, fetches PR reviews, and re-dispatches the agent
/// when a reviewer requests changes. The review context is stored in the
/// store and injected into the agent prompt.
pub(crate) async fn review_open_prs(
    backend: &Arc<dyn ExternalBackend>,
    repo: &str,
    config: &EngineConfig,
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
    dispatching: &Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
) -> anyhow::Result<()> {
    // Get tasks that are in review (have open PRs).
    // Read from the store first; fall back to backend if the store is empty.
    let mut in_review_tasks = {
        let store_tasks = store
            .list_by_status(repo, crate::store::TaskStatus::InReview)
            .await?;
        let external: Vec<_> = store_tasks
            .iter()
            .filter(|t| t.origin != "internal")
            .map(crate::engine::tasks::store_task_to_external)
            .collect();
        if store.has_tasks(repo).await {
            external
        } else {
            backend.list_by_status(Status::InReview).await?
        }
    };

    // Also include internal tasks in InReview — they create real PRs
    // and can receive human review comments just like external tasks.
    if let Ok(internal_in_review) = task_manager
        .list_internal_by_status(crate::store::TaskStatus::InReview)
        .await
    {
        in_review_tasks.extend(internal_in_review);
    }

    if in_review_tasks.is_empty() {
        return Ok(());
    }

    // Check if we should process reviews
    let auto_close_task = config.auto_close_task_on_approval;

    tracing::info!(
        count = in_review_tasks.len(),
        "checking in_review tasks for PR reviews"
    );

    let gh = GhHttp::new()?;

    for task in in_review_tasks {
        let task_id = &task.id.0;

        // Skip tasks currently being processed by the main tick (dispatch + review flow).
        let dispatch_key = format!("{}/{}", repo, task_id);
        {
            let guard = dispatching.lock().unwrap_or_else(|e| e.into_inner());
            if guard.contains(&dispatch_key) {
                tracing::debug!(
                    task_id,
                    "task locked by dispatch flow, skipping review_open_prs"
                );
                continue;
            }
        }

        // Get branch from store
        let branch = match super::cleanup::store_get_field(store, repo, task_id, "branch").await {
            Some(b) if !b.is_empty() => b,
            _ => {
                // No branch info — task is stuck in_review with no PR.
                // Move to needs_review so it doesn't poll forever.
                tracing::warn!(
                    task_id,
                    "in_review task has no branch info — setting needs_review"
                );
                if let Err(e) = task_manager
                    .update_task_status(&task.id, Status::NeedsReview)
                    .await
                {
                    tracing::warn!(task_id, err = %e, "failed to update status");
                }
                continue;
            }
        };

        // Get PR number from branch
        let pr_number = match gh.get_pr_number(repo, &branch).await {
            Ok(Some(n)) => n,
            Ok(None) => {
                // No open PR for this branch. Check if it was already merged.
                let merged = match gh.is_pr_merged(repo, &branch).await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(task_id, branch = %branch, err = %e, "merge check failed, skipping task this tick");
                        continue;
                    }
                };
                if merged {
                    tracing::info!(task_id, branch = %branch, "PR already merged, marking done");
                    if let Err(e) = task_manager
                        .update_task_status(&task.id, Status::Done)
                        .await
                    {
                        tracing::warn!(task_id, err = %e, "failed to update status to done");
                    }
                } else {
                    tracing::warn!(task_id, branch = %branch, "in_review but no open PR — re-dispatching");
                    if let Err(e) = task_manager
                        .update_task_status(&task.id, Status::Routed)
                        .await
                    {
                        tracing::warn!(task_id, err = %e, "failed to update status to routed");
                    } else {
                        // Reset per-cycle counters so stale counts from the previous cycle
                        // don't prematurely block the new attempt.
                        store_reset_counters(&Some(Arc::clone(store)), repo, task_id).await;
                    }
                }
                continue;
            }
            Err(e) => {
                tracing::warn!(task_id, branch = %branch, err = %e, "failed to get PR number");
                continue;
            }
        };

        // Store PR number for follow-up tasks
        store_set(
            &Some(Arc::clone(store)),
            repo,
            task_id,
            &[("pr_number", serde_json::json!(pr_number as i64))],
        )
        .await;

        // Get the last processed review timestamp to avoid re-processing the same reviews
        let last_review_ts =
            super::cleanup::store_get_field(store, repo, task_id, "last_review_ts")
                .await
                .unwrap_or_default();

        // Fetch PR reviews
        let reviews = match gh.get_pr_reviews(repo, pr_number).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(task_id, pr_number, err = %e, "failed to get PR reviews");
                continue;
            }
        };

        // Get all review comments for this PR (more efficient than per-review)
        let all_comments = match gh.get_pr_comments(repo, pr_number).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(task_id, pr_number, err = %e, "failed to get PR comments");
                continue;
            }
        };

        // Deduplicate reviews: keep only the latest review per reviewer.
        // GitHub returns reviews chronologically; when a reviewer submits
        // multiple reviews (e.g. CHANGES_REQUESTED then APPROVED), we only
        // care about the most recent one.
        let deduped_reviews = {
            let mut by_reviewer: std::collections::HashMap<
                String,
                &crate::github::types::GitHubReview,
            > = std::collections::HashMap::new();
            for review in &reviews {
                // Skip COMMENTED and DISMISSED — they don't express approval/rejection
                if review.state != "APPROVED" && review.state != "CHANGES_REQUESTED" {
                    continue;
                }
                let existing = by_reviewer.get(&review.user.login);
                let dominated = match existing {
                    Some(prev) => review.submitted_at > prev.submitted_at,
                    None => true,
                };
                if dominated {
                    by_reviewer.insert(review.user.login.clone(), review);
                }
            }
            by_reviewer
        };

        // Check aggregate state: if any reviewer still requests changes,
        // the PR is not fully approved.
        let any_changes_requested = deduped_reviews
            .values()
            .any(|r| r.state == "CHANGES_REQUESTED");
        let all_approved =
            !deduped_reviews.is_empty() && deduped_reviews.values().all(|r| r.state == "APPROVED");

        // Also check automated review comments on the PR (comment-based review workflow).
        // This is the primary review mechanism since GitHub doesn't allow reviewing your own PRs.
        let automated_review = gh
            .get_automated_review_status(repo, pr_number)
            .await
            .unwrap_or(None);

        let comment_approved = automated_review.as_deref() == Some("approve");
        let comment_changes_requested = automated_review.as_deref() == Some("changes_requested");

        // Handle fully-approved PRs (either via PR review API or comment-based review)
        if (all_approved || comment_approved)
            && auto_close_task
            && !comment_changes_requested
            && !any_changes_requested
        {
            // Check if the PR is already merged before marking done.
            // If not merged, attempt auto-merge so the PR doesn't get orphaned.
            let already_merged = match gh.is_pr_merged(repo, &branch).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(task_id, branch = %branch, err = %e, "merge check failed, skipping task this tick");
                    continue;
                }
            };

            if already_merged {
                tracing::info!(
                    task_id,
                    pr_number,
                    "PR already merged, marking task as done"
                );
                if let Err(e) = task_manager
                    .update_task_status(&task.id, Status::Done)
                    .await
                {
                    tracing::warn!(task_id, err = %e, "failed to update task status to done");
                }
            } else {
                // Check if PR has merge conflicts before attempting auto-merge
                let pr_details = gh.get_pr(repo, pr_number).await;
                let is_conflicting = pr_details
                    .as_ref()
                    .map(|pr| pr.mergeable == Some(false))
                    .unwrap_or(false);

                if is_conflicting {
                    let retries = super::cleanup::store_get_field(
                        store,
                        repo,
                        task_id,
                        "merge_conflict_retries",
                    )
                    .await
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                    if retries >= MAX_MERGE_CONFLICT_RETRIES {
                        tracing::error!(
                            task_id,
                            pr_number,
                            retries,
                            "PR approved but merge conflict retry limit reached — blocking for human review"
                        );
                        if let Err(e) = task_manager
                            .update_task_status(&task.id, Status::Blocked)
                            .await
                        {
                            tracing::warn!(task_id, err = %e, "failed to set Blocked");
                        }
                        continue;
                    }
                    tracing::info!(
                        task_id,
                        pr_number,
                        retries,
                        "PR approved but has merge conflicts — re-triggering review agent to rebase"
                    );
                    store_increment(
                        &Some(Arc::clone(store)),
                        repo,
                        task_id,
                        "merge_conflict_retries",
                    )
                    .await;
                    // Set NeedsReview — next tick re-triggers review agent
                    if let Err(e) = task_manager
                        .update_task_status(&task.id, Status::NeedsReview)
                        .await
                    {
                        tracing::warn!(task_id, err = %e, "failed to set NeedsReview for conflict retry");
                    }
                    continue;
                }

                tracing::info!(
                    task_id,
                    pr_number,
                    comment_approved,
                    "PR approved but not yet merged — attempting auto-merge"
                );
                // Get the task agent and model from store
                let task_agent = super::cleanup::store_get_field(store, repo, &task.id.0, "agent")
                    .await
                    .unwrap_or_else(|| "orch".to_string());
                let task_model = super::cleanup::store_get_field(store, repo, &task.id.0, "model")
                    .await
                    .unwrap_or_else(|| "unknown".to_string());
                if let Err(e) = auto_merge_pr(
                    &task,
                    &branch,
                    backend,
                    repo,
                    &task_agent,
                    &task_model,
                    task_manager,
                    store,
                )
                .await
                {
                    tracing::warn!(
                        task_id,
                        pr_number,
                        err = %e,
                        "auto-merge failed, keeping task in_review for next tick"
                    );
                }
                // auto_merge_pr handles all status transitions (Done / Routed / InReview)
            }
            continue;
        }

        // Process reviews that request changes (from either PR review API or comments)
        if !any_changes_requested && !comment_changes_requested {
            continue;
        }

        // Build review context for re-dispatch
        let mut review_context = String::new();
        let mut latest_review_ts = last_review_ts.clone();

        // Only process the latest CHANGES_REQUESTED reviews (already deduplicated)
        for review in deduped_reviews
            .values()
            .filter(|r| r.state == "CHANGES_REQUESTED")
        {
            // Track the latest review timestamp
            if review.submitted_at > latest_review_ts {
                latest_review_ts = review.submitted_at.clone();
            }

            // Skip if we've already processed this review
            if !last_review_ts.is_empty() && review.submitted_at <= last_review_ts {
                continue;
            }
            // Get comments for this review
            let review_comments: Vec<GitHubReviewComment> = all_comments
                .iter()
                .filter(|c| {
                    c.user.login == review.user.login && c.created_at >= review.submitted_at
                })
                .cloned()
                .collect();

            let pr_review = PullRequestReview {
                review: (*review).clone(),
                comments: review_comments.clone(),
            };

            // Add review info
            review_context.push_str(&format!(
                "### Review by @{} (CHANGES REQUESTED)\n",
                pr_review.review.user.login
            ));

            // Add overall review body if present
            if let Some(ref body) = pr_review.review.body {
                if !body.trim().is_empty() {
                    review_context.push_str(&format!("**Overall Feedback:** {}\n\n", body));
                }
            }

            // Add actionable comments
            let actionable = pr_review.actionable_comments();
            if !actionable.is_empty() {
                review_context.push_str("**Comments to address:**\n\n");
                for comment in actionable {
                    review_context.push_str(&format!(
                        "#### File: `{}` (line {})\n",
                        comment.path,
                        comment.line.map(|l| l.to_string()).unwrap_or_default()
                    ));

                    if let Some(ref diff_hunk) = comment.diff_hunk {
                        review_context.push_str("```diff\n");
                        review_context.push_str(diff_hunk);
                        review_context.push_str("\n```\n\n");
                    }

                    review_context.push_str(&format!("> {}\n\n", comment.body));
                }
            }
        }

        // Also include comment-based review feedback (from "Automated Review — Changes Requested" comments)
        if comment_changes_requested && review_context.is_empty() {
            // The PR review API had no changes, but the comment-based review does.
            // Fetch the latest "Automated Review — Changes Requested" comment body.
            let last_comment_ts =
                super::cleanup::store_get_field(store, repo, task_id, "last_comment_review_ts")
                    .await
                    .unwrap_or_default();
            if let Ok(comments) = gh.list_comments(repo, &pr_number.to_string()).await {
                for c in comments.iter().rev() {
                    if c.body
                        .starts_with("## Automated Review — Changes Requested")
                    {
                        // Skip if already processed
                        if !last_comment_ts.is_empty() && c.created_at <= last_comment_ts {
                            break;
                        }
                        // Extract the review body (skip the header line)
                        let body: String = c.body.lines().skip(1).collect::<Vec<_>>().join("\n");
                        review_context.push_str("### Automated Review (Changes Requested)\n\n");
                        review_context.push_str(&body);
                        review_context.push('\n');

                        // Track the timestamp
                        store_set(
                            &Some(Arc::clone(store)),
                            repo,
                            task_id,
                            &[(
                                "last_comment_review_ts",
                                serde_json::json!(c.created_at.clone()),
                            )],
                        )
                        .await;
                        break; // Only the latest
                    }
                }
            }
        }

        // Cap review context to avoid oversized values
        const MAX_REVIEW_CONTEXT_BYTES: usize = 16 * 1024;
        if review_context.len() > MAX_REVIEW_CONTEXT_BYTES {
            // Find safe UTF-8 char boundary
            let mut boundary = MAX_REVIEW_CONTEXT_BYTES;
            while !review_context.is_char_boundary(boundary) {
                boundary -= 1;
            }
            if let Some(pos) = review_context[..boundary].rfind('\n') {
                review_context.truncate(pos);
            } else {
                review_context.truncate(boundary);
            }
            review_context.push_str("\n... (review context truncated)");
        }

        // If we have new review feedback, store it and re-dispatch the task
        if !review_context.is_empty() {
            // Store the review context
            store_set(
                &Some(Arc::clone(store)),
                repo,
                task_id,
                &[(
                    "pr_review_context",
                    serde_json::json!(review_context.clone()),
                )],
            )
            .await;

            // Update the last review timestamp
            store_set(
                &Some(Arc::clone(store)),
                repo,
                task_id,
                &[(
                    "last_review_ts",
                    serde_json::json!(latest_review_ts.clone()),
                )],
            )
            .await;

            if let Err(e) = task_manager
                .update_task_status(&task.id, Status::Routed)
                .await
            {
                tracing::warn!(task_id, err = %e, "failed to set status to routed for re-dispatch");
            } else {
                tracing::info!(task_id, "re-dispatching task to address review feedback");
                // Reset per-cycle counters so transient failures from the previous review
                // cycle don't count against the budget for the next cycle.
                store_reset_counters(&Some(Arc::clone(store)), repo, task_id).await;
            }
        }
    }

    Ok(())
}

/// Run the review agent on a completed task and handle the outcome.
///
/// This is called after a task completes with status:done and a PR is created.
/// The review agent checks the changes and either approves (triggers auto-merge)
/// or requests changes (re-dispatches the original agent).
pub(crate) async fn review_and_merge(
    task: &ExternalTask,
    backend: &Arc<dyn ExternalBackend>,
    tmux: &Arc<TmuxManager>,
    repo: &str,
    router: &Arc<RwLock<Router>>,
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
) -> anyhow::Result<ReviewDecision> {
    // 2. Load worktree path, branch, agent from store
    let worktree = super::cleanup::store_get_field(store, repo, &task.id.0, "worktree").await;
    let branch = super::cleanup::store_get_field(store, repo, &task.id.0, "branch").await;
    let agent_summary = super::cleanup::store_get_field(store, repo, &task.id.0, "summary")
        .await
        .unwrap_or_default();

    let worktree_path = match worktree {
        Some(w) if !w.is_empty() => std::path::PathBuf::from(w),
        _ => {
            tracing::warn!(task_id = task.id.0, "no worktree found for review");
            return Ok(ReviewDecision::Failed("no worktree found".to_string()));
        }
    };

    let branch_name = match branch {
        Some(b) if !b.is_empty() => b,
        _ => {
            tracing::warn!(task_id = task.id.0, "no branch found for review");
            return Ok(ReviewDecision::Failed("no branch found".to_string()));
        }
    };

    // 2b. Verify an open PR exists before running the (expensive) review agent.
    // Check the store first (written by the runner right after PR creation) to
    // avoid GitHub's list-API cache race (~300 ms between PR creation and review).
    let stored_pr_number = super::cleanup::store_get_field(store, repo, &task.id.0, "pr_number")
        .await
        .and_then(|s| s.parse::<u64>().ok());
    let gh_check = GhHttp::new()?;
    let pr_number_early = if let Some(n) = stored_pr_number {
        tracing::info!(
            task_id = task.id.0,
            pr_number = n,
            branch = %branch_name,
            "open PR found in store, proceeding with review"
        );
        n
    } else {
        match gh_check.get_pr_number(repo, &branch_name).await {
            Ok(Some(n)) => {
                tracing::info!(
                    task_id = task.id.0,
                    pr_number = n,
                    branch = %branch_name,
                    "open PR found, proceeding with review"
                );
                n
            }
            Ok(None) => {
                // No open PR — check if branch has commits ahead of default branch.
                // If yes: agent forgot to create PR, try to create one and retry review.
                // If no: read-only task (e.g. code review), safe to mark done.
                let default_branch =
                    config::get("gh.default_branch").unwrap_or_else(|_| "main".to_string());
                let has_commits = tokio::process::Command::new("git")
                    .args([
                        "-C",
                        worktree_path.to_str().unwrap_or("."),
                        "rev-list",
                        "--count",
                        &format!("origin/{default_branch}..HEAD"),
                    ])
                    .output()
                    .await
                    .ok()
                    .and_then(|o| {
                        String::from_utf8_lossy(&o.stdout)
                            .trim()
                            .parse::<u64>()
                            .ok()
                    })
                    .unwrap_or(0)
                    > 0;

                if has_commits {
                    // Branch has unpushed or un-PR'd work — try to create a PR
                    tracing::warn!(
                        task_id = task.id.0,
                        branch = %branch_name,
                        "no open PR but branch has commits — attempting to create PR"
                    );
                    // Push first in case agent forgot
                    let _ = tokio::process::Command::new("git")
                        .args([
                            "-C",
                            worktree_path.to_str().unwrap_or("."),
                            "push",
                            "-u",
                            "origin",
                            &branch_name,
                        ])
                        .output()
                        .await;
                    // Try to create PR using GhHttp API first
                    let default_branch =
                        config::get("gh.default_branch").unwrap_or_else(|_| "main".to_string());
                    let task_ref = runner::git_ops::format_task_ref(&task.id.0);
                    let pr_body = format!(
                    "Resolves {task_ref}\n\nAuto-created by orch review gate (agent forgot to open PR)"
                );
                    let gh = GhHttp::new()?;
                    match gh
                        .create_pr(repo, &task.title, &pr_body, &branch_name, &default_branch)
                        .await
                    {
                        Ok(url) => {
                            // Extract PR number from URL and update store so subsequent
                            // review cycles check the correct PR (not a stale pr_number).
                            if let Some(pr_num) = url.rsplit('/').next() {
                                let pr_num_i64 = pr_num.parse::<i64>().unwrap_or(0);
                                store_set(
                                    &Some(Arc::clone(store)),
                                    repo,
                                    &task.id.0,
                                    &[("pr_number", serde_json::json!(pr_num_i64))],
                                )
                                .await;
                            }
                            tracing::info!(
                                task_id = task.id.0,
                                branch = %branch_name,
                                pr_url = %url,
                                "created missing PR via GhHttp — retrying review"
                            );
                            return Ok(ReviewDecision::Failed(
                                "created missing PR, retry".to_string(),
                            ));
                        }
                        Err(e) => {
                            let e_str = format!("{e}");
                            // If 422 "already exists", the PR was just created — GitHub's list
                            // API has brief eventual-consistency delays (observed ~300 ms).
                            // Retry get_pr_number after a short pause instead of trying to
                            // create again (which will also 422 and spin).
                            if e_str.contains("already exists") {
                                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                                if let Ok(Some(n)) =
                                    gh_check.get_pr_number(repo, &branch_name).await
                                {
                                    tracing::info!(
                                        task_id = task.id.0,
                                        pr_number = n,
                                        branch = %branch_name,
                                        "found existing PR after create_pr 422 — retrying review"
                                    );
                                    store_set(
                                        &Some(Arc::clone(store)),
                                        repo,
                                        &task.id.0,
                                        &[("pr_number", serde_json::json!(n as i64))],
                                    )
                                    .await;
                                    return Ok(ReviewDecision::Failed(
                                        "found existing PR after 422, retry".to_string(),
                                    ));
                                }
                            }
                            tracing::warn!(
                                task_id = task.id.0,
                                branch = %branch_name,
                                error = %e,
                                "create_pr failed via GhHttp, falling back to CLI"
                            );
                            // Fall back to CLI
                            let pr_result = tokio::process::Command::new("gh")
                                .args([
                                    "pr",
                                    "create",
                                    "--repo",
                                    repo,
                                    "--head",
                                    &branch_name,
                                    "--title",
                                    &task.title,
                                    "--body",
                                    &pr_body,
                                ])
                                .current_dir(&worktree_path)
                                .output()
                                .await;
                            match pr_result {
                                Ok(o) if o.status.success() => {
                                    // gh pr create prints the PR URL to stdout
                                    let stdout = String::from_utf8_lossy(&o.stdout);
                                    if let Some(pr_num) = stdout.trim().rsplit('/').next() {
                                        let pr_num_i64 = pr_num.parse::<i64>().unwrap_or(0);
                                        store_set(
                                            &Some(Arc::clone(store)),
                                            repo,
                                            &task.id.0,
                                            &[("pr_number", serde_json::json!(pr_num_i64))],
                                        )
                                        .await;
                                    }
                                    tracing::info!(
                                        task_id = task.id.0,
                                        branch = %branch_name,
                                        "created missing PR via CLI — retrying review"
                                    );
                                    return Ok(ReviewDecision::Failed(
                                        "created missing PR, retry".to_string(),
                                    ));
                                }
                                Ok(o) => {
                                    let stderr = String::from_utf8_lossy(&o.stderr);
                                    // gh pr create prints "already exists:\nhttps://..." to stderr
                                    // when the PR already exists — extract URL and proceed.
                                    if stderr.contains("already exists") {
                                        if let Some(pr_url) = stderr
                                            .lines()
                                            .find(|l| l.trim().starts_with("https://"))
                                        {
                                            let pr_url = pr_url.trim();
                                            if let Some(pr_num) = pr_url
                                                .rsplit('/')
                                                .next()
                                                .and_then(|n| n.parse::<i64>().ok())
                                            {
                                                store_set(
                                                    &Some(Arc::clone(store)),
                                                    repo,
                                                    &task.id.0,
                                                    &[("pr_number", serde_json::json!(pr_num))],
                                                )
                                                .await;
                                            }
                                            tracing::info!(
                                                task_id = task.id.0,
                                                branch = %branch_name,
                                                pr_url = %pr_url,
                                                "PR already exists (from CLI stderr) — retrying review"
                                            );
                                            return Ok(ReviewDecision::Failed(
                                                "PR already exists, retry".to_string(),
                                            ));
                                        }
                                    }
                                    tracing::error!(
                                        task_id = task.id.0,
                                        branch = %branch_name,
                                        stderr = %stderr,
                                        "failed to create missing PR — work may be stuck"
                                    );
                                    let failures = store_increment(
                                        &Some(Arc::clone(store)),
                                        repo,
                                        &task.id.0,
                                        "pr_create_failures",
                                    )
                                    .await;
                                    if failures >= MAX_PR_CREATE_FAILURES {
                                        return Ok(ReviewDecision::Blocked(format!(
                                            "no PR, create failed {failures} times: {stderr}"
                                        )));
                                    }
                                    return Ok(ReviewDecision::Failed(format!(
                                        "no PR, create failed: {stderr}"
                                    )));
                                }
                                Err(e) => {
                                    tracing::error!(
                                        task_id = task.id.0,
                                        error = %e,
                                        "failed to run gh pr create"
                                    );
                                    let failures = store_increment(
                                        &Some(Arc::clone(store)),
                                        repo,
                                        &task.id.0,
                                        "pr_create_failures",
                                    )
                                    .await;
                                    if failures >= MAX_PR_CREATE_FAILURES {
                                        return Ok(ReviewDecision::Blocked(format!(
                                            "no PR, gh error {failures} times: {e}"
                                        )));
                                    }
                                    return Ok(ReviewDecision::Failed(format!(
                                        "no PR, gh error: {e}"
                                    )));
                                }
                            }
                        }
                    }
                } else {
                    // No PR and no commits — agent either failed or completed a read-only task.
                    let merged = match gh_check.is_pr_merged(repo, &branch_name).await {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!(task_id = task.id.0, branch = %branch_name, err = %e, "merge check failed, skipping task this tick");
                            return Ok(ReviewDecision::Failed(format!("merge check failed: {e}")));
                        }
                    };
                    if merged {
                        tracing::info!(
                            task_id = task.id.0,
                            branch = %branch_name,
                            "PR already merged, marking done"
                        );
                        if let Err(e) = task_manager
                            .update_task_status(&task.id, crate::backends::Status::Done)
                            .await
                        {
                            tracing::error!(task_id = task.id.0, err = %e, "update_task_status(Done) failed — task may be stuck in InReview");
                        }
                        return Ok(ReviewDecision::Skipped);
                    }

                    let last_error =
                        super::cleanup::store_get_field(store, repo, &task.id.0, "last_error")
                            .await
                            .unwrap_or_default();
                    let reason = if !agent_summary.is_empty() {
                        agent_summary.clone()
                    } else {
                        last_error.clone()
                    };

                    // If the task has exhausted all attempts, block it.
                    // Continuing to re-route would spin forever since max_attempts is already hit.
                    if last_error.contains("exceeded max attempts") {
                        tracing::warn!(
                            task_id = task.id.0,
                            branch = %branch_name,
                            "no PR and no commits after max attempts — marking blocked to stop loop"
                        );
                        if let Err(e) = task_manager
                            .update_task_status(&task.id, crate::backends::Status::Blocked)
                            .await
                        {
                            tracing::error!(task_id = task.id.0, err = %e, "update_task_status(Blocked) failed — task may be stuck in InReview");
                        }
                        return Ok(ReviewDecision::Skipped);
                    }

                    // If the PR creation failed with 422/head-invalid, the work
                    // is already merged into main. Mark done instead of looping.
                    if last_error.contains("422") && last_error.contains("head") {
                        tracing::info!(
                            task_id = task.id.0,
                            branch = %branch_name,
                            "no PR — 422/head-invalid means work already merged, marking done"
                        );
                        if let Err(e) = task_manager
                            .update_task_status(&task.id, crate::backends::Status::Done)
                            .await
                        {
                            tracing::error!(task_id = task.id.0, err = %e, "update_task_status(Done) failed");
                        }
                        return Ok(ReviewDecision::Skipped);
                    }

                    tracing::warn!(
                        task_id = task.id.0,
                        branch = %branch_name,
                        reason = %reason,
                        "no PR and no commits — re-routing for retry"
                    );
                    if let Err(e) = task_manager
                        .update_task_status(&task.id, crate::backends::Status::New)
                        .await
                    {
                        tracing::error!(task_id = task.id.0, err = %e, "update_task_status(New) failed — task may be stuck in InReview");
                    }
                    return Ok(ReviewDecision::Skipped);
                }
            }
            Err(e) => {
                tracing::warn!(
                    task_id = task.id.0,
                    branch = %branch_name,
                    error = %e,
                    "failed to check PR status"
                );
                return Ok(ReviewDecision::Failed(format!("PR check failed: {e}")));
            }
        }
    }; // end if let Some(stored_pr_number) else match

    // 3. Build diff context (rebase is handled by the review agent via prompt)
    let default_branch = config::get("gh.default_branch").unwrap_or_else(|_| "main".to_string());
    let git_diff = runner::context::build_git_diff(&worktree_path, &default_branch).await;
    let git_log = runner::context::build_git_log(&worktree_path, &default_branch).await;

    // 4. Build review prompt
    let review_prompt = runner::agent::build_review_prompt(
        task,
        &agent_summary,
        &git_diff,
        &git_log,
        &default_branch,
    );

    // 5. Pick review agent via round-robin, excluding the agent that did the work
    let task_agent = super::cleanup::store_get_field(store, repo, &task.id.0, "agent")
        .await
        .unwrap_or_default();
    let review_agent = {
        let mut r = router.write().await;
        let exclude = if task_agent.is_empty() {
            None
        } else {
            Some(task_agent.as_str())
        };
        r.next_round_robin_agent(exclude)
            .unwrap_or_else(|| "claude".to_string())
    };
    let review_model = get_model_for_complexity("review", &review_agent);

    tracing::info!(
        task_id = task.id.0,
        agent = %review_agent,
        model = %review_model,
        "spawning review agent"
    );

    // 6. Build agent invocation for review
    let review_task_id = format!("{}-review", task.id.0);
    let review_attempt_dir = crate::home::task_attempt_dir(repo, &review_task_id, 1)?;
    let output_file = review_attempt_dir.join("output.json");

    let git_name = config::get("git.name").unwrap_or_else(|_| format!("{review_agent}[bot]"));
    let git_email = config::get("git.email")
        .unwrap_or_else(|_| format!("{review_agent}[bot]@users.noreply.github.com"));

    let system_prompt = runner::agent::review_system_prompt();

    let invocation = runner::agent::AgentInvocation {
        agent: review_agent.clone(),
        model: Some(review_model.clone()),
        work_dir: worktree_path.clone(),
        system_prompt,
        agent_message: review_prompt,
        task_id: review_task_id.clone(),
        disallowed_tools: vec![],
        git_author_name: git_name,
        git_author_email: git_email,
        output_file: output_file.clone(),
        timeout_seconds: 600, // 10 minute timeout for review
        repo: repo.to_string(),
        attempt: 1,
    };

    // 7. Spawn review agent in tmux
    let session = match runner::agent::spawn_in_tmux(tmux, &invocation).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(task_id = task.id.0, error = %e, "failed to spawn review agent");
            return Ok(ReviewDecision::Failed(format!("spawn failed: {e}")));
        }
    };

    // 8. Wait for completion
    let poll_interval = std::time::Duration::from_secs(5);
    let timeout_duration = std::time::Duration::from_secs(600);

    let wait_result = tokio::time::timeout(
        timeout_duration,
        tmux.wait_for_completion(&session, poll_interval),
    )
    .await;

    match wait_result {
        Ok(Ok(_)) => {
            tracing::info!(task_id = task.id.0, "review agent completed");
            // Clean up tmux session on success
            let _ = tmux.kill_session(&session).await;
        }
        Ok(Err(e)) => {
            tracing::error!(task_id = task.id.0, error = %e, "review agent error");
            let _ = tmux.kill_session(&session).await;
            return Ok(ReviewDecision::Failed(format!("agent error: {e}")));
        }
        Err(_) => {
            tracing::error!(task_id = task.id.0, "review agent timed out");
            let _ = tmux.kill_session(&session).await;
            return Ok(ReviewDecision::Failed("timeout".to_string()));
        }
    }

    // 9. Read and parse response
    let raw_output = runner::response::read_output_file(&review_task_id, &output_file, repo);
    let agent_runner = runner::agents::get_runner(&review_agent);

    // Read exit code
    let exit_code = std::fs::read_to_string(review_attempt_dir.join("exit.txt"))
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .unwrap_or(-1);

    let stderr = std::fs::read_to_string(review_attempt_dir.join("stderr.txt")).unwrap_or_default();

    // Abort on hard agent errors (non-zero exit, rate limit, auth, etc.)
    if exit_code != 0 || raw_output.is_empty() {
        let err = agent_runner.classify_error(exit_code, &raw_output, &stderr);
        let err_str = err.to_string();
        // Put rate-limited agents in cooldown so the round-robin skips them next time.
        if err_str.contains("rate limit")
            || err_str.contains("usage limit")
            || err_str.contains("rate_limit")
        {
            tracing::warn!(
                task_id = task.id.0,
                agent = %review_agent,
                "review agent hit rate limit — adding to cooldown"
            );
            runner::response::record_agent_failure(&review_agent);
        }
        tracing::error!(task_id = task.id.0, error = %err, "review agent failed");
        return Ok(ReviewDecision::Failed(format!("agent error: {err}")));
    }

    // Stage 1: strip the agent-specific output envelope to get the review text.
    //
    // For opencode this unwraps the NDJSON stream; for claude/kimi it unwraps
    // the JSON content envelope. When parsing fails (e.g. opencode emits NDJSON
    // with no "text" events due to a format change) or the summary is empty
    // (the agent put a ReviewResponse JSON where AgentResponse.summary was
    // expected), fall back to the raw output and let Stage 2 handle it.
    let text_for_review = match agent_runner.parse_response(&raw_output) {
        Ok(p) if !p.response.summary.is_empty() => p.response.summary,
        Ok(_) => {
            // Envelope parsed but summary is empty — the agent likely
            // output a ReviewResponse JSON directly (no AgentResponse wrapper).
            tracing::debug!(
                task_id = task.id.0,
                agent = %review_agent,
                "review agent: empty summary after parse, falling back to raw output"
            );
            raw_output.clone()
        }
        Err(runner::agents::AgentError::InvalidResponse { .. }) => {
            // Unparseable envelope (e.g. opencode NDJSON format change).
            // Fall back; parse_review_from_output handles NDJSON directly.
            tracing::warn!(
                task_id = task.id.0,
                agent = %review_agent,
                "review agent: envelope parse failed (InvalidResponse), falling back to raw output"
            );
            raw_output.clone()
        }
        Err(e) => {
            // Hard error — rate limit, auth, model unavailable, etc.
            tracing::error!(task_id = task.id.0, error = %e, "review agent error");
            return Ok(ReviewDecision::Failed(format!("agent error: {e}")));
        }
    };

    // Stage 2: parse the ReviewResponse from the extracted text.
    // parse_review_from_output handles JSON, markdown code blocks, and NDJSON.
    let review_response = match runner::response::parse_review_from_output(&text_for_review) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(
                task_id = task.id.0,
                error = %e,
                output = %text_for_review.chars().take(300).collect::<String>(),
                "failed to parse review response"
            );
            return Ok(ReviewDecision::Failed(format!("parse error: {e}")));
        }
    };

    // 10. Build automated review comment for the PR (before moving fields)
    let review_notes_for_comment = review_response.notes.clone();

    // 11. Convert to ReviewDecision
    let decision = match review_response.decision.as_str() {
        "approve" => ReviewDecision::Approve,
        "request_changes" => ReviewDecision::RequestChanges {
            notes: review_response.notes,
            issues: review_response.issues,
        },
        _ => ReviewDecision::Failed(format!("unknown decision: {}", review_response.decision)),
    };

    tracing::info!(
        task_id = task.id.0,
        pr_number = pr_number_early,
        decision = ?decision,
        "review agent decision received"
    );

    // 12. Post automated review comment on the PR
    let gh = GhHttp::new()?;
    let pr_number = gh.get_pr_number(repo, &branch_name).await.ok().flatten();

    if let Some(pr_num) = pr_number {
        let pr_comment = match &decision {
            ReviewDecision::Approve => {
                format!(
                    "## Automated Review \u{2014} Approve\n\n{}",
                    review_notes_for_comment
                )
            }
            ReviewDecision::RequestChanges { notes, issues } => {
                let mut body = format!(
                    "## Automated Review \u{2014} Changes Requested\n\n{}\n",
                    notes
                );
                if !issues.is_empty() {
                    body.push_str("\n**Issues Found:**\n");
                    for issue in issues {
                        body.push_str(&format!(
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
                body
            }
            _ => String::new(),
        };

        if !pr_comment.is_empty() {
            // Append attribution footer with review agent and model
            let footer = format!(
                "\n\n---\n*Reviewed by {}[bot] via [Orch](https://github.com/gabrielkoerich/orch) using `{}`*",
                review_agent, review_model
            );
            let pr_comment_with_footer = format!("{}{}", pr_comment, footer);
            if let Err(e) = gh
                .add_comment(repo, &pr_num.to_string(), &pr_comment_with_footer)
                .await
            {
                tracing::warn!(
                    task_id = task.id.0,
                    pr_number = pr_num,
                    error = %e,
                    "failed to post automated review comment on PR"
                );
            }
        }
    }

    // 13. Handle the decision
    match decision {
        ReviewDecision::Approve => {
            // Use the same flag as the human-review path (review_open_prs).
            // Falls back to workflow.auto_close (common config key), then
            // workflow.auto_merge for backwards-compatibility.
            // Must match the fallback chain in EngineConfig::from_config().
            let auto_merge = config::get("workflow.auto_close_task_on_approval")
                .or_else(|_| config::get("workflow.auto_close"))
                .or_else(|_| config::get("workflow.auto_merge"))
                .map(|v| v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);

            if auto_merge {
                if let Err(e) = auto_merge_pr(
                    task,
                    &branch_name,
                    backend,
                    repo,
                    &review_agent,
                    &review_model,
                    task_manager,
                    store,
                )
                .await
                {
                    tracing::error!(
                        task_id = task.id.0,
                        pr_number = pr_number_early,
                        branch = %branch_name,
                        error = %e,
                        "auto-merge failed"
                    );
                    return Ok(ReviewDecision::Failed(format!("merge failed: {e}")));
                }
            }
            Ok(ReviewDecision::Approve)
        }
        ReviewDecision::RequestChanges {
            ref notes,
            ref issues,
        } => {
            handle_review_changes(
                task,
                notes,
                issues,
                backend,
                repo,
                pr_number_early,
                &review_agent,
                &review_model,
                task_manager,
                store,
            )
            .await?;
            Ok(decision)
        }
        _ => Ok(decision),
    }
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
    // 1. Get PR number from branch
    let gh = GhHttp::new()?;
    let pr_number = match gh.get_pr_number(repo, branch).await? {
        Some(n) => n,
        None => {
            anyhow::bail!("no open PR found for branch {}", branch);
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

    // 3. Re-trigger the review gate workflow so it picks up the approve comment
    if let Err(e) = gh.dispatch_workflow(repo, "orch-review.yml", branch).await {
        tracing::debug!(
            task_id = task.id.0,
            error = %e,
            "failed to dispatch orch-review workflow (may not exist yet)"
        );
    }

    // 4. Wait for CI checks to pass (poll up to 5 minutes)
    let max_wait = std::time::Duration::from_secs(300);
    let poll_interval = std::time::Duration::from_secs(15);
    let start = std::time::Instant::now();

    loop {
        let (state, total, passing, failing, pending) =
            gh.get_combined_status(repo, branch).await?;

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

        match state.as_str() {
            "success" => break,
            "failure" => {
                // CI already in terminal failure — increment counter and re-route or block
                let ci_failures = store_increment(
                    &Some(Arc::clone(store)),
                    repo,
                    &task.id.0,
                    "ci_merge_failures",
                )
                .await;
                if ci_failures >= MAX_CI_MERGE_FAILURES {
                    tracing::error!(
                        task_id = task.id.0,
                        pr_number,
                        ci_failures,
                        "CI failure limit reached — blocking for human intervention"
                    );
                    task_manager
                        .update_task_status(&task.id, Status::Blocked)
                        .await?;
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
                    store_reset_counters(&Some(Arc::clone(store)), repo, &task.id.0).await;
                }
                return Ok(());
            }
            _ => {
                // pending — wait up to max_wait
                if start.elapsed() >= max_wait {
                    let ci_failures = store_increment(
                        &Some(Arc::clone(store)),
                        repo,
                        &task.id.0,
                        "ci_merge_failures",
                    )
                    .await;
                    if ci_failures >= MAX_CI_MERGE_FAILURES {
                        tracing::error!(
                            task_id = task.id.0,
                            ci_failures,
                            "CI timeout limit reached — blocking for human intervention"
                        );
                        task_manager
                            .update_task_status(&task.id, Status::Blocked)
                            .await?;
                    } else {
                        tracing::warn!(
                            task_id = task.id.0,
                            ci_failures,
                            "CI checks still pending after timeout — re-routing to agent"
                        );
                        task_manager
                            .update_task_status(&task.id, Status::Routed)
                            .await?;
                        store_reset_counters(&Some(Arc::clone(store)), repo, &task.id.0).await;
                    }
                    return Ok(());
                }
            }
        }

        tokio::time::sleep(poll_interval).await;
    }

    tracing::info!(
        task_id = task.id.0,
        pr_number,
        branch = %branch,
        "merging PR"
    );

    // 5. Merge via gh CLI
    if let Err(e) = gh.merge_pr(repo, pr_number, true).await {
        let err_msg = e.to_string().to_lowercase();
        let is_conflict = err_msg.contains("405")
            || err_msg.contains("not mergeable")
            || err_msg.contains("merge conflict");

        if is_conflict {
            // Merge failed due to conflicts — attempt rebase in the worktree directly.
            // Do NOT re-trigger the full review cycle; that doesn't fix the conflict.
            let retries =
                super::cleanup::store_get_field(store, repo, &task.id.0, "merge_conflict_retries")
                    .await
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
            if retries >= MAX_MERGE_CONFLICT_RETRIES {
                tracing::error!(
                    task_id = task.id.0,
                    retries,
                    "merge conflict retry limit reached"
                );
                task_manager
                    .update_task_status(&task.id, Status::Blocked)
                    .await?;
                let comment = format!("Auto-merge failed after {} rebase attempts: {}", retries, e);
                let footer = format!(
                    "\n\n---\n*Commented by {}[bot] via [Orch](https://github.com/gabrielkoerich/orch) using `{}`*",
                    review_agent, review_model
                );
                let _ = gh
                    .add_comment(
                        repo,
                        &pr_number.to_string(),
                        &format!("{}{}", comment, footer),
                    )
                    .await;
                // Task is already Blocked — return Ok so the caller does not reset to NeedsReview.
                return Ok(());
            }

            // Try rebase in the worktree
            let worktree_path =
                super::cleanup::store_get_field(store, repo, &task.id.0, "worktree").await;
            if let Some(wt) = worktree_path {
                let wt_path = std::path::PathBuf::from(&wt);
                if wt_path.exists() {
                    tracing::info!(
                        task_id = task.id.0,
                        worktree = %wt,
                        "attempting rebase to resolve merge conflict"
                    );
                    let default_branch =
                        config::get("gh.default_branch").unwrap_or_else(|_| "main".to_string());
                    let rebase_result = tokio::process::Command::new("sh")
                        .arg("-c")
                        .arg(format!(
                            "cd '{}' && git fetch origin && git rebase origin/{default_branch} && git push --force-with-lease",
                            wt
                        ))
                        .output()
                        .await;

                    match rebase_result {
                        Ok(out) if out.status.success() => {
                            tracing::info!(
                                task_id = task.id.0,
                                "rebase succeeded — retrying merge"
                            );
                            store_increment(
                                &Some(Arc::clone(store)),
                                repo,
                                &task.id.0,
                                "merge_conflict_retries",
                            )
                            .await;
                            // Retry merge once after successful rebase
                            if let Err(merge_err) = gh.merge_pr(repo, pr_number, true).await {
                                tracing::error!(
                                    task_id = task.id.0,
                                    error = %merge_err,
                                    "merge still failed after rebase — blocking"
                                );
                                task_manager
                                    .update_task_status(&task.id, Status::Blocked)
                                    .await?;
                                // Task is already Blocked — return Ok so the caller does not reset to NeedsReview.
                                return Ok(());
                            }
                            // Merge succeeded after rebase — fall through to done
                            task_manager
                                .update_task_status(&task.id, Status::Done)
                                .await?;
                            if let Err(ce) = cleanup_task_worktree(&task.id.0, repo, store).await {
                                tracing::warn!(task_id = task.id.0, err = %ce, "post-merge cleanup failed");
                            }
                            return Ok(());
                        }
                        Ok(out) => {
                            let stderr = String::from_utf8_lossy(&out.stderr);
                            tracing::error!(
                                task_id = task.id.0,
                                stderr = %stderr,
                                "rebase failed — blocking for human review"
                            );
                        }
                        Err(io_err) => {
                            tracing::error!(
                                task_id = task.id.0,
                                error = %io_err,
                                "rebase command error — blocking for human review"
                            );
                        }
                    }
                }
            }

            // Rebase failed or no worktree — block
            task_manager
                .update_task_status(&task.id, Status::Blocked)
                .await?;
            let comment = format!(
                "Auto-merge failed (merge conflict, rebase unsuccessful): {}",
                e
            );
            let footer = format!(
                "\n\n---\n*Commented by {}[bot] via [Orch](https://github.com/gabrielkoerich/orch) using `{}`*",
                review_agent, review_model
            );
            let _ = gh
                .add_comment(
                    repo,
                    &pr_number.to_string(),
                    &format!("{}{}", comment, footer),
                )
                .await;
            return Err(e);
        }

        // Non-conflict merge failure (permissions, branch protection, etc.)
        tracing::error!(task_id = task.id.0, error = %e, "merge failed — blocking for human review");
        task_manager
            .update_task_status(&task.id, Status::Blocked)
            .await?;
        let comment = format!("Auto-merge failed: {}", e);
        let footer = format!(
            "\n\n---\n*Commented by {}[bot] via [Orch](https://github.com/gabrielkoerich/orch) using `{}`*",
            review_agent, review_model
        );
        let _ = gh
            .add_comment(
                repo,
                &pr_number.to_string(),
                &format!("{}{}", comment, footer),
            )
            .await;
        return Err(e);
    }

    // 6. Update status to done (auto-closes the issue via backend)
    // Reset CI failure counter on successful merge.
    store_set(
        &Some(Arc::clone(store)),
        repo,
        &task.id.0,
        &[("ci_merge_failures", serde_json::json!(0))],
    )
    .await;
    task_manager
        .update_task_status(&task.id, Status::Done)
        .await?;

    // 7. Cleanup worktree + branches + pull main immediately
    // (can't rely on sync_tick because auto-close makes list_by_status(Done) miss it)
    if let Err(e) = cleanup_task_worktree(&task.id.0, repo, store).await {
        tracing::warn!(task_id = task.id.0, err = %e, "post-merge cleanup failed");
    }

    // 8. Post final comment on the PR
    let comment = "✅ PR reviewed, approved, and merged.";
    let footer = format!(
        "\n\n---\n*Reviewed by {}[bot] via [Orch](https://github.com/gabrielkoerich/orch) using `{}`*",
        review_agent, review_model
    );
    let _ = gh
        .add_comment(
            repo,
            &pr_number.to_string(),
            &format!("{}{}", comment, footer),
        )
        .await;

    tracing::info!(task_id = task.id.0, "auto-merge completed");

    Ok(())
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
    let review_cycles: u32 =
        super::cleanup::store_get_field(store, repo, &task.id.0, "review_cycles")
            .await
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

    let max_cycles: u32 = config::get("workflow.max_review_cycles")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);

    let gh = GhHttp::new()?;
    let pr_num_str = pr_number.to_string();

    if review_cycles >= max_cycles {
        // Too many review cycles — escalate to human
        tracing::warn!(
            task_id = task.id.0,
            review_cycles,
            max_cycles,
            "max review cycles exceeded, blocking for human review"
        );
        task_manager
            .update_task_status(&task.id, Status::Blocked)
            .await?;
        let escalation = format!(
            "🔍 Review agent requested changes after {} cycles. Escalating to human.\n\n**Review Notes:**\n{}",
            review_cycles, notes
        );
        let footer = format!(
            "\n\n---\n*Commented by {}[bot] via [Orch](https://github.com/gabrielkoerich/orch) using `{}`*",
            review_agent, review_model
        );
        if let Err(e) = gh
            .add_comment(repo, &pr_num_str, &format!("{}{}", escalation, footer))
            .await
        {
            tracing::warn!(task_id = task.id.0, pr_number, err = %e, "failed to post escalation comment on PR");
        }
        return Ok(());
    }

    // 2. Post review feedback as comment on the PR
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

    let footer = format!(
        "\n\n---\n*Reviewed by {}[bot] via [Orch](https://github.com/gabrielkoerich/orch) using `{}`*",
        review_agent, review_model
    );
    if let Err(e) = gh
        .add_comment(repo, &pr_num_str, &format!("{}{}", comment, footer))
        .await
    {
        tracing::warn!(task_id = task.id.0, pr_number, err = %e, "failed to post review comment on PR");
    }

    // 3. Store review context.
    //    The template (prompts/agent_message.md) already wraps PR_REVIEW_CONTEXT with
    //    "A reviewer has requested changes on your PR. Please address the following feedback:",
    //    so we store the comment body directly to avoid duplicating that header in the prompt.
    store_set(
        &Some(Arc::clone(store)),
        repo,
        &task.id.0,
        &[
            (
                "review_cycles",
                serde_json::json!((review_cycles + 1) as i64),
            ),
            ("pr_review_context", serde_json::json!(comment.clone())),
        ],
    )
    .await;

    // 4. Skip LLM re-classification and dispatch directly — the agent, model, and
    //    worktree from the first routing cycle are reused. Setting Routed (not New)
    //    bypasses the router so the existing store_route result is preserved.
    //    This is consistent with review_open_prs which also sets Routed for
    //    human-requested changes.
    task_manager
        .update_task_status(&task.id, Status::Routed)
        .await?;

    tracing::info!(
        task_id = task.id.0,
        review_cycles = review_cycles + 1,
        pr_number,
        "re-dispatching task (skipping re-route) to address review feedback on same PR"
    );

    Ok(())
}

/// Get the model for a given complexity and agent.
pub(crate) fn get_model_for_complexity(complexity: &str, agent: &str) -> String {
    // Read from config model_map
    let config_key = format!("model_map.{}.{}", complexity, agent);
    match config::get(&config_key) {
        Ok(model) => model,
        Err(_) => {
            // Defaults
            match agent {
                "claude" => match complexity {
                    "simple" => "haiku".to_string(),
                    "medium" => "sonnet".to_string(),
                    "complex" | "review" => "sonnet".to_string(),
                    _ => "sonnet".to_string(),
                },
                "codex" => match complexity {
                    "simple" => "gpt-5.1-codex-mini".to_string(),
                    "medium" | "review" => "gpt-5.2".to_string(),
                    "complex" => "gpt-5.3-codex".to_string(),
                    _ => "gpt-5.2".to_string(),
                },
                _ => "sonnet".to_string(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::types::{GitHubReview, GitHubReviewComment, GitHubUser, PullRequestReview};

    #[test]
    fn test_pull_request_review_requests_changes() {
        let review = PullRequestReview {
            review: GitHubReview {
                id: 1,
                user: GitHubUser {
                    login: "reviewer".to_string(),
                },
                body: Some("Please fix".to_string()),
                state: "CHANGES_REQUESTED".to_string(),
                html_url: None,
                submitted_at: "2024-01-01T00:00:00Z".to_string(),
                commit_id: None,
            },
            comments: vec![],
        };
        assert!(review.requests_changes());
    }

    #[test]
    fn test_pull_request_review_does_not_request_changes() {
        let review = PullRequestReview {
            review: GitHubReview {
                id: 1,
                user: GitHubUser {
                    login: "reviewer".to_string(),
                },
                body: Some("LGTM".to_string()),
                state: "APPROVED".to_string(),
                html_url: None,
                submitted_at: "2024-01-01T00:00:00Z".to_string(),
                commit_id: None,
            },
            comments: vec![],
        };
        assert!(!review.requests_changes());
    }

    #[test]
    fn test_pull_request_review_actionable_comments_filters_empty_and_replies() {
        let review = PullRequestReview {
            review: GitHubReview {
                id: 1,
                user: GitHubUser {
                    login: "reviewer".to_string(),
                },
                body: None,
                state: "CHANGES_REQUESTED".to_string(),
                html_url: None,
                submitted_at: "2024-01-01T00:00:00Z".to_string(),
                commit_id: None,
            },
            comments: vec![
                GitHubReviewComment {
                    id: 1,
                    user: GitHubUser {
                        login: "reviewer".to_string(),
                    },
                    body: "Fix this issue".to_string(),
                    path: "src/main.rs".to_string(),
                    line: Some(10),
                    original_line: Some(10),
                    commit_id: "abc".to_string(),
                    original_commit_id: "abc".to_string(),
                    html_url: "url".to_string(),
                    created_at: "2024-01-01T00:00:00Z".to_string(),
                    updated_at: "2024-01-01T00:00:00Z".to_string(),
                    in_reply_to_id: None,
                    diff_hunk: Some(
                        "@@ -8,5 +8,5 @@ fn main() {\n-    let x = 1;\n+    let x = 2;".to_string(),
                    ),
                },
                GitHubReviewComment {
                    id: 2,
                    user: GitHubUser {
                        login: "reviewer".to_string(),
                    },
                    body: "".to_string(),
                    path: "src/lib.rs".to_string(),
                    line: Some(20),
                    original_line: Some(20),
                    commit_id: "abc".to_string(),
                    original_commit_id: "abc".to_string(),
                    html_url: "url".to_string(),
                    created_at: "2024-01-01T00:00:00Z".to_string(),
                    updated_at: "2024-01-01T00:00:00Z".to_string(),
                    in_reply_to_id: None,
                    diff_hunk: None,
                },
                GitHubReviewComment {
                    id: 3,
                    user: GitHubUser {
                        login: "reviewer".to_string(),
                    },
                    body: "Reply to this".to_string(),
                    path: "src/lib.rs".to_string(),
                    line: Some(30),
                    original_line: Some(30),
                    commit_id: "abc".to_string(),
                    original_commit_id: "abc".to_string(),
                    html_url: "url".to_string(),
                    created_at: "2024-01-01T00:00:00Z".to_string(),
                    updated_at: "2024-01-01T00:00:00Z".to_string(),
                    in_reply_to_id: Some(1),
                    diff_hunk: None,
                },
            ],
        };
        let actionable = review.actionable_comments();
        assert_eq!(actionable.len(), 1);
        assert_eq!(actionable[0].id, 1);
        assert_eq!(actionable[0].body, "Fix this issue");
        assert_eq!(actionable[0].path, "src/main.rs");
        assert_eq!(
            actionable[0].diff_hunk.as_ref().unwrap(),
            "@@ -8,5 +8,5 @@ fn main() {\n-    let x = 1;\n+    let x = 2;"
        );
    }

    #[test]
    fn test_get_model_for_complexity_returns_nonempty() {
        assert!(!get_model_for_complexity("simple", "claude").is_empty());
        assert!(!get_model_for_complexity("medium", "claude").is_empty());
        assert!(!get_model_for_complexity("complex", "claude").is_empty());
        assert!(!get_model_for_complexity("review", "claude").is_empty());
    }

    #[test]
    fn test_get_model_for_complexity_unknown_agent() {
        let model = get_model_for_complexity("simple", "unknown_agent_xyz");
        assert!(!model.is_empty());
    }

    /// Regression test: handle_review_changes must set status to Routed (not New).
    ///
    /// Before this fix, the function set status to New, which caused the task to
    /// re-enter the LLM router on the next tick. This:
    ///   1. Wasted an LLM routing call per review cycle.
    ///   2. Risked the router selecting a different agent/model, discarding the
    ///      existing store_route result that tracked agent, complexity, and skills.
    ///   3. Was inconsistent with review_open_prs which correctly uses Routed for
    ///      human-requested changes.
    ///
    /// The fix sets status to Routed so the dispatch tick picks up the task
    /// directly without going through the routing phase, preserving the
    /// existing agent/model assignment.
    #[tokio::test]
    async fn handle_review_changes_sets_routed_not_new() {
        use crate::backends::{ExternalId, ExternalTask, Mention};
        use crate::engine::tasks::TaskManager;
        use crate::store::{NewTask, TaskStatus, TaskStore};
        use async_trait::async_trait;

        struct NoopBackend;
        #[async_trait]
        impl crate::backends::ExternalBackend for NoopBackend {
            fn name(&self) -> &str {
                "noop"
            }
            async fn create_task(
                &self,
                _t: &str,
                _b: &str,
                _l: &[String],
            ) -> anyhow::Result<ExternalId> {
                Ok(ExternalId("new".into()))
            }
            async fn get_task(&self, id: &ExternalId) -> anyhow::Result<ExternalTask> {
                Ok(ExternalTask {
                    id: id.clone(),
                    title: "t".into(),
                    body: "".into(),
                    state: "open".into(),
                    labels: vec![],
                    author: "bot".into(),
                    created_at: "2026-01-01T00:00:00Z".into(),
                    updated_at: "2026-01-01T00:00:00Z".into(),
                    url: "".into(),
                })
            }
            async fn list_by_status(&self, _s: Status) -> anyhow::Result<Vec<ExternalTask>> {
                Ok(vec![])
            }
            async fn list_routable(&self) -> anyhow::Result<Vec<ExternalTask>> {
                Ok(vec![])
            }
            async fn post_comment(&self, _id: &ExternalId, _b: &str) -> anyhow::Result<()> {
                Ok(())
            }
            async fn set_labels(&self, _id: &ExternalId, _l: &[String]) -> anyhow::Result<()> {
                Ok(())
            }
            async fn remove_label(&self, _id: &ExternalId, _l: &str) -> anyhow::Result<()> {
                Ok(())
            }
            async fn get_sub_issues(&self, _id: &ExternalId) -> anyhow::Result<Vec<ExternalId>> {
                Ok(vec![])
            }
            async fn create_sub_task(
                &self,
                _p: &ExternalId,
                _t: &str,
                _b: &str,
                _l: &[String],
            ) -> anyhow::Result<ExternalId> {
                Ok(ExternalId("child".into()))
            }
            async fn ensure_status_label(&self, _l: &str) -> anyhow::Result<()> {
                Ok(())
            }
            async fn has_open_issue_with_title(&self, _t: &str, _l: &str) -> anyhow::Result<bool> {
                Ok(false)
            }
            async fn health_check(&self) -> anyhow::Result<()> {
                Ok(())
            }
            async fn is_pr_merged(&self, _b: &str) -> anyhow::Result<bool> {
                Ok(false)
            }
            async fn get_authenticated_user(&self) -> anyhow::Result<Option<String>> {
                Ok(Some("bot".into()))
            }
            async fn get_mentions(&self, _s: &str) -> anyhow::Result<Vec<Mention>> {
                Ok(vec![])
            }
            async fn update_status(&self, _id: &ExternalId, _s: Status) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let repo = "owner/repo";

        // Create an internal task in InReview status (simulates the state when the
        // review agent has requested changes).
        let task_id_num = store
            .create(&NewTask {
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
            })
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
            title: "Implement feature X".to_string(),
            body: "body".to_string(),
            state: "open".to_string(),
            labels: vec![],
            author: "".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: "".to_string(),
        };

        // Call handle_review_changes. The GhHttp::add_comment call inside will fail
        // (no real GitHub token in tests) but the error is silently ignored — only
        // the status update and store writes matter for this test.
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

        // The function must succeed even when the comment post fails.
        assert!(
            result.is_ok(),
            "handle_review_changes returned Err: {result:?}"
        );

        // Status must be Routed (not New) — the task should skip the LLM router and
        // be picked up directly by tick_dispatch_tasks.
        let updated = store.get(task_id_num).await.unwrap();
        assert_eq!(
            updated.status,
            TaskStatus::Routed,
            "handle_review_changes must set Routed, not New, to reuse the existing \
             agent/model assignment and skip unnecessary LLM re-classification"
        );

        // pr_review_context must be stored for injection into the agent prompt.
        // It must NOT contain the "A reviewer has requested changes" prefix — that
        // prefix is already provided by the agent_message.md template wrapper so
        // storing it here would duplicate the header in the agent prompt.
        assert!(
            !updated.pr_review_context.is_empty(),
            "pr_review_context must be stored so the re-dispatched agent sees the feedback"
        );
        assert!(
            !updated
                .pr_review_context
                .starts_with("A reviewer has requested changes"),
            "pr_review_context must NOT start with the template header — \
             agent_message.md already adds it, double-prefix confuses the agent"
        );
        // The stored context must include the actual review content.
        assert!(
            updated
                .pr_review_context
                .contains("Please fix the type error on line 42"),
            "pr_review_context must contain the review notes passed to handle_review_changes"
        );

        // review_cycles must have been incremented.
        assert_eq!(
            updated.review_cycles, 1,
            "review_cycles must be incremented on each review change request"
        );
    }
}
