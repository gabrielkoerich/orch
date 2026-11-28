//! Success-path response handler — commit, push, PR, token storage.
//!
//! Extracted from `runner/mod.rs`. Handles the `Ok(parsed)` arm of the parse
//! result, including git operations and delegation storage. Also owns
//! `write_result_json`.

use crate::config;
use crate::parser::AgentResponse;
use crate::store;
use crate::store::TaskStore;
use std::io;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::Arc;

use tokio::fs;

use super::{git_ops, response, screenshots, worktree};
use crate::cooldown;
use crate::engine::agents;
use crate::engine::push_retry;
use crate::engine::review;
use crate::github::http::GhHttp;
use crate::status_heuristics;
use crate::store::Task;

/// Write a structured `result.json` to the attempt directory for debugging.
#[expect(
    clippy::too_many_arguments,
    reason = "each field of result.json is a separate argument"
)]
pub async fn write_result_json(
    attempt_dir: &Path,
    task_id: &str,
    agent_name: &str,
    model_name: Option<&str>,
    exit_code: i32,
    new_attempts: u32,
    parse_result: &Result<agents::ParsedResponse, agents::AgentError>,
    raw_stdout: &str,
    raw_stderr: &str,
) {
    let result_json = match parse_result {
        Ok(parsed) => {
            serde_json::json!({
                "outcome": "success",
                "agent": agent_name,
                "model": model_name.unwrap_or("default"),
                "exit_code": exit_code,
                "attempt": new_attempts,
                "status": parsed.response.status,
                "summary": parsed.response.summary,
                "input_tokens": parsed.input_tokens,
                "output_tokens": parsed.output_tokens,
                "duration_ms": parsed.duration_ms,
                "files": parsed.response.files,
                "accomplished": parsed.response.accomplished,
                "remaining": parsed.response.remaining,
                "error": parsed.response.error,
                "learnings": parsed.response.learnings,
                "delegations": parsed.response.delegations.iter()
                    .map(|d| serde_json::json!({"title": d.title, "body": d.body}))
                    .collect::<Vec<_>>(),
            })
        }
        Err(agent_err) => {
            serde_json::json!({
                "outcome": "error",
                "agent": agent_name,
                "model": model_name.unwrap_or("default"),
                "exit_code": exit_code,
                "attempt": new_attempts,
                "error_class": agents::error_class_name(agent_err),
                "error_message": agent_err.to_string(),
                "stderr_tail": agents::patterns::safe_tail(raw_stderr, 2000),
                "stdout_tail": agents::patterns::safe_tail(raw_stdout, 2000),
            })
        }
    };

    if let Err(e) = fs::write(
        attempt_dir.join("result.json"),
        serde_json::to_string_pretty(&result_json).unwrap_or_default(),
    )
    .await
    {
        tracing::debug!(task_id, ?e, "failed to write result.json");
    }
}

/// Input to the pure status-decision function [`classify_final_status`].
///
/// The caller is responsible for pre-computing counter values (via store
/// increments) and populating them here.  No I/O is performed inside the
/// decision function itself, making it trivially unit-testable.
#[derive(Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is an independent input to the status decision"
)]
struct DecisionInput<'a> {
    /// Agent-reported status (e.g. `"done"`, `"in_progress"`).
    agent_status: &'a str,
    /// Push was attempted and commits existed, but the push error contained a
    /// `workflow` scope complaint — non-retryable.
    is_workflow_scope_failure: bool,
    /// Push failure recovery attempted a rebase but failed (for example due to
    /// conflicts). This is non-retryable without human intervention.
    is_rebase_conflict_failure: bool,
    /// Push was attempted (commits existed) but failed.
    push_failed: bool,
    /// Persistent push-failure counter *after* this run's increment.
    /// Only meaningful when `push_failed` is `true`.
    push_failures: u64,
    /// The push failed on a GitHub or network error. A new agent run cannot change that, so the
    /// engine retries the push with a backoff instead (#4025).
    is_transient_push_failure: bool,
    /// A PR already exists (or was just created) for this task.
    has_pr: bool,
    /// The agent created sub-task delegations.
    has_delegations: bool,
    /// Commits were pushed to the remote branch successfully.
    has_pushed: bool,
    /// PR creation failed because the base branch was invalid (even after
    /// GitHub API fallback). This is a terminal error — the task should be
    /// blocked rather than re-dispatched.
    is_pr_base_invalid: bool,
    /// The task is external and requires a PR to be marked done.
    /// Always false for internal tasks.
    requires_pr: bool,
    /// Persistent no-code-reroute counter *after* this run's increment.
    no_code_reroutes: u64,
    /// Maximum no-code reroutes before blocking (from config).
    max_reroutes: u32,
    /// True if the agent that just ran is the same as the one that produced
    /// the previous no-code result. Same-agent loops should be blocked immediately.
    is_same_agent: bool,
    /// True when agent_status is "completed". Used to distinguish internal
    /// completed tasks (mark done) from "done" tasks where the git ops pipeline
    /// would have detected no-code reroutes. Pre-computed from agent_status
    /// by the caller so this function stays pure.
    is_completed_status: bool,
    /// Agent explicitly returned blocked, but reason appears transient and retryable.
    is_retryable_blocked: bool,
    /// The task was re-dispatched to resolve a merge conflict and came back with
    /// nothing ahead of the default branch, meaning the fix already landed and
    /// the open PR is superseded. Mark done instead of counting a no-code run.
    is_superseded_conflict: bool,
}

/// Determine the final task status from pre-computed state.
///
/// This is a pure function — it performs no I/O.  All store increments must be
/// done by the caller **before** calling this function (so counters reflect the
/// current run), and all store side-effects (clearing agent/model, writing
/// last_error) must be applied **after** based on the returned status.
fn classify_final_status(input: &DecisionInput<'_>) -> String {
    if input.is_workflow_scope_failure || input.is_rebase_conflict_failure {
        "blocked".to_string()
    } else if input.push_failed {
        if input.is_transient_push_failure && input.push_failures < push_retry::MAX_PUSH_FAILURES {
            "needs_review".to_string()
        } else if input.push_failures >= 3 {
            "blocked".to_string()
        } else {
            "new".to_string()
        }
    } else if input.agent_status == "done" && input.has_pr {
        "needs_review".to_string()
    } else if input.agent_status == "done" && !input.has_pr && input.has_delegations {
        "blocked".to_string()
    } else if input.agent_status == "done"
        && !input.has_pr
        && input.has_pushed
        && input.is_pr_base_invalid
    {
        // PR creation failed with an invalid base branch (even after GitHub API
        // fallback).  This is a terminal configuration error — re-dispatching
        // would just burn tokens on the same failure.
        "blocked".to_string()
    } else if input.agent_status == "done" && !input.has_pr && input.has_pushed {
        "routed".to_string()
    } else if input.is_superseded_conflict {
        "done".to_string()
    } else if input.agent_status == "done" && input.requires_pr {
        // Same-agent loop detection: if this agent is the same as the one that
        // produced the previous no-code result, block immediately (#2410, #2686).
        // Otherwise, block if max reroutes exhausted, else reroute.
        if input.is_same_agent || input.no_code_reroutes >= u64::from(input.max_reroutes) {
            "blocked".to_string()
        } else {
            "new".to_string()
        }
    } else if input.agent_status == "done" && !input.has_pushed {
        // Only internal tasks reach here — external done+!has_pr+!has_pushed is
        // always caught by branch 6 (requires_pr = true for external tasks with
        // !has_pr and done status), so is_external can never be true here.
        // Internal tasks may legitimately produce no git-visible changes.
        "done".to_string()
    } else if input.is_completed_status && input.has_pr {
        // Agent said "completed" and a PR was created — send to review.
        "needs_review".to_string()
    } else if input.is_completed_status && !input.has_pr && input.has_delegations {
        // Agent said "completed" with delegations but no PR — blocked on children.
        "blocked".to_string()
    } else if input.is_completed_status
        && !input.has_pr
        && input.has_pushed
        && input.is_pr_base_invalid
    {
        "blocked".to_string()
    } else if input.is_completed_status && !input.has_pr && input.has_pushed {
        // Agent said "completed", commits pushed but no PR — re-dispatch for PR.
        "routed".to_string()
    } else if input.is_completed_status && input.requires_pr {
        // Agent said "completed" on external task requiring PR without a PR or pushes.
        // Same-agent loop detection applies, then reroute or block.
        if input.is_same_agent || input.no_code_reroutes >= u64::from(input.max_reroutes) {
            "blocked".to_string()
        } else {
            "new".to_string()
        }
    } else if input.is_completed_status {
        // Agent said "completed" on internal task — mark done.
        // Git ops already ran and detected no commits, so this is a legitimate
        // completion without code changes.
        "done".to_string()
    } else if input.agent_status == "blocked" && input.is_retryable_blocked {
        "new".to_string()
    } else if status_heuristics::status_looks_like_descriptive_completion(input.agent_status) {
        "done".to_string()
    } else {
        input.agent_status.to_string()
    }
}

