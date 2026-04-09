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

/// Map an [`AgentError`] to the structured outcome string stored in `task_runs.outcome`.
///
/// Mirrors the logic in `runner::classify_run_outcome` so review runs carry the
/// same outcome fidelity as agent runs.
fn outcome_for_agent_error(err: &runner::agents::AgentError) -> &'static str {
    match err {
        runner::agents::AgentError::Timeout { .. } => "timeout",
        runner::agents::AgentError::RateLimit { .. } => "rate_limit",
        runner::agents::AgentError::Auth { .. } => "auth_error",
        runner::agents::AgentError::InvalidResponse { .. } => "parse_error",
        _ => "failed",
    }
}

/// Parse a PR number from a GitHub PR URL.
///
/// Validates that the URL matches the expected GitHub PR URL format
/// (`https://github.com/<owner>/<repo>/pull/<number>`) before extracting the
/// number. Returns `None` for any URL that doesn't conform to this structure,
/// including URLs with wrong domains, missing `/pull/` segments, or
/// non-numeric trailing components.
///
/// The extracted segment is stripped of any query string (`?`) or hash
/// fragment (`#`) before parsing so that URLs like
/// `https://github.com/owner/repo/pull/42?tab=files` are handled correctly.
pub(crate) fn parse_pr_number_from_url(url: &str) -> Option<u64> {
    let url = url.trim();
    if !url.starts_with("https://github.com/") {
        return None;
    }
    if !url.contains("/pull/") {
        return None;
    }
    url.trim_end_matches('/')
        .rsplit('/')
        .next()
        .and_then(|seg| seg.split(['?', '#']).next())
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n > 0)
}

fn review_started_comment(review_agent: &str, review_model: &str) -> String {
    let footer = attribution_footer("Review started", review_agent, Some(review_model));
    format!("🔍 Automated review started{}", footer)
}

fn should_skip_no_code_reroute_increment(last_error: &str) -> bool {
    crate::engine::runner::git_ops::is_transient_github_api_error(last_error)
}

use crate::backends::{ExternalBackend, ExternalTask, Status};
use crate::cmd::CommandErrorContext;
use crate::config;
use crate::engine::attribution_footer;
use crate::engine::auto_merge::{auto_merge_pr, handle_review_changes};
use crate::engine::runner;
use crate::engine::runner::worktree;
use crate::engine::tasks::TaskManager;
use crate::github::http::GhHttp;
use crate::store::store_log_activity;
use crate::store::store_set;
use crate::store::TaskStore;
use crate::store::{opt_store_get_task, set_review_session_expected, store_increment};
use crate::store::{CompleteRun, RunTokenUsage, StartRun};
use crate::tmux::TmuxManager;
use anyhow::Context;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::process::Command;
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

enum ReviewPhase<T> {
    Ready(T),
    EarlyReturn(ReviewDecision),
}

struct ReviewContext {
    store_id: Option<i64>,
    had_prev_push_failure: bool,
    review_attempt: u32,
    worktree_path: std::path::PathBuf,
    branch_name: String,
    pr_number: u64,
    default_branch: String,
    review_agent: String,
    review_model: Option<String>,
    review_task_id: String,
    review_attempt_dir: std::path::PathBuf,
    output_file: std::path::PathBuf,
    invocation: runner::agent::AgentInvocation,
}

impl ReviewContext {
    fn review_model_str(&self) -> &str {
        self.review_model.as_deref().unwrap_or("")
    }
}

struct ReviewRun {
    run_id: Option<i64>,
}

struct ParsedReview {
    run_id: Option<i64>,
    exit_code: i32,
    stderr: String,
    raw_output: String,
    text_for_review: String,
    token_usage: RunTokenUsage,
    review_notes_for_comment: String,
    decision: ReviewDecision,
}

fn build_review_disallowed_tools(worktree_path: &std::path::Path) -> Vec<String> {
    let mut tools = vec![
        "Bash(rm *)".to_string(),
        "Bash(rm -*)".to_string(),
        "Bash(git push*)".to_string(),
    ];
    let orch_yml = worktree_path.join(".orch.yml");
    let orch_yml_str = orch_yml.to_string_lossy();
    tools.extend([
        format!("Write({orch_yml_str})"),
        format!("Edit({orch_yml_str})"),
    ]);
    if let Ok(orch_home) = crate::home::orch_home() {
        for path in [
            orch_home.join("config.yml"),
            orch_home.join("config.example.yml"),
        ] {
            let path_str = path.to_string_lossy();
            tools.extend([
                format!("Read({path_str})"),
                format!("Write({path_str})"),
                format!("Edit({path_str})"),
            ]);
        }
    }
    tools
}

async fn select_review_agent(
    task_id: &str,
    task_agent: &str,
    store_id: Option<i64>,
    router: &Arc<RwLock<Router>>,
    store: &Arc<TaskStore>,
) -> (String, Option<String>) {
    let mut exclude_set: HashSet<String> = HashSet::new();
    if !task_agent.is_empty() {
        exclude_set.insert(task_agent.to_string());
    }
    if let Some(store_id) = store_id {
        if let Ok(failed) = store.previous_review_agents(store_id).await {
            for agent in failed {
                exclude_set.insert(agent);
            }
        }
    }

    let mut r = router.write().await;
    let mut tried_agents: HashSet<String> = HashSet::new();
    let mut chosen_agent: Option<String> = None;
    let mut chosen_model: Option<String> = None;
    let available_count = r.available_agents.len().max(1);

    loop {
        let tmp_exclude_refs: Vec<&str> = exclude_set.iter().map(|s| s.as_str()).collect();
        let agent = match r.next_round_robin_agent(&tmp_exclude_refs, "review") {
            Some(a) => a,
            None => break,
        };

        if tried_agents.contains(&agent) {
            break;
        }
        tried_agents.insert(agent.clone());

        let model = r.config.model_for_complexity(&agent, "review", task_id);
        if model.is_some() {
            chosen_agent = Some(agent);
            chosen_model = model;
            break;
        }

        exclude_set.insert(agent);
        if exclude_set.len() >= available_count {
            break;
        }
    }

    if let Some(agent) = chosen_agent {
        (agent, chosen_model)
    } else {
        let final_exclude_refs: Vec<&str> = exclude_set.iter().map(|s| s.as_str()).collect();
        let fallback_agent = r
            .next_round_robin_agent(&final_exclude_refs, "review")
            .unwrap_or_else(|| "claude".to_string());
        let fallback_model = r
            .config
            .model_for_complexity(&fallback_agent, "review", task_id);
        (fallback_agent, fallback_model)
    }
}

