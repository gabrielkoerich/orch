//! Auto-unblock and recovery sweeps for blocked tasks

use super::classify_failure_from_run;
use crate::backends::ExternalId;
use crate::backends::Status;
use crate::config;
use crate::engine::auto_merge::{CI_FAILURE_AUTO_MERGE_STAGE, is_pr_behind, required_checks_state};
use crate::engine::cleanup::cleanup_task_worktree;
use crate::engine::review;
use crate::engine::router::Router;
use crate::engine::tasks::TaskManager;
use crate::github::http::GhHttp;
use crate::store::Task;
use crate::store::TaskStatus;
use crate::store::TaskStore;
use dashmap::{DashMap, DashSet};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio::time;

fn auto_unblock_cooldown_elapsed(count: i32, last_at: &str) -> bool {
    if count == 0 {
        return true;
    }

    let required = match count {
        1 => chrono::Duration::minutes(30),
        2 => chrono::Duration::hours(2),
        _ => return false,
    };

    if last_at.trim().is_empty() {
        return true;
    }

    match chrono::DateTime::parse_from_rfc3339(last_at) {
        Ok(ts) => chrono::Utc::now() - ts.with_timezone(&chrono::Utc) >= required,
        Err(_) => true,
    }
}

/// True when a blocked task reached auto-merge and CI failed there.
/// Pre-review and bootstrap CI-failure reasons share the "CI failure limit" words
/// and must stay blocked for a person (#4140)
pub(super) fn is_ci_failure_block(task: &Task) -> bool {
    let reason = task.block_reason.as_deref().unwrap_or("");
    if reason.contains("CI checks timed out") {
        return true;
    }
    reason.contains("CI failure limit") && reason.contains(CI_FAILURE_AUTO_MERGE_STAGE)
}

pub(super) fn ci_failure_unblock_cooldown_elapsed(task: &Task) -> bool {
    if task.auto_unblock_count == 0 {
        return true;
    }

    // Short cooldowns — the PR may close quickly after the first check.
    // We never permanently block (no `_ => return false`): eventually the PR
    // will close or a human will intervene.
    let required = match task.auto_unblock_count {
        1 => chrono::Duration::minutes(10),
        2 => chrono::Duration::hours(1),
        3 => chrono::Duration::hours(6),
        _ => chrono::Duration::hours(24),
    };

    let last_at = &task.auto_unblock_last_at;
    if last_at.trim().is_empty() {
        return true;
    }

    match chrono::DateTime::parse_from_rfc3339(last_at) {
        Ok(ts) => chrono::Utc::now() - ts.with_timezone(&chrono::Utc) >= required,
        Err(_) => true,
    }
}