// ── Internal context ──────────────────────────────────────────────────────────

/// Bundles store references so helper functions don't repeat four parameters.
struct StoreCtx<'a> {
    store: &'a Option<Arc<TaskStore>>,
    store_id_opt: Option<i64>,
    repo: &'a str,
    task_id: &'a str,
}

impl StoreCtx<'_> {
    /// Write fields, using the pre-resolved `store_id` when available.
    async fn set(&self, fields: &[(&str, serde_json::Value)]) {
        if let Some(store_id) = self.store_id_opt {
            let _ = store::store_set_by_id(self.store.as_ref(), store_id, fields).await;
        } else {
            store::store_set(self.store, self.repo, self.task_id, fields).await;
        }
    }

    /// A successful run ends the streak of network errors and timeouts
    async fn reset_retry_streaks(&self) {
        self.set(&[
            ("network_retries", serde_json::json!(0)),
            ("timeout_retries", serde_json::json!(0)),
        ])
        .await;
    }

    /// Load the full task record.
    async fn get_task(&self) -> Option<Task> {
        if let Some(store_id) = self.store_id_opt {
            store::opt_store_get_task_by_id(self.store, store_id).await
        } else {
            store::opt_store_get_task(self.store, self.repo, self.task_id).await
        }
    }

    /// Atomically increment a counter field.
    async fn increment(&self, field: &str) -> anyhow::Result<u64> {
        if let Some(store_id) = self.store_id_opt {
            store::store_increment_by_id(self.store, store_id, field).await
        } else {
            store::store_increment(self.store, self.repo, self.task_id, field).await
        }
    }

    /// Append an activity event to the task timeline.
    async fn append_activity(
        &self,
        event_type: &str,
        agent: Option<&str>,
        model: Option<&str>,
        details: Option<&serde_json::Value>,
    ) {
        if let Some(store_id) = self.store_id_opt {
            if let Some(s) = self.store
                && let Err(e) = s
                    .append_activity(store_id, event_type, None, None, agent, model, details)
                    .await
            {
                tracing::warn!(
                    task_id = self.task_id,
                    event_type,
                    error = %e,
                    "store append_activity failed"
                );
            }
        } else {
            store::store_log_activity(
                self.store,
                self.repo,
                self.task_id,
                event_type,
                None,
                None,
                agent,
                model,
                details,
            )
            .await;
        }
    }
}

// ── Git operation result types ────────────────────────────────────────────────

/// Outcome of the auto-commit → push → PR pipeline.
#[derive(Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is an independent outcome of the git pipeline"
)]
struct GitOpsResult {
    has_pr: bool,
    has_pushed: bool,
    has_commits: bool,
    /// PR creation failed with an invalid base branch (after all retries
    /// and the GitHub API fallback).  Used to block the task instead of
    /// re-dispatching it indefinitely.
    pr_base_invalid: bool,
}

/// Push-failure detection result.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is an independent kind of push failure"
)]
struct PushFailureState {
    push_failed: bool,
    is_workflow_scope_failure: bool,
    is_rebase_conflict_failure: bool,
    /// GitHub or network error that the engine retries with a backoff, without an agent run.
    is_transient_failure: bool,
    /// Post-increment failure counter (0 when no push was attempted or failed).
    push_failures: u64,
    /// Last error stored in the task record (used for workflow-scope detection).
    stored_last_error: String,
}

// ── Private git-ops helpers ───────────────────────────────────────────────────

/// Attempt auto-commit; record any error in the store.
async fn auto_commit_changes(
    ctx: &StoreCtx<'_>,
    work_dir: &Path,
    task_title: &str,
    agent_name: &str,
    new_attempts: u32,
) {
    if let Err(e) = git_ops::auto_commit(
        work_dir,
        ctx.task_id,
        task_title,
        agent_name,
        new_attempts,
        super::commit::When::Always,
    )
    .await
    {
        tracing::error!(task_id = ctx.task_id, error = ?e, "auto commit failed");
        ctx.set(&[(
            "last_error",
            serde_json::json!(format!("auto commit failed: {e}")),
        )])
        .await;
    }
}

/// Check whether commits exist ahead of the default branch.
///
/// When there are no commits, clears any stale push-failure error so that the
/// review gate does not block an otherwise-approved task.
async fn check_commits_and_clear_stale_errors(
    ctx: &StoreCtx<'_>,
    work_dir: &Path,
    default_branch: &str,
) -> bool {
    let has_commits = git_ops::has_commits_ahead(work_dir, default_branch).await;
    if !has_commits {
        tracing::info!(
            task_id = ctx.task_id,
            "no commits ahead of default branch, skipping push + PR"
        );
        // Clear stale push failure from previous runs.
        let last_err = ctx
            .get_task()
            .await
            .map(|t| t.last_error)
            .unwrap_or_default();
        if last_err.contains("push failed") {
            ctx.set(&[("last_error", serde_json::json!(""))]).await;
        }
    }
    has_commits
}

/// Push the branch to the remote and log the activity.
///
/// On push failure due to diverged history (non-fast-forward), fetches the remote
/// branch and rebases local commits on top, then pushes again. If rebase fails
/// (conflicts), the task is blocked for human intervention.
///
/// Returns `PushResult::Success` on success, `PushResult::Failed` if push failed
/// and recovery was not possible, `PushResult::Skipped` if the push was skipped,
/// or `PushResult::NoCommits` if the branch has no commits ahead of the default branch
enum PushResult {
    Success,
    Failed,
    Skipped,         // push_branch returned Ok(false)
    NoCommits,       // agent made no code changes
    WorktreeMissing, // retry after rebuilding missing worktree
}

fn is_missing_path_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|ioe| ioe.kind() == ErrorKind::NotFound)
    }) || err
        .to_string()
        .to_ascii_lowercase()
        .contains("no such file or directory")
}