#[allow(clippy::too_many_arguments)]
async fn build_review_context(
    task: &ExternalTask,
    repo: &str,
    router: &Arc<RwLock<Router>>,
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
) -> anyhow::Result<ReviewPhase<ReviewContext>> {
    let stored_task = store
        .get_by_external_id(repo, &task.id.0)
        .await
        .with_context(|| format!("store lookup failed for task {}", task.id.0))?;

    let store_id: Option<i64> = stored_task.as_ref().map(|t| t.id);
    let had_prev_push_failure = stored_task
        .as_ref()
        .is_some_and(|t| t.last_error.contains("push failed"));
    let review_attempt: u32 = match store_increment(
        &Some(Arc::clone(store)),
        repo,
        &task.id.0,
        "review_invocations",
    )
    .await
    {
        Ok(v) => v as u32,
        Err(e) => {
            tracing::warn!(task_id = task.id.0, err = %e, "failed to increment review_invocations — using fallback attempt=1");
            1
        }
    };

    let mut worktree_path = match stored_task.as_ref().map(|t| t.worktree.as_str()) {
        Some(w) if !w.is_empty() => std::path::PathBuf::from(w),
        _ => {
            tracing::warn!(task_id = task.id.0, "no worktree found for review");
            return Ok(ReviewPhase::EarlyReturn(ReviewDecision::Failed(
                "no worktree found".to_string(),
            )));
        }
    };

    let mut branch_name = match stored_task.as_ref().map(|t| t.branch.as_str()) {
        Some(b) if !b.is_empty() => b.to_string(),
        _ => {
            tracing::warn!(task_id = task.id.0, "no branch found for review");
            return Ok(ReviewPhase::EarlyReturn(ReviewDecision::Failed(
                "no branch found".to_string(),
            )));
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
                return Ok(ReviewPhase::EarlyReturn(ReviewDecision::Blocked(reason)));
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
    let stored_pr_number = stored_task
        .as_ref()
        .and_then(|t| t.pr_number)
        .map(|n| n as u64)
        .filter(|&n| n > 0);

    let pr_number = match ensure_pr_exists(
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
        EnsurePrResult::EarlyReturn(decision) => return Ok(ReviewPhase::EarlyReturn(decision)),
    };

    if let Err(e) = Command::new("git")
        .args(["fetch", "origin", "--prune"])
        .current_dir(&worktree_path)
        .output_with_context()
        .await
    {
        tracing::warn!(
            task_id = task.id.0,
            error = %e,
            "review: git fetch failed — diff/log may use stale remote refs"
        );
    }

    let default_branch = runner::worktree::detect_default_branch(&worktree_path).await;
    let git_diff = runner::context::build_git_diff(&worktree_path, &default_branch).await;
    let git_log = runner::context::build_git_log(&worktree_path, &default_branch).await;
    let review_prompt = runner::agent::build_review_prompt(
        task,
        &agent_summary,
        &git_diff,
        &git_log,
        &default_branch,
        pr_number,
    );

    let task_agent = stored_task
        .as_ref()
        .and_then(|t| t.agent.clone())
        .unwrap_or_default();
    let (review_agent, review_model) =
        select_review_agent(&task.id.0, &task_agent, store_id, router, store).await;

    tracing::info!(
        task_id = task.id.0,
        agent = %review_agent,
        model = ?review_model,
        "spawning review agent"
    );

    let review_task_id = format!("{}-review", task.id.0);
    let review_attempt_dir =
        crate::home::task_attempt_dir_async(repo, &review_task_id, review_attempt).await?;
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
        })
        .max(1800);

    let invocation = runner::agent::AgentInvocation {
        agent: review_agent.clone(),
        model: review_model.clone(),
        work_dir: worktree_path.clone(),
        system_prompt: runner::agent::review_system_prompt(),
        agent_message: review_prompt,
        task_id: review_task_id.clone(),
        disallowed_tools: build_review_disallowed_tools(&worktree_path),
        git_author_name: git_name,
        git_author_email: git_email,
        output_file: output_file.clone(),
        timeout_seconds: review_timeout_secs,
        repo: repo.to_string(),
        attempt: review_attempt,
    };

    Ok(ReviewPhase::Ready(ReviewContext {
        store_id,
        had_prev_push_failure,
        review_attempt,
        worktree_path,
        branch_name,
        pr_number,
        default_branch,
        review_agent,
        review_model,
        review_task_id,
        review_attempt_dir,
        output_file,
        invocation,
    }))
}

#[allow(clippy::too_many_arguments)]
async fn complete_review_run(
    store: &Arc<TaskStore>,
    run_id: Option<i64>,
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
    parsed: &str,
    outcome: &str,
    error: &str,
    tokens: RunTokenUsage,
) {
    if let Some(run_id) = run_id {
        if let Err(e) = store
            .complete_run(&CompleteRun {
                run_id,
                exit_code,
                stdout,
                stderr,
                parsed,
                outcome,
                error,
                tokens,
            })
            .await
        {
            tracing::warn!(
                run_id,
                error = %e,
                "failed to record review run completion in audit trail"
            );
        }
    }
}

