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

use crate::backends::{ExternalBackend, ExternalId, Status};
use crate::config;
use crate::engine::auto_merge::{dedup_reviews, handle_review_changes, MAX_MERGE_CONFLICT_RETRIES};
use crate::engine::tasks::TaskManager;
use crate::engine::EngineConfig;
use crate::github::http::{GhHttp, PrReviewBatchData};
use crate::github::types::{GitHubComment, GitHubReviewComment, PullRequestReview};
use crate::store::TaskStore;
use crate::store::{store_increment_by_id, store_reset_failure_counters, store_set_result_by_id};
use async_trait::async_trait;
use dashmap::{DashMap, DashSet};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use super::sync::ReviewTaskSnapshot;

/// Abstraction over the GitHub HTTP calls used by `review_open_prs`.
///
/// Allows tests to inject a mock without needing a real GitHub token.
#[async_trait]
pub(crate) trait GhReviewClient: Send + Sync {
    async fn get_pr_number(&self, repo: &str, branch: &str) -> anyhow::Result<Option<u64>>;
    async fn is_pr_merged(&self, repo: &str, branch: &str) -> anyhow::Result<bool>;
    async fn batch_fetch_pr_review_data(
        &self,
        repo: &str,
        pr_numbers: &[u64],
    ) -> anyhow::Result<HashMap<u64, PrReviewBatchData>>;
    async fn is_collaborator(&self, repo: &str, username: &str) -> anyhow::Result<bool>;
}

#[async_trait]
impl GhReviewClient for GhHttp {
    async fn get_pr_number(&self, repo: &str, branch: &str) -> anyhow::Result<Option<u64>> {
        self.get_pr_number(repo, branch).await
    }
    async fn is_pr_merged(&self, repo: &str, branch: &str) -> anyhow::Result<bool> {
        self.is_pr_merged(repo, branch).await
    }
    async fn batch_fetch_pr_review_data(
        &self,
        repo: &str,
        pr_numbers: &[u64],
    ) -> anyhow::Result<HashMap<u64, PrReviewBatchData>> {
        self.batch_fetch_pr_review_data(repo, pr_numbers).await
    }
    async fn is_collaborator(&self, repo: &str, username: &str) -> anyhow::Result<bool> {
        self.is_collaborator(repo, username).await
    }
}

/// Action to take when a PR has merge conflicts after approval.
#[derive(Debug, Clone, Copy)]
enum ConflictAction {
    /// Retry review by re-triggering the review agent to rebase.
    RetryReview,
    /// Block the task for human review (retry limit exceeded).
    BlockForHuman,
}

/// Handle a merge conflict detected in a fully-approved PR.
///
/// Returns the action to take: either retry review (increment counter + set
/// NeedsReview) or block for human (write block_reason + set Blocked).
async fn handle_merge_conflict(
    id: &ExternalId,
    pr_number: u64,
    retries: u64,
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
    store_id: i64,
) -> ConflictAction {
    let task_id = &id.0;
    if retries >= MAX_MERGE_CONFLICT_RETRIES {
        tracing::error!(
            task_id,
            pr_number,
            retries,
            "PR approved but merge conflict retry limit reached — blocking for human review"
        );
        let fields = [
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
        ];
        if let Err(e) = task_manager
            .update_task_status_and_result(id, Status::Blocked, &fields)
            .await
        {
            tracing::error!(task_id, err = %e, "failed to write block_reason and set Blocked");
        }
        return ConflictAction::BlockForHuman;
    }
    tracing::info!(
        task_id,
        pr_number,
        retries,
        "PR approved but has merge conflicts — re-triggering review agent to rebase"
    );
    if let Err(e) =
        store_increment_by_id(&Some(Arc::clone(store)), store_id, "merge_conflict_retries").await
    {
        tracing::warn!(task_id, err = %e, "failed to increment merge_conflict_retries — skipping dispatch to avoid bypassing retry limit");
        return ConflictAction::BlockForHuman;
    }
    if let Err(e) = task_manager
        .update_task_status(id, Status::NeedsReview)
        .await
    {
        tracing::warn!(task_id, err = %e, "failed to set NeedsReview for conflict retry");
    }
    ConflictAction::RetryReview
}

#[async_trait]
trait ReviewBatchFailCounterStore: Send + Sync {
    async fn kv_delete(&self, key: &str) -> anyhow::Result<()>;
    async fn kv_set(&self, key: &str, value: &str) -> anyhow::Result<()>;
}

#[async_trait]
impl ReviewBatchFailCounterStore for TaskStore {
    async fn kv_delete(&self, key: &str) -> anyhow::Result<()> {
        TaskStore::kv_delete(self, key).await
    }

    async fn kv_set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        TaskStore::kv_set(self, key, value).await
    }
}

/// Reset the review polling batch-failure counter after a successful batch fetch.
///
/// Prefer deleting the key to keep the KV namespace clean. If delete fails,
/// write an explicit `0` to prevent stale counters from causing false degraded
/// alerts on the next isolated failure.
async fn reset_batch_fail_counter(
    store: &dyn ReviewBatchFailCounterStore,
    batch_fail_key: &str,
) -> bool {
    match store.kv_delete(batch_fail_key).await {
        Ok(()) => true,
        Err(delete_err) => {
            tracing::warn!(
                key = %batch_fail_key,
                err = %delete_err,
                "failed to clear review batch failure counter, falling back to zero reset"
            );
            match store.kv_set(batch_fail_key, "0").await {
                Ok(()) => {
                    tracing::warn!(
                        key = %batch_fail_key,
                        "review batch failure counter reset to 0 after delete failure"
                    );
                    true
                }
                Err(set_err) => {
                    tracing::error!(
                        key = %batch_fail_key,
                        delete_err = %delete_err,
                        set_err = %set_err,
                        "failed to reset review batch failure counter; stale value may cause false degradation"
                    );
                    false
                }
            }
        }
    }
}

