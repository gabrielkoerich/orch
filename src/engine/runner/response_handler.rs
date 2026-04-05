//! Success-path response handler — commit, push, PR, token storage, budget check.
//!
//! Extracted from `runner/mod.rs`. Handles the `Ok(parsed)` arm of the parse
//! result, including git operations, delegation storage, and budget enforcement.
//! Also owns `write_result_json`.

use crate::config;
use crate::store;
use crate::store::TaskStore;
use std::path::Path;
use std::sync::Arc;

use super::{agents, git_ops, response, worktree};

/// Write a structured `result.json` to the attempt directory for debugging.
#[allow(clippy::too_many_arguments)]
pub fn write_result_json(
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

    if let Err(e) = std::fs::write(
        attempt_dir.join("result.json"),
        serde_json::to_string_pretty(&result_json).unwrap_or_default(),
    ) {
        tracing::debug!(task_id, ?e, "failed to write result.json");
    }
}

/// Input to the pure status-decision function [`classify_final_status`].
///
/// The caller is responsible for pre-computing counter values (via store
/// increments) and populating them here.  No I/O is performed inside the
/// decision function itself, making it trivially unit-testable.
#[derive(Default)]
struct DecisionInput<'a> {
    /// Agent-reported status (e.g. `"done"`, `"in_progress"`).
    agent_status: &'a str,
    /// Push was attempted and commits existed, but the push error contained a
    /// `workflow` scope complaint — non-retryable.
    is_workflow_scope_failure: bool,
    /// Push was attempted (commits existed) but failed.
    push_failed: bool,
    /// Persistent push-failure counter *after* this run's increment.
    /// Only meaningful when `push_failed` is `true`.
    push_failures: u64,
    /// A PR already exists (or was just created) for this task.
    has_pr: bool,
    /// The agent created sub-task delegations.
    has_delegations: bool,
    /// Commits were pushed to the remote branch successfully.
    has_pushed: bool,
    /// Whether this is an external task (has a GitHub issue).
    is_external: bool,
    /// The task is external and requires a PR to be marked done.
    /// Always false for internal tasks.
    requires_pr: bool,
    /// Persistent no-code-reroute counter *after* this run's increment.
    /// Only meaningful when `is_external` is `true` and `has_pushed` is `false`.
    no_code_reroutes: u64,
    /// Maximum no-code reroutes before blocking (from config).
    max_reroutes: u32,
}

/// Determine the final task status from pre-computed state.
///
/// This is a pure function — it performs no I/O.  All store increments must be
/// done by the caller **before** calling this function (so counters reflect the
/// current run), and all store side-effects (clearing agent/model, writing
/// last_error) must be applied **after** based on the returned status.
fn classify_final_status(input: &DecisionInput<'_>) -> String {
    if input.is_workflow_scope_failure {
        "blocked".to_string()
    } else if input.push_failed {
        if input.push_failures >= 3 {
            "blocked".to_string()
        } else {
            "new".to_string()
        }
    } else if input.agent_status == "done" && input.has_pr {
        "needs_review".to_string()
    } else if input.agent_status == "done" && !input.has_pr && input.has_delegations {
        "blocked".to_string()
    } else if input.agent_status == "done" && !input.has_pr && input.has_pushed {
        "routed".to_string()
    } else if input.agent_status == "done" && input.requires_pr {
        if input.no_code_reroutes >= input.max_reroutes as u64 {
            "blocked".to_string()
        } else {
            "new".to_string()
        }
    } else if input.agent_status == "done" && !input.has_pushed {
        // Internal tasks and external non-code tasks may finish without a PR,
        // but only when commits were actually pushed. If no commits were pushed
        // (has_pushed=false), the task has no verifiable work product and must
        // be re-routed — an agent claiming "nothing to do" without producing
        // commits cannot close an issue.
        if input.is_external {
            // External task with no pushed commits: re-route up to max_reroutes,
            // then block for human review.
            if input.no_code_reroutes >= input.max_reroutes as u64 {
                "blocked".to_string()
            } else {
                "new".to_string()
            }
        } else {
            // Internal task with no pushed commits: still mark done (internal
            // tasks may legitimately produce no git-visible changes).
            "done".to_string()
        }
    } else {
        input.agent_status.to_string()
    }
}