async fn invoke_review_agent(
    task: &ExternalTask,
    tmux: &Arc<TmuxManager>,
    ctx: &ReviewContext,
    repo: &str,
    store: &Arc<TaskStore>,
) -> ReviewPhase<ReviewRun> {
    let run_id = if let Some(sid) = ctx.store_id {
        match store
            .start_run(&StartRun {
                task_id: sid,
                attempt: ctx.review_attempt as i32,
                run_type: "review",
                agent: &ctx.review_agent,
                model: ctx.review_model.as_deref().unwrap_or(""),
                command: "",
                prompt: "",
            })
            .await
        {
            Ok(run_id) => Some(run_id),
            Err(e) => {
                tracing::warn!(
                    task_id = task.id.0,
                    error = %e,
                    "failed to record review run start in audit trail"
                );
                None
            }
        }
    } else {
        None
    };

    let session = match runner::agent::spawn_in_tmux(tmux, &ctx.invocation).await {
        Ok(session) => session,
        Err(e) => {
            tracing::error!(task_id = task.id.0, error = %e, "failed to spawn review agent");
            return ReviewPhase::EarlyReturn(ReviewDecision::Failed(format!("spawn failed: {e}")));
        }
    };

    let start_comment = review_started_comment(&ctx.review_agent, ctx.review_model_str());
    match GhHttp::new() {
        Ok(gh) => {
            if let Err(e) = gh
                .add_comment(repo, &ctx.pr_number.to_string(), &start_comment)
                .await
            {
                tracing::debug!(
                    task_id = task.id.0,
                    pr_number = ctx.pr_number,
                    error = %e,
                    "failed to post review-started comment"
                );
            }
        }
        Err(e) => {
            tracing::debug!(
                task_id = task.id.0,
                pr_number = ctx.pr_number,
                error = %e,
                "failed to create GitHub client for review-started comment"
            );
        }
    }

    let poll_interval = std::time::Duration::from_secs(5);
    let timeout_duration = std::time::Duration::from_secs(ctx.invocation.timeout_seconds);
    let wait_result = tokio::time::timeout(
        timeout_duration,
        tmux.wait_for_completion(&session, poll_interval),
    )
    .await;

    match wait_result {
        Ok(Ok(_)) => {
            tracing::info!(task_id = task.id.0, "review agent completed");
            let _ = tmux.kill_session(&session).await;
            ReviewPhase::Ready(ReviewRun { run_id })
        }
        Ok(Err(e)) => {
            tracing::error!(task_id = task.id.0, error = %e, "review agent error");
            let _ = tmux.kill_session(&session).await;
            complete_review_run(
                store,
                run_id,
                Some(-1),
                "",
                &e.to_string(),
                "",
                "failed",
                &format!("agent error: {e}"),
                RunTokenUsage::default(),
            )
            .await;
            ReviewPhase::EarlyReturn(ReviewDecision::Failed(format!("agent error: {e}")))
        }
        Err(_) => {
            tracing::error!(task_id = task.id.0, "review agent timed out");
            let _ = tmux.kill_session(&session).await;
            complete_review_run(
                store,
                run_id,
                Some(-1),
                "",
                "timeout",
                "",
                "timeout",
                "timeout",
                RunTokenUsage::default(),
            )
            .await;
            ReviewPhase::EarlyReturn(ReviewDecision::Failed("timeout".to_string()))
        }
    }
}

async fn record_review_agent_failure(
    task_id: &str,
    review_agent: &str,
    err: &runner::agents::AgentError,
) {
    match err {
        runner::agents::AgentError::RateLimit { message }
        | runner::agents::AgentError::Auth { message } => {
            tracing::warn!(
                task_id,
                agent = %review_agent,
                "review agent hit error — adding to cooldown"
            );
            if let Some(reason) = crate::engine::cooldown::detect_credit_exhaustion(message) {
                crate::engine::cooldown::record_credit_exhaustion(review_agent, reason).await;
            } else {
                runner::response::record_agent_failure_with_message(review_agent, &err.to_string())
                    .await;
            }
        }
        _ => {}
    }
}

fn review_decision_from_response(
    review_response: runner::response::ReviewResponse,
) -> ReviewDecision {
    match review_response.decision.as_str() {
        "approve" => ReviewDecision::Approve,
        "request_changes" => ReviewDecision::RequestChanges {
            notes: review_response.notes,
            issues: review_response.issues,
        },
        _ => ReviewDecision::Failed(format!("unknown decision: {}", review_response.decision)),
    }
}

fn review_decision_name(decision: &ReviewDecision) -> &'static str {
    match decision {
        ReviewDecision::Approve => "approve",
        ReviewDecision::RequestChanges { .. } => "request_changes",
        ReviewDecision::Failed(_) => "failed",
        ReviewDecision::Blocked(_) => "blocked",
        ReviewDecision::Skipped => "skipped",
    }
}

