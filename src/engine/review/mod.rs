//! PR review pipeline — review agent orchestration.
//!
//! This module handles running the review agent on completed tasks, parsing its
//! decision, and posting automated review comments.
//!
//! Related modules:
//! - [`super::review_poll`] — polling open PRs for human review feedback
//! - [`super::auto_merge`] — merging approved PRs and handling change requests

mod approval;
mod parse;
mod pr;
mod publish;

use crate::engine::git;
pub(crate) use crate::github::types::parse_pr_number_from_url;

use crate::engine::agents;

use super::router::Router;
/// Maximum consecutive review agent failures before the task is blocked
/// for human intervention. Exported so `tick` and `sync` use the same threshold.
use crate::backends::{ExternalBackend, ExternalTask, Status};
use crate::config;
use crate::cooldown;
use crate::cooldown::CreditExhaustionReason;
use crate::engine::attribution_footer;
use crate::engine::auto_merge::{auto_merge_pr, handle_review_changes};
use crate::engine::git::git_ops;
use crate::engine::router::RouterConfig;
use crate::engine::runner;
use crate::engine::tasks;
use crate::engine::tasks::TaskManager;
use crate::github::http::GhHttp;
use crate::github::types::ReviewIssue;
use crate::home;
use crate::store::TaskStore;
use crate::store::store_increment;
use crate::store::store_log_activity;
use crate::store::{CompleteRun, RunTokenUsage, StartRun};
use crate::store::{store_set, store_set_result};
use crate::tmux::TmuxManager;
use anyhow::Context;
pub(crate) use approval::{
    carry_approval_across_base_update, carry_approval_after_rebase, has_recorded_approval,
};
use approval::{
    clear_approved_sha, pr_head_already_approved, record_approved_sha, record_carried_approval,
};
use parse::{ParsedReview, parse_review_output};
use pr::{EnsurePrResult, ensure_pr_exists, recover_missing_review_worktree};
use publish::{post_review_comment, post_review_decision, post_review_started, review_app};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs as tokio_fs;
use tokio::process::Command;
use tokio::sync::RwLock;
use tokio::task;
use tokio::time;

pub(crate) const MAX_REVIEW_AGENT_FAILURES: u64 = 3;

/// Maximum consecutive PR-creation failures before blocking the task.
const MAX_PR_CREATE_FAILURES: u64 = 3;

/// Map an [`AgentError`] to the structured outcome string stored in `task_runs.outcome`.
///
/// Mirrors the logic in `runner::classify_run_outcome` so review runs carry the
/// same outcome fidelity as agent runs.
fn outcome_for_agent_error(err: &agents::AgentError) -> &'static str {
    match err {
        agents::AgentError::Timeout { .. } => "timeout",
        agents::AgentError::Interrupted { .. } => "interrupted",
        // Consult credit-exhaustion detection first — generic rate-limit patterns
        // (e.g. "usage limit") also match billing-cycle exhaustion messages, so
        // reviews on a billing-cycle-exhausted model must not be stored as a
        // plain "rate_limit"/"auth_error" outcome (issue #3529).
        agents::AgentError::RateLimit { message } => {
            cooldown::detect_credit_exhaustion_outcome(message).unwrap_or("rate_limit")
        }
        agents::AgentError::Auth { message } => {
            cooldown::detect_credit_exhaustion_outcome(message).unwrap_or("auth_error")
        }
        agents::AgentError::InvalidResponse { .. } => "parse_error",
        _ => "failed",
    }
}

/// Format an [`AgentError`] as the compact string persisted to `task_runs.error`
/// and surfaced via [`ReviewDecision::Failed`].
///
/// Mirrors the per-variant formatting in `runner::fallback::handle_error` so review
/// runs never leak raw provider payloads (e.g. truncated `api_retry` NDJSON
/// fragments) into the audit trail. Without this, `err.to_string()` would expand
/// `AgentError::RateLimit { message }` via its `Display` impl and persist hundreds
/// of bytes of raw JSON into `task_runs.error` (issue #3149, regression of #2881).
fn format_review_error(agent: &str, err: &agents::AgentError) -> String {
    match err {
        agents::AgentError::RateLimit { message } => format!(
            "{agent} rate limit: {}",
            runner::fallback::summarize_rate_limit_error(message)
        ),
        agents::AgentError::Auth { message } => format!("{agent} auth error: {message}"),
        agents::AgentError::Timeout { elapsed } => {
            format!("{agent} timed out after {}s", elapsed.as_secs())
        }
        other => format!("{agent} agent error: {other}"),
    }
}