/// Handle a successful agent response: commit, push, PR, delegations, tokens, budget.
///
/// Returns `Ok((status, budget_exceeded, push_failed))` where `status` is the final task status
/// string, `budget_exceeded` is `true` if `run()` should return early, and `push_failed`
/// is `true` when a push was attempted but failed (for audit trail classification).
#[allow(clippy::too_many_arguments)]
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
) -> anyhow::Result<(String, bool, bool)> {
    let resp = parsed.response;
    tracing::info!(
        task_id,
        status = resp.status,
        summary = resp.summary,
        "agent completed successfully"
    );

    // Auto-commit, push, create PR
    let mut has_pr = false;
    let mut has_pushed = false;
    let mut has_commits = false;
    if resp.status == "done" || resp.status == "in_progress" || resp.status == "needs_review" {
        if let Err(e) =
            git_ops::auto_commit(&wt.work_dir, task_id, task_title, agent_name, new_attempts).await
        {
            tracing::error!(task_id, error = ?e, "auto commit failed");
            let msg = format!("auto commit failed: {e}");
            store::store_set(
                store,
                repo,
                task_id,
                &[("last_error", serde_json::json!(msg))],
            )
            .await;
        }

        // Skip push + PR if there are no commits ahead of the default branch.
        // No-op tasks (e.g. "nothing to execute") produce no commits, so pushing
        // and creating a PR would just waste API calls and trigger 422 errors.
        has_commits = git_ops::has_commits_ahead(&wt.work_dir, &wt.default_branch).await;
        if !has_commits {
            tracing::info!(
                task_id,
                "no commits ahead of default branch, skipping push + PR"
            );
            // Clear stale push failure from previous runs
            // Load stored task once to inspect last_error (avoid repeated DB reads)
            let last_err = store::opt_store_get_task(store, repo, task_id)
                .await
                .map(|t| t.last_error)
                .unwrap_or_default();
            if last_err.contains("push failed") {
                store::store_set(
                    store,
                    repo,
                    task_id,
                    &[("last_error", serde_json::json!(""))],
                )
                .await;
            }
        }

        // Push
        let push_ok = if !has_commits {
            store::store_log_activity(
                store,
                repo,
                task_id,
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
            false
        } else {
            match git_ops::push_branch(&wt.work_dir, &wt.branch, &wt.default_branch).await {
                Ok(_) => {
                    has_pushed = true;
                    store::store_log_activity(
                        store,
                        repo,
                        task_id,
                        "push",
                        None,
                        None,
                        Some(agent_name),
                        model_name,
                        Some(&serde_json::json!({
                            "status": "ok",
                            "branch": wt.branch,
                            "default_branch": wt.default_branch,
                        })),
                    )
                    .await;
                    // Clear any stale push failure from a previous run so review_and_merge
                    // does not incorrectly block an approved task.
                    store::store_set(
                        store,
                        repo,
                        task_id,
                        &[
                            ("last_error", serde_json::json!("")),
                            ("push_failures", serde_json::json!(0)),
                        ],
                    )
                    .await;
                    true
                }
                Err(e) => {
                    tracing::error!(task_id, error = ?e, "push failed");
                    store::store_log_activity(
                        store,
                        repo,
                        task_id,
                        "push",
                        None,
                        None,
                        Some(agent_name),
                        model_name,
                        Some(&serde_json::json!({
                            "status": "error",
                            "branch": wt.branch,
                            "default_branch": wt.default_branch,
                            "error": e.to_string(),
                        })),
                    )
                    .await;
                    let msg = format!("push failed: {e}");
                    store::store_set(
                        store,
                        repo,
                        task_id,
                        &[("last_error", serde_json::json!(msg))],
                    )
                    .await;
                    false
                }
            }
        };

        // Create PR (skip if push failed or repo is unknown)
        if has_commits && !push_ok {
            tracing::warn!(task_id, "skipping PR creation due to push failure");
        } else if !push_ok {
            // no commits — already logged "no commits ahead, skipping push + PR" at INFO level above
        } else if repo.is_empty() {
            tracing::warn!(
                task_id,
                "skipping PR creation — repo is empty (internal task?)"
            );
        } else {
            match git_ops::create_pr_if_needed(
                &wt.work_dir,
                &wt.branch,
                task_title,
                &resp.summary,
                &resp.accomplished,
                &resp.remaining,
                &resp.files,
                task_id,
                agent_name,
                model_name,
                repo,
                &wt.default_branch,
            )
            .await
            {
                Ok(ref url) => {
                    has_pr = true;
                    // Save pr_number to the store so the review gate can find it
                    // immediately without racing GitHub's list-API cache (~300 ms lag).
                    // This is set for both newly-created and pre-existing PRs.
                    if let Some(pr_num) = crate::engine::review::parse_pr_number_from_url(url) {
                        store::store_set(
                            store,
                            repo,
                            task_id,
                            &[("pr_number", serde_json::json!(pr_num as i64))],
                        )
                        .await;
                    }
                    store::store_log_activity(
                        store,
                        repo,
                        task_id,
                        "pr_create",
                        None,
                        None,
                        Some(agent_name),
                        model_name,
                        Some(&serde_json::json!({
                            "status": "created",
                            "url": url,
                        })),
                    )
                    .await;
                }
                Err(e) => {
                    let err_str = format!("{e}");
                    tracing::error!(task_id, error = ?e, "create PR failed");
                    store::store_log_activity(
                        store,
                        repo,
                        task_id,
                        "pr_create",
                        None,
                        None,
                        Some(agent_name),
                        model_name,
                        Some(&serde_json::json!({
                            "status": "error",
                            "branch": wt.branch,
                            "error": err_str.clone(),
                        })),
                    )
                    .await;
                    let msg = format!("create PR failed: {e}");
                    store::store_set(
                        store,
                        repo,
                        task_id,
                        &[("last_error", serde_json::json!(msg))],
                    )
                    .await;
                    // 422 "No commits between main and branch" means the agent
                    // made no code changes — the task is done without a PR.
                    // Also handles the "head" invalid variant (already merged).
                    // Clear has_pushed so we fall through to the "done" path
                    // instead of spinning in the review gate indefinitely.
                    if err_str.contains("422")
                        && (err_str.contains("No commits between") || err_str.contains("head"))
                    {
                        tracing::info!(
                            task_id,
                            "PR creation returned 422/no-commits — agent made no code changes, marking done"
                        );
                        has_pushed = false;
                    }
                }
            }
        }
    }

    // Store delegations in store if present (processed by run_with_context)
    if !resp.delegations.is_empty() {
        store::store_set(
            store,
            repo,
            task_id,
            &[("delegations", serde_json::json!(resp.delegations))],
        )
        .await;
    }

    // Store result in task store
    // If agent said "done" and a PR exists, send to review before merge.
    // If agent said "done", pushed commits, but PR creation failed — review gate creates PR.
    // If agent said "done", no PR, and no delegations — work is complete
    // (e.g., review/analysis jobs that create issues but no code changes).
    // If agent said "done", no PR, but has delegations — blocked on children.
    let has_delegations = !resp.delegations.is_empty();

    // Track push failures — block after 3 consecutive failures.
    // Check if push was attempted but failed: tried to push (has_commits),
    // but has_pushed is still false and last_error contains a push failure.
    // Applies regardless of agent-reported status — push failures must be
    // surfaced so task_runs records them correctly and the task is rerouted.
    // Read last_error once (reuse the value read earlier if available)
    let stored_last_error = store::opt_store_get_task(store, repo, task_id)
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

    // Determine whether this external task can be marked done without a PR.
    // External tasks always require a PR before reaching `done` — unless
    // commits were successfully pushed (in which case the `has_pushed` branch
    // handles routing). The `is_non_code_task` heuristic was removed because
    // it relied on agent output and could be fooled: an agent claiming
    // "already implemented" or "config-only" would match non-code keywords
    // and close the issue without verification.
    let is_external = !task_id.starts_with("internal:");
    let requires_pr = is_external && !has_pr && !resp.status.starts_with("needs_review");

    // ── Pre-compute counters needed by classify_final_status ─────────────────
    //
    // Store increments must happen *before* the pure decision so that the
    // returned counter values are already post-increment.

    // Push-failure counter — use atomic increment to avoid a read-modify-write race.
    // Workflow-scope failures are non-retryable and blocked immediately; do not
    // increment this retry counter for them so that later normal push failures
    // are not prematurely blocked.
    let push_failures: u64 = if push_failed && !is_workflow_scope_failure {
        store::store_increment(store, repo, task_id, "push_failures").await
    } else {
        0
    };

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
    let is_no_code_reroute = !is_workflow_scope_failure
        && !push_failed
        && resp.status == "done"
        && !has_delegations
        && !has_pushed
        && requires_pr;

    let no_code_reroutes: u64 = if is_no_code_reroute {
        tracing::warn!(
            task_id,
            attempts = new_attempts,
            max_reroutes,
            "agent reported done but produced no code changes on external task requiring PR"
        );
        store::store_increment(store, repo, task_id, "no_code_reroutes").await
    } else {
        0
    };

    // ── Pure status decision ─────────────────────────────────────────────────

    let final_status_owned = classify_final_status(&DecisionInput {
        agent_status: &resp.status,
        is_workflow_scope_failure,
        push_failed,
        push_failures,
        has_pr,
        has_delegations,
        has_pushed,
        is_external,
        requires_pr,
        no_code_reroutes,
        max_reroutes,
    });
    let final_status = final_status_owned.as_str();

    // ── Post-decision side effects (tracing + store updates) ─────────────────

    if is_workflow_scope_failure {
        // Non-retryable — block immediately with actionable guidance.
        tracing::error!(
            task_id,
            "push failed: token lacks `workflow` OAuth scope — blocking immediately \
             (rerouting would not help)"
        );
        store::store_set(
            store,
            repo,
            task_id,
            &[(
                "last_error",
                serde_json::json!(format!(
                    "push failed: GitHub token lacks `workflow` OAuth scope. \
                     The agent modified .github/workflows/ files but the token cannot push them. \
                     Fix: add `workflow` scope to your GitHub token, or use a GitHub App for auth. \
                     Original error: {}",
                    stored_last_error
                )),
            )],
        )
        .await;
    } else if push_failed {
        // Clear agent and model so router picks a different one on reroute (#1604)
        store::store_set(
            store,
            repo,
            task_id,
            &[
                ("agent", serde_json::json!(null)),
                ("model", serde_json::json!(null)),
            ],
        )
        .await;
        if push_failures >= 3 {
            tracing::error!(
                task_id,
                push_failures,
                "push failed {push_failures} times — blocking for human intervention"
            );
        } else {
            tracing::warn!(
                task_id,
                push_failures,
                "agent done but push failed ({push_failures}/3) — rerouting to different agent"
            );
        }
    } else if resp.status == "done" && !has_pr && has_delegations {
        tracing::info!(
            task_id,
            "agent reported done with delegations but no PR — setting blocked"
        );
    } else if resp.status == "done" && !has_pr && has_pushed {
        // Push succeeded but PR creation failed after retries.
        tracing::warn!(
            task_id,
            "agent done, commits pushed, but PR creation failed — re-dispatching as routed"
        );
    } else if is_no_code_reroute {
        if final_status == "blocked" {
            tracing::error!(
                task_id,
                no_code_reroutes,
                max_reroutes,
                "reached max reroute attempts for no-code-result on external task — blocking for human review"
            );
        }
        // Clear agent/model and record an explanatory last_error.
        let msg = if final_status == "blocked" {
            format!(
                "agent completed without code changes after {}/{} reroute attempts on external task requiring PR",
                no_code_reroutes, max_reroutes
            )
        } else {
            "agent completed without code changes on external task requiring PR".to_string()
        };
        store::store_set(
            store,
            repo,
            task_id,
            &[
                ("agent", serde_json::json!(null)),
                ("model", serde_json::json!(null)),
                ("last_error", serde_json::json!(msg)),
            ],
        )
        .await;
    } else if resp.status == "done" && !has_pr && is_external {
        // External task with non-code labels (e.g. documentation, research) —
        // allowed to be marked done without a PR.
        tracing::info!(
            task_id,
            "external task with non-code labels reported done — marking done without PR"
        );
    } else if resp.status == "done" && !has_pr {
        tracing::info!(
            task_id,
            "internal task reported done with no PR — marking done"
        );
    }
    store::store_set(
        store,
        repo,
        task_id,
        &[("summary", serde_json::json!(resp.summary))],
    )
    .await;

    // Store token usage — prefer agent-parsed tokens, fall back to response
    let input_tokens = parsed.input_tokens.or(resp.input_tokens);
    let output_tokens = parsed.output_tokens.or(resp.output_tokens);
    if let (Some(input), Some(output)) = (input_tokens, output_tokens) {
        let model = model_name.unwrap_or("haiku");
        if let Some(ref st) = store {
            // Resolve store_id once and reuse
            if let Ok(Some(store_id)) = st.resolve_task_id(repo, task_id).await {
                if let Err(e) = st
                    .store_tokens(store_id, input as i64, output as i64, model)
                    .await
                {
                    tracing::warn!(task_id, ?e, "failed to store token usage");
                }
            }
        }
    }

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
    )
    .await;

    // Check token budget with warning thresholds
    // Use the same config key as pre-run checks. If set to 0, disable budget
    // enforcement so tasks may continue without token budget gating.
    let max_tokens: u64 = config::get("workflow.max_tokens_per_task")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100_000);

    if max_tokens == 0 {
        // Budget checks disabled — nothing to do here.
        return Ok((final_status.to_string(), false, push_failed));
    }

    // Query total tokens and cost estimate together (single DB read)
    let (total_tokens, cost) = store::get_token_summary(store, repo, task_id).await;
    let warning_threshold = (max_tokens as f64 * 0.8) as u64;

    if total_tokens > max_tokens {
        tracing::warn!(task_id, total_tokens, max_tokens, "exceeded token budget");
        // Only override to needs_review if there's a PR to review;
        // otherwise keep the already-computed final_status (e.g. "done" for
        // read-only tasks with no code changes).
        let budget_status = if has_pr { "needs_review" } else { final_status };
        let budget_msg = format!(
            "token budget exceeded: {}/{} tokens (${:.4})",
            total_tokens, max_tokens, cost.total_cost_usd
        );
        store::store_set(
            store,
            repo,
            task_id,
            &[
                ("last_error", serde_json::json!(budget_msg)),
                ("budget_exceeded", serde_json::json!(true)),
            ],
        )
        .await;
        return Ok((budget_status.to_string(), true, push_failed)); // signal early return to caller
    } else if total_tokens > warning_threshold {
        let pct = (total_tokens as f64 / max_tokens as f64 * 100.0) as u32;
        tracing::warn!(
            task_id,
            total_tokens,
            max_tokens,
            pct,
            "approaching token budget"
        );
        let warning_msg = format!(
            "{}% of budget used ({}/{} tokens, ${:.4})",
            pct, total_tokens, max_tokens, cost.total_cost_usd
        );
        store::store_set(
            store,
            repo,
            task_id,
            &[("budget_warning", serde_json::json!(warning_msg))],
        )
        .await;
    }

    // Note: done → in_review transition is handled by the engine
    // after triggering the review agent (engine/mod.rs)
    Ok((final_status.to_string(), false, push_failed))
}