async fn parse_review_output(
    task: &ExternalTask,
    repo: &str,
    ctx: &ReviewContext,
    run: ReviewRun,
    store: &Arc<TaskStore>,
) -> ReviewPhase<ParsedReview> {
    let (file_exists, file_size, exit_code, stderr) = {
        let output_file_clone = ctx.output_file.clone();
        let review_attempt_dir_clone = ctx.review_attempt_dir.clone();
        match tokio::task::spawn_blocking(move || {
            let file_exists = output_file_clone.exists();
            let file_size = std::fs::metadata(&output_file_clone)
                .map(|m| m.len())
                .unwrap_or(0);
            let exit_code = std::fs::read_to_string(review_attempt_dir_clone.join("exit.txt"))
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok())
                .unwrap_or(-1);
            let stderr = std::fs::read_to_string(review_attempt_dir_clone.join("stderr.txt"))
                .unwrap_or_default();
            (file_exists, file_size, exit_code, stderr)
        })
        .await
        {
            Ok(tuple) => tuple,
            Err(e) => {
                tracing::error!(
                    task_id = task.id.0,
                    error = %e,
                    "spawn_blocking panicked reading review output metadata"
                );
                (false, 0, -1, format!("spawn_blocking failed: {e}"))
            }
        }
    };

    tracing::info!(
        task_id = task.id.0,
        path = %ctx.output_file.display(),
        file_exists,
        file_size,
        "reading review agent output"
    );

    let raw_output =
        runner::response::read_output_file(&ctx.review_task_id, &ctx.output_file, repo).await;
    let agent_runner = runner::agents::get_runner(&ctx.review_agent);
    let agent_result_for_tokens = runner::agents::find_agent_result(&ctx.review_agent, &raw_output);
    let token_usage = RunTokenUsage {
        input_tokens: agent_result_for_tokens
            .as_ref()
            .and_then(|r| r.input_tokens)
            .unwrap_or(0),
        output_tokens: agent_result_for_tokens
            .as_ref()
            .and_then(|r| r.output_tokens)
            .unwrap_or(0),
        total_cost_usd: agent_result_for_tokens
            .as_ref()
            .and_then(|r| r.cost_usd)
            .unwrap_or(0.0),
        duration_secs: 0.0,
    };

    let agent_result_is_error = agent_result_for_tokens
        .as_ref()
        .map(|r| r.is_error)
        .unwrap_or(false);
    let is_hard_failure = raw_output.is_empty() || agent_result_is_error;

    if is_hard_failure {
        tracing::warn!(
            task_id = task.id.0,
            exit_code,
            raw_output_len = raw_output.len(),
            stderr_len = stderr.len(),
            "review agent: entering error path"
        );
        let err = agent_runner.classify_error(exit_code, &raw_output, &stderr);
        record_review_agent_failure(&task.id.0, &ctx.review_agent, &err).await;
        tracing::error!(task_id = task.id.0, error = %err, "review agent failed");
        complete_review_run(
            store,
            run.run_id,
            Some(exit_code),
            &raw_output,
            &stderr,
            "",
            outcome_for_agent_error(&err),
            &err.to_string(),
            token_usage,
        )
        .await;
        return ReviewPhase::EarlyReturn(ReviewDecision::Failed(format!("agent error: {err}")));
    }

    let text_for_review = agent_result_for_tokens
        .as_ref()
        .map(|r| r.result_text.clone())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| {
            tracing::debug!(
                task_id = task.id.0,
                agent = %ctx.review_agent,
                "review agent: per-agent extractor returned no text, using raw output"
            );
            raw_output.clone()
        });

    let review_response = match runner::response::parse_review_response(&text_for_review) {
        Ok(response) => response,
        Err(e) => {
            if let Some(response) = runner::response::infer_review_response(&text_for_review) {
                tracing::warn!(
                    task_id = task.id.0,
                    agent = %ctx.review_agent,
                    "review response parsed via keyword fallback"
                );
                response
            } else {
                let already_errored = agent_result_for_tokens
                    .as_ref()
                    .map(|r| r.is_error)
                    .unwrap_or(false);
                if already_errored {
                    let err = agent_runner.classify_error(exit_code, &raw_output, &stderr);
                    record_review_agent_failure(&task.id.0, &ctx.review_agent, &err).await;
                    tracing::error!(
                        task_id = task.id.0,
                        error = %err,
                        "review agent error from per-agent extractor"
                    );
                    complete_review_run(
                        store,
                        run.run_id,
                        Some(exit_code),
                        &raw_output,
                        &stderr,
                        &text_for_review,
                        outcome_for_agent_error(&err),
                        &format!("per-agent error: {err}"),
                        token_usage,
                    )
                    .await;
                    return ReviewPhase::EarlyReturn(ReviewDecision::Failed(format!(
                        "per-agent error: {err}"
                    )));
                }

                if let Some(runner::agents::AgentError::RateLimit { message }) =
                    runner::agents::patterns::detect_rate_limit(&text_for_review)
                {
                    tracing::warn!(
                        task_id = task.id.0,
                        agent = %ctx.review_agent,
                        "review agent hit rate limit — adding to cooldown"
                    );
                    runner::response::record_agent_failure_with_message(
                        &ctx.review_agent,
                        &message,
                    )
                    .await;
                    complete_review_run(
                        store,
                        run.run_id,
                        Some(exit_code),
                        &raw_output,
                        &stderr,
                        &text_for_review,
                        "rate_limit",
                        &format!("rate limit: {message}"),
                        token_usage,
                    )
                    .await;
                    return ReviewPhase::EarlyReturn(ReviewDecision::Failed(format!(
                        "rate limit: {message}"
                    )));
                }

                if let Some(runner::agents::AgentError::Auth { message }) =
                    runner::agents::patterns::detect_auth_error(&text_for_review)
                {
                    tracing::warn!(
                        task_id = task.id.0,
                        agent = %ctx.review_agent,
                        "review agent hit auth error — adding to cooldown"
                    );
                    runner::response::record_agent_failure_with_message(
                        &ctx.review_agent,
                        &message,
                    )
                    .await;
                    complete_review_run(
                        store,
                        run.run_id,
                        Some(exit_code),
                        &raw_output,
                        &stderr,
                        &text_for_review,
                        "auth_error",
                        &format!("auth error: {message}"),
                        token_usage,
                    )
                    .await;
                    return ReviewPhase::EarlyReturn(ReviewDecision::Failed(format!(
                        "auth error: {message}"
                    )));
                }

                tracing::error!(
                    task_id = task.id.0,
                    error = %e,
                    agent = %ctx.review_agent,
                    output = %text_for_review.chars().take(300).collect::<String>(),
                    raw_output = %raw_output.chars().take(1000).collect::<String>(),
                    "failed to parse review response"
                );
                complete_review_run(
                    store,
                    run.run_id,
                    Some(exit_code),
                    &raw_output,
                    &stderr,
                    &text_for_review,
                    "parse_error",
                    &format!("parse error: {e}"),
                    token_usage,
                )
                .await;
                return ReviewPhase::EarlyReturn(ReviewDecision::Failed(format!(
                    "parse error: {e}"
                )));
            }
        }
    };

    let review_notes_for_comment = review_response.notes.clone();
    let decision = review_decision_from_response(review_response);

    tracing::info!(
        task_id = task.id.0,
        pr_number = ctx.pr_number,
        decision = ?decision,
        "review agent decision received"
    );
    store_log_activity(
        &Some(Arc::clone(store)),
        repo,
        &task.id.0,
        "review_decision",
        None,
        None,
        Some(&ctx.review_agent),
        ctx.review_model.as_deref(),
        Some(&serde_json::json!({
            "decision": review_decision_name(&decision),
            "pr_number": ctx.pr_number,
        })),
    )
    .await;

    ReviewPhase::Ready(ParsedReview {
        run_id: run.run_id,
        exit_code,
        stderr,
        raw_output,
        text_for_review,
        token_usage,
        review_notes_for_comment,
        decision,
    })
}

