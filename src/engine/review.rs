//! PR review pipeline — review agent orchestration.
//!
//! This module handles running the review agent on completed tasks, parsing its
//! decision, and posting automated review comments.
//!
//! Related modules:
//! - [`super::review_poll`] — polling open PRs for human review feedback
//! - [`super::auto_merge`] — merging approved PRs and handling change requests

/// Maximum consecutive review agent failures before the task is blocked
/// for human intervention. Exported so `tick` and `sync` use the same threshold.
pub(crate) const MAX_REVIEW_AGENT_FAILURES: u64 = 3;

/// Maximum consecutive PR-creation failures before blocking the task.
const MAX_PR_CREATE_FAILURES: u64 = 3;

fn review_started_comment(review_agent: &str, review_model: &str) -> String {
    format!(
        "🔍 Automated review started (agent: {}, model: {})",
        review_agent, review_model
    )
}

use crate::backends::{ExternalBackend, ExternalTask, Status};
use crate::engine::auto_merge::{attribution_footer, auto_merge_pr, handle_review_changes};
use crate::engine::runner;
use crate::engine::runner::worktree;
use crate::engine::tasks::TaskManager;
use crate::github::http::GhHttp;
use crate::store::store_set;
use crate::store::TaskStore;
use crate::store::{opt_store_get_task, set_review_session_expected, store_increment};
use crate::store::{CompleteRun, RunTokenUsage, StartRun};
use crate::tmux::TmuxManager;
use anyhow::Context;
use std::sync::Arc;
use tokio::sync::RwLock;

use super::router::Router;

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

/// Outcome of [`ensure_pr_exists`]: either a ready PR number or an early return decision.
enum EnsurePrResult {
    /// PR exists (found in store or via API, or just created). Proceed with review.
    Ready(u64),
    /// No PR and status already updated — return this decision to the caller.
    EarlyReturn(ReviewDecision),
}