/// Labels that indicate a task is non-code and can be marked done without a PR.
#[cfg(test)]
mod tests {
    use super::*;

    // ── classify_final_status — one test per decision branch ─────────────────

    /// Branch 1: workflow-scope push failure → block immediately (non-retryable).
    #[test]
    fn classify_workflow_scope_failure_blocks() {
        let status = classify_final_status(&DecisionInput {
            is_workflow_scope_failure: true,
            push_failed: true,
            push_failures: 1,
            ..Default::default()
        });
        assert_eq!(status, "blocked");
    }

    /// Branch 2a: push failed, < 3 times → reroute with "new".
    #[test]
    fn classify_push_failed_under_threshold_reroutes() {
        let status = classify_final_status(&DecisionInput {
            push_failed: true,
            push_failures: 2, // < 3
            ..Default::default()
        });
        assert_eq!(status, "new");
    }

    /// Branch 2b: push failed >= 3 times → block for human intervention.
    #[test]
    fn classify_push_failed_at_threshold_blocks() {
        let status = classify_final_status(&DecisionInput {
            push_failed: true,
            push_failures: 3, // >= 3
            ..Default::default()
        });
        assert_eq!(status, "blocked");
    }

    /// Branch 3: done + PR exists → send to review.
    #[test]
    fn classify_done_with_pr_goes_to_needs_review() {
        let status = classify_final_status(&DecisionInput {
            agent_status: "done",
            has_pr: true,
            ..Default::default()
        });
        assert_eq!(status, "needs_review");
    }