async fn finalize_review_run(
    task: &ExternalTask,
    ctx: &ReviewContext,
    parsed: &ParsedReview,
    store: &Arc<TaskStore>,
) {
    let outcome = match &parsed.decision {
        ReviewDecision::Approve
        | ReviewDecision::RequestChanges { .. }
        | ReviewDecision::Skipped => "success",
        ReviewDecision::Failed(_) | ReviewDecision::Blocked(_) => "failed",
    };
    let error = match &parsed.decision {
        ReviewDecision::Failed(error) | ReviewDecision::Blocked(error) => error.as_str(),
        _ => "",
    };

    complete_review_run(
        store,
        parsed.run_id,
        Some(parsed.exit_code),
        &parsed.raw_output,
        &parsed.stderr,
        &parsed.text_for_review,
        outcome,
        error,
        parsed.token_usage,
    )
    .await;

    if let Some(store_id) = ctx.store_id {
        if parsed.token_usage.input_tokens > 0 || parsed.token_usage.output_tokens > 0 {
            let model = if ctx.review_model_str().is_empty() {
                "unknown"
            } else {
                ctx.review_model_str()
            };
            if let Err(e) = store
                .store_tokens(
                    store_id,
                    parsed.token_usage.input_tokens,
                    parsed.token_usage.output_tokens,
                    model,
                )
                .await
            {
                tracing::warn!(
                    task_id = task.id.0,
                    ?e,
                    "failed to store review agent token usage"
                );
            }
        }
    }
}

async fn restore_review_config_if_needed(
    task_id: &str,
    worktree_path: &std::path::Path,
    default_branch: &str,
) {
    let _ = Command::new("git")
        .args(["checkout", "HEAD", "--", ".orch.yml"])
        .current_dir(worktree_path)
        .status()
        .await;

    let merge_base = Command::new("git")
        .args(["merge-base", "HEAD", default_branch])
        .current_dir(worktree_path)
        .output()
        .await
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    if merge_base.is_empty() {
        return;
    }

    let orch_yml_changed = Command::new("git")
        .args([
            "diff",
            "--name-only",
            &merge_base,
            "HEAD",
            "--",
            ".orch.yml",
        ])
        .current_dir(worktree_path)
        .output()
        .await
        .ok()
        .filter(|output| output.status.success())
        .map(|output| !output.stdout.is_empty())
        .unwrap_or(false);

    if !orch_yml_changed {
        return;
    }

    tracing::warn!(
        task_id,
        "review agent modified .orch.yml (forbidden); restoring to merge-base version"
    );
    let restore_ok = Command::new("git")
        .args(["checkout", &merge_base, "--", ".orch.yml"])
        .current_dir(worktree_path)
        .status()
        .await
        .map(|status| status.success())
        .unwrap_or(false);

    if !restore_ok {
        return;
    }

    let staged = Command::new("git")
        .args(["diff", "--cached", "--name-only", "--", ".orch.yml"])
        .current_dir(worktree_path)
        .output()
        .await
        .ok()
        .filter(|output| output.status.success())
        .map(|output| !output.stdout.is_empty())
        .unwrap_or(false);

    if staged {
        let _ = Command::new("git")
            .args([
                "commit",
                "-m",
                "revert: restore .orch.yml (review agent must not modify project config)",
            ])
            .current_dir(worktree_path)
            .status()
            .await;
    }
}

async fn push_review_branch(
    task: &ExternalTask,
    repo: &str,
    ctx: &ReviewContext,
    store: &Arc<TaskStore>,
) -> Result<(), String> {
    match runner::git_ops::push_branch(&ctx.worktree_path, &ctx.branch_name, &ctx.default_branch)
        .await
    {
        Ok(_) => {
            store_log_activity(
                &Some(Arc::clone(store)),
                repo,
                &task.id.0,
                "push",
                None,
                None,
                Some(&ctx.review_agent),
                ctx.review_model.as_deref(),
                Some(&serde_json::json!({
                    "status": "ok",
                    "branch": ctx.branch_name.clone(),
                    "default_branch": ctx.default_branch.clone(),
                    "phase": "review",
                })),
            )
            .await;
            if ctx.had_prev_push_failure {
                store_set(
                    &Some(Arc::clone(store)),
                    repo,
                    &task.id.0,
                    &[
                        ("last_error", serde_json::json!("")),
                        ("push_failures", serde_json::json!(0)),
                    ],
                )
                .await;
            }
            Ok(())
        }
        Err(e) => {
            store_log_activity(
                &Some(Arc::clone(store)),
                repo,
                &task.id.0,
                "push",
                None,
                None,
                Some(&ctx.review_agent),
                ctx.review_model.as_deref(),
                Some(&serde_json::json!({
                    "status": "error",
                    "branch": ctx.branch_name.clone(),
                    "default_branch": ctx.default_branch.clone(),
                    "phase": "review",
                    "error": e.to_string(),
                })),
            )
            .await;
            tracing::error!(
                task_id = task.id.0,
                branch = %ctx.branch_name,
                error = %e,
                "review push failed"
            );
            store_set(
                &Some(Arc::clone(store)),
                repo,
                &task.id.0,
                &[("last_error", serde_json::json!(format!("push failed: {e}")))],
            )
            .await;
            Err(e.to_string())
        }
    }
}

