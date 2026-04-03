//! Success-path response handler — commit, push, PR, token storage, budget check.
//!
//! Extracted from `runner/mod.rs`. Handles the `Ok(parsed)` arm of the parse
//! result, including git operations, delegation storage, and budget enforcement.
//! Also owns `write_result_json` and the `safe_utf8_tail` utility.

use crate::config;
use crate::parser::AgentResponse;
use crate::store;
use crate::store::TaskStore;
use std::path::Path;
use std::sync::Arc;

use super::{agents, git_ops, response, worktree};

/// Return the last `max_bytes` of `s`, walking forward to the nearest UTF-8
/// character boundary so the slice is always valid.
pub fn safe_utf8_tail(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let start = s.len() - max_bytes;
    // Walk forward from `start` until we land on a char boundary
    let mut idx = start;
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    &s[idx..]
}

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
                "stderr_tail": safe_utf8_tail(raw_stderr, 2000),
                "stdout_tail": safe_utf8_tail(raw_stdout, 2000),
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
        if !push_ok {
            tracing::warn!(task_id, "skipping PR creation due to push failure");
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
    // External tasks with code-related labels must have a merged PR before
    // reaching `done`. Tasks with non-code labels (from the allowlist) may
    // be marked done directly.
    let is_external = !task_id.starts_with("internal:");
    let requires_pr = is_external
        && !has_pr
        && !resp.status.starts_with("needs_review")
        && !is_non_code_task(&resp);

    let final_status = if is_workflow_scope_failure {
        // Workflow scope errors are non-retryable — the token lacks the `workflow`
        // OAuth scope. Rerouting won't help since all agents use the same token.
        // Block immediately with actionable guidance.
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
        "blocked"
    } else if push_failed {
        // Use atomic increment helper to avoid a read-increment-write race.
        let push_failures = store::store_increment(store, repo, task_id, "push_failures").await;

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
            "blocked"
        } else {
            tracing::warn!(
                task_id,
                push_failures,
                "agent done but push failed ({push_failures}/3) — rerouting to different agent"
            );
            "new"
        }
    } else if resp.status == "done" && has_pr {
        "needs_review"
    } else if resp.status == "done" && !has_pr && has_delegations {
        tracing::info!(
            task_id,
            "agent reported done with delegations but no PR — setting blocked"
        );
        "blocked"
    } else if resp.status == "done" && !has_pr && has_pushed {
        // Push succeeded but PR creation failed after retries. Re-dispatch so another
        // agent attempt will push and create the PR (the worktree branch already exists,
        // so the next run will detect the existing branch and create the PR).
        tracing::warn!(
            task_id,
            "agent done, commits pushed, but PR creation failed — re-dispatching as routed"
        );
        "routed"
    } else if resp.status == "done" && requires_pr {
        // Agent claimed done but produced no code changes on an external task
        // that requires a PR (has code-related labels). Use a dedicated
        // circuit-breaker counter persisted in the store so repeated reroutes
        // across separate runs are counted. Prefer a dedicated config key
        // `workflow.max_reroute_attempts` (fallback to `workflow.max_attempts`
        // for backwards compatibility).
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
            task_id,
            attempts = new_attempts,
            max_reroutes,
            "agent reported done but produced no code changes on external task requiring PR"
        );

        // Atomically increment the persistent reroute counter and decide.
        let reroutes = store::store_increment(store, repo, task_id, "no_code_reroutes").await;

        if reroutes as u32 >= max_reroutes {
            tracing::error!(
                task_id,
                reroutes,
                max_reroutes,
                "reached max reroute attempts for no-code-result on external task — blocking for human review"
            );
            // Clear agent/model and record an explanatory last_error
            let msg = format!(
                "agent completed without code changes after {}/{} reroute attempts on external task requiring PR",
                reroutes, max_reroutes
            );
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
            "blocked"
        } else {
            // Clear agent/model so router picks a different one and note the
            // fact that this attempt produced no code changes.
            store::store_set(
                store,
                repo,
                task_id,
                &[
                    ("agent", serde_json::json!(null)),
                    ("model", serde_json::json!(null)),
                    (
                        "last_error",
                        serde_json::json!(
                            "agent completed without code changes on external task requiring PR"
                        ),
                    ),
                ],
            )
            .await;
            "new"
        }
    } else if resp.status == "done" && !has_pr && is_external {
        // External task with non-code labels (e.g. documentation, research) —
        // allowed to be marked done without a PR.
        tracing::info!(
            task_id,
            "external task with non-code labels reported done — marking done without PR"
        );
        "done"
    } else if resp.status == "done" && !has_pr {
        tracing::info!(
            task_id,
            "internal task reported done with no PR — marking done"
        );
        "done"
    } else {
        &resp.status
    };
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
    let max_tokens: u64 = config::get("max_tokens_per_task")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100_000);

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
/// If a task has ANY of these labels (and NO code-related labels), it may be
/// marked `done` directly without requiring a merged PR.
const NON_CODE_LABELS: &[&str] = &[
    "documentation",
    "docs",
    "research",
    "analysis",
    "investigation",
    "question",
    "discussion",
    "planning",
    "design",
    "review",
    "audit",
    "config-change",
];

