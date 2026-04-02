//! PR review polling — `review_open_prs`.
//!
//! Extracted from `engine/review.rs`. Monitors tasks in `InReview` status,
//! checks for human review feedback, and re-dispatches agents when changes
//! are requested.
//!
//! ## Batching strategy
//!
//! Instead of making 4–5 individual REST calls per in-review task, the loop
//! is split into phases:
//!
//! 1. Validate tasks and load stored data (no API calls).
//! 2. Resolve missing PR numbers concurrently via REST (rare after first tick).
//! 3. Handle tasks with no open PR (merged / reroute logic).
//! 4. Fetch all PR review data in **one** GraphQL query for all tasks at once.
//! 5. Build a collaborator cache from batch issue comments (unique users only).
//! 6. Process each task using the pre-fetched batch data.
//!
//! With N in-review tasks the call count drops from ~5N REST calls to:
//! - M `get_pr_number` REST calls (M ≈ 0 after first tick per task)
//! - 1 GraphQL batch call
//! - K `is_collaborator` REST calls (K = unique automated-review users ≪ N)

use crate::backends::{ExternalBackend, Status};
use crate::config;
use crate::engine::auto_merge::{dedup_reviews, handle_review_changes, MAX_MERGE_CONFLICT_RETRIES};
use crate::engine::tasks::TaskManager;
use crate::engine::EngineConfig;
use crate::github::http::GhHttp;
use crate::github::types::{GitHubComment, GitHubReviewComment, PullRequestReview};
use crate::store::TaskStore;
use crate::store::{opt_store_get_task, store_increment, store_reset_failure_counters, store_set};
use std::collections::HashMap;
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

    // ─── Phase 1: Validate tasks and separate by PR number availability ───────

    struct ReadyTask {
        task: crate::backends::ExternalTask,
        stored: crate::store::Task,
        branch: String,
        /// `Some` = already stored in DB, `None` = needs REST lookup.
        pr_number: Option<u64>,
    }

    let mut ready_tasks: Vec<ReadyTask> = Vec::new();

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

        let stored = opt_store_get_task(&Some(Arc::clone(store)), repo, task_id).await;
        let stored = match stored {
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

        let branch = if stored.branch.is_empty() {
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
            stored.branch.clone()
        };

        let stored_pr_number = stored.pr_number.map(|n| n as u64);
        ready_tasks.push(ReadyTask {
            task,
            stored,
            branch,
            pr_number: stored_pr_number,
        });
    }

    if ready_tasks.is_empty() {
        return Ok(());
    }

    // ─── Phase 2: Resolve PR numbers for tasks that don't have one stored ─────
    // Collect branch strings for tasks that need a PR number lookup so we can
    // run the REST calls concurrently without borrowing `ready_tasks`.

    let lookup_indices: Vec<usize> = ready_tasks
        .iter()
        .enumerate()
        .filter_map(|(i, t)| if t.pr_number.is_none() { Some(i) } else { None })
        .collect();

    if !lookup_indices.is_empty() {
        let branches: Vec<String> = lookup_indices
            .iter()
            .map(|&i| ready_tasks[i].branch.clone())
            .collect();

        let pr_number_results =
            futures::future::join_all(branches.iter().map(|b| gh.get_pr_number(repo, b))).await;

        for (&task_idx, result) in lookup_indices.iter().zip(pr_number_results.iter()) {
            let task_id = ready_tasks[task_idx].task.id.0.clone();
            let branch = ready_tasks[task_idx].branch.clone();
            match result {
                Ok(Some(n)) => {
                    ready_tasks[task_idx].pr_number = Some(*n);
                    store_set(
                        &Some(Arc::clone(store)),
                        repo,
                        &task_id,
                        &[("pr_number", serde_json::json!(*n as i64))],
                    )
                    .await;
                }
                Ok(None) => {
                    // No open PR — handled in phase 3.
                }
                Err(e) => {
                    let e_str = format!("{e}");
                    // If transient GitHub 5xx/transport error, skip incrementing counters
                    // and let the task be retried on the next tick.
                    if crate::engine::runner::git_ops::is_transient_github_error(&e_str) {
                        tracing::warn!(task_id, branch = %branch, err = %e, "transient failure getting PR number; will retry later");
                    } else {
                        tracing::warn!(task_id, branch = %branch, err = %e, "failed to get PR number");
                    }
                }
            }
        }
    }

    // ─── Phase 3: Handle tasks with no open PR ────────────────────────────────
    // Split ready_tasks into those with a PR number and those without.

    let mut tasks_with_pr: Vec<ReadyTask> = Vec::new();

    for task_info in ready_tasks {
        let Some(pr_number) = task_info.pr_number else {
            // No open PR — check if merged, then reroute or block.
            let task_id = &task_info.task.id.0;
            let branch = &task_info.branch;
            let merged = match gh.is_pr_merged(repo, branch).await {
                Ok(v) => v,
                Err(e) => {
                    let e_str = format!("{e}");
                    if crate::engine::runner::git_ops::is_transient_github_error(&e_str) {
                        tracing::warn!(task_id, branch = %branch, err = %e, "transient GitHub error checking merge status; will retry later");
                        continue;
                    }
                    tracing::warn!(task_id, branch = %branch, err = %e, "merge check failed, skipping task this tick");
                    continue;
                }
            };
            if merged {
                tracing::info!(task_id, branch = %branch, "PR already merged, marking done");
                if let Err(e) = task_manager
                    .update_task_status(&task_info.task.id, Status::Done)
                    .await
                {
                    tracing::warn!(task_id, err = %e, "failed to update status to done");
                }
            } else {
                let max_reroutes: u32 = config::get("workflow.max_reroute_attempts")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .or_else(|| {
                        config::get("workflow.max_attempts")
                            .ok()
                            .and_then(|s| s.parse().ok())
                    })
                    .unwrap_or(3);

                // Avoid incrementing persistent reroute counters when the last
                // store error indicates a transient GitHub 5xx/transport failure.
                let last_error = task_info.stored.last_error.clone();
                if crate::engine::runner::git_ops::is_transient_github_error(&last_error) {
                    tracing::warn!(task_id, "transient GitHub error recorded in last_error; skipping persistent no_pr_reroutes increment and retrying later");
                    continue;
                }

                let reroutes =
                    store_increment(&Some(Arc::clone(store)), repo, task_id, "no_code_reroutes")
                        .await;

                if reroutes as u32 >= max_reroutes {
                    tracing::error!(
                        task_id,
                        reroutes,
                        max_reroutes,
                        "reached max reroute attempts for in_review no-PR — blocking for human review"
                    );
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
                        .update_task_status(&task_info.task.id, Status::Blocked)
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
                        .update_task_status(&task_info.task.id, Status::Routed)
                        .await
                    {
                        tracing::warn!(task_id, err = %e, "failed to update status to routed");
                    }
                }
            }
            continue;
        };

        tasks_with_pr.push(ReadyTask {
            pr_number: Some(pr_number),
            ..task_info
        });
    }

    if tasks_with_pr.is_empty() {
        return Ok(());
    }

    // ─── Phase 4: Batch GraphQL fetch — one call for all in-review PRs ────────

    let pr_numbers: Vec<u64> = tasks_with_pr.iter().filter_map(|t| t.pr_number).collect();

    let batch = match gh.batch_fetch_pr_review_data(repo, &pr_numbers).await {
        Ok(data) => data,
        Err(e) => {
            tracing::warn!(
                err = %e,
                pr_count = pr_numbers.len(),
                "batch PR review fetch failed, skipping this tick"
            );
            return Ok(());
        }
    };

    // ─── Phase 5: Build collaborator cache from batch issue comments ──────────
    // Collect unique user logins that appear in automated review comments so
    // we can verify collaborator status with a single pass instead of one REST
    // call per task.

    let mut unique_users: std::collections::HashSet<String> = Default::default();
    for data in batch.values() {
        for c in &data.issue_comments {
            if c.body.starts_with("## Automated Review") {
                unique_users.insert(c.user.login.clone());
            }
        }
    }

    let collab_logins: Vec<String> = unique_users.into_iter().collect();
    let collab_results =
        futures::future::join_all(collab_logins.iter().map(|u| gh.is_collaborator(repo, u))).await;
    let collab_cache: HashMap<String, bool> = collab_logins
        .into_iter()
        .zip(collab_results.into_iter())
        .map(|(u, r)| (u, r.unwrap_or(false)))
        .collect();

    // ─── Phase 6: Process each task using batch data ──────────────────────────

    for task_info in tasks_with_pr {
        let task = &task_info.task;
        let stored_task = &task_info.stored;
        let task_id = &task.id.0;
        let Some(pr_number) = task_info.pr_number else {
            // tasks_with_pr is constructed to only include items with pr_number,
            // but guard defensively against future refactors.
            tracing::warn!(task_id, "task in tasks_with_pr has no pr_number — skipping");
            continue;
        };

        // Persist PR number (idempotent if already stored).
        store_set(
            &Some(Arc::clone(store)),
            repo,
            task_id,
            &[("pr_number", serde_json::json!(pr_number as i64))],
        )
        .await;

        let batch_data = match batch.get(&pr_number) {
            Some(d) => d,
            None => {
                tracing::warn!(
                    task_id,
                    pr_number,
                    "PR not in batch data, skipping this tick"
                );
                continue;
            }
        };

        let last_review_ts = stored_task.last_review_ts.clone();

        let reviews = &batch_data.reviews;
        let all_comments = &batch_data.review_comments;

        // Deduplicate reviews: keep only the latest per reviewer.
        let deduped_reviews = dedup_reviews(reviews);

        let any_changes_requested = deduped_reviews
            .values()
            .any(|r| r.state == "CHANGES_REQUESTED");
        let all_approved =
            !deduped_reviews.is_empty() && deduped_reviews.values().all(|r| r.state == "APPROVED");

        // Determine automated review status from batch issue comments + collaborator cache.
        let automated_review =
            automated_review_from_comments(&batch_data.issue_comments, &collab_cache, pr_number);
        let comment_approved = automated_review.as_deref() == Some("approve");
        let comment_changes_requested = automated_review.as_deref() == Some("changes_requested");

        // Handle fully-approved PRs (auto-close enabled).
        if (all_approved || comment_approved)
            && auto_close_task
            && !comment_changes_requested
            && !any_changes_requested
        {
            // Use batch data instead of a separate is_pr_merged REST call.
            let already_merged = batch_data.merged;

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
                // Use batch data instead of a separate get_pr REST call.
                let is_conflicting = batch_data.mergeable == Some(false);

                if is_conflicting {
                    let retries = stored_task.merge_conflict_retries as u64;
                    if retries >= MAX_MERGE_CONFLICT_RETRIES {
                        tracing::error!(
                            task_id,
                            pr_number,
                            retries,
                            "PR approved but merge conflict retry limit reached — blocking for human review"
                        );
                        store_set(
                            &Some(Arc::clone(store)),
                            repo,
                            task_id,
                            &[
                                (
                                    "block_reason",
                                    serde_json::json!(format!(
                                        "merge conflict retry limit ({}) reached",
                                        MAX_MERGE_CONFLICT_RETRIES
                                    )),
                                ),
                                (
                                    "last_error",
                                    serde_json::json!(format!(
                                        "PR approved but has unresolved merge conflicts after {} retries",
                                        retries
                                    )),
                                ),
                            ],
                        )
                        .await;
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
                        tracing::warn!(
                            task_id,
                            err = %e,
                            "failed to set NeedsReview for conflict retry"
                        );
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

                // Spawn auto-merge in background to avoid blocking sync_tick.
                // The CI polling loop can take up to 10 minutes with backoff.
                let task_clone = task.clone();
                let branch_clone = task_info.branch.clone();
                let backend_clone = Arc::clone(backend);
                let repo_string = repo.to_string();
                let task_manager_clone = Arc::clone(task_manager);
                let store_clone = Arc::clone(store);

                tokio::spawn(async move {
                    if let Err(e) = crate::engine::auto_merge::auto_merge_pr(
                        &task_clone,
                        &branch_clone,
                        &backend_clone,
                        &repo_string,
                        &task_agent,
                        &task_model,
                        &task_manager_clone,
                        &store_clone,
                    )
                    .await
                    {
                        tracing::warn!(
                            task_id = task_clone.id.0,
                            pr_number,
                            err = %e,
                            "auto-merge failed, keeping task in_review for next tick"
                        );
                    }
                });
            }
            continue;
        }

        // If the PR is fully approved but auto-close is disabled, advance the
        // task state so it doesn't get repeatedly re-reviewed.
        if (all_approved || comment_approved)
            && !comment_changes_requested
            && !any_changes_requested
        {
            let already_merged = batch_data.merged;

            if already_merged {
                tracing::info!(task_id, branch = %task_info.branch, "PR already merged, marking done (auto_close disabled)");
                if let Err(e) = task_manager
                    .update_task_status(&task.id, Status::Done)
                    .await
                {
                    tracing::warn!(task_id, err = %e, "failed to update status to done");
                }
            } else {
                let is_conflicting = batch_data.mergeable == Some(false);

                if is_conflicting {
                    let retries = stored_task.merge_conflict_retries as u64;
                    if retries >= MAX_MERGE_CONFLICT_RETRIES {
                        tracing::error!(task_id, pr_number, retries, "PR approved but merge conflict retry limit reached — blocking for human review (auto_close disabled)");
                        if let Err(e) = task_manager
                            .update_task_status(&task.id, Status::Blocked)
                            .await
                        {
                            tracing::warn!(task_id, err = %e, "failed to set Blocked");
                        }
                        continue;
                    }
                    tracing::info!(task_id, pr_number, retries, "PR approved but has merge conflicts — re-triggering review agent to rebase (auto_close disabled)");
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
                        tracing::warn!(
                            task_id,
                            err = %e,
                            "failed to set NeedsReview for conflict retry"
                        );
                    }
                    continue;
                }

                tracing::info!(task_id, pr_number, comment_approved, "PR approved (auto_close disabled) — marking task done and leaving PR open for human merge");
                if let Err(e) = task_manager
                    .update_task_status(&task.id, Status::Done)
                    .await
                {
                    tracing::warn!(task_id, err = %e, "failed to update task status to done");
                }
            }
            continue;
        }

        // Process reviews that request changes.
        if !any_changes_requested && !comment_changes_requested {
            continue;
        }

        // Build review context for re-dispatch.
        let mut review_context = String::new();
        let mut latest_review_ts = last_review_ts.clone();

        for review in deduped_reviews
            .values()
            .filter(|r| r.state == "CHANGES_REQUESTED")
        {
            if review.submitted_at > latest_review_ts {
                latest_review_ts = review.submitted_at.clone();
            }

            // Skip if we've already processed this review.
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

        // Also include comment-based review feedback from batch issue comments.
        // Collect the new comment timestamp but do NOT persist it yet — we only
        // save it after handle_review_changes() succeeds so that a transient
        // failure does not silently drop the review on the next poll.
        let mut new_comment_review_ts: Option<String> = None;
        if comment_changes_requested {
            let last_comment_ts = stored_task.last_comment_review_ts.clone();
            for c in batch_data.issue_comments.iter().rev() {
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

        // Cap review context to avoid oversized values.
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
                task,
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

/// Determine automated review status from batch-fetched issue comments.
///
/// Mirrors the logic of `get_automated_review_status` but operates on
/// pre-fetched comments and a pre-built collaborator cache instead of making
/// additional REST calls.
fn automated_review_from_comments(
    issue_comments: &[GitHubComment],
    collab_cache: &HashMap<String, bool>,
    pr_number: u64,
) -> Option<String> {
    for c in issue_comments.iter().rev() {
        if !c.body.starts_with("## Automated Review") {
            continue;
        }
        if !collab_cache.get(&c.user.login).copied().unwrap_or(false) {
            tracing::warn!(
                user = %c.user.login,
                pr_number,
                "ignoring automated review comment from non-collaborator"
            );
            continue;
        }
        let first_line = c.body.lines().next().unwrap_or("");
        return if first_line.contains("Automated Review \u{2014} Approve") {
            Some("approve".to_string())
        } else if first_line.contains("Automated Review \u{2014} Changes Requested") {
            Some("changes_requested".to_string())
        } else {
            None
        };
    }
    None
}