pub(super) async fn try_unblock_ci_failure_task(
    gh: &GhHttp,
    repo: &str,
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
    task: &Task,
    auto_merge_in_flight: &Arc<DashSet<String>>,
) -> anyhow::Result<bool> {
    let Some(pr_number) = task.pr_number.and_then(|n| u64::try_from(n).ok()) else {
        tracing::debug!(
            task_id = task.id,
            "CI failure blocked task has no PR number"
        );
        return Ok(false);
    };

    let pr = match gh.get_pr(repo, pr_number).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(task_id = task.id, pr_number, err = %e, "failed to get PR status for CI failure block");
            return Ok(false);
        }
    };

    tracing::debug!(
        task_id = task.id,
        pr_number,
        merged = pr.merged,
        state = ?pr.state,
        "verifying PR state for CI failure blocked task"
    );

    if !pr.merged.unwrap_or(false) && pr.state.eq_ignore_ascii_case("closed") {
        tracing::info!(
            task_id = task.id,
            pr_number,
            "PR closed unmerged — leaving CI failure blocked task blocked"
        );
        let new_reason = format!(
            "CI failure limit reached (PR #{pr_number} closed unmerged — no further auto-recovery)"
        );
        if task.block_reason.as_deref() != Some(new_reason.as_str()) {
            let fields = vec![("block_reason", serde_json::json!(new_reason))];
            store.set_fields_silent(task.id, &fields).await?;
        }
        return Ok(false);
    }

    if pr.merged.unwrap_or(false) {
        tracing::info!(
            task_id = task.id,
            pr_number,
            "PR merged — unblocking CI failure blocked task"
        );
        let ext_id = task
            .external_id
            .clone()
            .unwrap_or_else(|| format!("internal:{}", task.id));
        // Update the store directly by the already-resolved numeric id instead of going
        // through `task_manager.update_task_status`, which resolves by `self.repo` — this
        // sweep runs cross-repo, so `task.repo` may not match the caller's own repo scope.
        task_manager
            .mark_task_done_by_store_id(task.id, &task.repo, &ExternalId(ext_id))
            .await?;
        let fields = vec![
            ("block_reason", serde_json::Value::Null),
            ("auto_unblock_count", serde_json::json!(0)),
            ("auto_unblock_last_at", serde_json::json!("")),
            ("auto_unblock_last_reason", serde_json::json!("")),
        ];
        store.set_fields(task.id, &fields).await?;
        return Ok(true);
    }

    // Skip if a recovery merge is already in flight for this task. The poll loop
    // can take up to 10 minutes, and the same task may be seen by both the per-repo
    // sync_tick and the global sweep in the same tick window.
    let task_id_str = task
        .external_id
        .clone()
        .unwrap_or_else(|| format!("internal:{}", task.id));
    if auto_merge_in_flight.contains(&task_id_str) {
        tracing::debug!(
            task_id = task.id,
            pr_number,
            "CI-failure recovery merge already in flight — skipping"
        );
        return Ok(false);
    }

    // Recovery was built for approved PRs. A pre-review CI block can share a
    // rewritten "during auto-merge" reason, so require a recorded approval
    if !review::has_recorded_approval(store, repo, &task_id_str, pr_number).await {
        tracing::info!(
            task_id = task.id,
            pr_number,
            "CI-failure recovery skipped, PR has no approving review"
        );
        return Ok(false);
    }

    // Give the PR an active chance to recover: merge the base branch into the PR
    // branch to re-trigger CI, then re-enable GitHub auto-merge. GitHub's queue
    // can silently stall even when checks are green and the branch is up to date,
    // so after arming auto-merge we poll for completion and fall back to a direct
    // merge once the PR is mergeable and CI is passing.
    let branch_update_result = gh.update_pr_branch(repo, pr_number).await;
    match branch_update_result {
        Ok(()) => {
            tracing::info!(
                task_id = task.id,
                pr_number,
                "updated CI-failure-blocked PR branch against base to retrigger CI"
            );
        }
        Err(ref e) => {
            // warn, not debug, so a silently-broken corrective retry doesn't look like a working one (#3561)
            tracing::warn!(
                task_id = task.id,
                pr_number,
                err = %e,
                outcome = "branch_update_failed",
                "failed to update CI-failure-blocked PR branch (may already be up to date)"
            );
        }
    }

    if let Err(e) = gh.enable_auto_merge(repo, pr_number).await {
        tracing::warn!(task_id = task.id, pr_number, err = %e, "failed to re-enable auto-merge after branch update");
    }

    // Record this attempt so the cooldown advances even though the actual
    // poll/merge now runs in the background.
    let pr_state = pr.state.as_str();
    let current_reason = task.block_reason.as_deref().unwrap_or("");
    let rewrite_reason = current_reason.contains(CI_FAILURE_AUTO_MERGE_STAGE);
    let new_reason = if let Err(ref e) = branch_update_result {
        format!(
            "CI failure limit reached {CI_FAILURE_AUTO_MERGE_STAGE} (PR #{pr_number} still open, state: {pr_state}, branch update failed: {e})"
        )
    } else {
        format!(
            "CI failure limit reached {CI_FAILURE_AUTO_MERGE_STAGE} (PR #{pr_number} still open, state: {pr_state})"
        )
    };
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    // Increment auto_unblock_count so the cooldown backoff (24h, 3d, never) kicks in
    // and we don't hammer the GitHub API every tick for a PR that stays open.
    let new_count = task.auto_unblock_count.saturating_add(1);
    let mut fields = Vec::new();
    if rewrite_reason {
        fields.push(("block_reason", serde_json::json!(&new_reason)));
    }
    fields.push(("auto_unblock_count", serde_json::json!(new_count)));
    fields.push(("auto_unblock_last_at", serde_json::json!(&now)));
    // Use set_fields_silent so the probe write does not bump updated_at on stale
    // blocked tasks — triage queries keyed off updated_at must only see real
    // status/result changes, not periodic re-checks of an open PR.
    store.set_fields_silent(task.id, &fields).await?;

    // Spawn the poll and direct-merge fallback in the background so the
    // recovery sweep does not block the main tick loop (or sync_tick) while
    // CI runs. The in-flight guard prevents double-spawns across ticks.
    auto_merge_in_flight.insert(task_id_str.clone());
    let gh_clone = gh.clone();
    let repo_string = repo.to_string();
    let task_manager_clone = Arc::clone(task_manager);
    let store_clone = Arc::clone(store);
    let task_clone = task.clone();
    let in_flight_clone = Arc::clone(auto_merge_in_flight);

    tokio::spawn(async move {
        let _guard = scopeguard::guard((), |()| {
            in_flight_clone.remove(&task_id_str);
        });

        match poll_and_merge_recovered_pr(&gh_clone, &repo_string, pr_number, task_clone.id).await {
            Ok(true) => {
                tracing::info!(
                    task_id = task_clone.id,
                    pr_number,
                    "CI-failure-blocked PR merged during recovery sweep"
                );
                let ext_id = task_clone
                    .external_id
                    .clone()
                    .unwrap_or_else(|| format!("internal:{}", task_clone.id));
                let fields = [
                    ("block_reason", serde_json::Value::Null),
                    ("auto_unblock_count", serde_json::json!(0)),
                    ("auto_unblock_last_at", serde_json::json!("")),
                    ("auto_unblock_last_reason", serde_json::json!("")),
                ];
                if let Err(e) = task_manager_clone
                    .update_task_status_and_result_by_store_id(
                        task_clone.id,
                        &task_clone.repo,
                        &ExternalId(ext_id),
                        Status::Done,
                        &fields,
                    )
                    .await
                {
                    tracing::warn!(
                        task_id = task_clone.id,
                        pr_number,
                        err = %e,
                        "failed to mark CI-failure-blocked task done after recovery merge"
                    );
                }
                if let Err(e) =
                    cleanup_task_worktree(&format!("{}", task_clone.id), &repo_string, &store_clone)
                        .await
                {
                    tracing::warn!(
                        task_id = task_clone.id,
                        pr_number,
                        err = %e,
                        "cleanup after CI-failure recovery merge failed"
                    );
                }
            }
            Ok(false) => {
                tracing::debug!(
                    task_id = task_clone.id,
                    pr_number,
                    "CI-failure-blocked PR not merged this recovery tick — poll completed without merge"
                );
            }
            Err(e) => {
                tracing::warn!(
                    task_id = task_clone.id,
                    pr_number,
                    err = %e,
                    "error while polling CI-failure-blocked PR for recovery"
                );
            }
        }
    });

    Ok(false)
}

