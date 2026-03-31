//! PR review polling — `review_open_prs`.
//!
//! Extracted from `engine/review.rs`. Monitors tasks in `InReview` status,
//! checks for human review feedback, and re-dispatches agents when changes
//! are requested.

use crate::backends::{ExternalBackend, Status};
use crate::config;
use crate::engine::auto_merge::{dedup_reviews, handle_review_changes, MAX_MERGE_CONFLICT_RETRIES};
use crate::engine::tasks::TaskManager;
use crate::engine::EngineConfig;
use crate::github::http::GhHttp;
use crate::github::types::{GitHubReviewComment, PullRequestReview};
use crate::store::TaskStore;
use crate::store::{opt_store_get_task, store_increment, store_reset_failure_counters, store_set};
use std::sync::Arc;

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
    let mut in_review_tasks = {
        let store_tasks = store
            .list_external_by_status(repo, crate::store::TaskStatus::InReview)
            .await?;
        if store.has_external_tasks(repo).await {
            store_tasks
                .iter()
                .map(crate::engine::tasks::store_task_to_external)
                .collect()
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
        tracing::debug!(count = 0, "checking in_review tasks for PR reviews");
        return Ok(());
    }

    let auto_close_task = config.auto_close_task_on_approval;

    tracing::info!(
        count = in_review_tasks.len(),
        "checking in_review tasks for PR reviews"
    );

    let gh = GhHttp::new()?;

    for task in in_review_tasks {
        let task_id = &task.id.0;

        // Skip tasks currently being processed by the main tick.
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

        let stored_task = opt_store_get_task(&Some(Arc::clone(store)), repo, task_id).await;
        let stored_task = match stored_task {
            Some(t) => t,
            None => {
                tracing::warn!(
                    task_id,
                    "in_review task missing from store — setting needs_review"
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

        let branch = if stored_task.branch.is_empty() {
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
        } else {
            stored_task.branch.clone()
        };

        // Get PR number from branch
        let pr_number = match gh.get_pr_number(repo, &branch).await {
            Ok(Some(n)) => n,
            Ok(None) => {
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
                    // No PR and not merged — re-route with circuit breaker to prevent loops.
                    // Use a dedicated counter persisted in the store so repeated reroutes
                    // across separate runs are counted. This prevents tasks from looping
                    // indefinitely through in_review → routed → in_progress → needs_review
                    // cycles.
                    let max_reroutes: u32 = config::get("workflow.max_reroute_attempts")
                        .ok()
                        .and_then(|s| s.parse().ok())
                        .or_else(|| {
                            config::get("workflow.max_attempts")
                                .ok()
                                .and_then(|s| s.parse().ok())
                        })
                        .unwrap_or(3);

                    let reroutes =
                        store_increment(&Some(Arc::clone(store)), repo, task_id, "no_pr_reroutes")
                            .await;

                    if reroutes as u32 >= max_reroutes {
                        tracing::error!(
                            task_id,
                            reroutes,
                            max_reroutes,
                            "reached max reroute attempts for in_review no-PR — blocking for human review"
                        );
                        // Clear agent/model and record an explanatory last_error
                        let msg = format!(
                            "no PR or code changes after {}/{} reroute attempts",
                            reroutes, max_reroutes
                        );
                        store_set(
                            &Some(Arc::clone(store)),
                            repo,
                            task_id,
                            &[
                                ("agent", serde_json::json!(null)),
                                ("model", serde_json::json!(null)),
                                ("last_error", serde_json::json!(msg)),
                            ],
                        )
                        .await;
                        if let Err(e) = task_manager
                            .update_task_status(&task.id, Status::Blocked)
                            .await
                        {
                            tracing::warn!(task_id, err = %e, "failed to update status to blocked");
                        }
                    } else {
                        tracing::warn!(
                            task_id,
                            branch = %branch,
                            reroutes,
                            max_reroutes,
                            "in_review but no open PR — re-dispatching"
                        );
                        if let Err(e) = task_manager
                            .update_task_status(&task.id, Status::Routed)
                            .await
                        {
                            tracing::warn!(task_id, err = %e, "failed to update status to routed");
                        }
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
        let last_review_ts = stored_task.last_review_ts.clone();

        // Fetch PR reviews
        let reviews = match gh.get_pr_reviews(repo, pr_number).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(task_id, pr_number, err = %e, "failed to get PR reviews");
                continue;
            }
        };

        // Get all review comments for this PR
        let all_comments = match gh.get_pr_comments(repo, pr_number).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(task_id, pr_number, err = %e, "failed to get PR comments");
                continue;
            }
        };

        // Deduplicate reviews: keep only the latest per reviewer.
        let deduped_reviews = dedup_reviews(&reviews);

        let any_changes_requested = deduped_reviews
            .values()
            .any(|r| r.state == "CHANGES_REQUESTED");
        let all_approved =
            !deduped_reviews.is_empty() && deduped_reviews.values().all(|r| r.state == "APPROVED");

        // Also check automated review comments on the PR (comment-based review workflow).
        let automated_review = gh
            .get_automated_review_status(repo, pr_number)
            .await
            .unwrap_or(None);

        let comment_approved = automated_review.as_deref() == Some("approve");
        let comment_changes_requested = automated_review.as_deref() == Some("changes_requested");

        // Handle fully-approved PRs
        if (all_approved || comment_approved)
            && auto_close_task
            && !comment_changes_requested
            && !any_changes_requested
        {
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
                let pr_details = gh.get_pr(repo, pr_number).await;
                let is_conflicting = pr_details
                    .as_ref()
                    .map(|pr| pr.mergeable == Some(false))
                    .unwrap_or(false);

                if is_conflicting {
                    let retries = stored_task.merge_conflict_retries as u64;
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
                let task_agent = stored_task
                    .agent
                    .clone()
                    .unwrap_or_else(|| "orch".to_string());
                let task_model = stored_task
                    .model
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string());
                if let Err(e) = crate::engine::auto_merge::auto_merge_pr(
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
            }
            continue;
        }

        // Process reviews that request changes
        if !any_changes_requested && !comment_changes_requested {
            continue;
        }

        // Build review context for re-dispatch
        let mut review_context = String::new();
        let mut latest_review_ts = last_review_ts.clone();

        for review in deduped_reviews
            .values()
            .filter(|r| r.state == "CHANGES_REQUESTED")
        {
            if review.submitted_at > latest_review_ts {
                latest_review_ts = review.submitted_at.clone();
            }

            // Skip if we've already processed this review
            if !last_review_ts.is_empty() && review.submitted_at <= last_review_ts {
                continue;
            }
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

            review_context.push_str(&format!(
                "### Review by @{} (CHANGES REQUESTED)\n",
                pr_review.review.user.login
            ));

            if let Some(ref body) = pr_review.review.body {
                if !body.trim().is_empty() {
                    review_context.push_str(&format!("**Overall Feedback:** {}\n\n", body));
                }
            }

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

        // Also include comment-based review feedback.
        // Collect the new comment timestamp but do NOT persist it yet — we only
        // save it after handle_review_changes() succeeds so that a transient
        // failure does not silently drop the review on the next poll.
        let mut new_comment_review_ts: Option<String> = None;
        if comment_changes_requested {
            let last_comment_ts = stored_task.last_comment_review_ts.clone();
            if let Ok(comments) = gh.list_comments(repo, &pr_number.to_string()).await {
                for c in comments.iter().rev() {
                    if c.body
                        .starts_with("## Automated Review \u{2014} Changes Requested")
                    {
                        if !last_comment_ts.is_empty() && c.created_at <= last_comment_ts {
                            break;
                        }
                        let body: String = c.body.lines().skip(1).collect::<Vec<_>>().join("\n");
                        review_context.push_str("### Automated Review (Changes Requested)\n\n");
                        review_context.push_str(&body);
                        review_context.push('\n');

                        // Capture the timestamp; persist only on success below.
                        new_comment_review_ts = Some(c.created_at.clone());
                        break;
                    }
                }
            }
        }

        // Cap review context to avoid oversized values
        const MAX_REVIEW_CONTEXT_BYTES: usize = 16 * 1024;
        if review_context.len() > MAX_REVIEW_CONTEXT_BYTES {
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

        // If we have new review feedback, re-dispatch the task.
        // Timestamps are only persisted after handle_review_changes() succeeds
        // so that a transient failure does not permanently skip this review on
        // the next poll tick.
        if !review_context.is_empty() {
            let task_agent = stored_task
                .agent
                .clone()
                .unwrap_or_else(|| "orch".to_string());
            let task_model = stored_task
                .model
                .clone()
                .unwrap_or_else(|| "unknown".to_string());
            let review_cycles = stored_task.review_cycles.max(0) as u32;
            let max_cycles: u32 = crate::config::get("workflow.max_review_cycles")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2);

            if let Err(e) = handle_review_changes(
                &task,
                &review_context,
                &[],
                backend,
                repo,
                pr_number,
                &task_agent,
                &task_model,
                task_manager,
                store,
            )
            .await
            {
                tracing::warn!(task_id, err = %e, "failed to handle review feedback");
                // Do NOT update timestamps — leave them unchanged so the same
                // review is retried on the next poll tick.
            } else {
                // Success: advance the watermark timestamps so we don't
                // re-process the same reviews on the next tick.
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

                if let Some(ts) = new_comment_review_ts {
                    store_set(
                        &Some(Arc::clone(store)),
                        repo,
                        task_id,
                        &[("last_comment_review_ts", serde_json::json!(ts))],
                    )
                    .await;
                }

                if review_cycles < max_cycles {
                    tracing::info!(task_id, "re-dispatching task to address review feedback");
                    store_reset_failure_counters(&Some(Arc::clone(store)), repo, task_id).await;
                }
            }
        }
    }

    Ok(())
}