async fn recover_missing_worktree_for_push(
    ctx: &StoreCtx<'_>,
    wt: &worktree::WorktreeSetup,
    task_title: &str,
    agent_name: &str,
    model_name: Option<&str>,
) -> anyhow::Result<worktree::WorktreeSetup> {
    tracing::warn!(
        task_id = ctx.task_id,
        worktree = %wt.work_dir.display(),
        branch = wt.branch,
        "worktree missing during push; attempting one-time recovery"
    );
    ctx.append_activity(
        "push_recover_worktree",
        Some(agent_name),
        model_name,
        Some(&serde_json::json!({
            "status": "start",
            "worktree": wt.work_dir.display().to_string(),
            "branch": wt.branch,
        })),
    )
    .await;

    let recovered = worktree::setup_worktree(
        ctx.task_id,
        task_title,
        &wt.main_project_dir,
        ctx.store,
        ctx.repo,
    )
    .await?;

    if !recovered.work_dir.exists() {
        anyhow::bail!(
            "recovered worktree path is still missing: {}",
            recovered.work_dir.display()
        );
    }

    ctx.append_activity(
        "push_recover_worktree",
        Some(agent_name),
        model_name,
        Some(&serde_json::json!({
            "status": "ok",
            "worktree": recovered.work_dir.display().to_string(),
            "branch": recovered.branch,
        })),
    )
    .await;

    Ok(recovered)
}

/// Push a branch that was just rebased onto its moved remote counterpart.
///
/// Separate from the first push attempt so a failure here is reported as orch's own
/// push failure rather than re-entering the diverged-branch recovery
async fn push_after_rebase(
    ctx: &StoreCtx<'_>,
    work_dir: &Path,
    branch: &str,
    default_branch: &str,
    agent_name: &str,
    model_name: Option<&str>,
) -> PushResult {
    match git_ops::push_branch(work_dir, branch, default_branch).await {
        Ok(true) => {
            ctx.append_activity(
                "push",
                Some(agent_name),
                model_name,
                Some(&serde_json::json!({
                    "status": "ok",
                    "branch": branch,
                    "default_branch": default_branch,
                    "reason": "pushed_after_rebase",
                })),
            )
            .await;
            ctx.set(&[
                ("last_error", serde_json::json!("")),
                ("push_failures", serde_json::json!(0)),
            ])
            .await;
            PushResult::Success
        }
        Ok(false) => {
            tracing::info!(
                task_id = ctx.task_id,
                branch,
                default_branch,
                "push skipped after rebase"
            );
            ctx.append_activity(
                "push",
                Some(agent_name),
                model_name,
                Some(&serde_json::json!({
                    "status": "skipped",
                    "branch": branch,
                    "default_branch": default_branch,
                    "reason": "pushed_after_rebase",
                })),
            )
            .await;
            PushResult::Skipped
        }
        Err(e) => {
            tracing::error!(
                task_id = ctx.task_id,
                branch = branch,
                error = ?e,
                "push failed again after rebasing on the remote branch"
            );
            ctx.append_activity(
                "push",
                Some(agent_name),
                model_name,
                Some(&serde_json::json!({
                    "status": "error",
                    "branch": branch,
                    "default_branch": default_branch,
                    "reason": "pushed_after_rebase",
                    "error": e.to_string(),
                })),
            )
            .await;
            ctx.set(&[(
                "last_error",
                serde_json::json!(format!("push failed after rebase on remote branch: {e}")),
            )])
            .await;
            PushResult::Failed
        }
    }
}

async fn push_branch_with_log(
    ctx: &StoreCtx<'_>,
    work_dir: &Path,
    branch: &str,
    default_branch: &str,
    agent_name: &str,
    model_name: Option<&str>,
) -> PushResult {
    match git_ops::push_branch(work_dir, branch, default_branch).await {
        Ok(true) => {
            ctx.append_activity(
                "push",
                Some(agent_name),
                model_name,
                Some(&serde_json::json!({
                    "status": "ok",
                    "branch": branch,
                    "default_branch": default_branch,
                })),
            )
            .await;
            // Clear any stale push failure from a previous run so review_and_merge
            // does not incorrectly block an approved task.
            ctx.set(&[
                ("last_error", serde_json::json!("")),
                ("push_failures", serde_json::json!(0)),
            ])
            .await;
            PushResult::Success
        }
        Ok(false) => {
            tracing::info!(
                task_id = ctx.task_id,
                branch,
                default_branch,
                "push skipped"
            );
            ctx.append_activity(
                "push",
                Some(agent_name),
                model_name,
                Some(&serde_json::json!({
                    "status": "skipped",
                    "branch": branch,
                    "default_branch": default_branch,
                })),
            )
            .await;
            PushResult::Skipped
        }
        Err(e) => {
            let err_str = e.to_string();
            tracing::error!(task_id = ctx.task_id, error = ?e, "push failed");

            if is_missing_path_error(&e) && !work_dir.exists() {
                tracing::warn!(
                    task_id = ctx.task_id,
                    worktree = %work_dir.display(),
                    "push failed because worktree path disappeared; will attempt recovery"
                );
                ctx.append_activity(
                    "push",
                    Some(agent_name),
                    model_name,
                    Some(&serde_json::json!({
                        "status": "retry",
                        "branch": branch,
                        "default_branch": default_branch,
                        "reason": "worktree_missing",
                        "error": err_str,
                    })),
                )
                .await;
                return PushResult::WorktreeMissing;
            }

            // Check if this is a non-fast-forward error that we can recover from.
            if push_error_is_diverged(&err_str) {
                tracing::info!(
                    task_id = ctx.task_id,
                    branch = branch,
                    "attempting fetch and rebase to recover from diverged branch"
                );

                // Attempt to fetch and rebase on origin/{branch}.
                match git_ops::rebase_on_branch(work_dir, branch).await {
                    Ok(true) => {
                        // Pushing is orch's step, not the agent's: the commits are already
                        // written, so push them onto the moved remote branch here. Re-routing
                        // instead re-ran a finished task until it hit the attempt limit (#3783)
                        tracing::info!(
                            task_id = ctx.task_id,
                            branch = branch,
                            "rebase succeeded — retrying the push"
                        );
                        ctx.append_activity(
                            "push_rebased",
                            Some(agent_name),
                            model_name,
                            Some(&serde_json::json!({
                                "status": "rebased",
                                "branch": branch,
                                "reason": "diverged_from_remote",
                            })),
                        )
                        .await;
                        return push_after_rebase(
                            ctx,
                            work_dir,
                            branch,
                            default_branch,
                            agent_name,
                            model_name,
                        )
                        .await;
                    }
                    Ok(false) => {
                        // The remote branch already holds every local commit, nothing to push
                        tracing::info!(
                            task_id = ctx.task_id,
                            branch = branch,
                            "no local commits after fetch — marking as success"
                        );
                        ctx.append_activity(
                            "push",
                            Some(agent_name),
                            model_name,
                            Some(&serde_json::json!({
                                "status": "ok",
                                "branch": branch,
                                "reason": "no_commits_after_fetch",
                            })),
                        )
                        .await;
                        ctx.set(&[
                            ("last_error", serde_json::json!("")),
                            ("push_failures", serde_json::json!(0)),
                        ])
                        .await;
                        return PushResult::Success;
                    }
                    Err(rebase_err) if git_ops::is_missing_remote_ref_error(&rebase_err) => {
                        // The branch never reached the remote, so there is nothing to rebase onto.
                        // Take the counted retry path instead of the rebase conflict block
                        tracing::warn!(
                            task_id = ctx.task_id,
                            branch = branch,
                            "remote branch does not exist, skipping rebase recovery"
                        );
                        ctx.append_activity(
                            "push",
                            Some(agent_name),
                            model_name,
                            Some(&serde_json::json!({
                                "status": "error",
                                "branch": branch,
                                "default_branch": default_branch,
                                "error": err_str,
                            })),
                        )
                        .await;
                        ctx.set(&[("last_error", serde_json::json!(format!("push failed: {e}")))])
                            .await;
                        return PushResult::Failed;
                    }
                    Err(rebase_err) => {
                        // Rebase failed (likely conflicts) — block for human intervention.
                        tracing::error!(
                            task_id = ctx.task_id,
                            branch = branch,
                            error = ?rebase_err,
                            "rebase failed — blocking task for human intervention"
                        );
                        ctx.append_activity(
                            "push_rebased",
                            Some(agent_name),
                            model_name,
                            Some(&serde_json::json!({
                                "status": "error",
                                "branch": branch,
                                "error": rebase_err.to_string(),
                            })),
                        )
                        .await;
                        ctx.set(&[(
                            "last_error",
                            serde_json::json!(format!(
                                "push failed and rebase failed: {rebase_err} — manual resolution required"
                            )),
                        )])
                        .await;
                        return PushResult::Failed;
                    }
                }
            }

            // Non-recoverable push error — log and return failure.
            ctx.append_activity(
                "push",
                Some(agent_name),
                model_name,
                Some(&serde_json::json!({
                    "status": "error",
                    "branch": branch,
                    "default_branch": default_branch,
                    "error": err_str,
                })),
            )
            .await;
            ctx.set(&[("last_error", serde_json::json!(format!("push failed: {e}")))])
                .await;
            PushResult::Failed
        }
    }
}