    /// Branch 4: done + delegations, no PR → block (waiting on child tasks).
    #[test]
    fn classify_done_with_delegations_no_pr_blocks() {
        let status = classify_final_status(&DecisionInput {
            agent_status: "done",
            has_delegations: true,
            ..Default::default()
        });
        assert_eq!(status, "blocked");
    }

    /// Branch 5: done + commits pushed but PR creation failed → re-dispatch.
    #[test]
    fn classify_done_pushed_no_pr_reroutes_for_pr_creation() {
        let status = classify_final_status(&DecisionInput {
            agent_status: "done",
            has_pushed: true,
            ..Default::default()
        });
        assert_eq!(status, "routed");
    }

    /// Branch 6a: done + requires_pr, under max reroutes → reroute.
    #[test]
    fn classify_done_requires_pr_under_max_reroutes() {
        let status = classify_final_status(&DecisionInput {
            agent_status: "done",
            is_external: true,
            requires_pr: true,
            no_code_reroutes: 1,
            max_reroutes: 3,
            ..Default::default()
        });
        assert_eq!(status, "new");
    }

    /// Branch 6b: done + requires_pr, at max reroutes → block.
    #[test]
    fn classify_done_requires_pr_at_max_reroutes_blocks() {
        let status = classify_final_status(&DecisionInput {
            agent_status: "done",
            is_external: true,
            requires_pr: true,
            no_code_reroutes: 3,
            max_reroutes: 3,
            ..Default::default()
        });
        assert_eq!(status, "blocked");
    }

