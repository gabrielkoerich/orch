//! Success-path response handler — commit, push, PR, token storage, budget check.
//!
//! Extracted from `runner/mod.rs`. Handles the `Ok(parsed)` arm of the parse
//! result, including git operations, delegation storage, and budget enforcement.
//! Also owns `write_result_json` and the `safe_utf8_tail` utility.

use crate::config;
use crate::sidecar;
use std::path::Path;

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
/// Returns `Ok(true)` if the token budget was exceeded and `run()` should return
/// early (without cleanup or metrics). Returns `Ok(false)` to continue normally.
pub async fn handle_success(
    task_id: &str,
    parsed: agents::ParsedResponse,
    wt: &worktree::WorktreeSetup,
    task_title: &str,
    agent_name: &str,
    model_name: Option<&str>,
    new_attempts: u32,
) -> anyhow::Result<bool> {
    let resp = parsed.response;
    tracing::info!(
        task_id,
        status = resp.status,
        summary = resp.summary,
        "agent completed successfully"
    );

    // Auto-commit, push, create PR
    let mut has_pr = false;
    if resp.status == "done" || resp.status == "in_progress" {
        if let Err(e) =
            git_ops::auto_commit(&wt.work_dir, task_id, task_title, agent_name, new_attempts).await
        {
            tracing::error!(task_id, error = ?e, "auto commit failed");
            sidecar::set(task_id, &[format!("last_error=auto commit failed: {e}")])?;
        }

        // Push
        let push_ok = match git_ops::push_branch(&wt.work_dir, &wt.branch, &wt.default_branch).await
        {
            Ok(_) => true,
            Err(e) => {
                tracing::error!(task_id, error = ?e, "push failed");
                sidecar::set(task_id, &[format!("last_error=push failed: {e}")])?;
                false
            }
        };

        // Create PR (skip if push failed)
        if !push_ok {
            tracing::warn!(task_id, "skipping PR creation due to push failure");
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
            )
            .await
            {
                Ok(Some(_url)) => has_pr = true,
                Ok(None) => {
                    // PR already existed
                    has_pr = true;
                }
                Err(e) => {
                    tracing::error!(task_id, error = ?e, "create PR failed");
                    sidecar::set(task_id, &[format!("last_error=create PR failed: {e}")])?;
                }
            }
        }
    }

    // Store delegations in sidecar if present (processed by run_with_context)
    if !resp.delegations.is_empty() {
        if let Ok(delegations_json) = serde_json::to_string(&resp.delegations) {
            sidecar::set(task_id, &[format!("delegations={delegations_json}")])?;
        }
    }

    // Store result in sidecar
    // If agent said "done" and a PR exists, send to review before merge.
    // If agent said "done", no PR, and no delegations — work is complete
    // (e.g., review/analysis jobs that create issues but no code changes).
    // If agent said "done", no PR, but has delegations — blocked on children.
    let has_delegations = !resp.delegations.is_empty();
    let final_status = if resp.status == "done" && has_pr {
        "needs_review"
    } else if resp.status == "done" && !has_pr && has_delegations {
        tracing::info!(
            task_id,
            "agent reported done with delegations but no PR — setting blocked"
        );
        "blocked"
    } else if resp.status == "done" && !has_pr {
        tracing::info!(
            task_id,
            "agent reported done with no PR and no delegations — marking done"
        );
        "done"
    } else {
        &resp.status
    };
    sidecar::set(
        task_id,
        &[
            format!("status={final_status}"),
            format!("summary={}", resp.summary),
        ],
    )?;

    // Store token usage — prefer agent-parsed tokens, fall back to response
    let input_tokens = parsed.input_tokens.or(resp.input_tokens);
    let output_tokens = parsed.output_tokens.or(resp.output_tokens);
    if let (Some(input), Some(output)) = (input_tokens, output_tokens) {
        let model = model_name.unwrap_or("haiku");
        if let Err(e) = sidecar::store_token_usage(task_id, input, output, model) {
            tracing::warn!(task_id, ?e, "failed to store token usage");
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
    );

    // Check token budget with warning thresholds
    let max_tokens: u64 = config::get("max_tokens_per_task")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100_000);

    let total_tokens = sidecar::get_total_tokens(task_id);
    let cost = sidecar::get_cost_estimate(task_id);
    let warning_threshold = (max_tokens as f64 * 0.8) as u64;

    if total_tokens > max_tokens {
        tracing::warn!(task_id, total_tokens, max_tokens, "exceeded token budget");
        // Only override to needs_review if there's a PR to review;
        // otherwise keep the already-computed final_status (e.g. "done" for
        // read-only tasks with no code changes).
        let budget_status = if has_pr { "needs_review" } else { final_status };
        sidecar::set(
            task_id,
            &[
                format!("status={budget_status}"),
                format!(
                    "last_error=token budget exceeded: {}/{} tokens (${:.4})",
                    total_tokens, max_tokens, cost.total_cost_usd
                ),
                "budget_exceeded=true".to_string(),
            ],
        )?;
        return Ok(true); // signal early return to caller
    } else if total_tokens > warning_threshold {
        let pct = (total_tokens as f64 / max_tokens as f64 * 100.0) as u32;
        tracing::warn!(
            task_id,
            total_tokens,
            max_tokens,
            pct,
            "approaching token budget"
        );
        sidecar::set(
            task_id,
            &[format!(
                "budget_warning={}% of budget used ({}/{} tokens, ${:.4})",
                pct, total_tokens, max_tokens, cost.total_cost_usd
            )],
        )?;
    }

    // Note: done → in_review transition is handled by the engine
    // after triggering the review agent (engine/mod.rs)
    Ok(false)
}