/// True when a push error means the remote branch moved. `[remote rejected]` is a server side
/// failure, only `! [rejected]` marks a divergence
fn push_error_is_diverged(err_str: &str) -> bool {
    err_str.contains("non-fast-forward")
        || err_str.contains("! [rejected]")
        || err_str.contains("fetch first")
        || err_str.contains("behind")
}

/// True when the GitHub client refused the call locally because its rate-limit window is active.
fn is_local_rate_limit_skip(err_str: &str) -> bool {
    err_str.contains("rate-limited, skipping call")
}

/// Create (or find an existing) PR and log the activity.
///
/// Returns `(has_pr, has_pushed, pr_base_invalid)`. `has_pushed` may be set to
/// `false` when a 422 "no commits" GitHub error indicates the branch was
/// already merged. `pr_base_invalid` is set when the base branch is rejected
/// by GitHub (even after the API fallback), which is a terminal condition.
async fn create_pr_with_log(
    ctx: &StoreCtx<'_>,
    wt: &worktree::WorktreeSetup,
    task_title: &str,
    resp: &AgentResponse,
    agent_name: &str,
    model_name: Option<&str>,
    mut has_pushed: bool,
) -> (bool, bool, bool) {
    match git_ops::create_pr_if_needed(
        &wt.work_dir,
        &wt.branch,
        task_title,
        &resp.summary,
        &resp.accomplished,
        &resp.remaining,
        &resp.files,
        ctx.task_id,
        agent_name,
        model_name,
        ctx.repo,
        &wt.default_branch,
    )
    .await
    {
        Ok(ref url) => {
            // Save pr_number to the store so the review gate can find it
            // immediately without racing GitHub's list-API cache (~300 ms lag).
            // This is set for both newly-created and pre-existing PRs.
            match review::parse_pr_number_from_url(url) {
                Ok(pr_num) => {
                    if let Err(e) = store::store_set_result(
                        ctx.store,
                        ctx.repo,
                        ctx.task_id,
                        &[("pr_number", serde_json::json!(pr_num))],
                    )
                    .await
                    {
                        tracing::error!(
                            task_id = ctx.task_id,
                            pr_url = %url,
                            err = %e,
                            "CRITICAL: failed to save pr_number to store — downstream review gate may trigger duplicate gh pr create"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = ctx.task_id,
                        pr_url = %url,
                        error = %e,
                        "PR created but failed to parse PR number from URL"
                    );
                }
            }
            ctx.append_activity(
                "pr_create",
                Some(agent_name),
                model_name,
                Some(&serde_json::json!({"status": "created", "url": url})),
            )
            .await;
            (true, has_pushed, false)
        }
        Err(e) => {
            let err_str = format!("{e}");
            tracing::error!(task_id = ctx.task_id, error = ?e, "create PR failed");
            ctx.append_activity(
                "pr_create",
                Some(agent_name),
                model_name,
                Some(&serde_json::json!({
                    "status": "error",
                    "branch": wt.branch,
                    "error": err_str.clone(),
                })),
            )
            .await;
            ctx.set(&[(
                "last_error",
                serde_json::json!(format!("create PR failed: {e}")),
            )])
            .await;
            // Distinguish between terminal 422 errors and recoverable ones.
            // - "No commits between" / "head invalid" → agent made no code changes
            //   or the branch was already merged.  Clearing has_pushed lets the
            //   task fall through to the "done" path.
            // - "base invalid" → the base branch name is wrong.  After the
            //   GitHub API fallback in create_pr_if_needed also fails, this is a
            //   terminal configuration error.  We set pr_base_invalid so the
            //   classifier blocks the task instead of re-dispatching forever.
            // A GitHub auth error is terminal too: the commits are pushed, so only a token fix helps
            let mut pr_base_invalid = false;
            if is_local_rate_limit_skip(&err_str) {
                // No request was sent and the branch is pushed. Re-running the agent cannot help,
                // so report the PR as pending and let the review gate create it once calls resume
                tracing::warn!(
                    task_id = ctx.task_id,
                    "PR creation skipped by local GitHub rate limit — review gate will create the PR"
                );
                return (true, has_pushed, false);
            }
            if agents::patterns::detect_auth_error(&err_str).is_some() {
                tracing::error!(
                    task_id = ctx.task_id,
                    "PR creation failed with GitHub auth error — blocking task"
                );
                pr_base_invalid = true;
            } else if err_str.contains("422") {
                if err_str.contains("No commits between") || err_str.contains("head") {
                    tracing::info!(
                        task_id = ctx.task_id,
                        "PR creation returned 422/no-commits/head-invalid — agent made no code changes or branch merged, marking done"
                    );
                    has_pushed = false;
                } else if git_ops::is_invalid_base_error(&err_str) {
                    tracing::error!(
                        task_id = ctx.task_id,
                        "PR creation failed with invalid base branch — blocking task"
                    );
                    pr_base_invalid = true;
                }
            }
            (false, has_pushed, pr_base_invalid)
        }
    }
}

/// Run the full auto-commit → push → PR pipeline.
async fn run_git_ops(
    ctx: &StoreCtx<'_>,
    wt: &worktree::WorktreeSetup,
    task_title: &str,
    resp: &AgentResponse,
    agent_name: &str,
    model_name: Option<&str>,
    new_attempts: u32,
) -> GitOpsResult {
    auto_commit_changes(ctx, &wt.work_dir, task_title, agent_name, new_attempts).await;

    let mut has_commits =
        check_commits_and_clear_stale_errors(ctx, &wt.work_dir, &wt.default_branch).await;

    // Skip push + PR if there are no commits ahead of the default branch.
    // No-op tasks (e.g. "nothing to execute") produce no commits, so pushing
    // and creating a PR would just waste API calls and trigger 422 errors.
    let push_result = if has_commits {
        push_branch_with_log(
            ctx,
            &wt.work_dir,
            &wt.branch,
            &wt.default_branch,
            agent_name,
            model_name,
        )
        .await
    } else {
        store::store_log_activity(
            ctx.store,
            ctx.repo,
            ctx.task_id,
            "push",
            None,
            None,
            Some(agent_name),
            model_name,
            Some(&serde_json::json!({
                "status": "skipped",
                "reason": "no_commits_ahead",
                "branch": wt.branch,
                "default_branch": wt.default_branch,
            })),
        )
        .await;
        PushResult::NoCommits
    };
    let push_result = if matches!(push_result, PushResult::WorktreeMissing) {
        match recover_missing_worktree_for_push(ctx, wt, task_title, agent_name, model_name).await {
            Ok(recovered) => {
                let wt_for_ops = &recovered;
                has_commits = check_commits_and_clear_stale_errors(
                    ctx,
                    &wt_for_ops.work_dir,
                    &wt_for_ops.default_branch,
                )
                .await;
                if has_commits {
                    push_branch_with_log(
                        ctx,
                        &wt_for_ops.work_dir,
                        &wt_for_ops.branch,
                        &wt_for_ops.default_branch,
                        agent_name,
                        model_name,
                    )
                    .await
                } else {
                    PushResult::NoCommits
                }
            }
            Err(e) => {
                tracing::error!(
                    task_id = ctx.task_id,
                    err = %e,
                    "worktree recovery before push failed"
                );
                ctx.append_activity(
                    "push_recover_worktree",
                    Some(agent_name),
                    model_name,
                    Some(&serde_json::json!({
                        "status": "error",
                        "worktree": wt.work_dir.display().to_string(),
                        "branch": wt.branch,
                        "error": e.to_string(),
                    })),
                )
                .await;
                ctx.set(&[(
                    "last_error",
                    serde_json::json!(format!(
                        "push failed: worktree disappeared and recovery failed: {e}"
                    )),
                )])
                .await;
                PushResult::Failed
            }
        }
    } else {
        push_result
    };

    let mut has_pushed = matches!(push_result, PushResult::Success);
    let mut has_pr = false;

    // Create PR (skip if push failed or repo is unknown)
    if has_commits && matches!(push_result, PushResult::Failed) {
        tracing::warn!(
            task_id = ctx.task_id,
            "skipping PR creation due to push failure"
        );
    } else if matches!(push_result, PushResult::Skipped) {
        tracing::info!(task_id = ctx.task_id, "push skipped, skipping PR creation");
    } else if matches!(push_result, PushResult::NoCommits) {
        tracing::info!(
            task_id = ctx.task_id,
            "no commits ahead of default branch, skipping push + PR"
        );
    } else if !has_pushed {
        // no commits — already logged "no commits ahead, skipping push + PR" at INFO level above
    } else if ctx.repo.is_empty() {
        tracing::warn!(
            task_id = ctx.task_id,
            "skipping PR creation — repo is empty (internal task?)"
        );
    } else {
        let (pr, updated_pushed, pr_base_invalid) = create_pr_with_log(
            ctx, wt, task_title, resp, agent_name, model_name, has_pushed,
        )
        .await;
        has_pr = pr;
        has_pushed = updated_pushed;
        return GitOpsResult {
            has_pr,
            has_pushed,
            has_commits,
            pr_base_invalid,
        };
    }

    GitOpsResult {
        has_pr,
        has_pushed,
        has_commits,
        pr_base_invalid: false,
    }
}

// ── Private status-computation helpers ───────────────────────────────────────

/// Comment the agent's screenshots on the PR. Returns the note for the task summary and channels
async fn post_screenshots(
    ctx: &StoreCtx<'_>,
    work_dir: &Path,
    resp: &AgentResponse,
) -> Option<String> {
    let note = resp.screenshot_note.as_deref().map(str::trim);
    let note = note.filter(|n| !n.is_empty());
    if resp.screenshots.is_empty() && note.is_none() {
        return None;
    }
    let pr_number = ctx.get_task().await.and_then(|t| t.pr_number);
    let (Some(pr_number), false) = (pr_number, ctx.repo.is_empty()) else {
        return note.map(String::from);
    };
    let Ok(pr) = u64::try_from(pr_number) else {
        return note.map(String::from);
    };

    let roots = screenshots::allowed_roots(work_dir).await;
    let (images, skipped) = screenshots::prepare(&resp.screenshots, work_dir, &roots, pr);
    let commit = if images.is_empty() {
        String::new()
    } else {
        match screenshots::publish(work_dir, &images, pr).await {
            Ok(commit) => commit,
            Err(e) => {
                tracing::warn!(task_id = ctx.task_id, err = %e, "failed to push screenshots");
                let body = screenshots::build_unpublished_comment(note, &skipped);
                comment_screenshots(ctx, pr, &body).await;
                return note.map(String::from);
            }
        }
    };
    let body = screenshots::build_comment(ctx.repo, &commit, &images, note, &skipped);
    comment_screenshots(ctx, pr, &body).await;
    note.map(String::from)
}

async fn comment_screenshots(ctx: &StoreCtx<'_>, pr: u64, body: &str) {
    match GhHttp::new() {
        Ok(gh) => {
            if let Err(e) = gh.add_comment(ctx.repo, &pr.to_string(), body).await {
                tracing::warn!(task_id = ctx.task_id, pr, err = %e, "failed to comment screenshots");
            }
        }
        Err(e) => tracing::warn!(task_id = ctx.task_id, err = %e, "cannot comment screenshots"),
    }
}

/// Comment on and close a PR whose change already landed elsewhere.
async fn close_superseded_pr(ctx: &StoreCtx<'_>, pr_number: u64) {
    if ctx.repo.is_empty() {
        return;
    }
    match GhHttp::new() {
        Ok(gh) => {
            let note = "Closing as superseded: the change is already on the default branch, so this branch has no commits ahead of it.";
            if let Err(e) = gh.add_comment(ctx.repo, &pr_number.to_string(), note).await {
                tracing::warn!(task_id = ctx.task_id, pr_number, err = %e, "failed to comment on superseded PR");
            }
            if let Err(e) = gh.close_pr(ctx.repo, pr_number).await {
                tracing::warn!(task_id = ctx.task_id, pr_number, err = %e, "failed to close superseded PR");
            }
        }
        Err(e) => {
            tracing::warn!(task_id = ctx.task_id, err = %e, "cannot close superseded PR");
        }
    }
    // Dropping the PR lets the blocker refresh read the closed, completed issue with no PR
    ctx.set(&[
        (
            "last_error",
            serde_json::json!(
                "PR closed as superseded: the fix already landed on the default branch"
            ),
        ),
        ("pr_number", serde_json::json!(null)),
    ])
    .await;
}

/// The PR to close when a merge-conflict reroute produced no commits.
///
/// `merge_conflict_retries > 0` means review_poll sent the task back to resolve a
/// conflict. Coming back with nothing ahead of the default branch means another
/// PR already landed the same fix, so the open PR is superseded (#3780)
fn superseded_conflict_pr(
    merge_conflict_retries: i32,
    pr_number: Option<i32>,
    has_commits: bool,
) -> Option<u64> {
    if has_commits || merge_conflict_retries <= 0 {
        return None;
    }
    pr_number
        .filter(|n| *n > 0)
        .and_then(|n| u64::try_from(n).ok())
}

/// Read push-failure state from the store and increment `push_failures` if needed.
async fn detect_push_failure_state(
    ctx: &StoreCtx<'_>,
    has_commits: bool,
    has_pushed: bool,
) -> PushFailureState {
    // Load stored task once to inspect last_error (avoid repeated DB reads).
    let stored_last_error = ctx
        .get_task()
        .await
        .map(|t| t.last_error)
        .unwrap_or_default();

    let push_failed = !has_pushed && has_commits && stored_last_error.contains("push failed");

    // Detect workflow scope errors — these are non-retryable token permission issues.
    // Rerouting to a different agent won't help since the same token is used.
    let is_workflow_scope_failure = push_failed
        && (stored_last_error.contains("workflow` scope")
            || stored_last_error.contains("workflow' scope")
            || (stored_last_error.contains("refusing to allow")
                && stored_last_error.contains("workflow")));

    let is_rebase_conflict_failure =
        push_failed && stored_last_error.contains("push failed and rebase failed:");

    let is_transient_failure = push_failed
        && !is_workflow_scope_failure
        && !is_rebase_conflict_failure
        && git_ops::is_transient_network_error(&stored_last_error);

    // Track push failures — block after 3 consecutive failures.
    // Workflow-scope failures are non-retryable and blocked immediately; do not
    // increment this retry counter for them so that later normal push failures
    // are not prematurely blocked.
    let push_failures = if push_failed && !is_workflow_scope_failure && !is_rebase_conflict_failure
    {
        match ctx.increment("push_failures").await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    task_id = ctx.task_id,
                    err = %e,
                    "failed to increment push_failures — reading current value from store"
                );
                // On DB error, read current value to ensure blocking threshold is still reachable
                ctx.get_task()
                    .await
                    .map_or(1, |t| u64::try_from(t.push_failures).unwrap_or(0) + 1)
                // assume at least 1 if we can't read either
            }
        }
    } else {
        0
    };

    PushFailureState {
        push_failed,
        is_workflow_scope_failure,
        is_rebase_conflict_failure,
        is_transient_failure,
        push_failures,
        stored_last_error,
    }
}

