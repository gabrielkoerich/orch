//! Opening the PR a review needs

use super::{
    MAX_PR_CREATE_FAILURES, ReviewDecision, parse_pr_number_from_url,
    should_skip_no_code_reroute_increment,
};
/// Maximum consecutive review agent failures before the task is blocked
/// for human intervention. Exported so `tick` and `sync` use the same threshold.
use crate::backends::{ExternalTask, Status};
use crate::config;
use crate::engine::ci_bootstrap;
use crate::engine::runner;
use crate::engine::runner::git_ops;
use crate::engine::runner::worktree;
use crate::engine::tasks::TaskManager;
use crate::github::http::GhHttp;
use crate::store::TaskStore;
use crate::store::store_set_result;
use crate::store::{opt_store_get_task, store_increment};
use anyhow::Context;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::time;

/// Outcome of [`ensure_pr_exists`]: either a ready PR number or an early return decision.
pub(super) enum EnsurePrResult {
    /// PR exists (found in store or via API, or just created). Proceed with review.
    Ready(u64),
    /// No PR and status already updated — return this decision to the caller.
    EarlyReturn(ReviewDecision),
}

pub(super) async fn recover_missing_review_worktree(
    task: &ExternalTask,
    repo: &str,
    store: &Arc<TaskStore>,
) -> anyhow::Result<worktree::WorktreeSetup> {
    let project_dir = config::get_projects_with_paths()?
        .into_iter()
        .find(|(candidate_repo, _)| candidate_repo == repo)
        .map(|(_, project_dir)| project_dir)
        .ok_or_else(|| anyhow::anyhow!("no project directory configured for repo {repo}"))?;

    tracing::warn!(
        task_id = task.id.0,
        repo,
        project_dir = %project_dir.display(),
        "review worktree missing, recreating via normal setup"
    );

    let store_ref = Some(Arc::clone(store));
    let wt = worktree::setup_worktree(&task.id.0, &task.title, &project_dir, &store_ref, repo)
        .await
        .context("recreating missing review worktree")?;

    if !wt.work_dir.exists() {
        anyhow::bail!(
            "recreated review worktree at {} for task {} is still missing",
            wt.work_dir.display(),
            task.id.0
        );
    }

    Ok(wt)
}

/// Count commits on the worktree branch that are ahead of `origin/<default_branch>`.
///
/// Returns `Err` when the git command cannot determine the count (e.g., missing
/// remote ref, non-zero exit, unparseable output). Callers must NOT treat this
/// as zero — a failure means the ancestry relationship is unknown.
pub(super) async fn count_ahead_commits(
    worktree_path: &Path,
    default_branch: &str,
) -> Result<u64, String> {
    let worktree_str = worktree_path.to_str().ok_or_else(|| {
        format!(
            "worktree path contains non-UTF-8 characters: {}",
            worktree_path.display()
        )
    })?;

    let output = Command::new("git")
        .args([
            "-C",
            worktree_str,
            "rev-list",
            "--count",
            &format!("origin/{default_branch}..HEAD"),
        ])
        .output()
        .await
        .map_err(|e| format!("git rev-list command failed to start: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!(
            "git rev-list exited with {:?}: {}",
            output.status.code(),
            if stderr.is_empty() {
                String::from_utf8_lossy(&output.stdout).trim().to_string()
            } else {
                stderr
            }
        ));
    }

    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .map_err(|e| format!("could not parse git rev-list output as u64: {e}"))
}