fn review_started_comment(review_agent: &str, review_model: &str) -> String {
    let footer = attribution_footer("Review started", review_agent, Some(review_model));
    format!("🔍 Automated review started{footer}")
}

fn should_skip_no_code_reroute_increment(last_error: &str) -> bool {
    git_ops::is_transient_github_api_error(last_error)
}

/// Review agent decision result.
#[derive(Debug, Clone)]
pub(crate) enum ReviewDecision {
    /// Review approved, PR can be merged.
    Approve { issues: Vec<ReviewIssue> },
    /// Changes requested, PR needs fixes.
    RequestChanges {
        notes: String,
        issues: Vec<ReviewIssue>,
    },
    /// Review agent failed or crashed (reason stored for logging).
    Failed(String),
    /// Unrecoverable failure — task should be blocked for human intervention.
    Blocked(String),
    /// No PR exists — nothing to review, task marked done directly.
    Skipped,
    /// No PR and no commits — task re-routed for retry (no_code_reroutes counter must not be reset).
    Rerouted,
}

enum ReviewPhase<T> {
    Ready(T),
    EarlyReturn(ReviewDecision),
}

struct ReviewContext {
    store_id: Option<i64>,
    had_prev_push_failure: bool,
    review_attempt: u32,
    worktree_path: PathBuf,
    branch_name: String,
    pr_number: u64,
    /// PR head SHA when the review started, the commit the decision applies to
    head_sha: Option<String>,
    default_branch: String,
    review_agent: String,
    review_model: Option<String>,
    review_task_id: String,
    review_attempt_dir: PathBuf,
    output_file: PathBuf,
    invocation: runner::agent::AgentInvocation,
    same_agent_fallback: bool,
}

impl ReviewContext {
    fn review_model_str(&self) -> &str {
        self.review_model.as_deref().unwrap_or("")
    }
}

struct ReviewRun {
    run_id: Option<i64>,
}

const REVIEW_TIMEOUT_DEFAULT_SECS: u64 = 900;
const REVIEW_TIMEOUT_MIN_SECS: u64 = 300;

// A review reads a diff and runs the tests, so it gets its own limit, shorter than a task's
fn review_timeout_secs(configured: Option<u64>) -> u64 {
    configured
        .unwrap_or(REVIEW_TIMEOUT_DEFAULT_SECS)
        .max(REVIEW_TIMEOUT_MIN_SECS)
}