async fn recover_missing_review_worktree(
    task: &ExternalTask,
    repo: &str,
    store: &Arc<TaskStore>,
) -> anyhow::Result<worktree::WorktreeSetup> {
    let project_dir = crate::config::get_projects_with_paths()?
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

/// Ensure an open PR exists for the task branch before running the review agent.
///
/// Checks the store first (avoids GitHub list-API cache race), then the API.
/// If no PR exists but the branch has commits, attempts to create one and
/// continues with the same review pass.
/// All error and no-op paths update task status and return [`EnsurePrResult::EarlyReturn`].
#[allow(clippy::too_many_arguments)]
async fn ensure_pr_exists(
    task: &ExternalTask,
    branch_name: &str,
    worktree_path: &std::path::Path,
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
            let worktree_str = worktree_path.to_str().ok_or_else(|| {
                anyhow::anyhow!(
                    "worktree path contains non-UTF-8 characters: {:?}",
                    worktree_path
                )
            })?;
            let has_commits = tokio::process::Command::new("git")
                .args([
                    "-C",
                    worktree_str,
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
                    .args(["-C", worktree_str, "push", "-u", "origin", branch_name])
                    .output()
                    .await;

                let task_ref = runner::git_ops::format_task_ref(&task.id.0);
                let pr_body = format!(
                    "Resolves {task_ref}\n\nAuto-created by orch review gate (agent forgot to open PR)"
                );
                let gh = GhHttp::new()?;
                match gh
                    .create_pr(repo, &task.title, &pr_body, branch_name, &default_branch)
                    .await
                {
                    Ok(url) => {
                        let pr_num = url
                            .rsplit('/')
                            .next()
                            .filter(|s| !s.is_empty())
                            .and_then(|s| s.parse::<i64>().ok())
                            .filter(|&n| n > 0);
                        if let Some(pr_num) = pr_num {
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
                            pr_url = %url,
                            "created missing PR via GhHttp — continuing review"
                        );
                        if let Some(pr_num) = pr_num {
                            Ok(EnsurePrResult::Ready(pr_num as u64))
                        } else {
                            Ok(EnsurePrResult::EarlyReturn(ReviewDecision::Failed(
                                "created missing PR, but could not parse PR number".to_string(),
                            )))
                        }
                    }
                    Err(e) => {
                        let e_str = format!("{e}");
                        if e_str.contains("already exists") {
                            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                            if let Ok(Some(n)) = gh_check.get_pr_number(repo, branch_name).await {
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
                                return Ok(EnsurePrResult::Ready(n));
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
                                branch_name,
                                "--title",
                                &task.title,
                                "--body",
                                &pr_body,
                            ])
                            .current_dir(worktree_path)
                            .output()
                            .await;
                        match pr_result {
                            Ok(o) if o.status.success() => {
                                let stdout = String::from_utf8_lossy(&o.stdout);
                                let pr_num = stdout
                                    .trim()
                                    .rsplit('/')
                                    .next()
                                    .filter(|s| !s.is_empty())
                                    .and_then(|s| s.parse::<i64>().ok())
                                    .filter(|&n| n > 0);
                                if let Some(pr_num) = pr_num {
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
                                    "created missing PR via CLI — continuing review"
                                );
                                if let Some(pr_num) = pr_num {
                                    Ok(EnsurePrResult::Ready(pr_num as u64))
                                } else {
                                    Ok(EnsurePrResult::EarlyReturn(ReviewDecision::Failed(
                                        "created missing PR via CLI, but could not parse PR number"
                                            .to_string(),
                                    )))
                                }
                            }
                            Ok(o) => {
                                let stderr = String::from_utf8_lossy(&o.stderr);
                                if stderr.contains("already exists") {
                                    if let Some(pr_url) =
                                        stderr.lines().find(|l| l.trim().starts_with("https://"))
                                    {
                                        let pr_url = pr_url.trim();
                                        let pr_num = pr_url
                                            .rsplit('/')
                                            .next()
                                            .and_then(|n| n.parse::<u64>().ok());
                                        if let Some(pr_num) = pr_num {
                                            store_set(
                                                &Some(Arc::clone(store)),
                                                repo,
                                                &task.id.0,
                                                &[("pr_number", serde_json::json!(pr_num as i64))],
                                            )
                                            .await;
                                            tracing::info!(
                                                task_id = task.id.0,
                                                branch = %branch_name,
                                                pr_url = %pr_url,
                                                "PR already exists (from CLI stderr) — retrying review"
                                            );
                                            return Ok(EnsurePrResult::Ready(pr_num));
                                        }
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
                                    return Ok(EnsurePrResult::EarlyReturn(
                                        ReviewDecision::Blocked(format!(
                                            "no PR, create failed {failures} times: {stderr}"
                                        )),
                                    ));
                                }
                                Ok(EnsurePrResult::EarlyReturn(ReviewDecision::Failed(
                                    format!("no PR, create failed: {stderr}"),
                                )))
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
                                    return Ok(EnsurePrResult::EarlyReturn(
                                        ReviewDecision::Blocked(format!(
                                            "no PR, gh error {failures} times: {e}"
                                        )),
                                    ));
                                }
                                Ok(EnsurePrResult::EarlyReturn(ReviewDecision::Failed(
                                    format!("no PR, gh error: {e}"),
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
                let reason = if !agent_summary.is_empty() {
                    agent_summary.to_string()
                } else {
                    last_error.clone()
                };

                if last_error.contains("exceeded max attempts") {
                    tracing::warn!(
                        task_id = task.id.0,
                        branch = %branch_name,
                        "no PR and no commits after max attempts — marking blocked to stop loop"
                    );
                    if let Err(e) = task_manager
                        .update_task_status(&task.id, Status::Blocked)
                        .await
                    {
                        tracing::error!(task_id = task.id.0, err = %e, "update_task_status(Blocked) failed — task may be stuck in InReview");
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

                tracing::warn!(
                    task_id = task.id.0,
                    branch = %branch_name,
                    reason = %reason,
                    "no PR and no commits — re-routing for retry"
                );
                if let Err(e) = task_manager.update_task_status(&task.id, Status::New).await {
                    tracing::error!(task_id = task.id.0, err = %e, "update_task_status(New) failed — task may be stuck in InReview");
                }
                Ok(EnsurePrResult::EarlyReturn(ReviewDecision::Skipped))
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

/// Run the review agent on a completed task and handle the outcome.
///
/// Called after a task completes with status:done and a PR is created.
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
    set_review_session_expected(store, repo, &task.id.0, true).await;

    // 2. Load worktree path, branch, summary, and pr_number from store.
    let stored_task = store
        .get_by_external_id(repo, &task.id.0)
        .await
        .with_context(|| format!("store lookup failed for task {}", task.id.0))?;

    let store_id: Option<i64> = stored_task.as_ref().map(|t| t.id);
    // Increment a per-invocation counter so every review agent run gets a unique
    // attempt directory, regardless of whether a previous attempt failed without
    // producing a `request_changes` decision (which is the only time review_cycles
    // increments). Stale output.json files from crashed attempts can no longer be
    // silently reused.
    let review_attempt: u32 = {
        let v = store_increment(
            &Some(Arc::clone(store)),
            repo,
            &task.id.0,
            "review_invocations",
        )
        .await;
        if v == 0 {
            1
        } else {
            v as u32
        }
    };

    let mut worktree_path = match stored_task.as_ref().map(|t| t.worktree.as_str()) {
        Some(w) if !w.is_empty() => std::path::PathBuf::from(w),
        _ => {
            tracing::warn!(task_id = task.id.0, "no worktree found for review");
            return Ok(ReviewDecision::Failed("no worktree found".to_string()));
        }
    };

    let mut branch_name = match stored_task.as_ref().map(|t| t.branch.as_str()) {
        Some(b) if !b.is_empty() => b.to_string(),
        _ => {
            tracing::warn!(task_id = task.id.0, "no branch found for review");
            return Ok(ReviewDecision::Failed("no branch found".to_string()));
        }
    };

    let mut missing_worktree_recovered = false;
    if !worktree_path.exists() {
        match recover_missing_review_worktree(task, repo, store).await {
            Ok(recovered) => {
                branch_name = recovered.branch;
                worktree_path = recovered.work_dir;
                missing_worktree_recovered = true;
            }
            Err(e) => {
                let reason = format!("missing review worktree recovery failed: {e}");
                tracing::error!(task_id = task.id.0, error = %reason, "cannot recover review worktree");
                store_set(
                    &Some(Arc::clone(store)),
                    repo,
                    &task.id.0,
                    &[("last_error", serde_json::json!(reason.clone()))],
                )
                .await;
                return Ok(ReviewDecision::Blocked(reason));
            }
        }
    }

    if missing_worktree_recovered {
        store_set(
            &Some(Arc::clone(store)),
            repo,
            &task.id.0,
            &[("last_error", serde_json::json!(""))],
        )
        .await;
    }

    let agent_summary = stored_task
        .as_ref()
        .map(|t| t.summary.clone())
        .unwrap_or_default();

    // 2b. Verify an open PR exists before running the (expensive) review agent.
    let stored_pr_number = stored_task
        .as_ref()
        .and_then(|t| t.pr_number)
        .map(|n| n as u64)
        .filter(|&n| n > 0);

    let pr_number_early = match ensure_pr_exists(
        task,
        &branch_name,
        &worktree_path,
        repo,
        &agent_summary,
        store,
        task_manager,
        stored_pr_number,
    )
    .await?
    {
        EnsurePrResult::Ready(n) => n,
        EnsurePrResult::EarlyReturn(d) => return Ok(d),
    };

    // 3. Build diff context
    let default_branch = runner::worktree::detect_default_branch(&worktree_path).await;
    let git_diff = runner::context::build_git_diff(&worktree_path, &default_branch).await;
    let git_log = runner::context::build_git_log(&worktree_path, &default_branch).await;

    // 4. Build review prompt
    let review_prompt = runner::agent::build_review_prompt(
        task,
        &agent_summary,
        &git_diff,
        &git_log,
        &default_branch,
        pr_number_early,
    );

    // 5. Pick review agent via round-robin, excluding the agent that did the work
    let task_agent = opt_store_get_task(&Some(Arc::clone(store)), repo, &task.id.0)
        .await
        .and_then(|t| t.agent)
        .unwrap_or_default();
    let (review_agent, review_model) = {
        // Exclude the task agent AND any agents that previously failed review
        // for this task, so we don't retry the same broken agent.
        let mut exclude_list: Vec<String> = Vec::new();
        if !task_agent.is_empty() {
            exclude_list.push(task_agent.clone());
        }
        if let Ok(Some(store_id)) = store.resolve_task_id(repo, &task.id.0).await {
            if let Ok(failed) = store.previous_review_agents(store_id).await {
                for agent in failed {
                    if !exclude_list.contains(&agent) {
                        exclude_list.push(agent);
                    }
                }
            }
        }
        let exclude_refs: Vec<&str> = exclude_list.iter().map(|s| s.as_str()).collect();
        let mut r = router.write().await;
        let agent = r
            .next_round_robin_agent(&exclude_refs)
            .unwrap_or_else(|| "claude".to_string());
        let model = r
            .config
            .model_for_complexity_or_default(&agent, "review", &task.id.0);
        (agent, model)
    };

    tracing::info!(
        task_id = task.id.0,
        agent = %review_agent,
        model = %review_model,
        "spawning review agent"
    );

    // 6. Build agent invocation for review
    let review_task_id = format!("{}-review", task.id.0);
    let review_attempt_dir = crate::home::task_attempt_dir(repo, &review_task_id, review_attempt)?;
    let output_file = review_attempt_dir.join("output.json");

    let git_name =
        crate::config::get("git.name").unwrap_or_else(|_| format!("{review_agent}[bot]"));
    let git_email = crate::config::get("git.email")
        .unwrap_or_else(|_| format!("{review_agent}[bot]@users.noreply.github.com"));

    let review_timeout_secs: u64 = crate::config::get("workflow.review_timeout_seconds")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            crate::config::get("workflow.timeout_seconds")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1800)
        });

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
        timeout_seconds: review_timeout_secs,
        repo: repo.to_string(),
        attempt: review_attempt,
    };

    // 7. Start run tracking before spawning review agent
    let run_id = if let Some(sid) = store_id {
        store
            .start_run(&StartRun {
                task_id: sid,
                attempt: review_attempt as i32,
                run_type: "review",
                agent: &review_agent,
                model: &review_model,
                command: "",
                prompt: "",
            })
            .await
            .ok()
    } else {
        None
    };

    // 8. Spawn review agent in tmux
    let session = match runner::agent::spawn_in_tmux(tmux, &invocation).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(task_id = task.id.0, error = %e, "failed to spawn review agent");
            return Ok(ReviewDecision::Failed(format!("spawn failed: {e}")));
        }
    };

    let start_comment = review_started_comment(&review_agent, &review_model);
    match GhHttp::new() {
        Ok(gh) => {
            if let Err(e) = gh
                .add_comment(repo, &pr_number_early.to_string(), &start_comment)
                .await
            {
                tracing::debug!(
                    task_id = task.id.0,
                    pr_number = pr_number_early,
                    error = %e,
                    "failed to post review-started comment"
                );
            }
        }
        Err(e) => {
            tracing::debug!(
                task_id = task.id.0,
                pr_number = pr_number_early,
                error = %e,
                "failed to create GitHub client for review-started comment"
            );
        }
    }

    // 8. Wait for completion
    let poll_interval = std::time::Duration::from_secs(5);
    let timeout_duration = std::time::Duration::from_secs(review_timeout_secs);

    let wait_result = tokio::time::timeout(
        timeout_duration,
        tmux.wait_for_completion(&session, poll_interval),
    )
    .await;

    match wait_result {
        Ok(Ok(_)) => {
            tracing::info!(task_id = task.id.0, "review agent completed");
            let _ = tmux.kill_session(&session).await;
        }
        Ok(Err(e)) => {
            tracing::error!(task_id = task.id.0, error = %e, "review agent error");
            let _ = tmux.kill_session(&session).await;
            if let Some(rid) = run_id {
                let _ = store
                    .complete_run(&CompleteRun {
                        run_id: rid,
                        exit_code: Some(-1),
                        stdout: "",
                        stderr: &e.to_string(),
                        parsed: "",
                        outcome: "failed",
                        error: &format!("agent error: {e}"),
                        tokens: RunTokenUsage::default(),
                    })
                    .await;
            }
            return Ok(ReviewDecision::Failed(format!("agent error: {e}")));
        }
        Err(_) => {
            tracing::error!(task_id = task.id.0, "review agent timed out");
            let _ = tmux.kill_session(&session).await;
            if let Some(rid) = run_id {
                let _ = store
                    .complete_run(&CompleteRun {
                        run_id: rid,
                        exit_code: Some(-1),
                        stdout: "",
                        stderr: "timeout",
                        parsed: "",
                        outcome: "failed",
                        error: "timeout",
                        tokens: RunTokenUsage::default(),
                    })
                    .await;
            }
            return Ok(ReviewDecision::Failed("timeout".to_string()));
        }
    }

    // 9. Read and parse response
    let file_exists = output_file.exists();
    let file_size = std::fs::metadata(&output_file)
        .map(|m| m.len())
        .unwrap_or(0);
    tracing::info!(
        task_id = task.id.0,
        path = %output_file.display(),
        file_exists,
        file_size,
        "reading review agent output"
    );
    let raw_output = runner::response::read_output_file(&review_task_id, &output_file, repo);
    let agent_runner = runner::agents::get_runner(&review_agent);

    let exit_code = std::fs::read_to_string(review_attempt_dir.join("exit.txt"))
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .unwrap_or(-1);

    let stderr = std::fs::read_to_string(review_attempt_dir.join("stderr.txt")).unwrap_or_default();

    // Check the NDJSON result event for is_error flag.
    // Agents like kimi return exit 0 but is_error:true in the result event.
    let result_is_error = raw_output
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|v| v.get("type").and_then(|t| t.as_str()) == Some("result"))
        .and_then(|v| v.get("is_error").and_then(|e| e.as_bool()))
        .unwrap_or(false);
    let is_hard_failure = raw_output.is_empty() || result_is_error;
    if is_hard_failure {
        tracing::warn!(
            task_id = task.id.0,
            exit_code,
            raw_output_len = raw_output.len(),
            stderr_len = stderr.len(),
            "review agent: entering error path"
        );
        let err = agent_runner.classify_error(exit_code, &raw_output, &stderr);
        match &err {
            runner::agents::AgentError::RateLimit { .. }
            | runner::agents::AgentError::Auth { .. } => {
                tracing::warn!(
                    task_id = task.id.0,
                    agent = %review_agent,
                    "review agent hit rate limit — adding to cooldown"
                );
                runner::response::record_agent_failure_with_message(
                    &review_agent,
                    &err.to_string(),
                );
            }
            _ => {}
        }
        tracing::error!(task_id = task.id.0, error = %err, "review agent failed");
        if let Some(rid) = run_id {
            let _ = store
                .complete_run(&CompleteRun {
                    run_id: rid,
                    exit_code: Some(exit_code),
                    stdout: &raw_output,
                    stderr: &stderr,
                    parsed: "",
                    outcome: "failed",
                    error: &err.to_string(),
                    tokens: RunTokenUsage::default(),
                })
                .await;
        }
        return Ok(ReviewDecision::Failed(format!("agent error: {err}")));
    }

    // Stage 1: strip the agent-specific output envelope to get the review text.
    let text_for_review = match agent_runner.extract_text(&raw_output) {
        Ok(text) if !text.is_empty() => text,
        Ok(_) => {
            tracing::debug!(
                task_id = task.id.0,
                agent = %review_agent,
                "review agent: empty text after envelope extraction, falling back to raw output"
            );
            raw_output.clone()
        }
        Err(e) => {
            tracing::error!(task_id = task.id.0, error = %e, "review agent error");
            match &e {
                runner::agents::AgentError::RateLimit { .. }
                | runner::agents::AgentError::Auth { .. } => {
                    tracing::warn!(
                        task_id = task.id.0,
                        agent = %review_agent,
                        "review agent hit rate limit — adding to cooldown"
                    );
                    runner::response::record_agent_failure_with_message(
                        &review_agent,
                        &e.to_string(),
                    );
                }
                _ => {}
            }
            if let Some(rid) = run_id {
                let _ = store
                    .complete_run(&CompleteRun {
                        run_id: rid,
                        exit_code: Some(exit_code),
                        stdout: &raw_output,
                        stderr: &stderr,
                        parsed: "",
                        outcome: "failed",
                        error: &format!("agent error: {e}"),
                        tokens: RunTokenUsage::default(),
                    })
                    .await;
            }
            return Ok(ReviewDecision::Failed(format!("agent error: {e}")));
        }
    };

    // Stage 2: parse the ReviewResponse from the extracted text.
    let review_response = match runner::response::parse_review_from_output(&text_for_review) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(
                task_id = task.id.0,
                error = %e,
                output = %text_for_review.chars().take(300).collect::<String>(),
                "failed to parse review response"
            );
            if let Some(rid) = run_id {
                let _ = store
                    .complete_run(&CompleteRun {
                        run_id: rid,
                        exit_code: Some(exit_code),
                        stdout: &raw_output,
                        stderr: &stderr,
                        parsed: &text_for_review,
                        outcome: "failed",
                        error: &format!("parse error: {e}"),
                        tokens: RunTokenUsage::default(),
                    })
                    .await;
            }
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
        let footer = attribution_footer("Reviewed", &review_agent, &review_model);
        let pr_comment_with_footer = format!("{}{}", pr_comment, footer);
        if let Err(e) = gh
            .add_comment(repo, &pr_number_early.to_string(), &pr_comment_with_footer)
            .await
        {
            tracing::warn!(
                task_id = task.id.0,
                pr_number = pr_number_early,
                error = %e,
                "failed to post automated review comment on PR"
            );
        }
    }

    // 13. Complete run tracking
    if let Some(run_id) = run_id {
        let outcome = match &decision {
            ReviewDecision::Approve => "success",
            ReviewDecision::RequestChanges { .. } => "success",
            ReviewDecision::Failed(_) => "failed",
            ReviewDecision::Blocked(_) => "success",
            ReviewDecision::Skipped => "success",
        };
        let error = match &decision {
            ReviewDecision::Failed(e) => e.as_str(),
            ReviewDecision::Blocked(e) => e.as_str(),
            _ => "",
        };
        let _ = store
            .complete_run(&CompleteRun {
                run_id,
                exit_code: Some(exit_code),
                stdout: &raw_output,
                stderr: &stderr,
                parsed: &text_for_review,
                outcome,
                error,
                tokens: RunTokenUsage::default(),
            })
            .await;
    }

    // 14. Check for push failures before acting on the decision.
    let has_push_failure = opt_store_get_task(&Some(Arc::clone(store)), repo, &task.id.0)
        .await
        .map(|t| t.last_error)
        .unwrap_or_default()
        .contains("push failed");

    // 15. Handle the decision
    match decision {
        ReviewDecision::Approve => {
            if has_push_failure {
                tracing::warn!(
                    task_id = task.id.0,
                    pr_number = pr_number_early,
                    "review approved but last push failed — blocking for human check"
                );
                if let Err(e) = task_manager
                    .update_task_status(&task.id, Status::Blocked)
                    .await
                {
                    tracing::error!(task_id = task.id.0, err = %e, "failed to block task");
                }
                return Ok(ReviewDecision::Blocked(
                    "review approved but last push failed — PR may have stale code".to_string(),
                ));
            }

            let auto_merge = crate::config::get("workflow.auto_close_task_on_approval")
                .or_else(|_| crate::config::get("workflow.auto_close"))
                .or_else(|_| crate::config::get("workflow.auto_merge"))
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
            } else {
                tracing::info!(
                    task_id = task.id.0,
                    pr_number = pr_number_early,
                    "review approved, PR left open for human merge — marking task done"
                );
                if let Err(e) = task_manager
                    .update_task_status(&task.id, Status::Done)
                    .await
                {
                    tracing::error!(
                        task_id = task.id.0,
                        err = %e,
                        "update_task_status(Done) failed — task may be stuck in InReview"
                    );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::ExternalTask;
    use crate::engine::router::RouterConfig;
    use crate::github::types::{GitHubReview, GitHubReviewComment, GitHubUser, PullRequestReview};
    use crate::store::TaskStore;
    use tempfile::TempDir;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {:?} failed", args);
    }

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
    fn test_router_config_model_for_complexity_returns_nonempty() {
        let cfg = RouterConfig::default();
        assert!(!cfg
            .model_for_complexity_or_default("claude", "simple", "")
            .is_empty());
        assert!(!cfg
            .model_for_complexity_or_default("claude", "medium", "")
            .is_empty());
        assert!(!cfg
            .model_for_complexity_or_default("claude", "complex", "")
            .is_empty());
        assert!(!cfg
            .model_for_complexity_or_default("claude", "review", "")
            .is_empty());
    }

    #[test]
    fn test_router_config_model_for_complexity_unknown_agent() {
        let cfg = RouterConfig::default();
        let model = cfg.model_for_complexity_or_default("unknown_agent_xyz", "simple", "");
        assert!(!model.is_empty());
    }

    #[test]
    fn review_started_comment_includes_agent_and_model() {
        assert_eq!(
            review_started_comment("kimi", "opus"),
            "🔍 Automated review started (agent: kimi, model: opus)"
        );
    }

    #[tokio::test]
    async fn recover_missing_review_worktree_recreates_worktree() {
        let temp_home = TempDir::new().unwrap();
        let orch_dir = temp_home.path().join(".orch");
        std::fs::create_dir_all(&orch_dir).unwrap();
        let old_orch_home = std::env::var_os("ORCH_HOME");
        std::env::set_var("ORCH_HOME", &orch_dir);

        let project_dir = temp_home.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        git(&project_dir, &["init", "-b", "main"]);
        git(&project_dir, &["config", "user.name", "Test User"]);
        git(&project_dir, &["config", "user.email", "test@example.com"]);
        std::fs::write(project_dir.join("README.md"), "hello").unwrap();
        git(&project_dir, &["add", "README.md"]);
        git(&project_dir, &["commit", "-m", "init"]);

        std::fs::write(
            orch_dir.join("config.yml"),
            format!("projects:\n  - {}\n", project_dir.display()),
        )
        .unwrap();
        std::fs::write(project_dir.join(".orch.yml"), "gh:\n  repo: owner/repo\n").unwrap();

        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let repo = "owner/repo";
        store
            .create_internal(repo, "Fix review recovery", "", "review", "review-1")
            .await
            .unwrap();
        let ext = ExternalTask {
            id: crate::backends::ExternalId("1".to_string()),
            title: "Fix review recovery".to_string(),
            body: "".to_string(),
            state: "open".to_string(),
            labels: vec![],
            author: "tester".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: "https://example.com/issues/1".to_string(),
        };
        store.ensure_external_task(repo, &ext).await.unwrap();

        let recovered = recover_missing_review_worktree(&ext, repo, &store)
            .await
            .unwrap();

        assert!(recovered.work_dir.exists());
        assert!(recovered.branch.starts_with("gh-issue-1-"));

        if let Some(old) = old_orch_home {
            std::env::set_var("ORCH_HOME", old);
        } else {
            std::env::remove_var("ORCH_HOME");
        }
    }
}