    /// Branch 6b edge: exceeding max also blocks.
    #[test]
    fn classify_done_requires_pr_exceeds_max_reroutes_blocks() {
        let status = classify_final_status(&DecisionInput {
            agent_status: "done",
            is_external: true,
            requires_pr: true,
            no_code_reroutes: 5,
            max_reroutes: 3,
            ..Default::default()
        });
        assert_eq!(status, "blocked");
    }

    /// External task with no pushed commits: reroute (agent must produce commits
    /// before the issue can be closed). This was the source of issue #1898 — an
    /// agent claiming "already implemented" without any code changes would
    /// previously match non-code keywords and close the issue falsely.
    #[test]
    fn classify_done_external_no_pushed_commits_reroutes() {
        let status = classify_final_status(&DecisionInput {
            agent_status: "done",
            is_external: true,
            has_pushed: false,
            requires_pr: false, // even non-code tasks need pushed commits
            no_code_reroutes: 0,
            max_reroutes: 3,
            ..Default::default()
        });
        assert_eq!(status, "new");
    }

    /// Internal task with no pushed commits may still be marked done — internal
    /// tasks may produce no git-visible changes.
    #[test]
    fn classify_done_internal_task_no_pr_is_done() {
        let status = classify_final_status(&DecisionInput {
            agent_status: "done",
            is_external: false,
            has_pushed: false,
            requires_pr: false,
            ..Default::default()
        });
        assert_eq!(status, "done");
    }

    /// Pass-through: non-"done" status is returned unchanged.
    #[test]
    fn classify_non_done_status_passes_through() {
        for status_str in &["in_progress", "needs_review", "blocked"] {
            let result = classify_final_status(&DecisionInput {
                agent_status: status_str,
                ..Default::default()
            });
            assert_eq!(
                result, *status_str,
                "status '{status_str}' should pass through unchanged"
            );
        }
    }

    /// push_failed takes precedence over done+has_pr (push failure detected first).
    #[test]
    fn classify_push_failed_takes_precedence_over_has_pr() {
        let status = classify_final_status(&DecisionInput {
            agent_status: "done",
            has_pr: true,
            push_failed: true,
            push_failures: 1,
            ..Default::default()
        });
        // push_failed branch runs before done+has_pr check
        assert_eq!(status, "new");
    }

    /// workflow_scope_failure takes precedence over push_failed < 3.
    #[test]
    fn classify_workflow_scope_failure_takes_precedence_over_push_reroute() {
        let status = classify_final_status(&DecisionInput {
            is_workflow_scope_failure: true,
            push_failed: true,
            push_failures: 1, // would normally reroute
            ..Default::default()
        });
        assert_eq!(status, "blocked");
    }
}