/// Read `HEAD` of a worktree. `None` when the worktree is gone or not a repository
pub(crate) async fn worktree_head_sha(dir: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

fn build_review_disallowed_tools(worktree_path: &Path) -> Vec<String> {
    let mut tools = vec![
        "Bash(rm *)".to_string(),
        "Bash(rm -*)".to_string(),
        "Bash(git push*)".to_string(),
        // Pushing and opening the PR are orch's steps, never the agent's (#3783)
        "Bash(gh pr create*)".to_string(),
    ];
    tools.extend(git::git_ops::remote_change_tools());
    let orch_yml = worktree_path.join(".orch.yml");
    let orch_yml_str = orch_yml.to_string_lossy();
    tools.extend([
        format!("Write({orch_yml_str})"),
        format!("Edit({orch_yml_str})"),
    ]);
    if let Ok(orch_home) = home::orch_home() {
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
) -> anyhow::Result<(String, Option<String>, bool)> {
    let mut exclude_set: HashSet<String> = HashSet::new();
    if !task_agent.is_empty() {
        exclude_set.insert(task_agent.to_string());
    }
    if let Some(store_id) = store_id
        && let Ok(failed) = store.previous_review_agents(store_id).await
    {
        for agent in failed {
            exclude_set.insert(agent);
        }
    }

    let mut r = router.write().await;
    let mut tried_agents: HashSet<String> = HashSet::new();
    let mut chosen_agent: Option<String> = None;
    let mut chosen_model: Option<String> = None;
    let available_count = r.available_agent_count().max(1);

    loop {
        let tmp_exclude_refs: Vec<&str> = exclude_set.iter().map(String::as_str).collect();
        let Some(agent) = r.next_round_robin_agent(&tmp_exclude_refs, "review") else {
            break;
        };

        if tried_agents.contains(&agent) {
            break;
        }
        tried_agents.insert(agent.clone());

        let model = r.model_for_complexity(&agent, "review", task_id);
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
        let same = !task_agent.is_empty() && agent == task_agent;
        Ok((agent, chosen_model, same))
    } else {
        let final_exclude_refs: Vec<&str> = exclude_set.iter().map(String::as_str).collect();
        let fallback_agent = r
            .next_round_robin_agent(&final_exclude_refs, "review")
            .unwrap_or_else(|| "claude".to_string());
        let fallback_model = r.model_for_complexity(&fallback_agent, "review", task_id);
        if fallback_model.is_none() {
            anyhow::bail!(
                "no review agent with a configured review model is available for task {task_id}"
            );
        }
        let same = !task_agent.is_empty() && fallback_agent == task_agent;
        Ok((fallback_agent, fallback_model, same))
    }
}

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

    // A commit that already has an approval must not be reviewed again. Checked before
    // the review_invocations increment so a stuck merge cannot exhaust the review quota
    if let Some(pr_number) = stored_task
        .as_ref()
        .and_then(|t| t.pr_number)
        .and_then(|n| u64::try_from(n).ok())
        && pr_head_already_approved(store, repo, &task.id.0, pr_number).await
    {
        tracing::info!(
            task_id = task.id.0,
            pr_number,
            "PR head already approved — skipping review, merge retry continues"
        );
        record_carried_approval(store, repo, &task.id.0, pr_number).await;
        return Ok(ReviewPhase::EarlyReturn(ReviewDecision::Approve {
            issues: Vec::new(),
        }));
    }

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
        Ok(v) => u32::try_from(v).unwrap_or(u32::MAX),
        Err(e) => {
            tracing::warn!(task_id = task.id.0, err = %e, "failed to increment review_invocations — using fallback attempt=1");
            1
        }
    };

    let no_code_review = stored_task.as_ref().is_some_and(|t| {
        t.worktree.trim().is_empty() && t.branch.trim().is_empty() && t.pr_number.is_none()
    });
    if no_code_review {
        if tasks::is_internal_id(&task.id.0) {
            // Internal tasks legitimately reach needs_review with nothing to review.
            tracing::info!(
                task_id = task.id.0,
                "no worktree/branch/pr for internal needs_review task — skipping review and marking done"
            );
            if let Err(e) = task_manager
                .update_task_status(&task.id, Status::Done)
                .await
            {
                tracing::error!(
                    task_id = task.id.0,
                    err = %e,
                    "update_task_status(Done) failed for no-code review skip"
                );
            }
            return Ok(ReviewPhase::EarlyReturn(ReviewDecision::Skipped));
        }

        // External task with no code produced: re-route instead of marking done.
        // Shares the no_code_reroutes circuit breaker with the no-PR/no-commits path below.
        let max_reroutes: u32 = config::get("workflow.max_reroute_attempts")
            .ok()
            .and_then(|s| s.parse().ok())
            .or_else(|| {
                config::get("workflow.max_attempts")
                    .ok()
                    .and_then(|s| s.parse().ok())
            })
            .unwrap_or(3);

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
                return Ok(ReviewPhase::EarlyReturn(ReviewDecision::Failed(format!(
                    "transient store error: {e}"
                ))));
            }
        };

        if reroutes >= u64::from(max_reroutes) {
            tracing::error!(
                task_id = task.id.0,
                reroutes,
                max_reroutes,
                "reached max reroute attempts for no-worktree/branch/pr result — blocking for human review"
            );
            let msg =
                format!("no worktree/branch/pr after {reroutes}/{max_reroutes} reroute attempts");
            let fields = [
                (
                    "block_reason",
                    serde_json::json!(format!(
                        "max reroute attempts ({}) reached — no worktree/branch/pr produced",
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
            return Ok(ReviewPhase::EarlyReturn(ReviewDecision::Blocked(msg)));
        }

        tracing::warn!(
            task_id = task.id.0,
            reroutes,
            max_reroutes,
            "no worktree/branch/pr for external needs_review task — re-routing to a different agent"
        );
        match store_set_result(
            &Some(Arc::clone(store)),
            repo,
            &task.id.0,
            &[
                ("agent", serde_json::json!(null)),
                ("model", serde_json::json!(null)),
                (
                    "last_error",
                    serde_json::json!("no worktree/branch/pr produced"),
                ),
            ],
        )
        .await
        {
            Err(e) => {
                tracing::error!(task_id = task.id.0, err = %e, "failed to clear agent/model for reroute — skipping status transition");
            }
            _ => {
                if let Err(e) = task_manager.update_task_status(&task.id, Status::New).await {
                    tracing::error!(task_id = task.id.0, err = %e, "update_task_status(New) failed — task may be stuck in InReview");
                }
            }
        }
        return Ok(ReviewPhase::EarlyReturn(ReviewDecision::Rerouted));
    }

    let mut worktree_path = match stored_task.as_ref().map(|t| t.worktree.as_str()) {
        Some(w) if !w.is_empty() => PathBuf::from(w),
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
        .and_then(|n| u64::try_from(n).ok())
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

    // Run `git fetch` with a timeout so a hung network call cannot stall the
    // review pipeline indefinitely. Treat a timeout or non-zero exit as a
    // non-fatal warning (stale remote refs are acceptable for the review).
    {
        let fetch_timeout = Duration::from_secs(60);
        let fetch_future = async {
            let mut cmd = git::git_ops::git().await;
            cmd.args(["fetch", "origin", "--prune"])
                .current_dir(&worktree_path);
            // Ensure the child is killed if the timeout elapses
            cmd.kill_on_drop(true);
            cmd.output_with_context().await
        };

        match time::timeout(fetch_timeout, fetch_future).await {
            Ok(Ok(o)) if o.status.success() => {
                // ok
            }
            Ok(Ok(o)) => {
                tracing::warn!(
                    task_id = task.id.0,
                    exit_code = ?o.status.code(),
                    stdout = %git_ops::sanitize_push_error(&String::from_utf8_lossy(&o.stdout)),
                    stderr = %git_ops::sanitize_push_error(&String::from_utf8_lossy(&o.stderr)),
                    "review: git fetch failed (non-zero exit) — diff/log may use stale remote refs"
                );
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    task_id = task.id.0,
                    error = %e,
                    "review: git fetch failed — diff/log may use stale remote refs"
                );
            }
            Err(_) => {
                tracing::warn!(
                    task_id = task.id.0,
                    "review: git fetch timed out after 60s — diff/log may use stale remote refs"
                );
            }
        }
    }

    let default_branch = git::worktree::detect_default_branch(&worktree_path).await;
    let git_diff = runner::context::build_git_diff(&worktree_path, &default_branch).await;
    let git_log = runner::context::build_git_log(&worktree_path, &default_branch).await;
    // An unknown project falls back to the global config, never to the worktree's .orch.yml,
    // which the task agent may have written
    let project_dir = review_project_dir(repo);
    let sandboxed = sandboxed_in(project_dir.as_deref());
    let main_project_dir = project_dir.unwrap_or_else(|| worktree_path.clone());
    let review_prompt = runner::agent::build_review_prompt(
        task,
        &agent_summary,
        &git_diff,
        &git_log,
        &default_branch,
        pr_number,
        runner::agent::Shipcheck::detect(&worktree_path)
            .await
            .in_sandbox(sandboxed, &worktree_path)
            .await,
    );

    let task_agent = stored_task
        .as_ref()
        .and_then(|t| t.agent.clone())
        .unwrap_or_default();
    let (review_agent, review_model, same_agent_fallback) =
        select_review_agent(&task.id.0, &task_agent, store_id, router, store).await?;

    anyhow::ensure!(
        review_model.is_some(),
        "review model missing for task {} after agent selection — refusing to dispatch",
        task.id.0
    );

    if same_agent_fallback {
        tracing::warn!(
            task_id = task.id.0,
            agent = %review_agent,
            "no other agent available for review — falling back to the agent that wrote the code"
        );
        store_log_activity(
            &Some(Arc::clone(store)),
            repo,
            &task.id.0,
            "review_agent_fallback",
            None,
            None,
            Some(&review_agent),
            review_model.as_deref(),
            Some(&serde_json::json!({
                "reason": "all available agents excluded (author + previous reviewers)",
            })),
        )
        .await;
    }

    tracing::info!(
        task_id = task.id.0,
        agent = %review_agent,
        model = ?review_model,
        "spawning review agent"
    );

    let review_task_id = format!("{}-review", task.id.0);
    let review_attempt_dir =
        home::task_attempt_dir_async(repo, &review_task_id, review_attempt).await?;
    let output_file = review_attempt_dir.join("output.json");

    let git_name = config::get("git.name").unwrap_or_else(|_| format!("{review_agent}[bot]"));
    let git_email = config::get("git.email")
        .unwrap_or_else(|_| format!("{review_agent}[bot]@users.noreply.github.com"));
    let review_timeout_secs = review_timeout_seconds();

    let invocation = runner::agent::AgentInvocation {
        agent: review_agent.clone(),
        model: review_model.clone(),
        reasoning_effort: None,
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
        sandbox: sandboxed,
        main_project_dir,
        branch: branch_name.clone(),
    };

    let head_sha = match GhHttp::new() {
        Ok(gh) => gh.get_pr(repo, pr_number).await.ok().map(|pr| pr.head.sha),
        Err(_) => None,
    };

    Ok(ReviewPhase::Ready(ReviewContext {
        store_id,
        had_prev_push_failure,
        review_attempt,
        worktree_path,
        branch_name,
        pr_number,
        head_sha,
        default_branch,
        review_agent,
        review_model,
        review_task_id,
        review_attempt_dir,
        output_file,
        invocation,
        same_agent_fallback,
    }))
}

#[expect(
    clippy::too_many_arguments,
    reason = "the run record needs the outcome, the agent and the timing together"
)]
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
    if let Some(run_id) = run_id
        && let Err(e) = store
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
                attempt: i32::try_from(ctx.review_attempt).unwrap_or(i32::MAX),
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
            complete_review_run(
                store,
                run_id,
                None,
                "",
                &e.to_string(),
                "",
                "failed",
                &format!("spawn failed: {e}"),
                RunTokenUsage::default(),
            )
            .await;
            return ReviewPhase::EarlyReturn(ReviewDecision::Failed(format!("spawn failed: {e}")));
        }
    };

    let start_comment = review_started_comment(&ctx.review_agent, ctx.review_model_str());
    match GhHttp::new() {
        Ok(gh) => {
            let app = review_app(repo).await;
            post_review_started(app.as_deref(), &gh, repo, ctx.head_sha.as_deref()).await;
            // The comment is comment mode's only start signal, the App's check run shows it already
            if app.is_none()
                && let Err(e) = gh
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

    let poll_interval = Duration::from_secs(5);
    let timeout_duration = Duration::from_secs(ctx.invocation.timeout_seconds);
    let wait_result = time::timeout(timeout_duration, async {
        tokio::select! {
            result = tmux.wait_for_completion(&session, poll_interval) => result,
            // An agent can write its decision and still hold the session open
            () = wait_for_review_decision(ctx, poll_interval) => {
                tracing::info!(task_id = task.id.0, "review decision is in while the session is still open, ending it");
                Ok(String::new())
            }
        }
    })
    .await;

    match wait_result {
        Ok(Ok(_)) => {
            tracing::info!(task_id = task.id.0, "review agent completed");
            let _ = tmux.kill_session(&session).await;
            ReviewPhase::Ready(ReviewRun { run_id })
        }
        Ok(Err(e)) => {
            tracing::warn!(task_id = task.id.0, error = %e, "review agent process exited with error");
            let _ = tmux.kill_session(&session).await;

            // Before recording failure, check if this is a "successful exit with non-zero
            // code" agent like kimi/minimax. Read the output file and check for a valid
            // result with is_error=false — if found, treat as success and proceed to
            // parse_review_output where the real result will be extracted.
            let exit_code: i32 = task::spawn_blocking({
                let review_attempt_dir = ctx.review_attempt_dir.clone();
                move || {
                    fs::read_to_string(review_attempt_dir.join("exit.txt"))
                        .ok()
                        .and_then(|s| s.trim().parse().ok())
                        .unwrap_or(-1)
                }
            })
            .await
            .unwrap_or(-1);

            if exit_code != 0 {
                // Try reading output via read_output_file which checks both the primary path
                // and all prior attempt dirs as a fallback. This handles agents like kimi/minimax
                // that exit non-zero even on success, as well as cases where the current attempt's
                // output.json was not written but a prior attempt's output contains a valid result.
                let raw_output =
                    runner::response::read_output_file(&ctx.review_task_id, &ctx.output_file, repo)
                        .await;
                let agent_result = agents::find_agent_result(&ctx.review_agent, &raw_output);
                // Treat as success if we found a valid result without is_error set
                if agent_result.as_ref().is_some_and(|r| !r.is_error) {
                    tracing::info!(
                        task_id = task.id.0,
                        agent = %ctx.review_agent,
                        exit_code,
                        output_found = !raw_output.is_empty(),
                        "review agent succeeded despite non-zero exit — proceeding to parse output"
                    );
                    return ReviewPhase::Ready(ReviewRun { run_id });
                }
            }

            tracing::error!(task_id = task.id.0, error = %e, "review agent failed");
            complete_review_run(
                store,
                run_id,
                Some(exit_code),
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
            let _ = tmux.kill_session(&session).await;
            if review_output_has_decision(ctx).await {
                tracing::warn!(
                    task_id = task.id.0,
                    "review agent timed out after writing its decision, using it"
                );
                return ReviewPhase::Ready(ReviewRun { run_id });
            }

            tracing::error!(task_id = task.id.0, "review agent timed out");
            store_review_timeout(store, run_id, -1, "", "timeout").await
        }
    }
}

/// Exit code of the `timeout` command that wraps the review agent
const REVIEW_TIMEOUT_EXIT_CODE: i32 = 124;

/// A slow review says nothing about the model, so no cooldown: the task goes back
/// to needs_review and the next review picks an agent as usual
async fn store_review_timeout<T>(
    store: &Arc<TaskStore>,
    run_id: Option<i64>,
    exit_code: i32,
    raw_output: &str,
    stderr: &str,
) -> ReviewPhase<T> {
    complete_review_run(
        store,
        run_id,
        Some(exit_code),
        raw_output,
        stderr,
        "",
        "timeout",
        "timeout",
        RunTokenUsage::default(),
    )
    .await;
    ReviewPhase::EarlyReturn(ReviewDecision::Failed("timeout".to_string()))
}

/// The decision is in when this attempt's final result parses as a review, whatever the session
/// does. Only this attempt's file counts, an older attempt may have reviewed another commit
async fn review_output_has_decision(ctx: &ReviewContext) -> bool {
    let raw = tokio_fs::read_to_string(&ctx.output_file)
        .await
        .unwrap_or_default();
    output_has_review_decision(&ctx.review_agent, &raw)
}

fn output_has_review_decision(agent: &str, raw: &str) -> bool {
    let raw = agents::claude::strip_tty_control_lines(raw);
    agents::find_agent_result(agent, &raw)
        .filter(|result| !result.is_error)
        .is_some_and(|result| runner::response::parse_review_response(&result.result_text).is_ok())
}

async fn wait_for_review_decision(ctx: &ReviewContext, poll_interval: Duration) {
    loop {
        time::sleep(poll_interval).await;
        if review_output_has_decision(ctx).await {
            return;
        }
    }
}

async fn record_review_agent_failure(
    task_id: &str,
    review_agent: &str,
    review_model: Option<&str>,
    err: &agents::AgentError,
) {
    match err {
        agents::AgentError::RateLimit { message } | agents::AgentError::Auth { message } => {
            tracing::warn!(
                task_id,
                agent = %review_agent,
                "review agent hit error — adding to cooldown"
            );
            if let Some(reason) = cooldown::detect_credit_exhaustion(message) {
                if reason == CreditExhaustionReason::BillingCycleExhausted {
                    if let Some(model) = review_model {
                        // Quota is model-scoped: don't penalize the whole agent.
                        cooldown::record_persistent_model_failure(review_agent, model).await;
                    } else {
                        cooldown::record_credit_exhaustion(review_agent, reason).await;
                    }
                } else {
                    cooldown::record_credit_exhaustion(review_agent, reason).await;
                }
            } else {
                runner::response::record_agent_failure_with_message(review_agent, &err.to_string())
                    .await;
            }
        }
        // Killed or truncated review run, not a model fault
        agents::AgentError::Interrupted { .. } => {}
        agents::AgentError::InvalidResponse { .. } => {
            // No usable review output, a short backoff since the model itself still works
            if let Some(model) = review_model {
                cooldown::record_transient_model_failure(review_agent, model).await;
            }
        }
        agents::AgentError::ModelUnavailable { message, .. } => {
            // The agent is not penalized, its other models still route normally
            if let Some(model) = review_model {
                cooldown::record_model_unavailable(review_agent, model, message).await;
            }
        }
        _ => {
            // Generic failure (AgentFailed, NetworkError, etc.)
            // Apply standard model backoff (5min→4h max) to prevent immediate re-selection.
            if let Some(model) = review_model {
                cooldown::record_model_failure(review_agent, model).await;
            }
        }
    }
}

async fn finalize_review_run(
    task: &ExternalTask,
    ctx: &ReviewContext,
    parsed: &ParsedReview,
    store: &Arc<TaskStore>,
) {
    let outcome = match &parsed.decision {
        ReviewDecision::Approve { .. }
        | ReviewDecision::RequestChanges { .. }
        | ReviewDecision::Skipped
        | ReviewDecision::Rerouted => "success",
        ReviewDecision::Failed(_) | ReviewDecision::Blocked(_) => "failed",
    };
    let error = match &parsed.decision {
        ReviewDecision::Failed(error) | ReviewDecision::Blocked(error) => error.as_str(),
        _ => "",
    };

    if outcome == "success" {
        // Mirror the runner's success path so review-only models don't ratchet
        // into multi-day cooldowns from isolated failures never offset by resets.
        cooldown::record_agent_success(&ctx.review_agent, ctx.review_model_str()).await;
    }

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

    if let Some(store_id) = ctx.store_id
        && (parsed.token_usage.input_tokens > 0 || parsed.token_usage.output_tokens > 0)
    {
        let cost_usd = Some(parsed.token_usage.total_cost_usd).filter(|c| *c > 0.0);
        if let Err(e) = store
            .store_tokens(
                store_id,
                parsed.token_usage.input_tokens,
                parsed.token_usage.output_tokens,
                cost_usd,
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

async fn restore_review_config_if_needed(
    task_id: &str,
    worktree_path: &Path,
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
        .is_some_and(|output| !output.stdout.is_empty());

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
        .is_ok_and(|status| status.success());

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
        .is_some_and(|output| !output.stdout.is_empty());

    if staged {
        let _ = Command::new("git")
            .args([
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "revert: restore .orch.yml (review agent must not modify project config)",
            ])
            .current_dir(worktree_path)
            .status()
            .await;
    }
}

fn is_missing_path_error_message(err: &str) -> bool {
    err.to_ascii_lowercase()
        .contains("no such file or directory")
}

async fn push_review_branch(
    task: &ExternalTask,
    repo: &str,
    ctx: &ReviewContext,
    store: &Arc<TaskStore>,
) -> Result<(), String> {
    let mut worktree_path = ctx.worktree_path.clone();
    let mut branch_name = ctx.branch_name.clone();
    let mut retried_missing_worktree = false;

    let push_result = loop {
        match git::git_ops::push_branch(&worktree_path, &branch_name, &ctx.default_branch).await {
            Ok(ok) => break Ok(ok),
            Err(e)
                if !retried_missing_worktree
                    && !worktree_path.exists()
                    && is_missing_path_error_message(&e.to_string()) =>
            {
                retried_missing_worktree = true;
                tracing::warn!(
                    task_id = task.id.0,
                    worktree = %worktree_path.display(),
                    branch = %branch_name,
                    "review push failed because worktree disappeared; attempting one-time recovery"
                );
                match recover_missing_review_worktree(task, repo, store).await {
                    Ok(recovered) => {
                        worktree_path = recovered.work_dir;
                        branch_name = recovered.branch;
                    }
                    Err(recover_err) => {
                        break Err(anyhow::anyhow!(
                            "missing review worktree recovery failed after push error: {recover_err}"
                        ));
                    }
                }
            }
            Err(e) => break Err(e),
        }
    };

    match push_result {
        Ok(true) => {
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
                    "branch": branch_name.clone(),
                    "default_branch": ctx.default_branch.clone(),
                    "phase": "review",
                })),
            )
            .await;
            if ctx.had_prev_push_failure
                && let Err(e) = store_set_result(
                    &Some(Arc::clone(store)),
                    repo,
                    &task.id.0,
                    &[
                        ("last_error", serde_json::json!("")),
                        ("push_failures", serde_json::json!(0)),
                    ],
                )
                .await
            {
                tracing::warn!(task_id = task.id.0, err = %e, "failed to reset push_failures — counter may drift");
            }
            Ok(())
        }
        Ok(false) => {
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
                    "status": "skipped",
                    "branch": branch_name.clone(),
                    "default_branch": ctx.default_branch.clone(),
                    "phase": "review",
                })),
            )
            .await;
            Err("push skipped".to_string())
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
                    "branch": branch_name.clone(),
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