/// Increment `no_code_reroutes` and log a warning when the agent completed
/// without producing code changes on an external task requiring a PR.
async fn detect_no_code_reroutes(
    ctx: &StoreCtx<'_>,
    is_no_code_reroute: bool,
    new_attempts: u32,
    max_reroutes: u32,
) -> u64 {
    if !is_no_code_reroute {
        return 0;
    }
    tracing::warn!(
        task_id = ctx.task_id,
        attempts = new_attempts,
        max_reroutes,
        "agent reported done but produced no code changes on external task requiring PR"
    );
    match ctx.increment("no_code_reroutes").await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                task_id = ctx.task_id,
                err = %e,
                "failed to increment no_code_reroutes — reading current value from store"
            );
            // On DB error, read current value to ensure blocking threshold is still reachable
            ctx.get_task()
                .await
                .map_or(1, |t| u64::try_from(t.no_code_reroutes).unwrap_or(0) + 1)
            // assume at least 1 if we can't read either
        }
    }
}

// ── Post-decision side-effect helpers ─────────────────────────────────────────

/// Build a human-readable reason string from a "blocked" agent response.
///
/// Priority: explicit `error` field → `summary` → fallback.
/// Appends `remaining` items if present so operators know what was left to do.
fn agent_blocked_reason(resp: &AgentResponse) -> String {
    let summary_opt = if resp.summary.is_empty() {
        None
    } else {
        Some(resp.summary.as_str())
    };
    let base = resp
        .error
        .as_deref()
        .filter(|s| !s.is_empty())
        .or(summary_opt)
        .unwrap_or("agent returned blocked status without a reason");
    if resp.remaining.is_empty() {
        base.to_string()
    } else {
        format!("{base}. Remaining: {}", resp.remaining.join("; "))
    }
}