fn build_pr_review_comment(decision: &ReviewDecision, review_notes_for_comment: &str) -> String {
    match decision {
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
                            .map(|line| line.to_string())
                            .unwrap_or_else(|| "?".to_string()),
                        issue.description,
                        issue.severity
                    ));
                }
            }
            body
        }
        _ => String::new(),
    }
}

async fn post_review_comment(
    task: &ExternalTask,
    repo: &str,
    ctx: &ReviewContext,
    decision: &ReviewDecision,
    review_notes_for_comment: &str,
) -> anyhow::Result<()> {
    let gh = match GhHttp::new() {
        Ok(gh) => gh,
        Err(e) => {
            tracing::warn!(
                task_id = task.id.0,
                error = %e,
                "failed to create GH client for review comment"
            );
            return Ok(());
        }
    };
    let pr_comment = build_pr_review_comment(decision, review_notes_for_comment);

    if !pr_comment.is_empty() {
        let footer =
            attribution_footer("Reviewed", &ctx.review_agent, Some(ctx.review_model_str()));
        let pr_comment_with_footer = format!("{}{}", pr_comment, footer);
        if let Err(e) = gh
            .add_comment(repo, &ctx.pr_number.to_string(), &pr_comment_with_footer)
            .await
        {
            tracing::warn!(
                task_id = task.id.0,
                pr_number = ctx.pr_number,
                error = %e,
                "failed to post automated review comment on PR"
            );
        }
    }

    Ok(())
}