/// Poll a recovered CI-failure PR until it merges or its checks settle, then
/// attempt a direct merge if GitHub's auto-merge queue has not completed it.
///
/// GitHub's `enablePullRequestAutoMerge` hands the PR off to GitHub's queue and
/// returns immediately; the queue can stall silently. This helper mirrors the
/// primary `auto_merge_pr` polling pattern but is scoped to the recovery sweep:
/// it waits for required checks to pass, verifies the PR is not behind the base
/// branch, and calls `gh.merge_pr()` directly when the PR is ready but still
/// open. Returns `Ok(true)` only when the PR is merged, `Ok(false)` when the
/// PR is closed unmerged or needs to wait for a future tick.
async fn poll_and_merge_recovered_pr(
    gh: &GhHttp,
    repo: &str,
    pr_number: u64,
    task_id: i64,
) -> anyhow::Result<bool> {
    // The branch can fall behind while CI runs. Retry the update-branch call a
    // few times inside the poll loop instead of exiting and waiting for the next
    // 24-hour recovery cooldown.
    const MAX_BEHIND_RETRIES: u32 = 3;

    let max_wait_secs: u64 = config::get("workflow.ci_poll_max_wait_secs")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(600);
    let base_interval_secs: u64 = config::get("workflow.ci_poll_interval_secs")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);
    let max_wait = Duration::from_secs(max_wait_secs);
    let start = Instant::now();
    let mut poll_count: u32 = 0;
    let mut behind_retries: u32 = 0;

    let initial_pr = gh.get_pr(repo, pr_number).await?;
    let base_branch = initial_pr.base.ref_.clone();
    let required_contexts = gh
        .get_required_status_check_contexts(repo, &base_branch)
        .await?;
    let repo_has_workflows = if required_contexts.is_empty() {
        gh.has_workflows(repo).await?
    } else {
        true
    };

    loop {
        let pr = gh.get_pr(repo, pr_number).await?;
        if pr.merged.unwrap_or(false) {
            tracing::info!(
                task_id = task_id,
                pr_number,
                "PR merged during recovery poll"
            );
            return Ok(true);
        }
        if pr.state.eq_ignore_ascii_case("closed") {
            tracing::info!(
                task_id = task_id,
                pr_number,
                "PR closed unmerged during recovery poll — leaving task blocked"
            );
            return Ok(false);
        }

        if pr.mergeable == Some(false) {
            tracing::info!(
                task_id = task_id,
                pr_number,
                outcome = "poll_exited_without_merge",
                reason = "merge_conflicts",
                "PR has merge conflicts after recovery update — leaving for next recovery cycle"
            );
            return Ok(false);
        }

        if is_pr_behind(&pr) {
            if behind_retries >= MAX_BEHIND_RETRIES {
                tracing::info!(
                    task_id = task_id,
                    pr_number,
                    behind_retries,
                    max_behind_retries = MAX_BEHIND_RETRIES,
                    outcome = "poll_exited_without_merge",
                    reason = "behind_retry_exhausted",
                    "PR still behind base after recovery retries — leaving for next recovery cycle"
                );
                return Ok(false);
            }
            behind_retries += 1;
            match gh.update_pr_branch(repo, pr_number).await {
                Ok(()) => {
                    tracing::info!(
                        task_id = task_id,
                        pr_number,
                        behind_retries,
                        "PR was behind base — updated branch and continuing recovery poll"
                    );
                    continue;
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = task_id,
                        pr_number,
                        behind_retries,
                        err = %e,
                        outcome = "branch_update_failed",
                        "failed to update behind PR branch during recovery poll"
                    );
                    return Ok(false);
                }
            }
        }

        let head_sha = pr.head.sha.clone();
        let (state, total, passing, failing, pending) = if required_contexts.is_empty() {
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
            required_checks_state(&required_contexts, &check_runs, &statuses)
        };

        tracing::info!(
            task_id = task_id,
            pr_number,
            state = %state,
            total,
            passing,
            failing,
            pending,
            "CI recovery status check"
        );

        match state.as_str() {
            "success" => {
                tracing::info!(
                    task_id = task_id,
                    pr_number,
                    "CI green after recovery — attempting direct merge"
                );
                match gh.merge_pr(repo, pr_number, true, &head_sha).await {
                    Ok(()) => {
                        tracing::info!(
                            task_id = task_id,
                            pr_number,
                            "direct merge succeeded after recovery"
                        );
                        return Ok(true);
                    }
                    Err(e) => {
                        let err = e.to_string().to_ascii_lowercase();
                        tracing::warn!(
                            task_id = task_id,
                            pr_number,
                            err = %e,
                            "direct merge failed after recovery"
                        );
                        // If the PR became merged between the get_pr call and the
                        // merge attempt, treat it as success rather than a failed tick.
                        if err.contains("already merged")
                            || err.contains("no merge commit")
                            || err.contains("pull request already merged")
                        {
                            return Ok(true);
                        }
                        // The head moved after we read it, and updating the branch would skip CI on the new commit
                        if err.contains("head branch was modified")
                            || err.contains("sha does not match")
                        {
                            tracing::info!(
                                task_id = task_id,
                                pr_number,
                                outcome = "poll_exited_without_merge",
                                reason = "head_moved",
                                "merge rejected because the head sha changed"
                            );
                            return Ok(false);
                        }
                        // GitHub may reject the merge because the branch fell behind
                        // again while CI was finishing. Retry the branch update inside
                        // the poll window instead of waiting for the next sweep.
                        if (err.contains("head branch is out of date")
                            || err.contains("behind")
                            || err.contains("409"))
                            && behind_retries < MAX_BEHIND_RETRIES
                        {
                            behind_retries += 1;
                            match gh.update_pr_branch(repo, pr_number).await {
                                Ok(()) => {
                                    tracing::info!(
                                        task_id = task_id,
                                        pr_number,
                                        behind_retries,
                                        "merge rejected due to out-of-date branch — updated and continuing recovery poll"
                                    );
                                    continue;
                                }
                                Err(update_err) => {
                                    tracing::warn!(
                                        task_id = task_id,
                                        pr_number,
                                        behind_retries,
                                        err = %update_err,
                                        outcome = "branch_update_failed",
                                        "failed to update out-of-date PR branch during recovery poll"
                                    );
                                }
                            }
                        }
                        tracing::info!(
                            task_id = task_id,
                            pr_number,
                            outcome = "poll_exited_without_merge",
                            reason = "direct_merge_failed",
                            "direct merge did not succeed — leaving for next recovery cycle"
                        );
                        return Ok(false);
                    }
                }
            }
            "failure" => {
                tracing::info!(
                    task_id = task_id,
                    pr_number,
                    outcome = "poll_exited_without_merge",
                    reason = "ci_failure",
                    "CI still failing after recovery — will retry later"
                );
                return Ok(false);
            }
            _ => {}
        }

        if start.elapsed() >= max_wait {
            tracing::info!(
                task_id = task_id,
                pr_number,
                outcome = "poll_exited_without_merge",
                reason = "poll_timeout",
                "CI still pending after recovery poll timeout — will retry later"
            );
            return Ok(false);
        }

        poll_count += 1;
        let multiplier = (2_u64).pow(poll_count.saturating_sub(1)).min(4);
        let sleep_secs = base_interval_secs.saturating_mul(multiplier);
        time::sleep(Duration::from_secs(sleep_secs)).await;
    }
}