#[expect(
    clippy::too_many_arguments,
    reason = "applying a decision touches the backend, the store, tmux and the router"
)]
async fn apply_review_decision(
    task: &ExternalTask,
    backend: &Arc<dyn ExternalBackend>,
    repo: &str,
    task_manager: &Arc<TaskManager>,
    store: &Arc<TaskStore>,
    ctx: &ReviewContext,
    parsed: &ParsedReview,
    router_config: &RouterConfig,
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
        ReviewDecision::Approve { .. } => {
            record_approved_sha(
                store,
                repo,
                &task.id.0,
                ctx.pr_number,
                &ctx.worktree_path,
                &ctx.branch_name,
                &ctx.default_branch,
            )
            .await;
            // auto_merge_pr refuses unless the stored status is already Approved
            if let Err(e) = task_manager
                .update_task_status_if(&task.id, Status::Approved, Status::InReview)
                .await
            {
                tracing::warn!(
                    task_id = task.id.0,
                    err = %e,
                    "failed to set Approved before auto-merge"
                );
            }
            // Always attempt auto-merge on approval. Tasks are only marked
            // Done when the PR is actually merged (detected by review_poll).
            // Never mark Done on approval alone — that closes the issue
            // before the code lands.
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
            Ok(parsed.decision.clone())
        }
        ReviewDecision::RequestChanges { notes, issues } => {
            clear_approved_sha(store, repo, &task.id.0, ctx.pr_number).await;
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
                router_config,
            )
            .await?;
            Ok(parsed.decision.clone())
        }
        _ => Ok(parsed.decision.clone()),
    }
}