async fn apply_review_decision(
    task: &ExternalTask,
    backend: &Arc<dyn ExternalBackend>,
    repo: &str,
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
    ctx: &ReviewContext,
    parsed: &ParsedReview,
) -> anyhow::Result<ReviewDecision> {
    restore_review_config_if_needed(&task.id.0, &ctx.worktree_path, &ctx.default_branch).await;

    if let Err(e) = push_review_branch(task, repo, ctx, store).await {
        return Ok(ReviewDecision::Failed(format!("push failed: {e}")));
    }

    post_review_comment(
        task,
        repo,
        ctx,
        &parsed.decision,
        &parsed.review_notes_for_comment,
    )
    .await?;

    match &parsed.decision {
        ReviewDecision::Approve => {
            let auto_merge = crate::config::get("workflow.auto_close_task_on_approval")
                .or_else(|_| crate::config::get("workflow.auto_close"))
                .or_else(|_| crate::config::get("workflow.auto_merge"))
                .map(|value| value.eq_ignore_ascii_case("true"))
                .unwrap_or(false);

            if auto_merge {
                if let Err(e) = auto_merge_pr(
                    task,
                    &ctx.branch_name,
                    backend,
                    repo,
                    &ctx.review_agent,
                    ctx.review_model_str(),
                    task_manager,
                    store,
                )
                .await
                {
                    let error_msg = e.to_string();
                    if error_msg.contains("not yet computed") {
                        tracing::warn!(
                            task_id = task.id.0,
                            pr_number = ctx.pr_number,
                            branch = %ctx.branch_name,
                            error = %e,
                            "auto-merge deferred — PR mergeability not yet computed"
                        );
                    } else {
                        tracing::error!(
                            task_id = task.id.0,
                            pr_number = ctx.pr_number,
                            branch = %ctx.branch_name,
                            error = %e,
                            "auto-merge failed"
                        );
                    }
                    return Ok(ReviewDecision::Failed(format!("merge failed: {e}")));
                }
            } else {
                tracing::info!(
                    task_id = task.id.0,
                    pr_number = ctx.pr_number,
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
        ReviewDecision::RequestChanges { notes, issues } => {
            handle_review_changes(
                task,
                notes,
                issues,
                backend,
                repo,
                ctx.pr_number,
                &ctx.review_agent,
                ctx.review_model_str(),
                task_manager,
                store,
            )
            .await?;
            Ok(parsed.decision.clone())
        }
        _ => Ok(parsed.decision.clone()),
    }
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

/// Count commits on the worktree branch that are ahead of `origin/<default_branch>`.
///
/// Returns `Err` when the git command cannot determine the count (e.g., missing
/// remote ref, non-zero exit, unparseable output). Callers must NOT treat this
/// as zero — a failure means the ancestry relationship is unknown.
async fn count_ahead_commits(
    worktree_path: &std::path::Path,
    default_branch: &str,
) -> Result<u64, String> {
    let worktree_str = worktree_path.to_str().ok_or_else(|| {
        format!(
            "worktree path contains non-UTF-8 characters: {:?}",
            worktree_path
        )
    })?;

    let output = tokio::process::Command::new("git")
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
                        "worktree path contains non-UTF-8 characters: {:?}",
                        worktree_path
                    )
                })?;
                // Push first in case agent forgot (use token auth like git_ops::push_branch)
                let auth_env = runner::git_ops::build_git_auth_env();

                // Run a credentialed push and log any errors so failures are
                // visible in logs instead of being silently discarded.
                // Wrap with a timeout to prevent indefinitely blocking a Tokio worker on a
                // hung network connection (corporate proxy, partial GitHub outage, TCP hang).
                let push_timeout = std::time::Duration::from_secs(120);
                match tokio::time::timeout(
                    push_timeout,
                    tokio::process::Command::new("git")
                        .args(["-C", worktree_str, "push", "-u", "origin", branch_name])
                        .envs(auth_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
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
                        if !o.status.success() {
                            tracing::warn!(
                                task_id = task.id.0,
                                branch = %branch_name,
                                exit_code = ?o.status.code(),
                                stdout = %String::from_utf8_lossy(&o.stdout),
                                stderr = %String::from_utf8_lossy(&o.stderr),
                                "review gate fallback push failed"
                            );
                        } else {
                            tracing::info!(task_id = task.id.0, branch = %branch_name, "review gate fallback push succeeded");
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
                let pr_body = format!(
                    "Resolves {task_ref}\n\nAuto-created by orch review gate (agent forgot to open PR)"
                );
                let gh = GhHttp::new()?;
                match gh
                    .create_pr(repo, &task.title, &pr_body, branch_name, &default_branch)
                    .await
                {
                    Ok(url) => {
                        let pr_num = parse_pr_number_from_url(&url);
                        if let Some(pr_num) = pr_num {
                            store_set(
                                &Some(Arc::clone(store)),
                                repo,
                                &task.id.0,
                                &[("pr_number", serde_json::json!(pr_num as i64))],
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
                            Ok(EnsurePrResult::Ready(pr_num))
                        } else {
                            Ok(EnsurePrResult::EarlyReturn(ReviewDecision::Failed(
                                "created missing PR, but could not parse PR number".to_string(),
                            )))
                        }
                    }
                    Err(e) => {
                        let e_str = format!("{e}");
                        if e_str.contains("already exists") {
                            // GitHub list-API cache may be stale immediately after a 422.
                            // Use two attempts with exponential backoff (2s → 4s) to avoid
                            // racing into a duplicate `gh pr create` call.
                            let backoff_delays = [2u64, 4u64];
                            let mut found_pr: Option<u64> = None;
                            for delay_secs in backoff_delays {
                                tokio::time::sleep(tokio::time::Duration::from_secs(delay_secs))
                                    .await;
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
                                let pr_num = parse_pr_number_from_url(stdout.trim());
                                if let Some(pr_num) = pr_num {
                                    store_set(
                                        &Some(Arc::clone(store)),
                                        repo,
                                        &task.id.0,
                                        &[("pr_number", serde_json::json!(pr_num as i64))],
                                    )
                                    .await;
                                }
                                tracing::info!(
                                    task_id = task.id.0,
                                    branch = %branch_name,
                                    "created missing PR via CLI — continuing review"
                                );
                                if let Some(pr_num) = pr_num {
                                    Ok(EnsurePrResult::Ready(pr_num))
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
                                        let pr_num = parse_pr_number_from_url(pr_url.trim());
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

                                // If this was a transient GitHub 5xx or transport error,
                                // do NOT increment the persistent pr_create_failures counter
                                // (which would eventually block the task). Let the
                                // engine retry later. Detect via git_ops::is_transient_github_api_error
                                // so generic agent/model transport errors (e.g. broken pipe)
                                // are NOT mis-classified as GitHub API transients.
                                if crate::engine::runner::git_ops::is_transient_github_api_error(
                                    &stderr,
                                ) {
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
                            Err(e) => {
                                tracing::error!(
                                    task_id = task.id.0,
                                    error = %e,
                                    "failed to run gh pr create"
                                );
                                let e_str = format!("{e}");
                                if crate::engine::runner::git_ops::is_transient_github_api_error(
                                    &e_str,
                                ) {
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

                if reroutes as u32 >= max_reroutes {
                    tracing::error!(
                        task_id = task.id.0,
                        reroutes,
                        max_reroutes,
                        "reached max reroute attempts for no-pr-result — blocking for human review"
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
                        .update_task_status_and_result(&task.id, Status::Blocked, &fields)
                        .await
                    {
                        tracing::error!(task_id = task.id.0, err = %e, "update_task_status_and_result(Blocked) failed — skipping block to avoid silent auto-unblock loop");
                    }
                } else {
                    // Clear agent/model so router picks a different one and note the
                    // fact that this attempt produced no PR or code changes.
                    store_set(
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
                    .await;
                    if let Err(e) = task_manager.update_task_status(&task.id, Status::New).await {
                        tracing::error!(task_id = task.id.0, err = %e, "update_task_status(New) failed — task may be stuck in InReview");
                    }
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

    let ctx = match build_review_context(task, repo, router, task_manager, store).await? {
        ReviewPhase::Ready(ctx) => ctx,
        ReviewPhase::EarlyReturn(decision) => return Ok(decision),
    };

    let run = match invoke_review_agent(task, tmux, &ctx, repo, store).await {
        ReviewPhase::Ready(run) => run,
        ReviewPhase::EarlyReturn(decision) => return Ok(decision),
    };

    let parsed = match parse_review_output(task, repo, &ctx, run, store).await {
        ReviewPhase::Ready(parsed) => parsed,
        ReviewPhase::EarlyReturn(decision) => return Ok(decision),
    };

    finalize_review_run(task, &ctx, &parsed, store).await;

    apply_review_decision(task, backend, repo, task_manager, store, &ctx, &parsed).await
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::backends::ExternalTask;
    use crate::engine::router::RouterConfig;
    use crate::github::types::{GitHubReview, GitHubReviewComment, GitHubUser, PullRequestReview};
    use crate::store::TaskStore;
    use tempfile::TempDir;

    // ── parse_pr_number_from_url ────────────────────────────────────────────

    #[test]
    fn parse_pr_number_standard_url() {
        assert_eq!(
            parse_pr_number_from_url("https://github.com/owner/repo/pull/42"),
            Some(42)
        );
    }

    #[test]
    fn parse_pr_number_trailing_slash() {
        assert_eq!(
            parse_pr_number_from_url("https://github.com/owner/repo/pull/42/"),
            Some(42)
        );
    }

    #[test]
    fn parse_pr_number_with_query_string() {
        assert_eq!(
            parse_pr_number_from_url("https://github.com/owner/repo/pull/42?tab=files"),
            Some(42)
        );
    }

    #[test]
    fn parse_pr_number_with_hash_fragment() {
        assert_eq!(
            parse_pr_number_from_url("https://github.com/owner/repo/pull/42#issuecomment-123"),
            Some(42)
        );
    }

    #[test]
    fn parse_pr_number_with_query_and_hash() {
        assert_eq!(
            parse_pr_number_from_url("https://github.com/owner/repo/pull/42?quick_pull=1#top"),
            Some(42)
        );
    }

    #[test]
    fn parse_pr_number_with_leading_whitespace() {
        assert_eq!(
            parse_pr_number_from_url("  https://github.com/owner/repo/pull/42\n"),
            Some(42)
        );
    }

    #[test]
    fn parse_pr_number_wrong_domain() {
        assert_eq!(
            parse_pr_number_from_url("https://gitlab.com/owner/repo/merge_requests/42"),
            None
        );
    }

    #[test]
    fn parse_pr_number_missing_pull_segment() {
        // Issue URL — not a PR URL
        assert_eq!(
            parse_pr_number_from_url("https://github.com/owner/repo/issues/42"),
            None
        );
    }

    #[test]
    fn parse_pr_number_zero_is_rejected() {
        assert_eq!(
            parse_pr_number_from_url("https://github.com/owner/repo/pull/0"),
            None
        );
    }

    #[test]
    fn parse_pr_number_non_numeric_segment() {
        assert_eq!(
            parse_pr_number_from_url("https://github.com/owner/repo/pull/abc"),
            None
        );
    }

    #[test]
    fn parse_pr_number_empty_string() {
        assert_eq!(parse_pr_number_from_url(""), None);
    }

    #[test]
    fn parse_pr_number_http_not_https() {
        // HTTP URLs are not accepted — only HTTPS
        assert_eq!(
            parse_pr_number_from_url("http://github.com/owner/repo/pull/42"),
            None
        );
    }

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
    fn test_router_config_model_for_complexity_returns_none_without_config() {
        // With no model_map configured, model_for_complexity must return None rather than
        // silently using a hardcoded model that may not exist for the given agent.
        let cfg = RouterConfig::default();
        assert!(cfg.model_for_complexity("claude", "simple", "").is_none());
        assert!(cfg.model_for_complexity("opencode", "review", "").is_none());
        assert!(cfg
            .model_for_complexity("unknown_agent_xyz", "simple", "")
            .is_none());
    }

    #[test]
    fn review_started_comment_includes_agent_and_model() {
        let expected_footer = attribution_footer("Review started", "kimi", Some("opus"));
        let expected = format!("🔍 Automated review started{}", expected_footer);
        assert_eq!(review_started_comment("kimi", "opus"), expected);
    }

    #[test]
    fn no_code_reroute_skip_check_rejects_agent_stream_disconnects() {
        let last_error =
            "codex failed: Reconnecting... (stream disconnected before completion: Broken pipe)";
        assert!(!should_skip_no_code_reroute_increment(last_error));
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
            .create_internal(repo, "Fix review recovery", "", "review", "review-1", None)
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

    // ── count_ahead_commits ─────────────────────────────────────────────────

    #[tokio::test]
    async fn count_ahead_commits_returns_zero_when_no_ahead_commits() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path();

        // Create a bare repo to act as the 'origin' remote
        let bare = temp.path().join("bare.git");
        std::fs::create_dir_all(&bare).unwrap();
        std::process::Command::new("git")
            .args(["init", "--bare"])
            .current_dir(&bare)
            .output()
            .unwrap();

        git(dir, &["init", "-b", "main"]);
        git(dir, &["config", "user.name", "Test"]);
        git(dir, &["config", "user.email", "test@test.com"]);
        std::fs::write(dir.join("f"), "a").unwrap();
        git(dir, &["add", "f"]);
        git(dir, &["commit", "-m", "init"]);
        std::process::Command::new("git")
            .args(["remote", "add", "origin", bare.to_str().unwrap()])
            .current_dir(dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["push", "origin", "main"])
            .current_dir(dir)
            .output()
            .unwrap();

        // origin/main now exists and HEAD is at the same commit — zero ahead
        let result = count_ahead_commits(dir, "main").await;
        assert!(
            result.is_ok(),
            "expected Ok when origin/main exists, got {result:?}"
        );
        assert_eq!(
            result.unwrap(),
            0,
            "expected 0 commits ahead when branch matches origin/main"
        );
    }

    #[tokio::test]
    async fn count_ahead_commits_returns_error_when_remote_ref_missing() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path();
        git(dir, &["init", "-b", "main"]);
        git(dir, &["config", "user.name", "Test"]);
        git(dir, &["config", "user.email", "test@test.com"]);
        std::fs::write(dir.join("f"), "a").unwrap();
        git(dir, &["add", "f"]);
        git(dir, &["commit", "-m", "init"]);
        // No remote 'origin' configured — rev-list will fail
        let result = count_ahead_commits(dir, "main").await;
        assert!(
            result.is_err(),
            "expected Err when origin/main does not exist, got {result:?}"
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("git rev-list exited"),
            "error should mention exit failure, got: {err}"
        );
    }

    #[tokio::test]
    async fn count_ahead_commits_returns_error_when_git_cannot_start() {
        // Use a path that is not a git repo — git will fail with non-zero exit
        let temp = TempDir::new().unwrap();
        let dir = temp.path();
        // Not a git repo at all
        let result = count_ahead_commits(dir, "main").await;
        assert!(
            result.is_err(),
            "expected Err when directory is not a git repo, got {result:?}"
        );
    }
}