/// Exact `block_reason` written when a task's NeedsReview refire counter exhausts
/// during a rebroadcast escalation (`refire_and_escalate_stale_needs_review_global`).
/// Shared with `auto_recover_rebroadcast_blocked_tasks_global` so escalation and
/// recovery never drift apart on the literal string
pub(super) const REBROADCAST_BLOCK_REASON: &str =
    "review agent rebroadcast escalated after repeated retries";

/// Global sweep: auto-recover rebroadcast-blocked tasks across all repos.
///
/// When a NeedsReview refire counter exhausts, the task is blocked with
/// `REBROADCAST_BLOCK_REASON`. Once any review agent is routable again, this
/// sweep resets matching tasks to `NeedsReview` so the subscriber can pick them up.
/// Writes through the store via `*_by_store_id` helpers, not a repo-scoped
/// `TaskManager`. Called once per main tick from `tick_global_sweeps`.
///
/// Before resetting, the attached PR's live state is checked (mirroring
/// `try_unblock_ci_failure_task`): merged means the task is marked done, closed
/// unmerged means it stays blocked with an annotated reason, and only an open PR
/// (or no PR) falls through to `NeedsReview`. A 5-minute block age prevents
/// same-tick escalate/recover flapping. Reset-to-NeedsReview still requires at
/// least one routable review agent
pub(crate) async fn auto_recover_rebroadcast_blocked_tasks_global(
    router: &Router,
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
    active_repos: &HashSet<String>,
) -> anyhow::Result<()> {
    const MIN_BLOCK_AGE_MINUTES: i64 = 5;

    let all_blocked = match store.list_all_by_status_global(TaskStatus::Blocked).await {
        Ok(tasks) => tasks,
        Err(e) => {
            tracing::warn!(err = %e, "global rebroadcast-blocked sweep: failed to list blocked tasks");
            return Ok(());
        }
    };

    let now = chrono::Utc::now();
    let candidates: Vec<_> = all_blocked
        .into_iter()
        .filter(|t| {
            if t.block_reason.as_deref() != Some(REBROADCAST_BLOCK_REASON) {
                return false;
            }
            // A repo outside the active set has no review subscriber, so a reset would loop
            if !active_repos.contains(&t.repo) {
                return false;
            }
            // Only recover tasks blocked long enough to rule out same-tick escalations.
            match chrono::DateTime::parse_from_rfc3339(&t.updated_at) {
                Ok(updated_at) => {
                    let age = now - updated_at.with_timezone(&chrono::Utc);
                    age.num_minutes() >= MIN_BLOCK_AGE_MINUTES
                }
                Err(_) => true,
            }
        })
        .collect();

    if candidates.is_empty() {
        return Ok(());
    }

    let gh = match GhHttp::new() {
        Ok(g) => Some(g),
        Err(e) => {
            tracing::warn!(err = %e, "global rebroadcast-blocked sweep: failed to create GhHttp — skipping PR-state checks this tick");
            None
        }
    };

    // Skip reset-to-NeedsReview when no review agent is routable, re-firing
    // while cooled would immediately re-exhaust the counter
    let any_routable = router.has_routable_review_agent();

    for task in candidates {
        let ext_id = task
            .external_id
            .clone()
            .unwrap_or_else(|| format!("internal:{}", task.id));

        if let (Some(gh), Some(pr_number)) =
            (&gh, task.pr_number.and_then(|n| u64::try_from(n).ok()))
        {
            match gh.get_pr(&task.repo, pr_number).await {
                Ok(pr) => {
                    if pr.merged.unwrap_or(false) {
                        tracing::info!(
                            task_id = task.id,
                            repo = %task.repo,
                            pr_number,
                            "global rebroadcast-blocked sweep: PR already merged — marking task done"
                        );
                        if let Err(e) = task_manager
                            .mark_task_done_by_store_id(
                                task.id,
                                &task.repo,
                                &ExternalId(ext_id.clone()),
                            )
                            .await
                        {
                            tracing::warn!(task_id = task.id, repo = %task.repo, err = %e, "global rebroadcast-blocked sweep: failed to mark merged task done");
                            continue;
                        }
                        let fields: &[(&str, serde_json::Value)] = &[
                            ("block_reason", serde_json::Value::Null),
                            ("needs_review_refires", serde_json::json!(0)),
                        ];
                        if let Err(e) = store.set_fields(task.id, fields).await {
                            tracing::warn!(task_id = task.id, err = %e, "global rebroadcast-blocked sweep: failed to clear fields after marking done");
                        }
                        continue;
                    }
                    if pr.state.eq_ignore_ascii_case("closed") {
                        tracing::info!(
                            task_id = task.id,
                            repo = %task.repo,
                            pr_number,
                            "global rebroadcast-blocked sweep: PR closed unmerged — leaving task blocked, not re-dispatching"
                        );
                        let new_reason = format!(
                            "{REBROADCAST_BLOCK_REASON} (PR #{pr_number} closed unmerged — no further auto-recovery)"
                        );
                        let fields: &[(&str, serde_json::Value)] =
                            &[("block_reason", serde_json::json!(new_reason))];
                        // set_fields_silent: this is a terminal annotation, not a real
                        // status change — don't bump updated_at so triage queries keyed
                        // off it aren't confused by a task that stays blocked forever.
                        if let Err(e) = store.set_fields_silent(task.id, fields).await {
                            tracing::warn!(task_id = task.id, err = %e, "global rebroadcast-blocked sweep: failed to annotate closed-unmerged block reason");
                        }
                        continue;
                    }
                    // PR still open — fall through to the reset-to-NeedsReview path.
                }
                Err(e) => {
                    tracing::warn!(task_id = task.id, repo = %task.repo, pr_number, err = %e, "global rebroadcast-blocked sweep: failed to get PR status — skipping this tick");
                    continue;
                }
            }
        }

        if !any_routable {
            continue;
        }

        if let Err(e) = task_manager
            .update_task_status_by_store_id(
                task.id,
                &task.repo,
                &ExternalId(ext_id.clone()),
                Status::NeedsReview,
            )
            .await
        {
            tracing::warn!(task_id = task.id, repo = %task.repo, err = %e, "global rebroadcast-blocked sweep: failed to reset task to needs_review — skipping");
            continue;
        }

        let fields: &[(&str, serde_json::Value)] = &[
            ("block_reason", serde_json::Value::Null),
            ("needs_review_refires", serde_json::json!(0)),
        ];
        if let Err(e) = store.set_fields(task.id, fields).await {
            tracing::warn!(task_id = task.id, err = %e, "global rebroadcast-blocked sweep: failed to clear block_reason/needs_review_refires after status reset");
        }

        tracing::info!(
            task_id = task.id,
            repo = %task.repo,
            ext_id = %ext_id,
            "global rebroadcast-blocked sweep: auto-recovered rebroadcast-blocked task — review agents now available"
        );
    }

    Ok(())
}