/// Check if the response indicates a non-code task based on labels in the
/// accomplished/remaining/summary text and delegations.
///
/// This is a heuristic — we check if the agent's output mentions non-code
/// labels or if delegations suggest non-code work. The definitive check
/// should come from GitHub issue labels, but those are not available in
/// the response handler. This function provides a best-effort classification.
///
/// Returns `true` if the task appears to be non-code (can be done without PR).
fn is_non_code_task(resp: &AgentResponse) -> bool {
    // Check delegations for non-code indicators
    for delegation in &resp.delegations {
        let combined = format!("{} {}", delegation.title, delegation.body).to_lowercase();
        if has_non_code_label(&combined) {
            return true;
        }
    }

    // Check accomplished items for non-code indicators
    for item in &resp.accomplished {
        let lower = item.to_lowercase();
        if has_non_code_label(&lower) {
            return true;
        }
    }

    // Check summary for non-code indicators
    let summary_lower = resp.summary.to_lowercase();
    if has_non_code_label(&summary_lower) {
        return true;
    }

    false
}

/// Check if text contains any non-code label as a whole word (not a substring of another word).
///
/// Uses word-boundary matching: a label matches only when it is surrounded by
/// non-alphanumeric characters (spaces, punctuation, start/end of string).
/// This prevents false positives like "reviewed" matching "review", or
/// "designing" matching "design".
fn has_non_code_label(text: &str) -> bool {
    NON_CODE_LABELS.iter().any(|label| {
        let bytes = text.as_bytes();
        let label_bytes = label.as_bytes();
        let label_len = label_bytes.len();
        let text_len = bytes.len();

        if label_len > text_len {
            return false;
        }

        // Slide a window of label_len across text looking for a whole-word match.
        for start in 0..=(text_len - label_len) {
            let end = start + label_len;
            if &bytes[start..end] != label_bytes {
                continue;
            }
            // Check that the character before the match is a word boundary.
            let left_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
            // Check that the character after the match is a word boundary.
            let right_ok = end == text_len || !bytes[end].is_ascii_alphanumeric();
            if left_ok && right_ok {
                return true;
            }
        }
        false
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::Delegation;

    #[test]
    fn test_has_non_code_label_detects_labels() {
        // Whole-word matches must be detected
        assert!(has_non_code_label("updated documentation"));
        assert!(has_non_code_label("research completed"));
        assert!(has_non_code_label("analysis of the issue"));
        assert!(has_non_code_label("design review done"));
        assert!(has_non_code_label("planning session"));
        assert!(!has_non_code_label("fixed the bug"));
        assert!(!has_non_code_label("refactored the code"));
    }

    #[test]
    fn test_has_non_code_label_no_false_positives_on_substrings() {
        // Words that *contain* a label as a substring but are NOT the label itself.
        // These must NOT match because the label is not a whole word.
        assert!(!has_non_code_label("i reviewed the code and fixed the bug")); // "reviewed" ≠ "review"
        assert!(!has_non_code_label("reviewed the implementation")); // "reviewed" ≠ "review"
        assert!(!has_non_code_label("redesign of the module")); // "redesign" ≠ "design"
        assert!(!has_non_code_label("designer wrote a component")); // "designer" ≠ "design"
        assert!(!has_non_code_label("analyzing patterns in code")); // "analyzing" ≠ "analysis"
        assert!(!has_non_code_label("investigations into root cause")); // "investigations" ≠ "investigation"

        // Whole-word "docs" must still match
        assert!(has_non_code_label("wrote docs for the feature"));
        // Whole-word "review" must still match
        assert!(has_non_code_label("code review complete"));
        // Whole-word "design" must still match (it IS a standalone word here)
        assert!(has_non_code_label("design phase finished"));
        // Whole-word at start/end of string
        assert!(has_non_code_label("research"));
        assert!(has_non_code_label("audit"));
    }

    #[test]
    fn test_is_non_code_task_no_false_positive_reviewed() {
        // Agent says "I reviewed the code and fixed the bug" — this is code work, not a review task.
        // "reviewed" contains "review" as a substring but is NOT a whole-word match.
        let resp = AgentResponse {
            status: "done".to_string(),
            summary: "I reviewed the code and fixed the bug".to_string(),
            accomplished: vec!["Patched the authentication module".to_string()],
            remaining: vec![],
            files: vec!["src/auth.rs".to_string()],
            error: None,
            learnings: vec![],
            delegations: vec![],
            input_tokens: None,
            output_tokens: None,
        };
        assert!(!is_non_code_task(&resp));
    }

    #[test]
    fn test_is_non_code_task_no_false_positive_analyzed() {
        // "analyzed" contains "analysis" — wait, no: "analyzed" does NOT contain "analysis".
        // But "analyzing" does not contain "analysis" either. This test covers "redesign"/"design".
        let resp = AgentResponse {
            status: "done".to_string(),
            summary: "Redesigned the module's internal state machine".to_string(),
            accomplished: vec!["Implemented state machine in state.rs".to_string()],
            remaining: vec![],
            files: vec!["src/state.rs".to_string()],
            error: None,
            learnings: vec![],
            delegations: vec![],
            input_tokens: None,
            output_tokens: None,
        };
        assert!(!is_non_code_task(&resp));
    }

    #[test]
    fn test_is_non_code_task_with_delegations() {
        let resp = AgentResponse {
            status: "done".to_string(),
            summary: "Delegated research tasks".to_string(),
            accomplished: vec!["Analyzed requirements".to_string()],
            remaining: vec![],
            files: vec![],
            error: None,
            learnings: vec![],
            delegations: vec![Delegation {
                title: "Research architecture".to_string(),
                body: "Investigate and document the new architecture".to_string(),
                labels: vec![],
            }],
            input_tokens: None,
            output_tokens: None,
        };
        assert!(is_non_code_task(&resp));
    }

    #[test]
    fn test_is_non_code_task_with_accomplished() {
        let resp = AgentResponse {
            status: "done".to_string(),
            summary: "Analysis complete".to_string(),
            accomplished: vec!["Completed documentation review".to_string()],
            remaining: vec![],
            files: vec![],
            error: None,
            learnings: vec![],
            delegations: vec![],
            input_tokens: None,
            output_tokens: None,
        };
        assert!(is_non_code_task(&resp));
    }

    #[test]
    fn test_is_non_code_task_code_task() {
        let resp = AgentResponse {
            status: "done".to_string(),
            summary: "Fixed authentication bug".to_string(),
            accomplished: vec!["Patched auth.rs".to_string()],
            remaining: vec![],
            files: vec!["src/auth.rs".to_string()],
            error: None,
            learnings: vec![],
            delegations: vec![],
            input_tokens: None,
            output_tokens: None,
        };
        assert!(!is_non_code_task(&resp));
    }
}
