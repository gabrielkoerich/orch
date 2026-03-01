//! PR review workflow — review agent, auto-merge, and review change handling.
//!
//! This module contains the entire PR review state machine:
//! - `review_open_prs()` — sync tick handler that checks PR review status
//! - `review_and_merge()` — spawns review agent and handles outcome
//! - `auto_merge_pr()` — CI check + merge after approval
//! - `handle_review_changes()` — re-dispatch on changes requested

use crate::backends::{ExternalBackend, ExternalTask, Status};
use crate::config;
use crate::db::Db;
use crate::engine::router::Router;
use crate::engine::runner;
use crate::github::http::GhHttp;
use crate::github::types::{GitHubReviewComment, PullRequestReview};
use crate::sidecar;
use crate::tmux::TmuxManager;
use std::sync::Arc;
use tokio::sync::RwLock;

use super::EngineConfig;

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
    /// Review agent is disabled, skip.
    #[allow(dead_code)]
    Skip,
    /// Review agent failed or crashed (reason stored for logging).
    #[allow(dead_code)]
    Failed(String),
}

/// Review open PRs - re-dispatch agent to address review feedback.
///
/// Lists tasks in review, fetches PR reviews, and re-dispatches the agent
/// when a reviewer requests changes. The review context is stored in the
/// sidecar and injected into the agent prompt.
pub(crate) async fn review_open_prs(
    backend: &Arc<dyn ExternalBackend>,
    _db: &Arc<Db>,
    repo: &str,
    config: &EngineConfig,
) -> anyhow::Result<()> {
    // Get tasks that are in review (have open PRs)
    let in_review_tasks = backend.list_by_status(Status::InReview).await?;

    if in_review_tasks.is_empty() {
        return Ok(());
    }

    // Check if we should process reviews
    let auto_close_task = config.auto_close_task_on_approval;

    tracing::info!(
        count = in_review_tasks.len(),
        "checking in_review tasks for PR reviews"
    );

    let gh = GhHttp::new();

    for task in in_review_tasks {
        let task_id = &task.id.0;

        // Get branch from sidecar
        let branch = match sidecar::get(task_id, "branch") {
            Ok(b) if !b.is_empty() => b,
            _ => continue,
        };

        // Get PR number from branch
        let pr_number = match gh.get_pr_number(repo, &branch).await {
            Ok(Some(n)) => n,
            Ok(None) => {
                tracing::debug!(task_id, branch = %branch, "no open PR found");
                continue;
            }
            Err(e) => {
                tracing::warn!(task_id, branch = %branch, err = %e, "failed to get PR number");
                continue;
            }
        };

        // Store PR number in sidecar for follow-up tasks
        if let Err(e) = sidecar::set(task_id, &[format!("pr_number={}", pr_number)]) {
            tracing::warn!(task_id, err = %e, "failed to store PR number in sidecar");
        }

        // Get the last processed review timestamp to avoid re-processing the same reviews
        let last_review_ts = sidecar::get(task_id, "last_review_ts").unwrap_or_default();

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
        if (all_approved || comment_approved) && auto_close_task && !comment_changes_requested {
            tracing::info!(
                task_id,
                pr_number,
                comment_approved,
                "PR approved, closing task (marking as done)"
            );
            if let Err(e) = backend.update_status(&task.id, Status::Done).await {
                tracing::warn!(task_id, err = %e, "failed to update task status to done");
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
                sidecar::get(task_id, "last_comment_review_ts").unwrap_or_default();
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
                        let _ = sidecar::set(
                            task_id,
                            &[format!("last_comment_review_ts={}", c.created_at)],
                        );
                        break; // Only the latest
                    }
                }
            }
        }

        // Cap review context to avoid oversized sidecar values
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
            // Store the review context in the sidecar
            if let Err(e) =
                sidecar::set(task_id, &[format!("pr_review_context={}", review_context)])
            {
                tracing::warn!(task_id, err = %e, "failed to store pr_review_context");
            }

            // Update the last review timestamp
            if let Err(e) = sidecar::set(task_id, &[format!("last_review_ts={}", latest_review_ts)])
            {
                tracing::warn!(task_id, err = %e, "failed to update last_review_ts");
            }

            // Re-dispatch: reset review_started so the next completion triggers review again
            if let Err(e) = sidecar::set(task_id, &["review_started=false".to_string()]) {
                tracing::warn!(task_id, err = %e, "failed to reset review_started");
            }
            if let Err(e) = backend.update_status(&task.id, Status::Routed).await {
                tracing::warn!(task_id, err = %e, "failed to set status to routed for re-dispatch");
            } else {
                tracing::info!(task_id, "re-dispatching task to address review feedback");
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
) -> anyhow::Result<ReviewDecision> {
    // 2. Load sidecar for worktree path, branch, agent
    let worktree = sidecar::get(&task.id.0, "worktree").ok();
    let branch = sidecar::get(&task.id.0, "branch").ok();
    let agent_summary = sidecar::get(&task.id.0, "summary").unwrap_or_default();

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

    // 2b. Verify an open PR exists before running the (expensive) review agent
    let gh_check = GhHttp::new();
    let pr_number_early = match gh_check.get_pr_number(repo, &branch_name).await {
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
            tracing::warn!(
                task_id = task.id.0,
                branch = %branch_name,
                "no open PR found, skipping review"
            );
            return Ok(ReviewDecision::Failed("no open PR".to_string()));
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
    };

    // 3. Rebase with main to resolve conflicts before review
    let default_branch = config::get("gh.default_branch").unwrap_or_else(|_| "main".to_string());
    {
        let wt = &worktree_path;
        // Fetch latest main
        let fetch = tokio::process::Command::new("git")
            .args(["fetch", "origin", &default_branch])
            .current_dir(wt)
            .output()
            .await;
        if let Ok(out) = fetch {
            if out.status.success() {
                let rebase = tokio::process::Command::new("git")
                    .args(["rebase", &format!("origin/{default_branch}")])
                    .current_dir(wt)
                    .output()
                    .await;
                match rebase {
                    Ok(out) if out.status.success() => {
                        tracing::info!(task_id = task.id.0, "rebased with {default_branch}");
                    }
                    Ok(out) => {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        tracing::warn!(task_id = task.id.0, stderr = %stderr, "rebase failed, aborting");
                        let _ = tokio::process::Command::new("git")
                            .args(["rebase", "--abort"])
                            .current_dir(wt)
                            .output()
                            .await;
                    }
                    Err(e) => {
                        tracing::warn!(task_id = task.id.0, error = %e, "rebase command failed");
                    }
                }
            }
        }
    }
    let git_diff = runner::context::build_git_diff(&worktree_path, &default_branch).await;
    let git_log = runner::context::build_git_log(&worktree_path, &default_branch).await;

    // 4. Build review prompt
    let review_prompt =
        runner::agent::build_review_prompt(task, &agent_summary, &git_diff, &git_log);

    // 5. Pick review agent via round-robin
    let review_agent = {
        let r = router.read().await;
        r.next_round_robin_agent()
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
        branch: branch_name.clone(),
        main_project_dir: worktree_path.clone(), // Use worktree as main dir for review
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

    // 9. Read and parse response using the same pipeline as normal tasks
    let raw_output = runner::response::read_output_file(&review_task_id, &output_file, repo);
    let agent_runner = runner::agents::get_runner(&review_agent);

    // Read exit code
    let exit_code = std::fs::read_to_string(review_attempt_dir.join("exit.txt"))
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .unwrap_or(-1);

    let stderr = std::fs::read_to_string(review_attempt_dir.join("stderr.txt")).unwrap_or_default();

    // Use agent-specific parsing (handles envelope, is_error, auth, rate limit)
    let parse_result = if exit_code == 0 && !raw_output.is_empty() {
        agent_runner.parse_response(&raw_output)
    } else {
        Err(agent_runner.classify_error(exit_code, &raw_output, &stderr))
    };

    let parsed = match parse_result {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(task_id = task.id.0, error = %e, "review agent error: {}", e);
            return Ok(ReviewDecision::Failed(format!("agent error: {e}")));
        }
    };

    // The agent's summary contains the review JSON (or markdown with JSON block)
    let review_text = &parsed.response.summary;
    let review_response = match runner::response::parse_review_response(review_text) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(task_id = task.id.0, error = %e, review_text = %review_text, "failed to parse review response");
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
    let gh = GhHttp::new();
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
            if let Err(e) = gh.add_comment(repo, &pr_num.to_string(), &pr_comment).await {
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
            let auto_merge = config::get("workflow.auto_merge")
                .map(|v| v == "true")
                .unwrap_or(true);

            if auto_merge {
                if let Err(e) = auto_merge_pr(task, &branch_name, backend, repo).await {
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
            handle_review_changes(task, notes, issues, backend, repo, pr_number_early).await?;
            Ok(decision)
        }
        _ => Ok(decision),
    }
}

/// Auto-merge a PR after review approval.
///
/// Checks that the automated review comment says "approve" and that CI checks
/// are green before merging. If the review gate workflow failed, re-triggers it
/// and polls until CI passes (up to a timeout).
async fn auto_merge_pr(
    task: &ExternalTask,
    branch: &str,
    backend: &Arc<dyn ExternalBackend>,
    repo: &str,
) -> anyhow::Result<()> {
    // 1. Get PR number from branch
    let gh = GhHttp::new();
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
                "automated review says changes_requested — skipping merge"
            );
            return Ok(());
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
                // If there are failed runs, try re-running them once
                if start.elapsed() < std::time::Duration::from_secs(30) {
                    if let Ok(Some((run_id, _, _))) =
                        gh.get_latest_run_for_branch(repo, branch).await
                    {
                        match gh.rerun_failed_jobs(repo, run_id).await {
                            Ok(()) => tracing::info!(
                                task_id = task.id.0,
                                run_id,
                                "re-triggered failed CI jobs"
                            ),
                            Err(e) => tracing::warn!(
                                task_id = task.id.0,
                                run_id,
                                error = %e,
                                "failed to re-trigger CI jobs"
                            ),
                        }
                    }
                }

                if start.elapsed() >= max_wait {
                    tracing::warn!(task_id = task.id.0, "CI checks still failing after timeout");
                    let _ = gh.add_comment(
                        repo,
                        &pr_number.to_string(),
                        "⚠️ Auto-merge: CI checks are failing. PR is approved but cannot be merged until CI passes.",
                    ).await;
                    backend.update_status(&task.id, Status::NeedsReview).await?;
                    return Ok(());
                }
            }
            _ => {
                // pending
                if start.elapsed() >= max_wait {
                    tracing::warn!(task_id = task.id.0, "CI checks still pending after timeout");
                    let _ = gh.add_comment(
                        repo,
                        &pr_number.to_string(),
                        "⚠️ Auto-merge: CI checks still pending after timeout. Will merge when checks complete.",
                    ).await;
                    // Don't merge with unknown CI status — set status to in_review
                    // so the next engine tick re-checks
                    backend.update_status(&task.id, Status::InReview).await?;
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
        // Merge failed (conflicts, branch protection, etc.)
        tracing::error!(task_id = task.id.0, error = %e, "merge failed");
        backend.update_status(&task.id, Status::NeedsReview).await?;
        let _ = gh
            .add_comment(
                repo,
                &pr_number.to_string(),
                &format!("Auto-merge failed: {}", e),
            )
            .await;
        return Err(e);
    }

    // 6. Update status to done
    backend.update_status(&task.id, Status::Done).await?;

    // 7. Close issue if auto_close enabled
    let auto_close = config::get("workflow.auto_close")
        .map(|v| v == "true")
        .unwrap_or(true);

    if auto_close {
        if let Err(e) = gh.close_issue(repo, &task.id.0).await {
            tracing::warn!(task_id = task.id.0, error = %e, "failed to close issue after merge");
        }
    }

    // 8. Mark worktree for cleanup
    let _ = sidecar::set(&task.id.0, &["worktree_cleaned=false".to_string()]);

    // 9. Post final comment on the PR
    let _ = gh
        .add_comment(
            repo,
            &pr_number.to_string(),
            "✅ PR reviewed, approved, and merged.",
        )
        .await;

    tracing::info!(task_id = task.id.0, "auto-merge completed");

    Ok(())
}