pub(super) async fn auto_unblock_blocked_tasks(
    repo: &str,
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
    dispatching: &Arc<DashMap<String, String>>,
    auto_merge_in_flight: &Arc<DashSet<String>>,
) -> anyhow::Result<()> {
    let blocked = match store.list_by_status(repo, TaskStatus::Blocked).await {
        Ok(tasks) => tasks,
        Err(e) => {
            tracing::warn!(err = %e, "failed to list blocked tasks — skipping auto-unblock this tick");
            return Ok(());
        }
    };

    if blocked.is_empty() {
        return Ok(());
    }

    // Batch-load all runs for blocked tasks in a single query instead of N individual queries.
    let task_ids: Vec<i64> = blocked.iter().map(|t| t.id).collect();
    let mut runs_by_task = match store.get_runs_batch(&task_ids).await {
        Ok(map) => map,
        Err(e) => {
            tracing::warn!(err = %e, "get_runs_batch failed — skipping auto-unblock this tick");
            return Ok(());
        }
    };

    // Separate CI-failure blocked tasks from other blocked tasks.
    let (ci_failure_tasks, other_blocked): (Vec<_>, Vec<_>) =
        blocked.into_iter().partition(is_ci_failure_block);

    // Handle CI-failure blocked tasks by verifying PR state.
    if !ci_failure_tasks.is_empty() {
        let gh = match GhHttp::new() {
            Ok(g) => Some(g),
            Err(e) => {
                tracing::warn!(err = %e, "failed to create GhHttp — skipping CI failure unblock this tick");
                None
            }
        };
        for task in ci_failure_tasks {
            if ci_failure_unblock_cooldown_elapsed(&task)
                && let Some(ref gh) = gh
                && let Err(e) = try_unblock_ci_failure_task(
                    gh,
                    repo,
                    task_manager,
                    store,
                    &task,
                    auto_merge_in_flight,
                )
                .await
            {
                tracing::warn!(task_id = task.id, err = %e, "CI failure unblock check failed");
            }
        }
    }

    // Process regular blocked tasks (those without a block_reason).
    for task in other_blocked {
        if task.block_reason.is_some() {
            continue;
        }

        let task_id_str = task
            .external_id
            .clone()
            .unwrap_or_else(|| format!("internal:{}", task.id));
        let dispatch_key = format!("{repo}/{task_id_str}");
        if dispatching.contains_key(&dispatch_key) {
            continue;
        }

        let runs = runs_by_task.remove(&task.id).unwrap_or_default();
        let relevant_runs: Vec<_> = runs
            .into_iter()
            .filter(|r| r.run_type == "agent" || r.run_type == "review")
            .collect();
        let mut failures = Vec::new();
        let mut has_review_failure = false;
        for run in relevant_runs.iter().rev() {
            if let Some(category) = classify_failure_from_run(run) {
                if run.run_type == "review" {
                    has_review_failure = true;
                }
                failures.push(category);
            }
            if failures.len() >= 3 {
                break;
            }
        }

        if failures.is_empty() || !failures.iter().all(|f| f.is_recoverable()) {
            continue;
        }

        // Determine the most recent failure category as the "reason key" for this unblock.
        let reason_key = failures.first().map_or("", |f| f.as_str());

        // Detect reason changes to reset exponential backoff.
        // New reasons bypass old cooldown windows and start fresh attempt counters.
        let current_reason = task.auto_unblock_last_reason.clone();
        let reason_changed = reason_key != current_reason;

        // Compute effective counter: when the reason changes, start from 0;
        // otherwise use the accumulated count for exponential backoff.
        let cooldown_count = if reason_changed {
            0
        } else {
            task.auto_unblock_count
        };

        // Gate on attempt count using the effective count — if the reason changed, the
        // counter was reset above so cooldown_count is 0 and a new reason always gets a
        // fresh attempt regardless of how many times the old reason fired.
        if cooldown_count >= 3 {
            continue;
        }

        if !auto_unblock_cooldown_elapsed(cooldown_count, &task.auto_unblock_last_at) {
            continue;
        }

        // Compute the new counter value: when reason changed, cooldown_count is 0,
        // so incrementing gives 1 (first attempt for the new reason). When the reason is the
        // same, advance from current count for exponential backoff.
        let new_count = cooldown_count + 1;
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

        let ext_id = task
            .external_id
            .clone()
            .unwrap_or_else(|| format!("internal:{}", task.id));
        let new_status = if has_review_failure {
            Status::NeedsReview
        } else {
            Status::New
        };
        let new_status_str = if has_review_failure {
            "needs_review"
        } else {
            "new"
        };

        // First update the status; only increment the counter if this succeeds.
        // This prevents transient GitHub API errors from exhausting the retry quota.
        if let Err(e) = task_manager
            .update_task_status(&ExternalId(ext_id), new_status)
            .await
        {
            tracing::warn!(
                task_id = task.id,
                err = %e,
                "auto-unblock failed to update status — counter not incremented"
            );
            continue;
        }

        // Status update succeeded; now record the counter increment and clear related fields.
        let mut fields: Vec<(&str, serde_json::Value)> = vec![
            ("auto_unblock_count", serde_json::json!(new_count)),
            ("auto_unblock_last_at", serde_json::json!(now)),
            ("auto_unblock_last_reason", serde_json::json!(reason_key)),
            ("agent", serde_json::Value::Null),
            ("model", serde_json::Value::Null),
        ];
        fields.push(("no_code_reroutes", serde_json::json!(0)));
        fields.push(("no_code_last_agent", serde_json::json!("")));
        if has_review_failure {
            fields.push(("review_agent_failures", serde_json::json!(0)));
            fields.push(("review_invocations", serde_json::json!(0)));
            fields.push(("review_resets", serde_json::json!(0)));
        }
        if let Err(e) = store.set_fields(task.id, &fields).await {
            tracing::warn!(task_id = task.id, err = %e, "failed to set auto_unblock fields after successful status update");
            // Task is unblocked (correct behavior) but counter hasn't advanced.
            // Slightly over-eager retries are better than permanent task stalls.
        }

        if let Err(e) = store
            .append_activity(
                task.id,
                "auto_unblock",
                Some("blocked"),
                Some(new_status_str),
                None,
                None,
                Some(&serde_json::json!({
                    "reason": reason_key,
                    "count": if reason_changed { 1 } else { task.auto_unblock_count + 1 },
                    "failures": failures.iter().map(|f| format!("{f:?}")).collect::<Vec<_>>(),
                })),
            )
            .await
        {
            tracing::warn!(task_id = task.id, err = %e, "failed to log auto_unblock activity");
        }

        tracing::info!(
            task_id = task.id,
            failures = ?failures,
            review_failure = has_review_failure,
            "auto-unblocked task with recoverable failures"
        );
    }

    Ok(())
}