/// True when a blocked response names a denied `git push` and nothing else as the blocker.
fn is_denied_push_block(resp: &AgentResponse) -> bool {
    let r = agent_blocked_reason(resp).to_lowercase();
    r.contains("push")
        && [
            "denied",
            "not allowed",
            "permission",
            "disallowed",
            "approval",
        ]
        .iter()
        .any(|p| r.contains(p))
}

/// Heuristic classifier for agent-returned blocked reasons.
///
/// Returns true when the reason looks transient/retryable (network/provider/
/// auth-temporary/environment availability issues), and false for likely
/// permanent/input/configuration issues.
fn is_retryable_blocked_reason(reason: &str) -> bool {
    let r = reason.to_lowercase();

    let permanent_patterns = [
        "not found",
        "does not exist",
        "invalid",
        "malformed",
        "syntax error",
        "unsupported",
        "unimplemented",
        "manual intervention required",
        "human intervention required",
    ];
    if permanent_patterns.iter().any(|p| r.contains(p)) {
        return false;
    }

    let transient_patterns = [
        "unavailable",
        "temporarily unavailable",
        "timeout",
        "timed out",
        "connection reset",
        "connection refused",
        "network error",
        "rate limit",
        "try again",
        "service down",
        "permission denied",
        "worktree lock",
        "git lock",
        "lockfile permission",
        "lock permission",
        "resource busy",
        "could not resolve host",
        // Credential providers in headless/tmux sessions can fail transiently
        // when GPG agent/passphrase is unavailable.
        "gpg decryption fails",
        "gpg decryption failed",
        "gpg agent",
        "no passphrase available",
        "passphrase unavailable",
        "pass store",
        "passwordstoreprovider",
        "agentvaultprovider",
        "onepasswordprovider",
        "decryption failed in this agent session",
    ];
    transient_patterns.iter().any(|p| r.contains(p))
}