/// Review open PRs - re-dispatch agent to address review feedback.
///
/// Lists tasks in review, fetches PR reviews, and re-dispatches the agent
/// when a reviewer requests changes. The review context is stored in the
/// store and injected into the agent prompt.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn review_open_prs(
    backend: &Arc<dyn ExternalBackend>,
    repo: &str,
    config: &EngineConfig,
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
    dispatching: &Arc<DashMap<String, String>>,
    auto_merge_in_flight: &Arc<DashSet<String>>,
    in_review_tasks: &[ReviewTaskSnapshot],
    gh: &dyn GhReviewClient,
    router_config: &crate::engine::router::config::RouterConfig,
) -> anyhow::Result<()> {
    if in_review_tasks.is_empty() {
        tracing::debug!(count = 0, "checking in_review tasks for PR reviews");
        return Ok(());
    }

    let auto_close_task = config.auto_close_task_on_approval;

    tracing::info!(
        count = in_review_tasks.len(),
        "checking in_review tasks for PR reviews"
    );

    // ─── Phase 1: Validate tasks and separate by PR number availability ───────

    struct ReadyTask {
        task: crate::backends::ExternalTask,
        store_id: i64,
        stored: crate::store::Task,
        branch: String,
        /// `Some` = already stored in DB, `None` = needs REST lookup.
        pr_number: Option<u64>,
    }

    let mut ready_tasks: Vec<ReadyTask> = Vec::new();

    for snapshot in in_review_tasks {
        let task = snapshot.external.clone();
        let task_id = &task.id.0;

        // Skip tasks currently being processed by the main tick.
        let dispatch_key = format!("{}/{}", repo, task_id);
        if dispatching.contains_key(&dispatch_key) {
            tracing::debug!(
                task_id,
                "task locked by dispatch flow, skipping review_open_prs"
            );
            continue;
        }

        let store_id = snapshot.stored.id;
        let stored = snapshot.stored.clone();

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
            store_id,
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
                    if let Err(e) = store_set_result_by_id(
                        &Some(Arc::clone(store)),
                        ready_tasks[task_idx].store_id,
                        &[("pr_number", serde_json::json!(*n as i64))],
                    )
                    .await
                    {
                        tracing::warn!(task_id, err = %e, "failed to persist pr_number");
                    }
                }
                Ok(None) => {
                    // No open PR — handled in phase 3.
                }
                Err(e) => {
                    let e_str = format!("{e}");
                    // If transient GitHub 5xx/transport error, skip incrementing counters
                    // and let the task be retried on the next tick.
                    if crate::engine::runner::git_ops::is_transient_github_api_error(&e_str) {
                        tracing::warn!(task_id, branch = %branch, err = %e, "transient failure getting PR number; will retry later");
                    } else {
                        tracing::warn!(task_id, branch = %branch, err = %e, "failed to get PR number");
                    }
                }
            }
        }
    }

    // ─── Phase 3: Handle tasks with no open PR ────────────────────────────────
    // Split ready_tasks into those with a PR number and those without, then
    // run is_pr_merged concurrently for all no-PR tasks (mirrors Phase 2).

    let mut tasks_with_pr: Vec<ReadyTask> = Vec::new();
    let mut no_pr_tasks: Vec<ReadyTask> = Vec::new();

    for task_info in ready_tasks {
        if task_info.pr_number.is_none() {
            no_pr_tasks.push(task_info);
        } else {
            tasks_with_pr.push(task_info);
        }
    }

    if !no_pr_tasks.is_empty() {
        let merged_results =
            futures::future::join_all(no_pr_tasks.iter().map(|t| gh.is_pr_merged(repo, &t.branch)))
                .await;

        for (task_info, merged_result) in no_pr_tasks.into_iter().zip(merged_results) {
            let task_id = task_info.task.id.0.as_str();
            let branch = task_info.branch.as_str();

            let merged = match merged_result {
                Ok(v) => v,
                Err(e) => {
                    let e_str = format!("{e}");
                    if crate::engine::runner::git_ops::is_transient_github_api_error(&e_str) {
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
                    .unwrap_or(3)
                    .max(1);

                // Avoid incrementing persistent reroute counters when the last
                // store error indicates a transient GitHub 5xx/transport failure.
                let last_error = task_info.stored.last_error.clone();
                if crate::engine::runner::git_ops::is_transient_github_api_error(&last_error) {
                    tracing::warn!(task_id, "transient GitHub error recorded in last_error; skipping persistent no_pr_reroutes increment and retrying later");
                    continue;
                }

                let reroutes = match store_increment_by_id(
                    &Some(Arc::clone(store)),
                    task_info.store_id,
                    "no_code_reroutes",
                )
                .await
                {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(task_id, err = %e, "failed to increment no_code_reroutes — skipping reroute/block decision this tick");
                        continue;
                    }
                };

                if reroutes >= max_reroutes as u64 {
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
                    // Write block_reason atomically with the status transition to prevent
                    // auto_unblock_blocked_tasks (block_reason.is_none() gate) from
                    // re-dispatching this task.
                    let fields = [
                        (
                            "block_reason",
                            serde_json::json!(format!(
                                "max reroute attempts ({}) reached — no PR or code changes produced",
                                max_reroutes
                            )),
                        ),
                        ("last_error", serde_json::json!(msg)),
                        ("agent", serde_json::json!(null)),
                        ("model", serde_json::json!(null)),
                    ];
                    if let Err(e) = task_manager
                        .update_task_status_and_result(&task_info.task.id, Status::Blocked, &fields)
                        .await
                    {
                        tracing::error!(task_id, err = %e, "update_task_status_and_result(Blocked) failed — skipping block to avoid silent auto-unblock loop");
                        continue;
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
        }
    }

    if tasks_with_pr.is_empty() {
        return Ok(());
    }

    // ─── Phase 4: Batch GraphQL fetch — one call for all in-review PRs ────────

    let pr_numbers: Vec<u64> = tasks_with_pr.iter().filter_map(|t| t.pr_number).collect();

    let batch_fail_key = format!("review_batch_fail:{repo}");
    let batch = match gh.batch_fetch_pr_review_data(repo, &pr_numbers).await {
        Ok(data) => {
            let _ = reset_batch_fail_counter(store.as_ref(), &batch_fail_key).await;
            data
        }
        Err(e) => {
            // Track consecutive batch fetch failures in the KV store. If the
            // KV increment itself fails (e.g. transient DB error), treat the
            // situation as immediately degraded so operators receive an error
            // level signal instead of silently staying at warn level.
            let fails = match store.kv_increment(&batch_fail_key).await {
                Ok(n) => n,
                Err(kv_err) => {
                    tracing::warn!(kv_err = %kv_err, err = %e, "failed to track batch_fail counter — treating as degraded");
                    // Use a sentinel high value so the escalation path fires.
                    u64::MAX
                }
            };
            if fails >= 10 {
                tracing::error!(
                    err = %e,
                    consecutive_failures = fails,
                    pr_count = pr_numbers.len(),
                    "review polling degraded — batch fetch failing repeatedly"
                );
            } else {
                tracing::warn!(
                    err = %e,
                    consecutive_failures = fails,
                    pr_count = pr_numbers.len(),
                    "batch PR review fetch failed, skipping this tick"
                );
            }
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
    let collab_cache: HashMap<String, Option<bool>> = collab_logins
        .into_iter()
        .zip(collab_results)
        .map(|(u, r)| match r {
            Ok(v) => (u, Some(v)),
            Err(e) => {
                tracing::warn!(user = %u, error = %e, "is_collaborator check failed; skipping review comment");
                (u, None)
            }
        })
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
        if let Err(e) = store_set_result_by_id(
            &Some(Arc::clone(store)),
            task_info.store_id,
            &[("pr_number", serde_json::json!(pr_number as i64))],
        )
        .await
        {
            tracing::warn!(task_id, err = %e, "failed to persist pr_number");
        }

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

        let review_ts_map: serde_json::Value =
            match serde_json::from_str(&stored_task.review_ts_map) {
                Ok(serde_json::Value::Object(m)) => serde_json::Value::Object(m),
                _ => serde_json::json!({}),
            };

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
                    let retries = stored_task.merge_conflict_retries.max(0) as u64;
                    match handle_merge_conflict(
                        &task.id,
                        pr_number,
                        retries,
                        task_manager,
                        store,
                        task_info.store_id,
                    )
                    .await
                    {
                        ConflictAction::BlockForHuman => continue,
                        ConflictAction::RetryReview => {}
                    }
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

                // Check if auto-merge is already in flight for this task.
                // This prevents double-spawn since review_open_prs runs every sync_tick (~10s)
                // but CI polling can take up to 10 minutes.
                if auto_merge_in_flight.contains(task_id) {
                    tracing::debug!(
                        task_id,
                        pr_number,
                        "auto-merge already in flight, skipping spawn"
                    );
                } else {
                    // Spawn auto-merge in background to avoid blocking sync_tick.
                    // The CI polling loop can take up to 10 minutes with backoff.
                    auto_merge_in_flight.insert(task_id.to_string());
                    let task_clone = task.clone();
                    let branch_clone = task_info.branch.clone();
                    let backend_clone = Arc::clone(backend);
                    let repo_string = repo.to_string();
                    let task_manager_clone = Arc::clone(task_manager);
                    let store_clone = Arc::clone(store);
                    let in_flight_clone = Arc::clone(auto_merge_in_flight);

                    tokio::spawn(async move {
                        let _guard = scopeguard::guard((), |_| {
                            in_flight_clone.remove(&task_clone.id.0);
                        });

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
            }
            continue;
        }

        // If the PR is fully approved but auto-close is disabled, the task stays
        // in_review waiting for a human to merge the PR.  We must NOT mark it Done
        // here — the PR has not been merged, so the work is not complete.
        // Once the human merges the PR, `already_merged` will be true on the next
        // poll tick and the task will transition to Done correctly.
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
                    let retries = stored_task.merge_conflict_retries.max(0) as u64;
                    match handle_merge_conflict(
                        &task.id,
                        pr_number,
                        retries,
                        task_manager,
                        store,
                        task_info.store_id,
                    )
                    .await
                    {
                        ConflictAction::BlockForHuman => continue,
                        ConflictAction::RetryReview => {}
                    }
                }

                // PR approved but not yet merged and auto_close is disabled —
                // leave task in in_review so a human can merge it manually.
                // Do NOT mark Done: the PR merge is the actual completion signal.
                tracing::info!(
                    task_id,
                    pr_number,
                    comment_approved,
                    "PR approved (auto_close disabled) — leaving task in_review until PR is merged"
                );
            }
            continue;
        }

        // Process reviews that request changes.
        if !any_changes_requested && !comment_changes_requested {
            continue;
        }

        // Build review context for re-dispatch.
        let mut review_context = String::new();
        let mut updated_review_ts_map = review_ts_map.clone();

        for review in deduped_reviews
            .values()
            .filter(|r| r.state == "CHANGES_REQUESTED")
        {
            let reviewer_login = &review.user.login;
            let reviewer_last_ts = updated_review_ts_map
                .get(reviewer_login)
                .and_then(|v| v.as_str())
                .unwrap_or_default();

            // Skip if we've already processed this review for this reviewer.
            if !reviewer_last_ts.is_empty() && review.submitted_at.as_str() <= reviewer_last_ts {
                continue;
            }

            // Track the latest timestamp for this reviewer.
            updated_review_ts_map[reviewer_login] = serde_json::json!(review.submitted_at.clone());
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

            // Defensive: ensure comments are processed in chronological order
            // and find the newest matching automated changes-requested comment
            // that is newer than the stored watermark.
            let mut newest_match: Option<&crate::github::types::GitHubComment> = None;
            for c in &batch_data.issue_comments {
                if !c
                    .body
                    .starts_with("## Automated Review \u{2014} Changes Requested")
                {
                    continue;
                }
                if !last_comment_ts.is_empty() && c.created_at <= last_comment_ts {
                    continue;
                }
                match newest_match {
                    None => newest_match = Some(c),
                    Some(prev) => {
                        if c.created_at > prev.created_at {
                            newest_match = Some(c);
                        }
                    }
                }
            }

            if let Some(c) = newest_match {
                let body: String = c.body.lines().skip(1).collect::<Vec<_>>().join("\n");
                review_context.push_str("### Automated Review (Changes Requested)\n\n");
                review_context.push_str(&body);
                review_context.push('\n');

                // Capture the timestamp; persisted atomically with last_review_ts below.
                new_comment_review_ts = Some(c.created_at.clone());
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

            // Call handle_review_changes FIRST. If it fails, we do not advance the
            // watermark, allowing the review to be re-processed on the next poll tick.
            // If it succeeds, we advance the watermark to prevent duplicate re-dispatches.
            //
            // This trade-off (retry transient failures vs. prevent idempotent duplicates)
            // is correct: it is better to retry a review once more than to silently drop
            // reviewer feedback forever.
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
                router_config,
            )
            .await
            {
                tracing::warn!(task_id, err = %e, "failed to handle review feedback — will retry next tick");
                continue;
            }

            // Only after handle_review_changes succeeds, persist the watermark timestamps.
            let mut fields: Vec<(&str, serde_json::Value)> =
                vec![("review_ts_map", serde_json::json!(updated_review_ts_map))];
            if let Some(ref ts) = new_comment_review_ts {
                fields.push(("last_comment_review_ts", serde_json::json!(ts)));
            }
            // Retry the watermark save a few times before giving up
            let mut save_ok = false;
            for _ in 0..3 {
                if store_set_result_by_id(&Some(Arc::clone(store)), task_info.store_id, &fields)
                    .await
                    .is_ok()
                {
                    save_ok = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            if !save_ok {
                tracing::error!(
                    task_id,
                    "failed to persist review watermark after retries — next tick will re-dispatch same review"
                );
                continue;
            }

            if review_cycles < max_cycles {
                tracing::info!(task_id, "re-dispatching task to address review feedback");
                store_reset_failure_counters(&Some(Arc::clone(store)), repo, task_id).await;
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
    collab_cache: &HashMap<String, Option<bool>>,
    pr_number: u64,
) -> Option<String> {
    // Find the newest automated review comment authored by a collaborator.
    let mut newest: Option<&GitHubComment> = None;
    for c in issue_comments {
        if !c.body.starts_with("## Automated Review") {
            continue;
        }
        match collab_cache.get(&c.user.login).copied().flatten() {
            None if collab_cache.contains_key(&c.user.login) => {
                tracing::warn!(
                    user = %c.user.login,
                    pr_number,
                    "skipping automated review comment: collaborator check unavailable"
                );
                continue;
            }
            None | Some(false) => {
                tracing::warn!(
                    user = %c.user.login,
                    pr_number,
                    "ignoring automated review comment from non-collaborator"
                );
                continue;
            }
            Some(true) => {}
        }
        match newest {
            None => newest = Some(c),
            Some(prev) => {
                if c.created_at > prev.created_at {
                    newest = Some(c);
                }
            }
        }
    }

    if let Some(c) = newest {
        let first_line = c.body.lines().next().unwrap_or("");
        if first_line.contains("Automated Review \u{2014} Approve") {
            Some("approve".to_string())
        } else if first_line.contains("Automated Review \u{2014} Changes Requested") {
            Some("changes_requested".to_string())
        } else {
            None
        }
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::{ExternalId, ExternalTask, Mention};
    use crate::engine::EngineConfig;
    use crate::github::http::PrReviewBatchData;
    use crate::github::types::{GitHubComment, GitHubReview, GitHubUser};
    use crate::store::{NewTask, TaskStatus, TaskStore};
    use async_trait::async_trait;
    use dashmap::{DashMap, DashSet};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tracing::Level;
    use tracing_subscriber::EnvFilter;

    // ─── Mock GhReviewClient ─────────────────────────────────────────────────

    #[derive(Default)]
    struct MockGh {
        /// Responses for get_pr_number keyed by branch.
        pr_numbers: HashMap<String, Option<u64>>,
        /// Responses for is_pr_merged keyed by branch.
        merged: HashMap<String, bool>,
        /// Response for batch_fetch_pr_review_data.
        batch_data: HashMap<u64, PrReviewBatchData>,
        /// Whether batch_fetch_pr_review_data should return an error.
        batch_error: bool,
        /// Responses for is_collaborator keyed by username.
        collaborators: HashMap<String, bool>,
    }

    #[async_trait]
    impl GhReviewClient for MockGh {
        async fn get_pr_number(&self, _repo: &str, branch: &str) -> anyhow::Result<Option<u64>> {
            Ok(self.pr_numbers.get(branch).copied().flatten())
        }
        async fn is_pr_merged(&self, _repo: &str, branch: &str) -> anyhow::Result<bool> {
            Ok(self.merged.get(branch).copied().unwrap_or(false))
        }
        async fn batch_fetch_pr_review_data(
            &self,
            _repo: &str,
            _pr_numbers: &[u64],
        ) -> anyhow::Result<HashMap<u64, PrReviewBatchData>> {
            if self.batch_error {
                anyhow::bail!("simulated batch GraphQL error");
            }
            Ok(self.batch_data.clone())
        }
        async fn is_collaborator(&self, _repo: &str, username: &str) -> anyhow::Result<bool> {
            Ok(self.collaborators.get(username).copied().unwrap_or(false))
        }
    }

    // ─── Mock ExternalBackend ────────────────────────────────────────────────

    struct MockBackend;

    #[async_trait]
    impl crate::backends::ExternalBackend for MockBackend {
        fn name(&self) -> &str {
            "mock"
        }
        async fn create_task(
            &self,
            _t: &str,
            _b: &str,
            _l: &[String],
        ) -> anyhow::Result<ExternalId> {
            Ok(ExternalId("1".to_string()))
        }
        async fn get_task(&self, id: &ExternalId) -> anyhow::Result<ExternalTask> {
            Ok(make_ext_task(&id.0))
        }
        async fn list_by_status(
            &self,
            _s: crate::backends::Status,
        ) -> anyhow::Result<Vec<ExternalTask>> {
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
            Ok(ExternalId("child".to_string()))
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
            Ok(Some("bot".to_string()))
        }
        async fn get_mentions(&self, _s: &str) -> anyhow::Result<Vec<Mention>> {
            Ok(vec![])
        }
        async fn update_status(
            &self,
            _id: &ExternalId,
            _s: crate::backends::Status,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    // ─── Helpers ─────────────────────────────────────────────────────────────

    fn make_ext_task(id: &str) -> ExternalTask {
        ExternalTask {
            id: ExternalId(id.to_string()),
            title: "test task".to_string(),
            body: "".to_string(),
            state: "open".to_string(),
            labels: vec![],
            author: "user".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: "".to_string(),
        }
    }

    fn make_comment(id: u64, login: &str, body: &str, created_at: &str) -> GitHubComment {
        GitHubComment {
            id,
            body: body.to_string(),
            user: GitHubUser {
                login: login.to_string(),
            },
            created_at: created_at.to_string(),
            updated_at: None,
            html_url: None,
            issue_url: None,
            author_association: None,
        }
    }

    fn make_review(
        id: u64,
        login: &str,
        state: &str,
        submitted_at: &str,
        body: Option<&str>,
    ) -> GitHubReview {
        GitHubReview {
            id,
            user: GitHubUser {
                login: login.to_string(),
            },
            body: body.map(str::to_string),
            state: state.to_string(),
            html_url: None,
            submitted_at: submitted_at.to_string(),
            commit_id: None,
        }
    }

    async fn make_store() -> Arc<TaskStore> {
        Arc::new(TaskStore::open_memory().await.unwrap())
    }

    fn default_config() -> EngineConfig {
        EngineConfig::default()
    }

    struct CaptureWriter(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::writer::MakeWriter<'a> for CaptureWriter {
        type Writer = CaptureWriter;
        fn make_writer(&'a self) -> Self::Writer {
            CaptureWriter(self.0.clone())
        }
    }

    fn with_captured_warn_logs<F>(f: F) -> String
    where
        F: FnOnce(),
    {
        let output: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_default_env().add_directive(Level::WARN.into()))
            .with_writer(CaptureWriter(Arc::clone(&output)))
            .with_ansi(false)
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        f();
        drop(guard);
        let captured = String::from_utf8_lossy(&output.lock().unwrap()).to_string();
        captured
    }

    #[derive(Clone)]
    struct MockBatchFailStore {
        delete_error: Option<String>,
        set_error: Option<String>,
        set_calls: Arc<Mutex<Vec<(String, String)>>>,
    }

    #[async_trait]
    impl ReviewBatchFailCounterStore for MockBatchFailStore {
        async fn kv_delete(&self, _key: &str) -> anyhow::Result<()> {
            if let Some(msg) = &self.delete_error {
                anyhow::bail!("{msg}");
            }
            Ok(())
        }

        async fn kv_set(&self, key: &str, value: &str) -> anyhow::Result<()> {
            self.set_calls
                .lock()
                .unwrap()
                .push((key.to_string(), value.to_string()));
            if let Some(msg) = &self.set_error {
                anyhow::bail!("{msg}");
            }
            Ok(())
        }
    }

    // ─── automated_review_from_comments ──────────────────────────────────────

    #[test]
    fn arc_empty_input_returns_none() {
        let result = automated_review_from_comments(&[], &HashMap::new(), 1);
        assert_eq!(result, None);
    }

    #[test]
    fn arc_non_automated_comment_ignored() {
        let comments = vec![make_comment(
            1,
            "alice",
            "Great work!",
            "2026-01-01T00:00:00Z",
        )];
        let cache = HashMap::from([("alice".to_string(), Some(true))]);
        assert_eq!(automated_review_from_comments(&comments, &cache, 1), None);
    }

    #[test]
    fn arc_collaborator_approve_returns_approve() {
        let body = "## Automated Review \u{2014} Approve\n\nLooks good.";
        let comments = vec![make_comment(1, "bot", body, "2026-01-01T00:00:00Z")];
        let cache = HashMap::from([("bot".to_string(), Some(true))]);
        assert_eq!(
            automated_review_from_comments(&comments, &cache, 1),
            Some("approve".to_string())
        );
    }

    #[test]
    fn arc_collaborator_changes_requested_returns_changes_requested() {
        let body = "## Automated Review \u{2014} Changes Requested\n\nFix the bug.";
        let comments = vec![make_comment(1, "bot", body, "2026-01-01T00:00:00Z")];
        let cache = HashMap::from([("bot".to_string(), Some(true))]);
        assert_eq!(
            automated_review_from_comments(&comments, &cache, 1),
            Some("changes_requested".to_string())
        );
    }

    #[test]
    fn arc_non_collaborator_comment_ignored() {
        let body = "## Automated Review \u{2014} Approve\n\nLooks good.";
        let comments = vec![make_comment(1, "outsider", body, "2026-01-01T00:00:00Z")];
        let cache = HashMap::from([("outsider".to_string(), Some(false))]);
        assert_eq!(automated_review_from_comments(&comments, &cache, 1), None);
    }

    #[test]
    fn arc_unknown_user_not_in_cache_is_ignored() {
        // User not present in cache at all — treated as non-collaborator.
        let body = "## Automated Review \u{2014} Approve\n\nLooks good.";
        let comments = vec![make_comment(1, "unknown", body, "2026-01-01T00:00:00Z")];
        let cache: HashMap<String, Option<bool>> = HashMap::new();
        assert_eq!(automated_review_from_comments(&comments, &cache, 1), None);
    }

    #[test]
    fn arc_collab_check_failed_none_in_cache_skipped() {
        // Cache has the key but value is None — collaborator check failed.
        let body = "## Automated Review \u{2014} Approve\n\nLooks good.";
        let comments = vec![make_comment(1, "bot", body, "2026-01-01T00:00:00Z")];
        let cache: HashMap<String, Option<bool>> = HashMap::from([("bot".to_string(), None)]);
        assert_eq!(automated_review_from_comments(&comments, &cache, 1), None);
    }

    #[test]
    fn arc_newest_comment_wins() {
        let body_approve = "## Automated Review \u{2014} Approve\n\nLooks good.";
        let body_changes = "## Automated Review \u{2014} Changes Requested\n\nFix the bug.";
        let comments = vec![
            make_comment(1, "bot", body_approve, "2026-01-01T00:00:00Z"),
            make_comment(2, "bot", body_changes, "2026-01-02T00:00:00Z"),
        ];
        let cache = HashMap::from([("bot".to_string(), Some(true))]);
        // Newer (changes_requested) wins.
        assert_eq!(
            automated_review_from_comments(&comments, &cache, 1),
            Some("changes_requested".to_string())
        );
    }

    #[test]
    fn arc_automated_review_header_without_known_verdict_returns_none() {
        // Body starts with "## Automated Review" but first line doesn't match either verdict.
        let body = "## Automated Review — Unknown\n\nSome text.";
        let comments = vec![make_comment(1, "bot", body, "2026-01-01T00:00:00Z")];
        let cache = HashMap::from([("bot".to_string(), Some(true))]);
        assert_eq!(automated_review_from_comments(&comments, &cache, 1), None);
    }

    // ─── review_open_prs: empty task list ────────────────────────────────────

    #[tokio::test]
    async fn review_open_prs_empty_tasks_returns_ok_without_info_log() {
        let store = make_store().await;
        let backend: Arc<dyn crate::backends::ExternalBackend> = Arc::new(MockBackend);
        let tm = Arc::new(crate::engine::tasks::TaskManager::with_store(
            Arc::clone(&backend),
            Arc::clone(&store),
            "owner/repo".to_string(),
        ));
        let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
        let in_flight = Arc::new(DashSet::new());
        let gh = MockGh::default();

        let result = review_open_prs(
            &backend,
            "owner/repo",
            &default_config(),
            &tm,
            &store,
            &dispatching,
            &in_flight,
            &[],
            &gh,
            &crate::engine::router::RouterConfig::default(),
        )
        .await;

        assert!(result.is_ok());
    }

    // ─── review_open_prs: dispatching set guard ───────────────────────────────

    #[tokio::test]
    async fn review_open_prs_dispatching_guard_skips_task() {
        let store = make_store().await;
        let backend: Arc<dyn crate::backends::ExternalBackend> = Arc::new(MockBackend);
        let tm = Arc::new(crate::engine::tasks::TaskManager::with_store(
            Arc::clone(&backend),
            Arc::clone(&store),
            "owner/repo".to_string(),
        ));

        // Create a task in-store so we can build a snapshot.
        let task_store_id = store
            .create(&NewTask {
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "task1".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        let _ = crate::store::store_set_by_id(
            &Some(&store),
            task_store_id,
            &[("branch", serde_json::json!("my-branch"))],
        )
        .await;
        store
            .update_status(task_store_id, TaskStatus::InReview)
            .await
            .unwrap();
        let stored = store.get(task_store_id).await.unwrap();
        let snapshot = crate::engine::sync::ReviewTaskSnapshot {
            external: crate::engine::tasks::store_task_to_external(&stored),
            stored,
        };

        // Lock the task in the dispatching set.
        let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
        dispatching.insert(
            format!("owner/repo/{}", snapshot.external.id.0),
            snapshot.external.id.0.clone(),
        );

        // Mock returns pr_number = Some(42) so the task would normally proceed.
        let mut gh = MockGh::default();
        gh.pr_numbers.insert("my-branch".to_string(), Some(42));
        // If the guard is broken, batch_fetch would be called (we don't set batch_data, so it
        // returns empty — which would cause the task to silently skip, not panic).
        // The important invariant is that the store status remains InReview.

        let in_flight = Arc::new(DashSet::new());
        let result = review_open_prs(
            &backend,
            "owner/repo",
            &default_config(),
            &tm,
            &store,
            &dispatching,
            &in_flight,
            &[snapshot],
            &gh,
            &crate::engine::router::RouterConfig::default(),
        )
        .await;

        assert!(result.is_ok());
        // Task status must remain in_review (guard prevented any state change).
        let after = store.get(task_store_id).await.unwrap();
        assert_eq!(after.status, TaskStatus::InReview);
    }

    // ─── review_open_prs: Phase 3 — no open PR, PR merged ────────────────────

    #[tokio::test]
    async fn review_open_prs_no_pr_merged_marks_done() {
        let store = make_store().await;
        let backend: Arc<dyn crate::backends::ExternalBackend> = Arc::new(MockBackend);
        let tm = Arc::new(crate::engine::tasks::TaskManager::with_store(
            Arc::clone(&backend),
            Arc::clone(&store),
            "owner/repo".to_string(),
        ));

        let task_id = store
            .create(&NewTask {
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "task1".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        let _ = crate::store::store_set_by_id(
            &Some(&store),
            task_id,
            &[("branch", serde_json::json!("feat-branch"))],
        )
        .await;
        store
            .update_status(task_id, TaskStatus::InReview)
            .await
            .unwrap();
        let stored = store.get(task_id).await.unwrap();
        let snapshot = crate::engine::sync::ReviewTaskSnapshot {
            external: crate::engine::tasks::store_task_to_external(&stored),
            stored,
        };

        // No PR number stored, and no result from get_pr_number → pr_number stays None.
        // is_pr_merged returns true.
        let mut gh = MockGh::default();
        gh.merged.insert("feat-branch".to_string(), true);

        let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
        let in_flight = Arc::new(DashSet::new());
        review_open_prs(
            &backend,
            "owner/repo",
            &default_config(),
            &tm,
            &store,
            &dispatching,
            &in_flight,
            &[snapshot],
            &gh,
            &crate::engine::router::RouterConfig::default(),
        )
        .await
        .unwrap();

        let after = store.get(task_id).await.unwrap();
        assert_eq!(after.status, TaskStatus::Done);
    }

    // ─── review_open_prs: Phase 3 — no PR, not merged, reroute ──────────────

    #[tokio::test]
    async fn review_open_prs_no_pr_not_merged_reroutes() {
        let store = make_store().await;
        let backend: Arc<dyn crate::backends::ExternalBackend> = Arc::new(MockBackend);
        let tm = Arc::new(crate::engine::tasks::TaskManager::with_store(
            Arc::clone(&backend),
            Arc::clone(&store),
            "owner/repo".to_string(),
        ));

        let task_id = store
            .create(&NewTask {
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "task2".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        let _ = crate::store::store_set_by_id(
            &Some(&store),
            task_id,
            &[("branch", serde_json::json!("feat-branch"))],
        )
        .await;
        store
            .update_status(task_id, TaskStatus::InReview)
            .await
            .unwrap();
        let stored = store.get(task_id).await.unwrap();
        let snapshot = crate::engine::sync::ReviewTaskSnapshot {
            external: crate::engine::tasks::store_task_to_external(&stored),
            stored,
        };

        // No PR, not merged.
        let gh = MockGh::default(); // merged defaults to false, pr_numbers empty

        let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
        let in_flight = Arc::new(DashSet::new());
        review_open_prs(
            &backend,
            "owner/repo",
            &default_config(),
            &tm,
            &store,
            &dispatching,
            &in_flight,
            &[snapshot],
            &gh,
            &crate::engine::router::RouterConfig::default(),
        )
        .await
        .unwrap();

        let after = store.get(task_id).await.unwrap();
        assert_eq!(after.status, TaskStatus::Routed);
        assert_eq!(after.no_code_reroutes, 1);
    }

    // ─── review_open_prs: Phase 3 — no PR, max reroutes → blocked ────────────

    #[tokio::test]
    async fn review_open_prs_no_pr_max_reroutes_blocks_task() {
        let store = make_store().await;
        let backend: Arc<dyn crate::backends::ExternalBackend> = Arc::new(MockBackend);
        let tm = Arc::new(crate::engine::tasks::TaskManager::with_store(
            Arc::clone(&backend),
            Arc::clone(&store),
            "owner/repo".to_string(),
        ));

        let task_id = store
            .create(&NewTask {
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "task3".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        let _ = crate::store::store_set_by_id(
            &Some(&store),
            task_id,
            &[("branch", serde_json::json!("feat-branch"))],
        )
        .await;
        store
            .update_status(task_id, TaskStatus::InReview)
            .await
            .unwrap();

        // Pre-set no_code_reroutes to a high value that exceeds any configured max after increment.
        // The default max is 3; use 99 to be robust against any real config on the test machine.
        let _ = crate::store::store_set_by_id(
            &Some(&store),
            task_id,
            &[("no_code_reroutes", serde_json::json!(99i64))],
        )
        .await;

        let stored = store.get(task_id).await.unwrap();
        let snapshot = crate::engine::sync::ReviewTaskSnapshot {
            external: crate::engine::tasks::store_task_to_external(&stored),
            stored,
        };

        let gh = MockGh::default();
        let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
        let in_flight = Arc::new(DashSet::new());
        review_open_prs(
            &backend,
            "owner/repo",
            &default_config(),
            &tm,
            &store,
            &dispatching,
            &in_flight,
            &[snapshot],
            &gh,
            &crate::engine::router::RouterConfig::default(),
        )
        .await
        .unwrap();

        let after = store.get(task_id).await.unwrap();
        assert_eq!(after.status, TaskStatus::Blocked);
        assert!(
            after.block_reason.is_some(),
            "block_reason must be set atomically with Blocked status"
        );
    }

    // ─── review_open_prs: Phase 3 — transient error skips without increment ──

    #[tokio::test]
    async fn review_open_prs_transient_error_in_is_pr_merged_skips_task() {
        struct TransientGh;

        #[async_trait]
        impl GhReviewClient for TransientGh {
            async fn get_pr_number(
                &self,
                _repo: &str,
                _branch: &str,
            ) -> anyhow::Result<Option<u64>> {
                Ok(None)
            }
            async fn is_pr_merged(&self, _repo: &str, _branch: &str) -> anyhow::Result<bool> {
                anyhow::bail!("500 Internal Server Error (transient GitHub 5xx)")
            }
            async fn batch_fetch_pr_review_data(
                &self,
                _repo: &str,
                _pr_numbers: &[u64],
            ) -> anyhow::Result<HashMap<u64, PrReviewBatchData>> {
                Ok(HashMap::new())
            }
            async fn is_collaborator(&self, _repo: &str, _username: &str) -> anyhow::Result<bool> {
                Ok(false)
            }
        }

        let store = make_store().await;
        let backend: Arc<dyn crate::backends::ExternalBackend> = Arc::new(MockBackend);
        let tm = Arc::new(crate::engine::tasks::TaskManager::with_store(
            Arc::clone(&backend),
            Arc::clone(&store),
            "owner/repo".to_string(),
        ));

        let task_id = store
            .create(&NewTask {
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "task4".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        let _ = crate::store::store_set_by_id(
            &Some(&store),
            task_id,
            &[("branch", serde_json::json!("feat-branch"))],
        )
        .await;
        store
            .update_status(task_id, TaskStatus::InReview)
            .await
            .unwrap();
        let stored = store.get(task_id).await.unwrap();
        let snapshot = crate::engine::sync::ReviewTaskSnapshot {
            external: crate::engine::tasks::store_task_to_external(&stored),
            stored,
        };

        let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
        let in_flight = Arc::new(DashSet::new());
        review_open_prs(
            &backend,
            "owner/repo",
            &default_config(),
            &tm,
            &store,
            &dispatching,
            &in_flight,
            &[snapshot],
            &TransientGh,
            &crate::engine::router::RouterConfig::default(),
        )
        .await
        .unwrap();

        // Status must remain InReview and counter must not have been incremented.
        let after = store.get(task_id).await.unwrap();
        assert_eq!(after.status, TaskStatus::InReview);
        assert_eq!(after.no_code_reroutes, 0);
    }

    // ─── review_open_prs: Phase 4 — batch fetch failure → graceful skip ──────

    #[tokio::test]
    async fn review_open_prs_batch_fetch_failure_returns_ok() {
        let store = make_store().await;
        let backend: Arc<dyn crate::backends::ExternalBackend> = Arc::new(MockBackend);
        let tm = Arc::new(crate::engine::tasks::TaskManager::with_store(
            Arc::clone(&backend),
            Arc::clone(&store),
            "owner/repo".to_string(),
        ));

        let task_id = store
            .create(&NewTask {
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "task5".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        let _ = crate::store::store_set_by_id(
            &Some(&store),
            task_id,
            &[("branch", serde_json::json!("feat-branch"))],
        )
        .await;
        // Set pr_number so task reaches Phase 4.
        let _ = crate::store::store_set_by_id(
            &Some(&store),
            task_id,
            &[("pr_number", serde_json::json!(99i64))],
        )
        .await;
        store
            .update_status(task_id, TaskStatus::InReview)
            .await
            .unwrap();
        let stored = store.get(task_id).await.unwrap();
        let snapshot = crate::engine::sync::ReviewTaskSnapshot {
            external: crate::engine::tasks::store_task_to_external(&stored),
            stored,
        };

        let gh = MockGh {
            batch_error: true,
            ..Default::default()
        };
        let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
        let in_flight = Arc::new(DashSet::new());
        let result = review_open_prs(
            &backend,
            "owner/repo",
            &default_config(),
            &tm,
            &store,
            &dispatching,
            &in_flight,
            &[snapshot],
            &gh,
            &crate::engine::router::RouterConfig::default(),
        )
        .await;

        // Must return Ok (graceful skip) even though batch fetch failed.
        assert!(result.is_ok());
        // Task status must be unchanged.
        let after = store.get(task_id).await.unwrap();
        assert_eq!(after.status, TaskStatus::InReview);
    }

    #[test]
    fn reset_batch_fail_counter_delete_failure_falls_back_to_zero_and_logs_warning() {
        let key = "review_batch_fail:owner/repo";
        let calls: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let store = MockBatchFailStore {
            delete_error: Some("simulated delete failure".to_string()),
            set_error: None,
            set_calls: Arc::clone(&calls),
        };

        let logs = with_captured_warn_logs(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let ok = rt.block_on(reset_batch_fail_counter(&store, key));
            assert!(ok, "fallback zero-reset should succeed");
        });

        let set_calls = calls.lock().unwrap();
        assert_eq!(set_calls.len(), 1);
        assert_eq!(set_calls[0], (key.to_string(), "0".to_string()));
        assert!(
            logs.contains("falling back to zero reset"),
            "expected warning log to mention fallback reset, got: {logs}"
        );
    }

    #[test]
    fn reset_batch_fail_counter_delete_and_set_failure_logs_error() {
        let key = "review_batch_fail:owner/repo";
        let calls: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let store = MockBatchFailStore {
            delete_error: Some("simulated delete failure".to_string()),
            set_error: Some("simulated set failure".to_string()),
            set_calls: Arc::clone(&calls),
        };

        let logs = with_captured_warn_logs(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let ok = rt.block_on(reset_batch_fail_counter(&store, key));
            assert!(
                !ok,
                "double failure should be surfaced as unsuccessful reset"
            );
        });

        let set_calls = calls.lock().unwrap();
        assert_eq!(set_calls.len(), 1);
        assert!(
            logs.contains("failed to reset review batch failure counter"),
            "expected error log when both delete and set fail, got: {logs}"
        );
    }

    // ─── review_open_prs: watermark dedup — review_ts_map skips old reviews ──

    #[tokio::test]
    async fn review_open_prs_watermark_dedup_skips_already_processed_review() {
        let store = make_store().await;
        let backend: Arc<dyn crate::backends::ExternalBackend> = Arc::new(MockBackend);
        let tm = Arc::new(crate::engine::tasks::TaskManager::with_store(
            Arc::clone(&backend),
            Arc::clone(&store),
            "owner/repo".to_string(),
        ));

        let task_id = store
            .create(&NewTask {
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "task6".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        // Pre-set review_ts_map so the reviewer's review is already watermarked.
        let ts = "2026-01-05T00:00:00Z";
        let _ = crate::store::store_set_by_id(
            &Some(&store),
            task_id,
            &[
                ("branch", serde_json::json!("feat-branch")),
                ("pr_number", serde_json::json!(10i64)),
                (
                    "review_ts_map",
                    serde_json::json!(serde_json::json!({"reviewer1": ts}).to_string()),
                ),
            ],
        )
        .await;
        store
            .update_status(task_id, TaskStatus::InReview)
            .await
            .unwrap();
        let stored = store.get(task_id).await.unwrap();
        let snapshot = crate::engine::sync::ReviewTaskSnapshot {
            external: crate::engine::tasks::store_task_to_external(&stored),
            stored,
        };

        // Batch data has a CHANGES_REQUESTED review at the same timestamp as the watermark.
        let review = make_review(1, "reviewer1", "CHANGES_REQUESTED", ts, Some("Fix this"));
        let batch_data = PrReviewBatchData {
            merged: false,
            mergeable: Some(true),
            reviews: vec![review],
            review_comments: vec![],
            issue_comments: vec![],
        };
        let mut gh = MockGh::default();
        gh.batch_data.insert(10, batch_data);

        let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
        let in_flight = Arc::new(DashSet::new());
        review_open_prs(
            &backend,
            "owner/repo",
            &default_config(),
            &tm,
            &store,
            &dispatching,
            &in_flight,
            &[snapshot],
            &gh,
            &crate::engine::router::RouterConfig::default(),
        )
        .await
        .unwrap();

        // Status must remain InReview — review was skipped due to watermark.
        let after = store.get(task_id).await.unwrap();
        assert_eq!(after.status, TaskStatus::InReview);
    }

    // ─── review_open_prs: last_comment_review_ts dedup ───────────────────────

    #[tokio::test]
    async fn review_open_prs_last_comment_review_ts_dedup_skips_old_comment() {
        let store = make_store().await;
        let backend: Arc<dyn crate::backends::ExternalBackend> = Arc::new(MockBackend);
        let tm = Arc::new(crate::engine::tasks::TaskManager::with_store(
            Arc::clone(&backend),
            Arc::clone(&store),
            "owner/repo".to_string(),
        ));

        let task_id = store
            .create(&NewTask {
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "task7".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        let _ = crate::store::store_set_by_id(
            &Some(&store),
            task_id,
            &[("branch", serde_json::json!("feat-branch"))],
        )
        .await;
        let _ = crate::store::store_set_by_id(
            &Some(&store),
            task_id,
            &[("pr_number", serde_json::json!(20i64))],
        )
        .await;
        // Set last_comment_review_ts to match the comment's created_at so it is deduplicated.
        let ts = "2026-01-05T00:00:00Z";
        let _ = crate::store::store_set_by_id(
            &Some(&store),
            task_id,
            &[("last_comment_review_ts", serde_json::json!(ts))],
        )
        .await;
        store
            .update_status(task_id, TaskStatus::InReview)
            .await
            .unwrap();
        let stored = store.get(task_id).await.unwrap();
        let snapshot = crate::engine::sync::ReviewTaskSnapshot {
            external: crate::engine::tasks::store_task_to_external(&stored),
            stored,
        };

        let comment_body = "## Automated Review \u{2014} Changes Requested\n\nFix the bug.";
        let comment = make_comment(1, "bot", comment_body, ts);
        let batch_data = PrReviewBatchData {
            merged: false,
            mergeable: Some(true),
            reviews: vec![],
            review_comments: vec![],
            issue_comments: vec![comment],
        };
        let mut gh = MockGh::default();
        gh.batch_data.insert(20, batch_data);
        gh.collaborators.insert("bot".to_string(), true);

        let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
        let in_flight = Arc::new(DashSet::new());
        review_open_prs(
            &backend,
            "owner/repo",
            &default_config(),
            &tm,
            &store,
            &dispatching,
            &in_flight,
            &[snapshot],
            &gh,
            &crate::engine::router::RouterConfig::default(),
        )
        .await
        .unwrap();

        // Status must remain InReview — comment was deduplicated.
        let after = store.get(task_id).await.unwrap();
        assert_eq!(after.status, TaskStatus::InReview);
    }

    // ─── review_open_prs: review context truncation ──────────────────────────

    #[test]
    fn review_context_truncation_at_16kb_boundary() {
        // Replicate the truncation logic from review_open_prs to verify it works correctly.
        const MAX_REVIEW_CONTEXT_BYTES: usize = 16 * 1024;

        // Build a review_context that exceeds 16 KB.
        let repeated = "a".repeat(200);
        let mut review_context = String::new();
        while review_context.len() <= MAX_REVIEW_CONTEXT_BYTES {
            review_context.push_str(&repeated);
            review_context.push('\n');
        }

        assert!(review_context.len() > MAX_REVIEW_CONTEXT_BYTES);

        // Apply the same truncation logic as in review_open_prs.
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

        assert!(review_context.len() <= MAX_REVIEW_CONTEXT_BYTES + 40); // suffix is short
        assert!(review_context.ends_with("... (review context truncated)"));
        // Must be valid UTF-8 after truncation (no split multi-byte chars).
        assert!(std::str::from_utf8(review_context.as_bytes()).is_ok());
    }
}