/// Global sweep for CI-failure blocked tasks from inactive or removed projects.
///
/// `auto_unblock_blocked_tasks` is called from `sync_tick` and only runs for the
/// currently-active repo. Tasks that were blocked for a project that is later removed
/// from orch config never get processed and accumulate in the DB indefinitely.
///
/// This function queries all blocked tasks across all repos and handles merge-time
/// CI-failure cases (`during auto-merge` or `CI checks timed out`) so they are
/// eventually cleaned up regardless of project activity. Pre-review CI-failure
/// blocks stay blocked for a person. Called once per main tick from
/// `tick_global_sweeps`.
pub(crate) async fn auto_unblock_ci_failure_blocked_tasks_global(
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
    auto_merge_in_flight: &Arc<DashSet<String>>,
) -> anyhow::Result<()> {
    let all_blocked = match store.list_all_by_status_global(TaskStatus::Blocked).await {
        Ok(tasks) => tasks,
        Err(e) => {
            tracing::warn!(err = %e, "global CI-failure blocked sweep: failed to list blocked tasks");
            return Ok(());
        }
    };

    let ci_failure_tasks: Vec<_> = all_blocked
        .into_iter()
        .filter(is_ci_failure_block)
        .collect();
    if ci_failure_tasks.is_empty() {
        return Ok(());
    }

    let gh = match GhHttp::new() {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!(err = %e, "global CI-failure blocked sweep: failed to create GhHttp");
            return Ok(());
        }
    };

    for task in ci_failure_tasks {
        if !ci_failure_unblock_cooldown_elapsed(&task) {
            continue;
        }
        if let Err(e) = try_unblock_ci_failure_task(
            &gh,
            &task.repo,
            task_manager,
            store,
            &task,
            auto_merge_in_flight,
        )
        .await
        {
            tracing::warn!(
                task_id = task.id,
                repo = %task.repo,
                err = %e,
                "global CI-failure blocked sweep: unblock check failed"
            );
        }
    }

    Ok(())
}