fn review_project_dir(repo: &str) -> Option<PathBuf> {
    config::get_projects_with_paths()
        .unwrap_or_default()
        .into_iter()
        .find(|(candidate, _)| candidate == repo)
        .map(|(_, dir)| dir)
}

fn sandboxed_in(project_dir: Option<&Path>) -> bool {
    match project_dir {
        Some(dir) => runner::sandbox::enabled(dir),
        None => runner::sandbox::enabled_globally(),
    }
}

/// Whether the review agent for `repo` runs in the OS sandbox
pub(crate) fn review_sandboxed(repo: &str) -> bool {
    sandboxed_in(review_project_dir(repo).as_deref())
}

/// The review agent's own time limit, as configured.
pub(crate) fn review_timeout_seconds() -> u64 {
    review_timeout_secs(
        config::get("workflow.review_timeout_seconds")
            .ok()
            .and_then(|s| s.parse().ok()),
    )
}

/// Seconds a finished review needs to get its result from the agent's output file into the engine.
/// The agent ends its tmux session first, so the no-session recovery sees a dead session while the
/// result is still in flight
pub(crate) const REVIEW_RESULT_DELIVERY_SECS: u64 = 120;

/// Smallest no-session recovery limit that cannot cut a review short. Below the review agent's own
/// timeout plus delivery time, recovery resets a task whose finished review is still arriving and
/// the result is dropped as stale
pub(crate) fn no_session_review_floor(configured: u64, review_timeout: u64) -> u64 {
    configured.max(review_timeout + REVIEW_RESULT_DELIVERY_SECS)
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

    let router_config = router.read().await.config.clone();
    apply_review_decision(
        task,
        backend,
        repo,
        task_manager,
        store,
        &ctx,
        &parsed,
        &router_config,
    )
    .await
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod approval_tests;