/// Ensure an open PR exists for the task branch before running the review agent.
///
/// Checks the store first (avoids GitHub list-API cache race), then the API.
/// If no PR exists but the branch has commits, attempts to create one and
/// continues with the same review pass.
/// All error and no-op paths update task status and return [`EnsurePrResult::EarlyReturn`].
#[expect(
    clippy::too_many_arguments,
    reason = "every early return path needs its own piece of review state"
)]
pub(super) async fn ensure_pr_exists(
    task: &ExternalTask,
    branch_name: &str,
    worktree_path: &Path,
    repo: &str,
    agent_summary: &str,
    store: &Arc<TaskStore>,
    task_manager: &Arc<TaskManager>,
    stored_pr_number: Option<u64>,
) -> anyhow::Result<EnsurePrResult> {
    let gh_check = GhHttp::new()?;

    if let Some(n) = stored_pr_number {
        tracing::info!(
            task_id = task.id.0,
            pr_number = n,
            branch = %branch_name,
            "open PR found in store, proceeding with review"
        );
        return Ok(EnsurePrResult::Ready(n));
    }

    match gh_check.get_pr_number(repo, branch_name).await {
        Ok(Some(n)) => {
            tracing::info!(
                task_id = task.id.0,
                pr_number = n,
                branch = %branch_name,
                "open PR found, proceeding with review"
            );
            Ok(EnsurePrResult::Ready(n))
        }
        Ok(None) => {
            // No open PR — check if branch has commits ahead of default branch.
            let default_branch = worktree::detect_default_branch(worktree_path).await;
            let has_commits = match count_ahead_commits(worktree_path, &default_branch).await {
                Ok(count) => count > 0,
                Err(e) => {
                    tracing::error!(
                        task_id = task.id.0,
                        branch = %branch_name,
                        default_branch = %default_branch,
                        err = %e,
                        "could not determine if branch has commits ahead of default"
                    );
                    return Ok(EnsurePrResult::EarlyReturn(ReviewDecision::Failed(
                        format!("git ancestry check failed: {e}"),
                    )));
                }
            };

            if has_commits {
                // Branch has unpushed or un-PR'd work — try to create a PR
                tracing::warn!(
                    task_id = task.id.0,
                    branch = %branch_name,
                    "no open PR but branch has commits — attempting to create PR"
                );
                let worktree_str = worktree_path.to_str().ok_or_else(|| {
                    anyhow::anyhow!(
                        "worktree path contains non-UTF-8 characters: {}",
                        worktree_path.display()
                    )
                })?;
                // Push first in case agent forgot (use token auth like git_ops::push_branch)

                // Run a credentialed push and log any errors so failures are
                // visible in logs instead of being silently discarded.
                // Wrap with a timeout to prevent indefinitely blocking a Tokio worker on a
                // hung network connection (corporate proxy, partial GitHub outage, TCP hang).
                let push_timeout = Duration::from_secs(120);
                match time::timeout(
                    push_timeout,
                    git_ops::git()
                        .await
                        .args(["-C", worktree_str, "push", "-u", "origin", branch_name])
                        .output(),
                )
                .await
                {
                    Err(_elapsed) => {
                        tracing::warn!(
                            task_id = task.id.0,
                            branch = %branch_name,
                            timeout_secs = push_timeout.as_secs(),
                            "review gate fallback push timed out"
                        );
                    }
                    Ok(Ok(o)) => {
                        if o.status.success() {
                            tracing::info!(task_id = task.id.0, branch = %branch_name, "review gate fallback push succeeded");
                        } else {
                            tracing::warn!(
                                task_id = task.id.0,
                                branch = %branch_name,
                                exit_code = ?o.status.code(),
                                stdout = %git_ops::sanitize_push_error(&String::from_utf8_lossy(&o.stdout)),
                                stderr = %git_ops::sanitize_push_error(&String::from_utf8_lossy(&o.stderr)),
                                "review gate fallback push failed"
                            );
                        }
                    }
                    Ok(Err(e)) => {
                        tracing::error!(
                            task_id = task.id.0,
                            branch = %branch_name,
                            error = %e,
                            "review gate fallback push command failed to run"
                        );
                    }
                }

                let task_ref = runner::git_ops::format_task_ref(&task.id.0);
                // CI-01 must stay open after merge until the default-branch ruleset exists
                let close_word = if ci_bootstrap::is_ci_bootstrap_task(&task.body) {
                    "Task"
                } else {
                    "Resolves"
                };
                let pr_body = format!(
                    "{close_word} {task_ref}\n\nAuto-created by orch review gate (agent forgot to open PR)"
                );
                let gh = GhHttp::new()?;
                match gh
                    .create_pr(repo, &task.title, &pr_body, branch_name, &default_branch)
                    .await
                {
                    Ok(url) => match parse_pr_number_from_url(&url) {
                        Ok(pr_num) => {
                            store_set_result(
                                &Some(Arc::clone(store)),
                                repo,
                                &task.id.0,
                                &[("pr_number", serde_json::json!(pr_num))],
                            )
                            .await?;
                            tracing::info!(
                                task_id = task.id.0,
                                branch = %branch_name,
                                pr_url = %url,
                                "created missing PR via GhHttp — continuing review"
                            );
                            Ok(EnsurePrResult::Ready(pr_num))
                        }
                        Err(e) => {
                            tracing::warn!(
                                task_id = task.id.0,
                                branch = %branch_name,
                                pr_url = %url,
                                error = %e,
                                "created missing PR via GhHttp, but failed to parse PR number"
                            );
                            Ok(EnsurePrResult::EarlyReturn(ReviewDecision::Failed(
                                format!("created missing PR, but could not parse PR number: {e}"),
                            )))
                        }
                    },
                    Err(e) => {
                        let e_str = format!("{e}");
                        if e_str.contains("already exists") {
                            // GitHub list-API cache may be stale immediately after a 422.
                            // Use two attempts with exponential backoff (2s → 4s) to avoid
                            // racing into a duplicate `gh pr create` call.
                            let backoff_delays = [2u64, 4u64];
                            let mut found_pr: Option<u64> = None;
                            for delay_secs in backoff_delays {
                                time::sleep(time::Duration::from_secs(delay_secs)).await;
                                if let Ok(Some(n)) = gh_check.get_pr_number(repo, branch_name).await
                                {
                                    found_pr = Some(n);
                                    break;
                                }
                            }
                            if let Some(n) = found_pr {
                                tracing::info!(
                                    task_id = task.id.0,
                                    pr_number = n,
                                    branch = %branch_name,
                                    "found existing PR after create_pr 422 — retrying review"
                                );
                                store_set_result(
                                    &Some(Arc::clone(store)),
                                    repo,
                                    &task.id.0,
                                    &[("pr_number", serde_json::json!(n))],
                                )
                                .await?;
                                return Ok(EnsurePrResult::Ready(n));
                            }
                        }
                        tracing::warn!(
                            task_id = task.id.0,
                            branch = %branch_name,
                            error = %e,
                            "create_pr failed via GhHttp, falling back to CLI"
                        );
                        // Fall back to CLI. Wrap the `gh pr create` call in a timeout so
                        // a hung CLI/network does not block the review pipeline.
                        let pr_timeout = Duration::from_secs(120);
                        let mut gh_cmd = Command::new("gh");
                        gh_cmd.args([
                            "pr",
                            "create",
                            "--repo",
                            repo,
                            "--head",
                            branch_name,
                            "--title",
                            &task.title,
                            "--body",
                            &pr_body,
                        ]);
                        gh_cmd.current_dir(worktree_path);
                        gh_cmd.kill_on_drop(true);

                        let pr_result = time::timeout(pr_timeout, gh_cmd.output()).await;
                        match pr_result {
                            Ok(Ok(o)) if o.status.success() => {
                                let stdout = String::from_utf8_lossy(&o.stdout);
                                match parse_pr_number_from_url(stdout.trim()) {
                                    Ok(pr_num) => {
                                        store_set_result(
                                            &Some(Arc::clone(store)),
                                            repo,
                                            &task.id.0,
                                            &[("pr_number", serde_json::json!(pr_num))],
                                        )
                                        .await?;
                                        tracing::info!(
                                            task_id = task.id.0,
                                            branch = %branch_name,
                                            "created missing PR via CLI — continuing review"
                                        );
                                        Ok(EnsurePrResult::Ready(pr_num))
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            task_id = task.id.0,
                                            branch = %branch_name,
                                            stdout = %stdout,
                                            error = %e,
                                            "created missing PR via CLI, but failed to parse PR number"
                                        );
                                        Ok(EnsurePrResult::EarlyReturn(ReviewDecision::Failed(
                                            format!(
                                                "created missing PR via CLI, but could not parse PR number: {e}"
                                            ),
                                        )))
                                    }
                                }
                            }
                            Ok(Ok(o)) => {
                                let stderr = String::from_utf8_lossy(&o.stderr);
                                if stderr.contains("already exists")
                                    && let Some(pr_url) =
                                        stderr.lines().find(|l| l.trim().starts_with("https://"))
                                {
                                    match parse_pr_number_from_url(pr_url.trim()) {
                                        Ok(pr_num) => {
                                            if let Err(e) = store_set_result(
                                                &Some(Arc::clone(store)),
                                                repo,
                                                &task.id.0,
                                                &[("pr_number", serde_json::json!(pr_num))],
                                            )
                                            .await
                                            {
                                                tracing::warn!(
                                                    task_id = task.id.0,
                                                    pr_num,
                                                    err = %e,
                                                    "failed to persist pr_number from already-exists recovery — downstream may retry PR creation"
                                                );
                                            }
                                            tracing::info!(
                                                task_id = task.id.0,
                                                branch = %branch_name,
                                                pr_url = %pr_url,
                                                "PR already exists (from CLI stderr) — retrying review"
                                            );
                                            return Ok(EnsurePrResult::Ready(pr_num));
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                task_id = task.id.0,
                                                branch = %branch_name,
                                                pr_url = %pr_url,
                                                error = %e,
                                                "found existing PR URL in CLI stderr, but failed to parse PR number"
                                            );
                                            // Continue to the error handling below
                                        }
                                    }
                                }
                                tracing::error!(
                                    task_id = task.id.0,
                                    branch = %branch_name,
                                    stderr = %stderr,
                                    "failed to create missing PR — work may be stuck"
                                );

                                // If this was a transient GitHub 5xx or transport error,
                                // do NOT increment the persistent pr_create_failures counter
                                // (which would eventually block the task). Let the
                                // engine retry later. Detect via git_ops::is_transient_github_api_error
                                // so generic agent/model transport errors (e.g. broken pipe)
                                // are NOT mis-classified as GitHub API transients.
                                if git_ops::is_transient_github_api_error(&stderr) {
                                    tracing::warn!(
                                        task_id = task.id.0,
                                        branch = %branch_name,
                                        "transient GitHub error creating PR; will retry later without incrementing persistent failure counter"
                                    );
                                    return Ok(EnsurePrResult::EarlyReturn(
                                        ReviewDecision::Failed(format!(
                                            "transient github error creating PR: {stderr}"
                                        )),
                                    ));
                                }

                                match store_increment(
                                    &Some(Arc::clone(store)),
                                    repo,
                                    &task.id.0,
                                    "pr_create_failures",
                                )
                                .await
                                {
                                    Ok(failures) if failures >= MAX_PR_CREATE_FAILURES => {
                                        return Ok(EnsurePrResult::EarlyReturn(
                                            ReviewDecision::Blocked(format!(
                                                "no PR, create failed {failures} times: {stderr}"
                                            )),
                                        ));
                                    }
                                    Ok(_) => {}
                                    Err(e) => {
                                        tracing::warn!(task_id = task.id.0, err = %e, "failed to increment pr_create_failures — skipping blocking decision this tick");
                                    }
                                }
                                Ok(EnsurePrResult::EarlyReturn(ReviewDecision::Failed(
                                    format!("no PR, create failed: {stderr}"),
                                )))
                            }
                            Ok(Err(e)) => {
                                tracing::error!(
                                    task_id = task.id.0,
                                    error = %e,
                                    "failed to run gh pr create"
                                );
                                let e_str = format!("{e}");
                                if git_ops::is_transient_github_api_error(&e_str) {
                                    tracing::warn!(
                                        task_id = task.id.0,
                                        branch = %branch_name,
                                        "transient GitHub error from gh CLI fallback; will retry later without incrementing persistent failure counter"
                                    );
                                    return Ok(EnsurePrResult::EarlyReturn(
                                        ReviewDecision::Failed(format!("transient gh error: {e}")),
                                    ));
                                }

                                match store_increment(
                                    &Some(Arc::clone(store)),
                                    repo,
                                    &task.id.0,
                                    "pr_create_failures",
                                )
                                .await
                                {
                                    Ok(failures) if failures >= MAX_PR_CREATE_FAILURES => {
                                        return Ok(EnsurePrResult::EarlyReturn(
                                            ReviewDecision::Blocked(format!(
                                                "no PR, gh error {failures} times: {e}"
                                            )),
                                        ));
                                    }
                                    Ok(_) => {}
                                    Err(e) => {
                                        tracing::warn!(task_id = task.id.0, err = %e, "failed to increment pr_create_failures — skipping blocking decision this tick");
                                    }
                                }
                                Ok(EnsurePrResult::EarlyReturn(ReviewDecision::Failed(
                                    format!("no PR, gh error: {e}"),
                                )))
                            }
                            Err(_elapsed) => {
                                // Timeout from tokio::time::timeout
                                tracing::error!(
                                    task_id = task.id.0,
                                    timeout_secs = pr_timeout.as_secs(),
                                    "gh pr create CLI fallback timed out"
                                );
                                let e_str = format!(
                                    "gh pr create timed out after {}s",
                                    pr_timeout.as_secs()
                                );
                                if git_ops::is_transient_github_api_error(&e_str) {
                                    tracing::warn!(
                                        task_id = task.id.0,
                                        branch = %branch_name,
                                        "transient GitHub timeout from gh CLI fallback; will retry later without incrementing persistent failure counter"
                                    );
                                    return Ok(EnsurePrResult::EarlyReturn(
                                        ReviewDecision::Failed(format!(
                                            "transient gh error: {e_str}"
                                        )),
                                    ));
                                }

                                match store_increment(
                                    &Some(Arc::clone(store)),
                                    repo,
                                    &task.id.0,
                                    "pr_create_failures",
                                )
                                .await
                                {
                                    Ok(failures) if failures >= MAX_PR_CREATE_FAILURES => {
                                        return Ok(EnsurePrResult::EarlyReturn(
                                            ReviewDecision::Blocked(format!(
                                                "no PR, gh timeout {failures} times: {e_str}"
                                            )),
                                        ));
                                    }
                                    Ok(_) => {}
                                    Err(e) => {
                                        tracing::warn!(task_id = task.id.0, err = %e, "failed to increment pr_create_failures — skipping blocking decision this tick");
                                    }
                                }
                                Ok(EnsurePrResult::EarlyReturn(ReviewDecision::Failed(
                                    format!("no PR, gh timeout: {e_str}"),
                                )))
                            }
                        }
                    }
                }
            } else {
                // No PR and no commits — agent either failed or completed a read-only task.
                let merged = match gh_check.is_pr_merged(repo, branch_name).await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(task_id = task.id.0, branch = %branch_name, err = %e, "merge check failed, skipping task this tick");
                        return Ok(EnsurePrResult::EarlyReturn(ReviewDecision::Failed(
                            format!("merge check failed: {e}"),
                        )));
                    }
                };
                if merged {
                    tracing::info!(
                        task_id = task.id.0,
                        branch = %branch_name,
                        "PR already merged, marking done"
                    );
                    if let Err(e) = task_manager
                        .update_task_status(&task.id, Status::Done)
                        .await
                    {
                        tracing::error!(task_id = task.id.0, err = %e, "update_task_status(Done) failed — task may be stuck in InReview");
                    }
                    return Ok(EnsurePrResult::EarlyReturn(ReviewDecision::Skipped));
                }

                let last_error = opt_store_get_task(&Some(Arc::clone(store)), repo, &task.id.0)
                    .await
                    .map(|t| t.last_error)
                    .unwrap_or_default();
                let reason = if agent_summary.is_empty() {
                    last_error.clone()
                } else {
                    agent_summary.to_string()
                };

                if last_error.contains("exceeded max attempts") {
                    tracing::warn!(
                        task_id = task.id.0,
                        branch = %branch_name,
                        "no PR and no commits after max attempts — marking blocked to stop loop"
                    );
                    // Write block_reason atomically with the status transition to prevent
                    // auto_unblock_blocked_tasks (block_reason.is_none() gate) from
                    // re-dispatching this task.
                    let fields = [
                        (
                            "block_reason",
                            serde_json::json!("max attempts exceeded — no PR or commits produced"),
                        ),
                        ("last_error", serde_json::json!(last_error)),
                    ];
                    if let Err(e) = task_manager
                        .update_task_status_and_result(&task.id, Status::Blocked, &fields)
                        .await
                    {
                        tracing::error!(task_id = task.id.0, err = %e, "update_task_status_and_result(Blocked) failed — skipping block to avoid silent auto-unblock loop");
                        return Ok(EnsurePrResult::EarlyReturn(ReviewDecision::Failed(
                            format!("failed to write block_reason: {e}"),
                        )));
                    }
                    return Ok(EnsurePrResult::EarlyReturn(ReviewDecision::Skipped));
                }

                if last_error.contains("422") && last_error.contains("head") {
                    tracing::info!(
                        task_id = task.id.0,
                        branch = %branch_name,
                        "no PR — 422/head-invalid means work already merged, marking done"
                    );
                    if let Err(e) = task_manager
                        .update_task_status(&task.id, Status::Done)
                        .await
                    {
                        tracing::error!(task_id = task.id.0, err = %e, "update_task_status(Done) failed");
                    }
                    return Ok(EnsurePrResult::EarlyReturn(ReviewDecision::Skipped));
                }

                if last_error.contains("no fallback agents")
                    || last_error.contains("all agents exhausted")
                {
                    tracing::warn!(
                        task_id = task.id.0,
                        branch = %branch_name,
                        last_error = %last_error,
                        "no PR and no commits after failover exhaustion — marking blocked to stop loop"
                    );
                    // Write block_reason atomically with the status transition to prevent
                    // auto_unblock_blocked_tasks (block_reason.is_none() gate) from
                    // re-dispatching this task.
                    let fields = [
                        (
                            "block_reason",
                            serde_json::json!("all agents exhausted — no PR or commits produced"),
                        ),
                        ("last_error", serde_json::json!(last_error)),
                    ];
                    if let Err(e) = task_manager
                        .update_task_status_and_result(&task.id, Status::Blocked, &fields)
                        .await
                    {
                        tracing::error!(task_id = task.id.0, err = %e, "update_task_status_and_result(Blocked) failed — skipping block to avoid silent auto-unblock loop");
                        return Ok(EnsurePrResult::EarlyReturn(ReviewDecision::Failed(
                            format!("failed to write block_reason: {e}"),
                        )));
                    }
                    return Ok(EnsurePrResult::EarlyReturn(ReviewDecision::Skipped));
                }

                // No PR and no commits — agent either failed or completed a read-only task.
                // Use a dedicated circuit-breaker counter persisted in the store so
                // repeated reroutes across separate runs are counted. This prevents
                // internal tasks from looping new→in_progress→needs_review→new indefinitely.
                // Prefer a dedicated config key `workflow.max_reroute_attempts` (fallback to
                // `workflow.max_attempts` for backwards compatibility).
                let max_reroutes: u32 = config::get("workflow.max_reroute_attempts")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .or_else(|| {
                        config::get("workflow.max_attempts")
                            .ok()
                            .and_then(|s| s.parse().ok())
                    })
                    .unwrap_or(3);

                tracing::warn!(
                    task_id = task.id.0,
                    branch = %branch_name,
                    reason = %reason,
                    "no PR and no commits — re-routing for retry"
                );

                // If the last_error indicates a transient GitHub 5xx/transport error,
                // do NOT increment the persistent reroute counter which would
                // eventually block the task. Instead, retry later.
                if should_skip_no_code_reroute_increment(&last_error) {
                    tracing::warn!(
                        task_id = task.id.0,
                        last_error = %last_error,
                        "transient GitHub error recorded; skipping persistent no_pr_reroutes increment"
                    );
                    return Ok(EnsurePrResult::EarlyReturn(ReviewDecision::Failed(
                        format!("transient github error: {last_error}"),
                    )));
                }

                // Atomically increment the persistent reroute counter and decide.
                let reroutes = match store_increment(
                    &Some(Arc::clone(store)),
                    repo,
                    &task.id.0,
                    "no_code_reroutes",
                )
                .await
                {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(task_id = task.id.0, err = %e, "failed to increment no_code_reroutes — skipping reroute/block decision this tick");
                        // Skip action this tick
                        return Ok(EnsurePrResult::EarlyReturn(ReviewDecision::Failed(
                            format!("transient store error: {e}"),
                        )));
                    }
                };

                if reroutes >= u64::from(max_reroutes) {
                    tracing::error!(
                        task_id = task.id.0,
                        reroutes,
                        max_reroutes,
                        "reached max reroute attempts for no-pr-result — blocking for human review"
                    );
                    let msg = format!(
                        "no PR or code changes after {reroutes}/{max_reroutes} reroute attempts"
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
                        .update_task_status_and_result(&task.id, Status::Blocked, &fields)
                        .await
                    {
                        tracing::error!(task_id = task.id.0, err = %e, "update_task_status_and_result(Blocked) failed — skipping block to avoid silent auto-unblock loop");
                    }
                    Ok(EnsurePrResult::EarlyReturn(ReviewDecision::Blocked(msg)))
                } else if task.id.0.starts_with("internal:") {
                    // Internal tasks with no PR or code changes are read-only/analysis
                    // tasks (e.g. morning briefing, evening retrospective) that
                    // legitimately produce no git-visible changes. Mark them done
                    // instead of re-routing, which would create a loop: needs_review
                    // → review agent → no PR/no commits → reroute to New → agent
                    // runs → needs_review → ...
                    tracing::info!(
                        task_id = task.id.0,
                        "no PR and no commits for internal task — marking done"
                    );
                    if let Err(e) = task_manager
                        .update_task_status(&task.id, Status::Done)
                        .await
                    {
                        tracing::error!(task_id = task.id.0, err = %e, "update_task_status(Done) failed — task may be stuck in InReview");
                    }
                    Ok(EnsurePrResult::EarlyReturn(ReviewDecision::Skipped))
                } else {
                    // Clear agent/model so router picks a different one and note the
                    // fact that this attempt produced no PR or code changes.
                    // Use store_set_result so we only transition status when the
                    // store write succeeds — partial failure would orphan the task.
                    match store_set_result(
                        &Some(Arc::clone(store)),
                        repo,
                        &task.id.0,
                        &[
                            ("agent", serde_json::json!(null)),
                            ("model", serde_json::json!(null)),
                            (
                                "last_error",
                                serde_json::json!("no PR or code changes produced"),
                            ),
                        ],
                    )
                    .await
                    {
                        Err(e) => {
                            tracing::error!(task_id = task.id.0, err = %e, "failed to clear agent/model for reroute — skipping status transition");
                            // Don't update status — task stays in InReview and will be retried next tick
                        }
                        _ => {
                            if let Err(e) =
                                task_manager.update_task_status(&task.id, Status::New).await
                            {
                                tracing::error!(task_id = task.id.0, err = %e, "update_task_status(New) failed — task may be stuck in InReview");
                            }
                        }
                    }
                    Ok(EnsurePrResult::EarlyReturn(ReviewDecision::Rerouted))
                }
            }
        }
        Err(e) => {
            tracing::warn!(
                task_id = task.id.0,
                branch = %branch_name,
                error = %e,
                "failed to check PR status"
            );
            Ok(EnsurePrResult::EarlyReturn(ReviewDecision::Failed(
                format!("PR check failed: {e}"),
            )))
        }
    }
}