/// Handle review changes request — re-dispatch the original agent.
async fn handle_review_changes(
    task: &ExternalTask,
    notes: &str,
    issues: &[crate::engine::runner::response::ReviewIssue],
    backend: &Arc<dyn ExternalBackend>,
    repo: &str,
    pr_number: u64,
) -> anyhow::Result<()> {
    // 1. Check review cycle count (max 2 review rounds)
    let review_cycles: u32 = sidecar::get(&task.id.0, "review_cycles")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let max_cycles: u32 = config::get("workflow.max_review_cycles")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);

    let gh = GhHttp::new();
    let pr_num_str = pr_number.to_string();

    if review_cycles >= max_cycles {
        // Too many review cycles — escalate to human
        tracing::warn!(
            task_id = task.id.0,
            review_cycles,
            max_cycles,
            "max review cycles exceeded, escalating to human"
        );
        backend.update_status(&task.id, Status::NeedsReview).await?;
        let escalation = format!(
            "🔍 Review agent requested changes after {} cycles. Escalating to human.\n\n**Review Notes:**\n{}",
            review_cycles, notes
        );
        if let Err(e) = gh.add_comment(repo, &pr_num_str, &escalation).await {
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

    if let Err(e) = gh.add_comment(repo, &pr_num_str, &comment).await {
        tracing::warn!(task_id = task.id.0, pr_number, err = %e, "failed to post review comment on PR");
    }

    // 3. Update sidecar with review context (including pr_review_context so the
    //    re-dispatched agent can see what the reviewer found wrong)
    let review_context = format!(
        "A reviewer has requested changes on your PR. Please address the following feedback:\n\n{}",
        comment
    );
    let _ = sidecar::set(
        &task.id.0,
        &[
            format!("review_cycles={}", review_cycles + 1),
            format!("review_notes={}", notes),
            format!("pr_review_context={}", review_context),
            "review_started=false".to_string(),
            "status=routed".to_string(),
        ],
    );

    // 4. Re-dispatch — set status back to routed (keeps same agent/branch/worktree)
    backend.update_status(&task.id, Status::Routed).await?;

    tracing::info!(
        task_id = task.id.0,
        review_cycles = review_cycles + 1,
        "re-dispatched task for review changes"
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

    #[test]
    fn test_get_model_for_complexity_returns_nonempty() {
        // Should always return a non-empty model name (from config or defaults)
        assert!(!get_model_for_complexity("simple", "claude").is_empty());
        assert!(!get_model_for_complexity("medium", "claude").is_empty());
        assert!(!get_model_for_complexity("complex", "claude").is_empty());
        assert!(!get_model_for_complexity("review", "claude").is_empty());
    }

    #[test]
    fn test_get_model_for_complexity_unknown_agent() {
        // Unknown agents should still return a model name
        let model = get_model_for_complexity("simple", "unknown_agent_xyz");
        assert!(!model.is_empty());
    }
}