/// Apply tracing and store writes that depend on the final status decision.
#[expect(
    clippy::too_many_arguments,
    reason = "the decision and its context arrive as separate values"
)]
async fn apply_post_decision_effects(
    ctx: &StoreCtx<'_>,
    final_status: &str,
    resp_status: &str,
    push_state: &PushFailureState,
    is_no_code_reroute: bool,
    has_pr: bool,
    has_pushed: bool,
    is_pr_base_invalid: bool,
    has_delegations: bool,
    is_external: bool,
    no_code_reroutes: u64,
    max_reroutes: u32,
    agent_name: &str,
) {
    if push_state.is_workflow_scope_failure {
        // Non-retryable — block immediately with actionable guidance.
        tracing::error!(
            task_id = ctx.task_id,
            "push failed: token lacks `workflow` OAuth scope — blocking immediately \
             (rerouting would not help)"
        );
        ctx.set(&[(
            "last_error",
            serde_json::json!(format!(
                "push failed: GitHub token lacks `workflow` OAuth scope. \
                 The agent modified .github/workflows/ files but the token cannot push them. \
                 Fix: add `workflow` scope to your GitHub token, or use a GitHub App for auth. \
                 Original error: {}",
                push_state.stored_last_error
            )),
        )])
        .await;
    } else if push_state.is_rebase_conflict_failure {
        tracing::error!(
            task_id = ctx.task_id,
            "push failed and automatic rebase recovery failed — blocking for human intervention"
        );
        ctx.set(&[
            ("agent", serde_json::json!(null)),
            ("model", serde_json::json!(null)),
        ])
        .await;
    } else if push_state.push_failed && final_status == "needs_review" {
        // The engine retries the push from the review gate. Keep agent and model, since the
        // task never goes back to the router
        tracing::warn!(
            task_id = ctx.task_id,
            push_failures = push_state.push_failures,
            "agent done but push failed on a GitHub or network error — engine retries the push"
        );
    } else if push_state.push_failed {
        // Clear agent and model so router picks a different one on reroute (#1604)
        ctx.set(&[
            ("agent", serde_json::json!(null)),
            ("model", serde_json::json!(null)),
        ])
        .await;
        let push_failures = push_state.push_failures;
        if push_failures >= 3 {
            tracing::error!(
                task_id = ctx.task_id,
                push_failures,
                "push failed {push_failures} times — blocking for human intervention"
            );
        } else {
            tracing::warn!(
                task_id = ctx.task_id,
                push_failures,
                "agent done but push failed ({push_failures}/3) — rerouting to different agent"
            );
        }
    } else if resp_status == "done" && !has_pr && has_delegations {
        tracing::info!(
            task_id = ctx.task_id,
            "agent reported done with delegations but no PR — setting blocked"
        );
    } else if (resp_status == "done" || resp_status == "completed")
        && !has_pr
        && has_pushed
        && is_pr_base_invalid
    {
        tracing::error!(
            task_id = ctx.task_id,
            "PR creation failed with invalid base branch — blocking for human intervention"
        );
        ctx.set(&[(
            "last_error",
            serde_json::json!(
                "PR creation failed terminally (invalid base branch or GitHub auth error). The agent's commits were pushed successfully. \
                 Check the repository default branch and the GitHub token, then create the PR manually or unblock the task."
            ),
        )])
        .await;
    } else if resp_status == "done" && !has_pr && has_pushed {
        // Push succeeded but PR creation failed after retries.
        tracing::warn!(
            task_id = ctx.task_id,
            "agent done, commits pushed, but PR creation failed — re-dispatching as routed"
        );
    } else if is_no_code_reroute {
        // Record which agent produced this no-code result so the same-agent
        // detection on the next run can prevent loop-backs (#2410, #2686).
        // Also clear agent/model to force re-routing.

        if final_status == "blocked" {
            // final_status is "blocked" either because max_reroutes was exhausted
            // or because is_same_agent was detected (both cases are handled by
            // classify_final_status). Log accordingly.
            let prev_no_code_agent = ctx
                .get_task()
                .await
                .map(|t| t.no_code_last_agent)
                .unwrap_or_default();
            let is_same_agent = !prev_no_code_agent.is_empty() && prev_no_code_agent == agent_name;

            let msg = if is_same_agent {
                tracing::error!(
                    task_id = ctx.task_id,
                    agent = %agent_name,
                    "blocking same-agent no-code loop: agent {} would be selected again",
                    agent_name
                );
                format!(
                    "agent {agent_name} completed without code changes twice — same-agent loop detected, blocking for human review"
                )
            } else {
                tracing::error!(
                    task_id = ctx.task_id,
                    no_code_reroutes,
                    max_reroutes,
                    "blocking no-code reroute: reached max reroute attempts ({}/{}) for no-code-result",
                    no_code_reroutes,
                    max_reroutes
                );
                format!(
                    "agent completed without code changes after {no_code_reroutes}/{max_reroutes} reroute attempts on external task requiring PR"
                )
            };
            ctx.set(&[("last_error", serde_json::json!(msg))]).await;
        } else {
            // Rerouting — no blocking error to record yet.
            ctx.set(&[(
                "last_error",
                serde_json::json!(
                    "agent completed without code changes on external task requiring PR"
                ),
            )])
            .await;
        }

        // Always record which agent produced this no-code result so the router can
        // skip it on the next attempt. Also clear agent/model to force re-routing
        // (the router will pick a fresh agent, which will be caught by the same-agent
        // check on the next round if it's the same agent).
        ctx.set(&[
            ("no_code_last_agent", serde_json::json!(agent_name)),
            ("agent", serde_json::json!(null)),
            ("model", serde_json::json!(null)),
        ])
        .await;
    } else if resp_status == "done" && !has_pr && is_external {
        // External task with non-code labels (e.g. documentation, research) —
        // allowed to be marked done without a PR.
        tracing::info!(
            task_id = ctx.task_id,
            "external task with non-code labels reported done — marking done without PR"
        );
    } else if resp_status == "done" && !has_pr {
        tracing::info!(
            task_id = ctx.task_id,
            "internal task reported done with no PR — marking done"
        );
    }
}

/// Store token usage if both input and output token counts are known, with the cost the agent
/// reported for the run.
async fn store_token_usage(
    ctx: &StoreCtx<'_>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cost_usd: Option<f64>,
) {
    let (Some(input), Some(output)) = (input_tokens, output_tokens) else {
        return;
    };
    if let Some(st) = ctx.store {
        if let Some(store_id) = ctx.store_id_opt {
            if let Err(e) = st.store_tokens(store_id, input, output, cost_usd).await {
                tracing::warn!(task_id = ctx.task_id, ?e, "failed to store token usage");
            }
        } else if let Ok(Some(store_id)) = st.resolve_task_id(ctx.repo, ctx.task_id).await
            && let Err(e) = st.store_tokens(store_id, input, output, cost_usd).await
        {
            tracing::warn!(task_id = ctx.task_id, ?e, "failed to store token usage");
        }
    }
}

/// Handle a successful agent response: commit, push, PR, delegations, tokens.
///
/// Returns `Ok((status, push_failed))` where `status` is the final task status
/// string and `push_failed` is `true` when a push was attempted but failed (for
/// audit trail classification).
#[expect(
    clippy::too_many_arguments,
    reason = "the runner passes each part of the run result"
)]
pub async fn handle_success(
    task_id: &str,
    parsed: agents::ParsedResponse,
    wt: &worktree::WorktreeSetup,
    task_title: &str,
    agent_name: &str,
    model_name: Option<&str>,
    new_attempts: u32,
    repo: &str,
    store: &Option<Arc<TaskStore>>,
    raw_stdout: &str,
) -> anyhow::Result<(String, bool)> {
    // Extract token counts before consuming parsed.
    let input_tokens = parsed.input_tokens;
    let output_tokens = parsed.output_tokens;
    let cost_usd = parsed.cost_usd;
    let mut resp = parsed.response;

    tracing::info!(
        task_id,
        status = resp.status,
        summary = resp.summary,
        "agent completed successfully"
    );

    let push_denied = resp.status == "blocked" && is_denied_push_block(&resp);

    // Resolve numeric store_id once so we can reuse it for multiple store ops
    // in this hot path and avoid repeated external_id -> store_id SQL lookups.
    let store_id_opt = store::resolve_store_id(store, repo, task_id).await;
    let ctx = StoreCtx {
        store,
        store_id_opt,
        repo,
        task_id,
    };

    ctx.reset_retry_streaks().await;

    // Auto-commit, push, create PR
    let git = if resp.status == "done"
        || resp.status == "completed"
        || resp.status == "in_progress"
        || resp.status == "needs_review"
        || push_denied
    {
        run_git_ops(
            &ctx,
            wt,
            task_title,
            &resp,
            agent_name,
            model_name,
            new_attempts,
        )
        .await
    } else {
        GitOpsResult::default()
    };

    // The agent's only blocker was the denied push and orch pushed for it, so
    // the task continues as if the agent had returned done
    if push_denied && (git.has_pr || git.has_pushed) {
        tracing::info!(
            task_id,
            "agent blocked on denied push, orch pushed the branch"
        );
        resp.status = "done".to_string();
    }

    let screenshot_note = if git.has_pr {
        post_screenshots(&ctx, &wt.work_dir, &resp).await
    } else {
        None
    };

    // Store delegations in store if present (processed by run_with_context)
    if !resp.delegations.is_empty() {
        ctx.set(&[("delegations", serde_json::json!(resp.delegations))])
            .await;
    }

    // Store result in task store
    // If agent said "done" and a PR exists, send to review before merge.
    // If agent said "done", pushed commits, but PR creation failed — review gate creates PR.
    // If agent said "done", no PR, and no delegations — work is complete
    // (e.g., review/analysis jobs that create issues but no code changes).
    // If agent said "done", no PR, but has delegations — blocked on children.
    let has_delegations = !resp.delegations.is_empty();

    // Detect push failure, increment counters.
    let push_state = detect_push_failure_state(&ctx, git.has_commits, git.has_pushed).await;

    // Determine whether this external task can be marked done without a PR.
    // External tasks always require a PR before reaching `done` — unless
    // commits were successfully pushed (in which case the `has_pushed` branch
    // handles routing). The `is_non_code_task` heuristic was removed because
    // it relied on agent output and could be fooled: an agent claiming
    // "already implemented" or "config-only" would match non-code keywords
    // and close the issue without verification.
    let is_external = !task_id.starts_with("internal:");
    let requires_pr = is_external && !git.has_pr && !resp.status.starts_with("needs_review");

    // Preferred config key `workflow.max_reroute_attempts`; fall back to
    // `workflow.max_attempts` for backwards compatibility.
    let max_reroutes: u32 = config::get("workflow.max_reroute_attempts")
        .ok()
        .and_then(|s| s.parse().ok())
        .or_else(|| {
            config::get("workflow.max_attempts")
                .ok()
                .and_then(|s| s.parse().ok())
        })
        .unwrap_or(3);

    // Determine whether we are entering the no-code-reroute branch (all
    // earlier chain conditions are false + done + requires_pr).
    let is_no_code_reroute = !push_state.is_workflow_scope_failure
        && !push_state.push_failed
        && (resp.status == "done" || resp.status == "completed")
        && !has_delegations
        && !git.has_pushed
        && requires_pr;

    // A conflict reroute with nothing ahead of the default branch is a landed
    // fix, not a no-code run: close the superseded PR and mark the task done.
    let superseded_pr = match (is_no_code_reroute, ctx.get_task().await) {
        (true, Some(task)) => {
            superseded_conflict_pr(task.merge_conflict_retries, task.pr_number, git.has_commits)
        }
        _ => None,
    };
    let is_no_code_reroute = is_no_code_reroute && superseded_pr.is_none();

    let no_code_reroutes =
        detect_no_code_reroutes(&ctx, is_no_code_reroute, new_attempts, max_reroutes).await;

    let blocked_reason = if resp.status == "blocked" {
        Some(agent_blocked_reason(&resp))
    } else {
        None
    };
    let is_retryable_blocked = blocked_reason
        .as_deref()
        .is_some_and(is_retryable_blocked_reason);

    // ── Detect same-agent loop ───────────────────────────────────────────────
    //
    // If this is a no-code reroute and the agent is the same as the one that
    // produced the previous no-code result, flag it for blocking (#2410, #2686).
    let is_same_agent = if is_no_code_reroute {
        let prev_no_code_agent = ctx
            .get_task()
            .await
            .map(|t| t.no_code_last_agent)
            .unwrap_or_default();
        !prev_no_code_agent.is_empty() && prev_no_code_agent == agent_name
    } else {
        false
    };

    // ── Pure status decision ─────────────────────────────────────────────────

    let final_status_owned = classify_final_status(&DecisionInput {
        agent_status: &resp.status,
        is_workflow_scope_failure: push_state.is_workflow_scope_failure,
        is_rebase_conflict_failure: push_state.is_rebase_conflict_failure,
        push_failed: push_state.push_failed,
        push_failures: push_state.push_failures,
        is_transient_push_failure: push_state.is_transient_failure,
        has_pr: git.has_pr,
        has_delegations,
        has_pushed: git.has_pushed,
        is_pr_base_invalid: git.pr_base_invalid,
        requires_pr,
        no_code_reroutes,
        max_reroutes,
        is_same_agent,
        is_completed_status: resp.status == "completed",
        is_retryable_blocked,
        is_superseded_conflict: superseded_pr.is_some(),
    });
    let final_status = final_status_owned.as_str();

    // ── Post-decision side effects (tracing + store updates) ─────────────────

    apply_post_decision_effects(
        &ctx,
        final_status,
        &resp.status,
        &push_state,
        is_no_code_reroute,
        git.has_pr,
        git.has_pushed,
        git.pr_base_invalid,
        has_delegations,
        is_external,
        no_code_reroutes,
        max_reroutes,
        agent_name,
    )
    .await;

    // When the agent explicitly returned "blocked", persist its explanation into
    // last_error so `orch task get` surfaces the reason without requiring manual
    // inspection of task_runs.parsed_response.  The apply_post_decision_effects
    // branches above only fire on push/no-code scenarios; a raw agent-blocked
    // response falls through all of them and would leave last_error empty.
    if resp.status == "blocked" {
        let reason = blocked_reason.unwrap_or_else(|| agent_blocked_reason(&resp));
        if final_status == "new" {
            // Retryable external block: cooldown failed agent/model, clear selection,
            // and reroute after backoff instead of permanently blocking.
            cooldown::record_agent_failure_with_message(agent_name, &reason).await;
            if let Some(m) = model_name {
                cooldown::record_model_failure(agent_name, m).await;
            }
            ctx.set(&[
                (
                    "last_error",
                    serde_json::json!(format!(
                        "transient blocker (auto-retry scheduled): {reason}"
                    )),
                ),
                ("agent", serde_json::json!(null)),
                ("model", serde_json::json!(null)),
            ])
            .await;
            tracing::warn!(
                task_id = ctx.task_id,
                reason = %reason,
                "agent returned retryable blocked status — rerouting as new with cooldown"
            );
        } else if final_status == "blocked" {
            ctx.set(&[("last_error", serde_json::json!(reason))]).await;
        }
    }

    if let Some(pr_number) = superseded_pr {
        tracing::info!(
            task_id = ctx.task_id,
            pr_number,
            "conflict reroute found the fix already on the default branch — closing superseded PR"
        );
        close_superseded_pr(&ctx, pr_number).await;
    }

    // The summary reaches the channels, so a missing skill or browser goes with it
    let summary = match &screenshot_note {
        Some(note) => format!("{}\n\nScreenshots: {note}", resp.summary),
        None => resp.summary.clone(),
    };
    ctx.set(&[("summary", serde_json::json!(summary))]).await;

    // Store token usage — prefer agent-parsed tokens, fall back to response
    store_token_usage(
        &ctx,
        input_tokens.or(resp.input_tokens),
        output_tokens.or(resp.output_tokens),
        cost_usd,
    )
    .await;

    // Store learnings for memory (for future retries)
    response::store_learnings_from_response(
        task_id,
        new_attempts,
        agent_name,
        model_name,
        &resp,
        resp.error.as_deref(),
        store,
        repo,
        raw_stdout,
    )
    .await;

    // Note: done → in_review transition is handled by the engine
    // after triggering the review agent (engine/mod.rs)
    Ok((final_status.to_string(), push_state.push_failed))
}

/// Labels that indicate a task is non-code and can be marked done without a PR.
#[cfg(test)]
mod tests;
