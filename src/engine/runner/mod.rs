//! Task runner — executes tasks using AI agents.
//!
//! Replaces `run_task.sh` with pure Rust. The runner:
//! 1. Sets up a git worktree for isolation
//! 2. Builds context (project instructions, comments)
//! 3. Renders prompts and spawns the agent in tmux
//! 4. Waits for completion, collects and classifies the response
//! 5. Handles errors (reroute on limits, fallback agents)
//! 6. Auto-commits, pushes, and creates PRs
//! 7. Updates status labels and posts result comments
//!
//! Submodules handle the heavy lifting:
//! - [`task_init`] — guard checks, worktree setup, and invocation building
//! - [`session`] — tmux session lifecycle and output collection
//! - [`response_handler`] — success path: commit, push, PR, token storage
//! - [`fallback`] — error classification and recovery strategies

pub mod agent;
pub mod context;
pub mod fallback;
pub mod ndjson;
#[cfg(test)]
mod recorded_fixtures;
pub mod response;
pub mod response_handler;
pub mod sandbox;
pub mod session;
pub mod task_init;

use crate::backends::{ExternalBackend, ExternalTask, Status};
use crate::config;
use crate::cooldown;
use crate::engine;
use crate::engine::agents;
use crate::engine::git::cleanup;
use crate::engine::git::git_ops;
use crate::engine::router::RouteResult;
use crate::engine::tasks;
use crate::engine::tasks::is_internal_id;
#[cfg(not(test))]
use crate::github::http;
use crate::github::http::GhHttp;
use crate::home;
use crate::parser;
use crate::parser::AgentResponse;
use crate::parser::Delegation;
use crate::status_heuristics;
use crate::store;
use crate::store::InsertTaskMetric;
use crate::store::RunTokenUsage;
use crate::store::TaskStatus;
use crate::store::TaskStore;
use crate::tmux::TmuxManager;
use anyhow::Context;
use chrono::Utc;
pub use response::WeightSignal;
use serde_json::Value;
use std::collections::HashSet;
use std::env;
use std::error;
use std::fmt;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::path::PathBuf;
use std::slice;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tokio::task;

/// Fully materialized task-run audit data.
#[derive(Debug, Clone)]
pub struct RunAudit {
    pub stdout: String,
    pub stderr: String,
    pub parsed_response: String,
    pub outcome: String,
    pub error: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_cost_usd: f64,
    pub duration_secs: f64,
}

/// Result returned by `run()` so callers can persist the audit trail.
#[derive(Debug, Clone)]
pub struct RunExecution {
    pub status: String,
    pub exit_code: Option<i32>,
    pub audit: RunAudit,
}

fn serialize_parsed_response(
    parse_result: &Result<agents::ParsedResponse, agents::AgentError>,
    raw_stdout: &str,
) -> String {
    match parse_result {
        Ok(parsed) => serde_json::json!({
            "response": parsed.response,
            "input_tokens": parsed.input_tokens,
            "output_tokens": parsed.output_tokens,
            "cost_usd": parsed.cost_usd,
            "duration_ms": parsed.duration_ms,
        })
        .to_string(),
        Err(agents::AgentError::InvalidResponse { raw }) => {
            serde_json::json!({"raw": raw}).to_string()
        }
        Err(_) => serde_json::json!({"raw": raw_stdout}).to_string(),
    }
}

fn classify_run_outcome(
    status: &str,
    parse_result: &Result<agents::ParsedResponse, agents::AgentError>,
    push_failed: bool,
) -> &'static str {
    if push_failed {
        return "push_failed";
    }
    match parse_result {
        Err(agents::AgentError::Timeout { .. }) => "timeout",
        Err(agents::AgentError::Interrupted { .. }) => "interrupted",
        // Generic rate-limit patterns (e.g. "usage limit") also match billing-cycle
        // exhaustion messages, so consult the credit-exhaustion detector first —
        // otherwise Kimi-style "usage limit for this billing cycle" errors are
        // stored as a plain "rate_limit" outcome (issue #3529).
        Err(agents::AgentError::RateLimit { message }) => {
            cooldown::detect_credit_exhaustion_outcome(message).unwrap_or("rate_limit")
        }
        Err(agents::AgentError::Auth { message }) => {
            cooldown::detect_credit_exhaustion_outcome(message).unwrap_or("failed")
        }
        Err(agents::AgentError::InvalidResponse { .. }) => "parse_error",
        Ok(_)
            if matches!(
                status,
                "done" | "completed" | "in_progress" | "in_review" | "needs_review" | "routed"
            ) =>
        {
            "success"
        }
        Ok(_) if status == "blocked" => "blocked",
        // Non-canonical status that clearly reads as completion — success, not
        // "unrecognized status" (#3627).
        Ok(_) if status_heuristics::status_looks_like_descriptive_completion(status) => "success",
        Err(_) | Ok(_) => "failed",
    }
}

fn classify_run_error_type(last_error: &str) -> &'static str {
    if last_error.is_empty() {
        // No error: agent completed successfully and created a PR waiting for review
        "success"
    } else if last_error.contains("timeout") {
        "timeout"
    } else if last_error.contains("interrupted") {
        "interrupted"
    } else if last_error.contains("billing cycle") || last_error.contains("monthly quota") {
        "billing_cycle_exhausted"
    } else if last_error.contains("rate limit") || last_error.contains("usage limit") {
        "rate_limit"
    } else if last_error.contains("auth") {
        "auth_error"
    } else if last_error.contains("push failed") {
        "push_failed"
    } else if last_error.contains("No commits between") || last_error.contains("no commits") {
        "no_commits"
    } else if last_error.contains("create PR failed")
        || last_error.contains("failed to create pull request")
        || last_error.contains("pull request creation failed")
    {
        "pr_failed"
    } else if last_error.contains("invalid response") || last_error.contains("parse error") {
        "parse_error"
    } else if last_error.contains("exceeded max attempts") {
        "max_attempts"
    } else {
        let lower = last_error.to_ascii_lowercase();
        if lower.contains("build failed")
            || lower.contains("tests failed")
            || lower.contains("test failed")
            || lower.contains("test suite failed")
            || lower.contains("lint failed")
        {
            "ci_failure"
        } else {
            "failed"
        }
    }
}

fn extract_run_tokens(
    parse_result: &Result<agents::ParsedResponse, agents::AgentError>,
) -> (u64, u64) {
    match parse_result {
        Ok(parsed) => (
            parsed
                .input_tokens
                .or(parsed.response.input_tokens)
                .unwrap_or(0),
            parsed
                .output_tokens
                .or(parsed.response.output_tokens)
                .unwrap_or(0),
        ),
        Err(_) => (0, 0),
    }
}

#[expect(clippy::cast_precision_loss, reason = "a run duration in seconds")]
fn run_duration_secs(started_at: &chrono::DateTime<Utc>) -> f64 {
    (Utc::now() - *started_at).num_milliseconds() as f64 / 1000.0
}

struct RunAuditInput<'a> {
    task_id: &'a str,
    status: &'a str,
    parse_result: &'a Result<agents::ParsedResponse, agents::AgentError>,
    raw_stdout: &'a str,
    raw_stderr: &'a str,
    started_at: &'a chrono::DateTime<Utc>,
    error_override: Option<String>,
    elapsed_secs: Option<u64>,
    push_failed: bool,
}

fn parse_success_output(
    task_id: &str,
    agent_name: &str,
    agent_runner: &dyn agents::AgentRunner,
    raw_stdout: &str,
) -> Result<agents::ParsedResponse, agents::AgentError> {
    // The last result envelope is the agent's final answer and carries the run's tokens and cost.
    // Claude writes terminal_reason:completed on every run, so this goes before that shortcut
    let agent_result = agents::find_agent_result(agent_name, raw_stdout);
    if let Some(result) = &agent_result
        && !result.is_error
        && let Ok(response) = parser::parse(&result.result_text)
    {
        return Ok(agents::ParsedResponse {
            response,
            input_tokens: result.input_tokens,
            output_tokens: result.output_tokens,
            cost_usd: result.cost_usd,
            duration_ms: result.duration_ms,
        });
    }

    // Quick NDJSON terminal_reason:completed shortcut: when the agent
    // emitted a session envelope that explicitly signals completion we
    // prefer to interpret that as a successful run (or at least a
    // soft/empty success) rather than immediately classifying an error
    // from the trailing process output.  Do not override explicit
    // in-envelope `is_error: true` signals.
    if raw_stdout.contains("\"terminal_reason\":\"completed\"")
        && !raw_stdout.contains("\"is_error\":true")
    {
        // Try to recover a structured response from assistant messages first
        let assistant_text = agents::collect_assistant_messages_text(agent_name, raw_stdout);
        if !assistant_text.is_empty() {
            if let Ok(response) = parser::parse(&assistant_text) {
                tracing::debug!(
                    task_id,
                    agent = agent_name,
                    "found AgentResponse JSON in earlier assistant message (terminal_reason:completed)"
                );
                return Ok(agents::ParsedResponse {
                    response,
                    input_tokens: None,
                    output_tokens: None,
                    cost_usd: None,
                    duration_ms: None,
                });
            }
            if let Some(response) = agents::synthesize_response_from_text(&assistant_text) {
                tracing::warn!(
                    task_id,
                    agent = agent_name,
                    "synthesizing response from assistant message due to terminal_reason:completed"
                );
                return Ok(agents::ParsedResponse {
                    response,
                    input_tokens: None,
                    output_tokens: None,
                    cost_usd: None,
                    duration_ms: None,
                });
            }
        }
        // If we couldn't recover structured data from earlier assistant
        // messages, fall through and let the regular NDJSON envelope parsing
        // / agent-specific parsers handle the terminal_reason case. This
        // preserves the existing behavior where an empty `result` field
        // combined with exit != 0 becomes an InvalidResponse that is then
        // interpreted by parse_session_output (see tests).
    }

    // 1. Try to use the unified agent result extractor first
    if let Some(agent_result) = agent_result {
        // Treat is_error as authoritative for the success path
        if agent_result.is_error {
            // Classify the error from the agent's result text rather than
            // blindly returning AgentFailed — this preserves rate-limit, auth,
            // and other actionable error types for proper cooldown/reroute.
            // Without this, e.g. a claude session-limit error with is_error=true
            // would be downgraded to AgentFailed, losing the RateLimit signal
            // that the fallback chain needs for UsageLimit recovery (issue #3292).
            if let Some(e) = agents::patterns::detect_rate_limit(&agent_result.result_text) {
                return Err(e);
            }
            if let Some(e) = agents::patterns::detect_auth_error(&agent_result.result_text) {
                return Err(e);
            }
            if let Some(e) = agents::patterns::detect_model_unavailable(&agent_result.result_text) {
                return Err(e);
            }
            // Grok puts "the wait for background work ran out" in errors[] and
            // exits 1. That is a timeout, so handle_failover retries
            let classified = agent_runner.classify_error(1, raw_stdout, "");
            if matches!(classified, agents::AgentError::Timeout { .. }) {
                return Err(classified);
            }
            return Err(agents::AgentError::AgentFailed {
                message: agent_result.result_text,
            });
        }

        // Strategy 2: scan all earlier assistant messages in the NDJSON
        // stream for valid AgentResponse JSON. Agents often emit the
        // JSON in an intermediate turn and then summarise in prose as
        // their final output, so the `type:result` text may be pure
        // narrative while a prior assistant turn contains the JSON.
        let assistant_text = agents::collect_assistant_messages_text(agent_name, raw_stdout);
        if !assistant_text.is_empty()
            && let Ok(response) = parser::parse(&assistant_text)
        {
            tracing::debug!(
                task_id,
                agent = agent_name,
                "found AgentResponse JSON in earlier assistant message"
            );
            return Ok(agents::ParsedResponse {
                response,
                input_tokens: agent_result.input_tokens,
                output_tokens: agent_result.output_tokens,
                cost_usd: agent_result.cost_usd,
                duration_ms: agent_result.duration_ms,
            });
        }

        // Only fall back to text synthesis when parsing fails
        if let Some(response) = agents::synthesize_response_from_text(&agent_result.result_text) {
            tracing::warn!(
                task_id,
                agent = agent_name,
                "parse failed on agent result, synthesizing response from plain text"
            );
            return Ok(agents::ParsedResponse {
                response,
                input_tokens: agent_result.input_tokens,
                output_tokens: agent_result.output_tokens,
                cost_usd: agent_result.cost_usd,
                duration_ms: agent_result.duration_ms,
            });
        }

        // NDJSON envelope fallback: the envelope explicitly reports
        // success (is_error=false) but the result content is domain
        // JSON or plain prose that doesn't match AgentResponse schema
        // and doesn't contain completion keywords.  Rather than
        // returning InvalidResponse (which causes the task to re-run),
        // synthesize a minimal "done" response from the result text.
        //
        // Guard against kimi/minimax edge case: when terminal_reason=completed
        // but result text is empty, synthesize returns None, parse_success_output
        // returns InvalidResponse, and parse_session_output can intercept it
        // before classify_error. Requiring non-empty result here means the
        // envelope fallback only fires when we have actual text to report.
        if !agent_result.is_error
            && result_envelope_completed(raw_stdout)
            && !agent_result.result_text.is_empty()
        {
            let summary_end = agents::truncate_at_char_boundary(&agent_result.result_text, 500);
            let summary = agent_result.result_text[..summary_end].to_string();
            tracing::warn!(
                task_id,
                agent = agent_name,
                "NDJSON envelope indicates success but result lacks AgentResponse \
                 schema — synthesizing done response"
            );
            return Ok(agents::ParsedResponse {
                response: AgentResponse {
                    status: "done".to_string(),
                    summary,
                    ..Default::default()
                },
                input_tokens: agent_result.input_tokens,
                output_tokens: agent_result.output_tokens,
                cost_usd: agent_result.cost_usd,
                duration_ms: agent_result.duration_ms,
            });
        }
    }

    // 2. Fall back to generic agent-specific parser if extraction failed
    match agent_runner.parse_response(raw_stdout) {
        Ok(parsed) => Ok(parsed),
        Err(agents::AgentError::InvalidResponse { raw }) => {
            if let Some(response) = agents::synthesize_response_from_text(&raw) {
                tracing::warn!(
                    task_id,
                    agent = agent_name,
                    "parse failed, synthesizing response from plain text"
                );
                // Try to extract token metadata from the original raw NDJSON
                // output (not the truncated/extracted text from the error), so
                // cost tracking isn't lost for agents that return free-form text.
                let agent_result = agents::find_agent_result(agent_name, raw_stdout);
                let input_tokens = agent_result.as_ref().and_then(|r| r.input_tokens);
                let output_tokens = agent_result.as_ref().and_then(|r| r.output_tokens);
                let cost_usd = agent_result.as_ref().and_then(|r| r.cost_usd);
                let duration_ms = agent_result.as_ref().and_then(|r| r.duration_ms);
                Ok(agents::ParsedResponse {
                    response,
                    input_tokens,
                    output_tokens,
                    cost_usd,
                    duration_ms,
                })
            } else {
                Err(agents::AgentError::InvalidResponse { raw })
            }
        }
        Err(err) => Err(err),
    }
}

/// Whether the last `type: result` line reports `subtype: success` or
/// `terminal_reason: completed`. Grok omits `terminal_reason`.
fn result_envelope_line(raw_stdout: &str) -> Option<serde_json::Value> {
    raw_stdout
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .find(|v| v.get("type").and_then(|t| t.as_str()) == Some("result"))
}

fn has_result_envelope(raw_stdout: &str) -> bool {
    result_envelope_line(raw_stdout).is_some()
}

fn result_envelope_completed(raw_stdout: &str) -> bool {
    result_envelope_line(raw_stdout).is_some_and(|v| {
        v.get("subtype").and_then(|t| t.as_str()) == Some("success")
            || v.get("terminal_reason").and_then(|t| t.as_str()) == Some("completed")
    })
}

// A timeout kill has no result envelope. Salvage only the wrap-up shape: a successful
// `git commit`, then only shell calls that all returned ok. Returns the last assistant text
fn salvage_wrapup(raw: &str) -> Option<String> {
    // (tool name, command, finished ok) in call order
    let mut calls: Vec<(String, String, Option<bool>)> = Vec::new();
    let mut last_text = String::new();
    for v in raw
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
    {
        let blocks = v.pointer("/message/content").and_then(|c| c.as_array());
        for b in blocks.into_iter().flatten() {
            match b.get("type").and_then(|t| t.as_str()) {
                Some("tool_use") => calls.push((
                    b["name"].as_str().unwrap_or("").to_string(),
                    b.pointer("/input/command")
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string(),
                    None,
                )),
                Some("text") if v["type"] == "assistant" => {
                    last_text = b["text"].as_str().unwrap_or("").to_string();
                }
                Some("tool_result") => {
                    let ok = b.get("is_error").and_then(serde_json::Value::as_bool) != Some(true);
                    // results arrive in call order, fill the first unanswered call
                    if let Some(c) = calls.iter_mut().find(|c| c.2.is_none()) {
                        c.2 = Some(ok);
                    }
                }
                _ => {}
            }
        }
    }
    let commit = calls
        .iter()
        .rposition(|(_, cmd, ok)| cmd.contains("git commit") && *ok == Some(true))?;
    let tail_ok = calls[commit..].iter().all(|(name, _, ok)| {
        matches!(name.as_str(), "Bash" | "run_terminal_cmd") && *ok == Some(true)
    });
    let lower = last_text.to_lowercase();
    if !tail_ok || lower.contains("still running") || lower.contains("in progress") {
        return None;
    }
    Some(last_text.chars().take(500).collect())
}

/**
 * Tries `parse_success_output` first when stdout is non-empty. If that succeeds,
 * returns success regardless of exit code. If it fails and `exit_code != 0`,
 * checks for `"terminal_reason":"completed"` or cost-telemetry-only stdout
 * (`"costUSD":`) before falling back to `classify_error`, so valid sessions that
 * exit 1 produce `InvalidResponse` (soft error, re-route) instead of a garbled
 * `Unknown`/`Auth` error (hard failure with spurious cooldown)
 **/
pub fn parse_session_output(
    task_id: &str,
    agent_name: &str,
    agent_runner: &dyn agents::AgentRunner,
    session_output: &session::SessionOutput,
) -> Result<agents::ParsedResponse, agents::AgentError> {
    // Some agents may emit NDJSON/telemetry to stderr instead of stdout. Combine
    // both streams so parse_success_output can inspect either place for the
    // terminal_reason/cost telemetry envelope. Only skip parsing when both are
    // empty so we still attempt recovery when stderr contains the envelope.
    let combined_output = format!("{}{}", session_output.raw_stdout, session_output.raw_stderr);
    if !(session_output.raw_stdout.is_empty() && session_output.raw_stderr.is_empty()) {
        match parse_success_output(task_id, agent_name, agent_runner, &combined_output) {
            Ok(parsed) => return Ok(parsed),
            Err(err) => {
                if session_output.exit_code == 124 {
                    if let Some(summary) = salvage_wrapup(&combined_output) {
                        tracing::warn!(
                            task_id,
                            agent = agent_name,
                            "timed out after commit during wrap-up, salvaging as done"
                        );
                        return Ok(agents::ParsedResponse {
                            response: AgentResponse {
                                status: "done".to_string(),
                                summary,
                                ..Default::default()
                            },
                            input_tokens: None,
                            output_tokens: None,
                            cost_usd: None,
                            duration_ms: None,
                        });
                    }
                    // timeout(1) killed the process. A cancelled error envelope is the CLI
                    // reacting to SIGTERM
                    return Err(agent_runner.classify_error_with_elapsed(
                        session_output.exit_code,
                        &session_output.raw_stdout,
                        &session_output.raw_stderr,
                        session_output.elapsed_secs,
                    ));
                }
                // If parse_success_output already returned a classified error
                // (RateLimit, AgentFailed, Timeout, Auth, etc.), propagate it
                // directly — only InvalidResponse (unparseable output) should
                // be intercepted by the terminal_reason:completed / cost-telemetry
                // guard below. Without this, e.g. a claude session-limit error
                // with terminal_reason=completed would be downgraded from RateLimit
                // to InvalidResponse → Failed, losing the correct cooldown/reroute.
                if !matches!(err, agents::AgentError::InvalidResponse { .. }) {
                    let mut err = err;
                    if let agents::AgentError::Timeout { elapsed } = &mut err
                        && let Some(secs) = session_output.elapsed_secs
                    {
                        *elapsed = Duration::from_secs(secs);
                    }
                    return Err(err);
                }
                if session_output.exit_code != 0 {
                    // kimi/minimax/glm sometimes emit valid NDJSON with
                    // "terminal_reason":"completed" or cost-only telemetry
                    // ("costUSD":) but exit 1 due to a post-session hook or
                    // minor CLI issue. Scanning the tail would just grab JSON
                    // metadata, producing a garbled error. Detect either
                    // completion signal and treat as a soft parse error so the
                    // task re-routes instead of hitting a false cooldown from
                    // Unknown or Auth error variants. Use the combined output
                    // (stdout+stderr) so envelopes written to stderr are
                    // observed.
                    let combined = &combined_output;
                    let is_completed_signal =
                        combined.contains("\"terminal_reason\":\"completed\"");
                    // GLM emits a cost-telemetry JSON object (containing
                    // "costUSD":) as its final stdout/stderr line on clean
                    // exit even when the process exits with code 1. If the
                    // only content is such telemetry (no is_error=true in the
                    // envelope), treat it the same as terminal_reason:completed.
                    let is_cost_telemetry_only = combined.contains("\"costUSD\":")
                        && !combined.contains("\"is_error\":true");
                    if is_completed_signal || is_cost_telemetry_only {
                        let raw_tail = agents::patterns::safe_tail(combined, 300);
                        return Err(agents::AgentError::InvalidResponse {
                            raw: raw_tail.to_string(),
                        });
                    }
                    return Err(agent_runner.classify_error_with_elapsed(
                        session_output.exit_code,
                        &session_output.raw_stdout,
                        &session_output.raw_stderr,
                        session_output.elapsed_secs,
                    ));
                }
                if !has_result_envelope(&combined_output) {
                    return Err(agents::AgentError::Interrupted {
                        message: "exited without a result envelope".to_string(),
                    });
                }
                return Err(err);
            }
        }
    }

    if session_output.exit_code != 0 {
        return Err(agent_runner.classify_error_with_elapsed(
            session_output.exit_code,
            &session_output.raw_stdout,
            &session_output.raw_stderr,
            session_output.elapsed_secs,
        ));
    }

    Err(agents::AgentError::Unknown {
        exit_code: session_output.exit_code,
        message: "empty-output-exit0: opencode returned exit 0 with empty stdout".to_string(),
    })
}

/// The repo has no project directory: not in the active config, no legacy
/// `project_dir`, no bare clone. It fails before any agent starts
#[derive(Debug)]
pub(crate) struct NoProjectDir {
    pub(crate) repo: String,
    pub(crate) bare_clone: PathBuf,
}

impl NoProjectDir {
    /// Block reason for a task of this repo
    pub(crate) const BLOCK_REASON: &str = "project is not configured and has no bare clone";
}

impl fmt::Display for NoProjectDir {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "no resolvable project directory for repo {} — not in active config, no bare clone at {}, no legacy project_dir set",
            self.repo,
            self.bare_clone.display()
        )
    }
}

impl error::Error for NoProjectDir {}

/// Task runner configuration.
pub struct TaskRunner {
    /// Repository slug (owner/repo)
    repo: String,
    /// Path to the orch home directory
    orch_home: PathBuf,
    /// Unified task store for run audit trail
    store: Option<Arc<TaskStore>>,
}

impl Debug for TaskRunner {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskRunner")
            .field("repo", &self.repo)
            .field("orch_home", &self.orch_home)
            .finish_non_exhaustive()
    }
}

impl TaskRunner {
    pub fn new(repo: String) -> Self {
        let orch_home = home::orch_home().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "orch_home() failed — falling back to /tmp/.orch");
            PathBuf::from("/tmp").join(".orch")
        });

        Self {
            repo,
            orch_home,
            store: None,
        }
    }

    /// Set the unified task store for run audit trail.
    #[must_use]
    pub fn with_store(mut self, store: Arc<TaskStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// Read a field from the task store.
    async fn get_field(&self, task_id: &str, field: &str) -> Option<String> {
        let task = store::opt_store_get_task(&self.store, &self.repo, task_id).await?;
        match field {
            "route_reason" => Some(task.route_reason),
            "worktree" => Some(task.worktree),
            "attempts" => Some(task.attempts.to_string()),
            "delegations" => serde_json::to_string(&task.delegations).ok(),
            "summary" => Some(task.summary),
            "last_error" => Some(task.last_error),
            "agent" => task.agent,
            _ => None,
        }
    }

    /// Keep a stored fallback on reroute, wipe agent/model when the run's agent is still stored
    pub(crate) async fn clear_agent_on_reroute_unless_fallback(
        &self,
        task_id: &str,
        ran_agent: &str,
    ) {
        let stored = store::opt_store_get_task(&self.store, &self.repo, task_id).await;
        if Self::reroute_keeps_stored_agent(stored.as_ref(), ran_agent) {
            tracing::debug!(
                task_id,
                "stored fallback agent present, skipping agent/model clear"
            );
            return;
        }
        if let Err(e) = store::store_set_result(
            &self.store,
            &self.repo,
            task_id,
            &[
                ("agent", serde_json::json!("")),
                ("model", serde_json::json!("")),
            ],
        )
        .await
        {
            tracing::warn!(task_id, err = %e, "failed to clear agent/model for reroute in store");
        }
    }

    /// True when a fallback is already stored and must survive this reroute
    fn reroute_keeps_stored_agent(stored: Option<&store::Task>, ran_agent: &str) -> bool {
        let Some(task) = stored else {
            return false;
        };
        if !matches!(task.status, TaskStatus::InProgress) {
            return true;
        }
        let stored_agent = task.agent.as_deref().unwrap_or("");
        !stored_agent.is_empty() && stored_agent != ran_agent
    }

    async fn build_run_audit(&self, input: RunAuditInput<'_>) -> RunAudit {
        // Filter empty strings so that a blank last_error does not shadow the
        // actual parse_result error.  Without this, `Option::or(Some(""))` would
        // return `Some("")` and prevent the `unwrap_or_else` fallback from ever
        // running, causing task_runs.error to be written as "" even when the
        // agent returned a classifiable error (fixes issue #2527).
        let last_error = self
            .get_field(input.task_id, "last_error")
            .await
            .filter(|s| !s.is_empty());
        let outcome =
            classify_run_outcome(input.status, input.parse_result, input.push_failed).to_string();

        let error = if outcome == "success" {
            String::new()
        } else {
            // Prefer this run's parsed error over a last_error left by a previous
            // agent's failover. last_error stays the Err/EarlyReturn fallback (#3851)
            let error = input
                .error_override
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or_else(|| match input.parse_result {
                    Ok(parsed) => parsed
                        .response
                        .error
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                    Err(_) => None,
                })
                .or(last_error)
                .unwrap_or_else(|| match input.parse_result {
                    Ok(_) => String::new(),
                    Err(err) => err.to_string(),
                });

            if error.is_empty() {
                if input.status == "blocked" {
                    match input.parse_result {
                        Ok(parsed) if !parsed.response.summary.is_empty() => {
                            parsed.response.summary.clone()
                        }
                        _ => "task blocked without explicit error".to_string(),
                    }
                } else if matches!(
                    input.status,
                    "done"
                        | "completed"
                        | "in_progress"
                        | "in_review"
                        | "needs_review"
                        | "routed"
                        | "new"
                ) {
                    // Canonical non-success statuses have no error message (e.g. "routed").
                    String::new()
                } else {
                    // Non-canonical status with no error message — surface the status
                    // explicitly so operators can identify which statuses need to be
                    // added to the normalization map in parser.rs.
                    format!("unrecognized status: {}", input.status)
                }
            } else {
                error
            }
        };

        let total_cost_usd = input
            .parse_result
            .as_ref()
            .ok()
            .and_then(|p| p.cost_usd)
            .unwrap_or(0.0);

        let (input_tokens, output_tokens) = extract_run_tokens(input.parse_result);

        #[expect(clippy::cast_precision_loss, reason = "a run duration in seconds")]
        let duration_secs = input
            .elapsed_secs
            .map_or_else(|| run_duration_secs(input.started_at), |s| s as f64);

        RunAudit {
            stdout: input.raw_stdout.to_string(),
            stderr: input.raw_stderr.to_string(),
            parsed_response: serialize_parsed_response(input.parse_result, input.raw_stdout),
            outcome,
            error,
            input_tokens,
            output_tokens,
            total_cost_usd,
            duration_secs,
        }
    }

    /// The blocked outcome for a sandbox that cannot start. No agent ran, so none is cooled down
    async fn sandbox_blocked(&self, task_id: &str, reason: String) -> RunExecution {
        tracing::error!(task_id, error = %reason, "blocking task");
        if let Err(e) = store::store_set_result(
            &self.store,
            &self.repo,
            task_id,
            &[
                ("last_error", serde_json::json!(reason)),
                ("block_reason", serde_json::json!(reason)),
            ],
        )
        .await
        {
            tracing::warn!(task_id, err = %e, "failed to write the sandbox block_reason");
        }
        RunExecution {
            status: "blocked".to_string(),
            exit_code: None,
            audit: RunAudit {
                stdout: String::new(),
                stderr: String::new(),
                parsed_response: String::new(),
                outcome: "failed".to_string(),
                error: reason,
                input_tokens: 0,
                output_tokens: 0,
                total_cost_usd: 0.0,
                duration_secs: 0.0,
            },
        }
    }

    /// Run a task through the full execution pipeline.
    ///
    /// Returns `Ok(None)` if the runner guard skipped the task (caller should not
    /// post stale results). Returns `Ok(Some(status))` with the final task status.
    pub async fn run(
        &self,
        task_id: &str,
        agent: Option<&str>,
        model: Option<&str>,
        backend: Option<&dyn ExternalBackend>,
        started_at: &chrono::DateTime<Utc>,
    ) -> anyhow::Result<Option<RunExecution>> {
        tracing::info!(
            task_id,
            agent = agent.unwrap_or("default"),
            model = model.unwrap_or("default"),
            "starting task execution"
        );

        // Check task guards; returns outcome indicating whether to proceed.
        let attempts = match task_init::check_guards(task_id, &self.repo, &self.store).await {
            Ok(task_init::GuardOutcome::Proceed(a)) => a,
            Ok(task_init::GuardOutcome::Skip) => {
                return Ok(None);
            }
            Ok(task_init::GuardOutcome::MaxAttempts) => {
                // The caller marks the task blocked from the returned status, a task that ran out
                // of attempts did not finish, so it must never reach done
                if backend.is_some() {
                    // If this was label-forced to a specific agent, remove the agent label
                    // so that /retry can route to a different agent instead of looping.
                    let route_reason = self
                        .get_field(task_id, "route_reason")
                        .await
                        .unwrap_or_default();
                    if route_reason.starts_with("label agent:") {
                        let agent_label = route_reason.trim_start_matches("label ");
                        let gh = GhHttp::new()?;
                        match gh.remove_label(&self.repo, task_id, agent_label).await {
                            Err(e) => {
                                tracing::warn!(task_id, label = agent_label, err = %e, "failed to remove agent label after max attempts");
                            }
                            _ => {
                                tracing::info!(
                                    task_id,
                                    label = agent_label,
                                    "removed forced agent label after max attempts — /retry will use free routing"
                                );
                            }
                        }
                    }
                }
                return Ok(Some(RunExecution {
                    status: "blocked".to_string(),
                    exit_code: None,
                    audit: RunAudit {
                        stdout: String::new(),
                        stderr: String::new(),
                        parsed_response: String::new(),
                        outcome: "failed".to_string(),
                        error: "max attempts reached".to_string(),
                        input_tokens: 0,
                        output_tokens: 0,
                        total_cost_usd: 0.0,
                        duration_secs: 0.0,
                    },
                }));
            }
            Err(e) => return Err(e),
        };

        // Resolve project directory
        let project_dir = self.resolve_project_dir().await?;

        // Prepare task: worktree, context, prompts, and agent invocation
        let init = task_init::prepare_task(
            task_id,
            agent,
            model,
            backend,
            &self.repo,
            &project_dir,
            attempts,
            &self.store,
        )
        .await?;

        // A sandbox that cannot start blocks the task, which never runs without it. It is the
        // operator's setup that fails, so no agent or model is cooled down
        if init.invocation.sandbox
            && let Err(e) = sandbox::argv(
                &init.invocation.sandbox_scope(),
                &env::temp_dir().join("orch-sandbox-preflight"),
            )
            .await
        {
            return Ok(Some(self.sandbox_blocked(task_id, e.to_string()).await));
        }

        // Run agent session: spawn in tmux, wait for completion, collect output
        let (tmux, tmux_session, session_output) = session::run_agent_session(
            task_id,
            &init.invocation,
            &init.attempt_dir,
            &self.orch_home,
        )
        .await;

        // The pre-flight policy passed but the one built at spawn did not: block the same way
        if let Some(reason) = session_output.sandbox_error.clone() {
            return Ok(Some(self.sandbox_blocked(task_id, reason).await));
        }

        // Touch updated_at immediately after session exit so the stuck-task recovery
        // timer resets before response_handler post-processing (git push, PR creation,
        // status update) begins.  Without this, a session that ran longer than
        // no_session_stuck_timeout (default 10 min) would be re-dispatched by the tick
        // while the runner is still writing results.
        store::store_touch_updated_at(&self.store, &self.repo, task_id).await;

        // If silence detection (tick phase1b) already rerouted this task while the
        // session was running, skip all fallback/review processing. Without this check the
        // runner would call `fallback::handle_error` on the empty output, exhaust fallback
        // agents, and mark the task `needs_review` — overwriting the rerouted status and
        // triggering a spurious review cycle.
        // Silence detection now sets Routed (with fallback agent) or NeedsReview (no fallback).
        // Check for both, plus New for backward compatibility.
        if let Some(stored) = store::opt_store_get_task(&self.store, &self.repo, task_id).await {
            let silence_reset = matches!(
                stored.status,
                TaskStatus::New | TaskStatus::Routed | TaskStatus::NeedsReview
            );
            if silence_reset {
                let status_str = stored.status.as_str();
                tracing::info!(
                    task_id,
                    status = status_str,
                    "task already reset by the engine — skipping fallback processing"
                );
                session::cleanup_session(task_id, &tmux, &tmux_session).await;
                let silence_err = agents::patterns::classify_from_text(0, "silence detected");
                let silence_parse: Result<agents::ParsedResponse, agents::AgentError> =
                    Err(silence_err);
                let audit = self
                    .build_run_audit(RunAuditInput {
                        task_id,
                        status: status_str,
                        parse_result: &silence_parse,
                        raw_stdout: &session_output.raw_stdout,
                        raw_stderr: &session_output.raw_stderr,
                        started_at,
                        error_override: Some(format!("task was reset by the engine while the session ran (status {status_str})")),
                        elapsed_secs: session_output.elapsed_secs,
                        push_failed: false,
                    })
                    .await;
                return Ok(Some(RunExecution {
                    status: status_str.to_string(),
                    exit_code: Some(session_output.exit_code),
                    audit,
                }));
            }
        }

        // Log raw output for debugging agent failures
        let stdout_len = session_output.raw_stdout.len();
        let stderr_len = session_output.raw_stderr.len();
        let stdout_tail: String = session_output
            .raw_stdout
            .chars()
            .rev()
            .take(500)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        let stderr_tail: String = session_output
            .raw_stderr
            .chars()
            .rev()
            .take(500)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        tracing::info!(
            task_id,
            exit_code = session_output.exit_code,
            stdout_len,
            stderr_len,
            stdout_tail = %stdout_tail,
            stderr_tail = %stderr_tail,
            "agent raw output"
        );

        // Use agent-specific parsing where possible. Previously we only attempted
        // parse_success_output when exit_code == 0 which caused runs that emitted
        // valid NDJSON but exited non-zero (kimi / minimax) to be classed as
        // failures. Try parsing success output first when stdout is present; if
        // parsing succeeds treat as success regardless of exit code. Only fall
        // back to error classification when parsing fails and the process exit
        // code indicates an error.
        for window in agents::claude::rate_limit_windows(&session_output.raw_stdout) {
            cooldown::record_rate_limit_window(&init.agent_name, &window).await;
        }
        let agent_runner = agents::get_runner(&init.agent_name);
        let parse_result =
            parse_session_output(task_id, &init.agent_name, &*agent_runner, &session_output);

        // Write structured result.json for deterministic testing and debugging
        response_handler::write_result_json(
            &init.attempt_dir,
            task_id,
            &init.agent_name,
            init.model_name.as_deref(),
            session_output.exit_code,
            init.new_attempts,
            &parse_result,
            &session_output.raw_stdout,
            &session_output.raw_stderr,
        )
        .await;

        // Handle outcome: success or error recovery
        // `fallback_error` carries the attributed error message from `handle_error()` when
        // the Continue path is taken.  It is passed as `error_override` to `build_run_audit`
        // so that stale `last_error` values from a previous agent's run are never written
        // into the current run's audit record (fixes misattribution bug: task_run shows
        // agent=kimi but error="minimax timed out...").
        let (final_status, push_failed, fallback_error) = match parse_result {
            Ok(ref parsed) => {
                let (status, push_failed) = response_handler::handle_success(
                    task_id,
                    parsed.clone(),
                    &init.wt,
                    &init.task_title,
                    &init.agent_name,
                    init.model_name.as_deref(),
                    init.new_attempts,
                    &self.repo,
                    &self.store,
                    &session_output.raw_stdout,
                )
                .await?;
                (status, push_failed, None)
            }
            Err(ref agent_err) => {
                match fallback::handle_error(
                    task_id,
                    agent_err,
                    &init.agent_name,
                    &*agent_runner,
                    init.model_name.as_deref(),
                    init.complexity.as_deref(),
                    init.new_attempts,
                    &self.store,
                    &self.repo,
                )
                .await?
                {
                    fallback::ErrorHandleResult::EarlyReturn { status } => {
                        // Task rerouted — clean up tmux session and env secrets
                        session::cleanup_session(task_id, &tmux, &tmux_session).await;
                        // The fallback handler already writes a sanitized `last_error`
                        // into the store for EarlyReturn paths (reroutes / needs_review).
                        // Passing the raw agent_err here would record noisy provider
                        // payloads (e.g. full api_retry NDJSON). Leave error_override
                        // as None so build_run_audit will prefer the store's last_error
                        // (see src/engine/runner/fallback/mod.rs which persists a compact
                        // summary for rate-limit and reroute cases).
                        let audit = self
                            .build_run_audit(RunAuditInput {
                                task_id,
                                status: &status,
                                parse_result: &parse_result,
                                raw_stdout: &session_output.raw_stdout,
                                raw_stderr: &session_output.raw_stderr,
                                started_at,
                                error_override: None,
                                elapsed_secs: session_output.elapsed_secs,
                                push_failed: false,
                            })
                            .await;
                        return Ok(Some(RunExecution {
                            status,
                            exit_code: Some(session_output.exit_code),
                            audit,
                        }));
                    }
                    fallback::ErrorHandleResult::Continue { status, error } => {
                        (status, false, Some(error))
                    }
                }
            }
        };

        // Kill tmux session if still alive
        session::cleanup_session(task_id, &tmux, &tmux_session).await;

        // For no-op done tasks (agent finished with no code changes and no PR),
        // clean up the worktree and branch immediately — tmux session is already
        // dead so the tmux guard won't block removal.
        if final_status.as_str() == "done"
            && let Some(ref st) = self.store
            && let Err(e) = cleanup::cleanup_task_worktree(task_id, &self.repo, st).await
        {
            tracing::warn!(task_id, error = ?e, "worktree cleanup failed after no-op done");
        }

        // Return final status and the session exit code so callers can record it.
        // Use `fallback_error` as error_override when available: it carries the error message
        // attributed to the current agent (set by handle_error), preventing stale last_error
        // values from a prior run from contaminating this run's audit record.
        let audit = self
            .build_run_audit(RunAuditInput {
                task_id,
                status: &final_status,
                parse_result: &parse_result,
                raw_stdout: &session_output.raw_stdout,
                raw_stderr: &session_output.raw_stderr,
                started_at,
                error_override: fallback_error,
                elapsed_secs: session_output.elapsed_secs,
                push_failed,
            })
            .await;

        Ok(Some(RunExecution {
            status: final_status.clone(),
            exit_code: Some(session_output.exit_code),
            audit,
        }))
    }

    /// Record task execution metrics to the database.
    #[expect(
        clippy::too_many_arguments,
        reason = "a metric row needs the agent, model, route, timing and outcome together"
    )]
    async fn record_metrics(
        &self,
        task_id: &str,
        agent_name: &str,
        model_name: Option<&str>,
        route_result: Option<&RouteResult>,
        started_at: &chrono::DateTime<Utc>,
        attempts: u32,
        final_status: &str,
        error_type: Option<&str>,
    ) {
        let completed_at = Utc::now();
        #[expect(clippy::cast_precision_loss, reason = "a run duration in seconds")]
        let duration_seconds = (completed_at - *started_at).num_milliseconds() as f64 / 1000.0;

        // Classify outcome based on the final status and optional error_type/last_error
        let outcome = match final_status {
            "done" | "in_progress" | "in_review" => "success",
            "new" => "rerouted",
            "needs_review" => classify_run_error_type(error_type.unwrap_or("")),
            _ => "unknown",
        };

        let complexity = route_result.map(|r| r.complexity.clone());
        let files_changed = git_ops::count_changed_files(&PathBuf::from(
            self.get_field(task_id, "worktree")
                .await
                .unwrap_or_default(),
        ))
        .await
        .unwrap_or(0);

        if let Some(ref store) = self.store {
            // Only set error_type for non-success outcomes
            let db_error_type: Option<String> = if outcome == "success" {
                None
            } else {
                Some(outcome.to_string())
            };

            let usage = store::get_token_usage(&self.store, &self.repo, task_id).await;
            let total_cost = store::get_reported_cost(&self.store, &self.repo, task_id).await;
            let input_tokens = if usage.input_tokens > 0 {
                Some(i64::try_from(usage.input_tokens).unwrap_or(i64::MAX))
            } else {
                None
            };
            let output_tokens = if usage.output_tokens > 0 {
                Some(i64::try_from(usage.output_tokens).unwrap_or(i64::MAX))
            } else {
                None
            };

            let metric = InsertTaskMetric {
                repo: &self.repo,
                task_id,
                agent: agent_name,
                model: model_name,
                complexity: complexity.as_deref(),
                outcome,
                duration_seconds,
                started_at,
                completed_at: &completed_at,
                attempts: i32::try_from(attempts.saturating_add(1)).unwrap_or(i32::MAX),
                files_changed: i32::try_from(files_changed).unwrap_or(i32::MAX),
                error_type: db_error_type.as_deref(),
                input_tokens,
                output_tokens,
                total_cost_usd: total_cost,
            };

            if let Err(e) = store.insert_task_metric(&metric).await {
                tracing::error!(task_id, ?e, "failed to record task metrics");
            }
        }
    }

    /// Run a task with full engine context (backend, tmux, capture).
    ///
    /// Called by the engine dispatch loop with richer context.
    /// Returns a `WeightSignal` for the engine to feed back to the router.
    pub async fn run_with_context(
        &self,
        task: &ExternalTask,
        backend: &Arc<dyn ExternalBackend>,
        _tmux: &Arc<TmuxManager>,
        route_result: Option<&RouteResult>,
    ) -> anyhow::Result<WeightSignal> {
        let task_id = &task.id.0;
        let agent = route_result.map(|r| r.agent.as_str());
        let agent_name = agent.unwrap_or("claude").to_string();
        let model = route_result.and_then(|r| r.model.as_deref());

        // Record start time for metrics (before any work begins)
        let started_at = Utc::now();

        // Store task info for prompt building (title/body already in store tasks table)

        // Record run start in task_runs audit trail
        let run_audit_id = if let Some(ref store) = self.store {
            let attempt: i32 = self
                .get_field(task_id, "attempts")
                .await
                .and_then(|s| s.parse().ok())
                .unwrap_or(0)
                + 1;
            match store.resolve_task_id(&self.repo, task_id).await {
                Ok(Some(store_id)) => {
                    match store
                        .start_run(&crate::store::StartRun {
                            task_id: store_id,
                            attempt,
                            run_type: "agent",
                            agent: &agent_name,
                            model: model.unwrap_or(""),
                            command: &format!(
                                "{} --model {}",
                                agent_name,
                                model.unwrap_or("default")
                            ),
                            prompt: &task.body,
                        })
                        .await
                    {
                        Ok(run_id) => Some(run_id),
                        Err(e) => {
                            tracing::warn!(
                                task_id,
                                error = %e,
                                "failed to record run start in audit trail"
                            );
                            None
                        }
                    }
                }
                _ => None,
            }
        } else {
            None
        };

        // Run the task.
        // Always finalize the run audit row, even on early-return/error paths.
        let run_result = match self
            .run(task_id, agent, model, Some(&**backend), &started_at)
            .await
        {
            Ok(run_result) => run_result,
            Err(e) => {
                if let Some(run_id) = run_audit_id
                    && let Some(ref store) = self.store
                    && let Err(complete_err) = store
                        .complete_run(&crate::store::CompleteRun {
                            run_id,
                            exit_code: Some(-1),
                            stdout: "",
                            stderr: "",
                            parsed: "",
                            outcome: "failed",
                            error: &e.to_string(),
                            tokens: RunTokenUsage::default(),
                        })
                        .await
                {
                    tracing::warn!(
                        task_id,
                        run_id,
                        error = %complete_err,
                        "failed to record run completion in audit trail after runner error"
                    );
                }
                return Err(e);
            }
        };

        // If the runner guard skipped the task, do not re-post stale data as a new comment.
        let (status, exit_code_opt, run_audit) = if let Some(result) = run_result {
            (result.status, result.exit_code, result.audit)
        } else {
            if let Some(run_id) = run_audit_id
                && let Some(ref store) = self.store
                && let Err(e) = store
                    .complete_run(&crate::store::CompleteRun {
                        run_id,
                        exit_code: Some(-1),
                        stdout: "",
                        stderr: "",
                        parsed: "",
                        outcome: "aborted",
                        error: "run skipped by guard",
                        tokens: RunTokenUsage::default(),
                    })
                    .await
            {
                tracing::warn!(
                    task_id,
                    run_id,
                    error = %e,
                    "failed to record skipped run completion in audit trail"
                );
            }
            tracing::info!(task_id, "guard skipped task — not posting stale result");
            return Ok(WeightSignal::None);
        };

        // Process delegations if the agent requested subtasks
        let delegations_raw = self
            .get_field(task_id, "delegations")
            .await
            .unwrap_or_default();
        if !delegations_raw.is_empty() {
            match serde_json::from_str::<Vec<Delegation>>(&delegations_raw) {
                Ok(delegations) if !delegations.is_empty() => {
                    self.process_delegations(task, &delegations, backend)
                        .await?;
                    if let Err(e) = store::store_set_result(
                        &self.store,
                        &self.repo,
                        task_id,
                        &[("delegations", serde_json::json!([]))],
                    )
                    .await
                    {
                        tracing::warn!(task_id, err = %e, "failed to clear delegations in store");
                    }
                }
                Ok(_) => {} // empty list, nothing to do
                Err(e) => {
                    tracing::error!(task_id, error = %e, "corrupt delegations JSON — clearing");
                    if let Err(e2) = store::store_set_result(
                        &self.store,
                        &self.repo,
                        task_id,
                        &[("delegations", serde_json::json!([]))],
                    )
                    .await
                    {
                        tracing::warn!(task_id, err = %e2, "failed to clear corrupt delegations in store");
                    }
                    if let Err(e2) = store::store_set_result(
                        &self.store,
                        &self.repo,
                        task_id,
                        &[(
                            "last_error",
                            serde_json::json!(format!("delegation parse failed: {e}")),
                        )],
                    )
                    .await
                    {
                        tracing::warn!(task_id, err = %e2, "failed to write delegation_parse_failed last_error to store");
                    }
                }
            }
        }

        // Post result to GitHub
        let summary = self.get_field(task_id, "summary").await.unwrap_or_default();
        let last_error = self
            .get_field(task_id, "last_error")
            .await
            .unwrap_or_default();

        // Determine weight signal based on outcome.
        // Any reroute ("new"/"routed") should avoid review-gate and go back through
        // normal dispatch, regardless of why it was rerouted.
        let is_rerouted = status == "new" || status == "routed";
        if is_rerouted {
            self.clear_agent_on_reroute_unless_fallback(task_id, &agent_name)
                .await;
        }
        // Determine weight signal based on outcome.
        // Reroutes should only emit RateLimited when the underlying error is genuinely
        // a rate limit (not silence detection, timeouts, parse errors, etc.). This prevents
        // false cooldown cascades where non-rate-limit failures are treated as rate limits.
        let is_rate_limit_error = classify_run_error_type(&last_error) == "rate_limit";
        let weight_signal = if is_rerouted {
            if is_rate_limit_error {
                WeightSignal::RateLimited {
                    agent: agent_name.clone(),
                }
            } else {
                // Non-rate-limit reroutes (silence detection, timeout, parse error): signal
                // the engine to keep the task in "new" for re-routing. Using Rerouted (not
                // None) prevents tick from converting the silence-reset back to needs_review,
                // which would trigger the no-code review shortcut and falsely mark it done.
                WeightSignal::Rerouted
            }
        } else if status == "needs_review" {
            // Rate limit errors that escalate to needs_review must NOT reset backoff
            // counters (issue #2153).  These errors indicate the agent is genuinely
            // unavailable — resetting would wipe the cooldown and cause the next dispatch
            // to retry the same rate-limited agent immediately.
            //
            // Detection: classify_run_error_type() confirms "rate limit" in last_error.
            // Reuse the same classifier as `is_rate_limit_error` above rather than a raw
            // substring check — billing-cycle exhaustion messages (e.g. Kimi's "usage
            // limit for this billing cycle") also contain "usage limit"/"rate limit", so
            // a bare substring check would record a rate-limit event into the router's
            // health signal for a condition that isn't a genuine rate limit (issue #3529).
            let signal = needs_review_signal(&last_error, &agent_name);
            match &signal {
                WeightSignal::RateLimited { .. } => {
                    // Record in the metrics store so the router's health check detects it.
                    if let Some(ref store) = self.store
                        && let Err(e) = store
                            .record_rate_limit(&agent_name, "rate", Some(task_id))
                            .await
                    {
                        tracing::warn!(task_id, agent = %agent_name, err = %e, "failed to persist rate_limit event — router health check may miss this signal");
                    }
                    // Also record via the cooldown system so model_for_complexity() skips
                    // this model.
                    cooldown::record_rate_limit(&agent_name, model, &last_error).await;
                }
                WeightSignal::Success { .. } => {
                    // Reset backoff so the next failure starts fresh.
                    cooldown::record_agent_success(&agent_name, model.unwrap_or("")).await;
                }
                _ => {}
            }
            signal
        } else if status == "done" || status == "in_progress" || status == "in_review" {
            // Reset exponential backoff counters so the next failure starts fresh.
            cooldown::record_agent_success(&agent_name, model.unwrap_or("")).await;
            WeightSignal::Success {
                agent: agent_name.clone(),
            }
        } else if status == "blocked" {
            WeightSignal::Blocked
        } else {
            WeightSignal::None
        };

        // Record metrics
        {
            let attempts: u32 = self
                .get_field(task_id, "attempts")
                .await
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            self.record_metrics(
                task_id,
                &agent_name,
                model,
                route_result,
                &started_at,
                attempts,
                &status,
                if last_error.is_empty() {
                    None
                } else {
                    Some(&last_error)
                },
            )
            .await;
        }

        // Complete run in task_runs audit trail (include exit code from runner when available)
        if let Some(run_id) = run_audit_id
            && let Some(ref store) = self.store
            && let Err(e) = store
                .complete_run(&crate::store::CompleteRun {
                    run_id,
                    exit_code: exit_code_opt,
                    stdout: &run_audit.stdout,
                    stderr: &run_audit.stderr,
                    parsed: &run_audit.parsed_response,
                    outcome: &run_audit.outcome,
                    error: &run_audit.error,
                    tokens: RunTokenUsage {
                        input_tokens: run_audit.input_tokens,
                        output_tokens: run_audit.output_tokens,
                        total_cost_usd: run_audit.total_cost_usd,
                        duration_secs: run_audit.duration_secs,
                    },
                })
                .await
        {
            tracing::warn!(
                task_id,
                run_id,
                error = %e,
                "failed to record run completion in audit trail"
            );
        }

        // If task was rerouted (status=routed or new after run), update GitHub agent label
        // so the router doesn't re-route back to the same failed agent.
        if is_rerouted && !is_internal_id(task_id) {
            let new_agent = self.get_field(task_id, "agent").await.unwrap_or_default();
            if !new_agent.is_empty() && new_agent != agent_name {
                update_agent_label_after_reroute(
                    &self.repo,
                    task,
                    backend,
                    &agent_name,
                    &new_agent,
                )
                .await;
            }
        }

        // Internal tasks: skip GitHub status/comment updates — dispatch phase handles status.
        if is_internal_id(task_id) {
            return Ok(weight_signal);
        }

        // Update GitHub status
        let new_status = match status.as_str() {
            "done" => Status::Done,
            "in_progress" => Status::InProgress,
            "in_review" => Status::InReview,
            "blocked" => Status::Blocked,
            "routed" => Status::Routed, // Rerouted (agent/model already set)
            "new" => Status::New,       // Legacy reroute path
            _ => Status::NeedsReview,
        };
        // Store-first: update SQLite (must succeed), then mirror to backend.
        if let Some(ref store) = self.store
            && let Ok(Some(store_id)) = store.resolve_task_id(&self.repo, &task.id.0).await
        {
            store
                .update_status(store_id, tasks::status_to_task_status(new_status))
                .await
                .context("store-first status update failed in runner")?;
        }
        if let Err(e) = backend.update_status(&task.id, new_status).await {
            tracing::warn!(
                task_id,
                ?new_status,
                err = %e,
                "failed to mirror runner status to backend — store is authoritative"
            );
        }

        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
        let mut comment = if !summary.is_empty() {
            format!("[{now}] {status}: {summary}")
        } else if !last_error.is_empty() {
            format!("[{now}] {status}: {last_error}")
        } else {
            format!("[{now}] {status}")
        };

        // Append visible attribution footer (matches PR body style)
        let footer = engine::attribution_footer("Created", &agent_name, model);
        comment.push_str(&footer);

        backend.post_comment(&task.id, &comment).await?;

        Ok(weight_signal)
    }

    /// Process delegations from an agent response.
    ///
    /// Creates child GitHub issues for each delegation and marks the parent
    /// as blocked. The engine's Phase 4 unblock mechanism will re-activate
    /// the parent when all children are done.
    async fn process_delegations(
        &self,
        parent_task: &ExternalTask,
        delegations: &[Delegation],
        backend: &Arc<dyn ExternalBackend>,
    ) -> anyhow::Result<()> {
        let parent_id = &parent_task.id;

        // Fetch existing open tasks once for dedup — prevents creating duplicate
        // GitHub issues when the same delegations are produced on repeated runs.
        let existing_titles: HashSet<String> = match backend.list_all_tasks().await {
            Ok(tasks) => tasks.into_iter().map(|t| t.title).collect(),
            Err(e) => {
                tracing::warn!(err = %e, "failed to fetch tasks for dedup — will create all");
                HashSet::new()
            }
        };

        for delegation in delegations {
            // Skip if an issue with the same title already exists
            if existing_titles.contains(&delegation.title) {
                tracing::info!(
                    parent = parent_id.0,
                    title = delegation.title,
                    "skipping delegation — issue with same title already exists"
                );
                continue;
            }

            // Build labels: status:new + any labels from the delegation
            let mut labels = delegation.labels.clone();
            labels.push("status:new".to_string());

            // Build child body with delegation reference
            let child_body = format!(
                "{}\n\n---\n_Delegated from #{}_{}",
                delegation.body,
                parent_id.0,
                engine::orch_footer()
            );

            match backend
                .create_sub_task(parent_id, &delegation.title, &child_body, &labels)
                .await
            {
                Ok(child_id) => {
                    tracing::info!(
                        parent = parent_id.0,
                        child = child_id.0,
                        title = delegation.title,
                        "created delegated subtask"
                    );
                }
                Err(e) => {
                    tracing::error!(
                        parent = parent_id.0,
                        title = delegation.title,
                        err = %e,
                        "failed to create delegated subtask"
                    );
                }
            }
        }

        // Mark parent as blocked — store-first, then mirror to backend.
        if let Some(ref store) = self.store
            && let Ok(Some(store_id)) = store.resolve_task_id(&self.repo, &parent_id.0).await
        {
            store
                .update_status(store_id, TaskStatus::Blocked)
                .await
                .context("store-first: failed to block parent in store")?;
        }
        if let Err(e) = backend.update_status(parent_id, Status::Blocked).await {
            tracing::warn!(
                parent = parent_id.0,
                err = %e,
                "failed to mirror blocked status to backend — store is authoritative"
            );
        }

        // Post summary comment on parent
        let summary = delegations
            .iter()
            .enumerate()
            .map(|(i, d)| format!("{}. {}", i + 1, d.title))
            .collect::<Vec<_>>()
            .join("\n");

        backend
            .post_comment(
                parent_id,
                &format!(
                    "Delegated {} subtask(s):\n\n{}\n\nParent task is blocked until all subtasks complete.{}",
                    delegations.len(),
                    summary,
                    engine::orch_footer()
                ),
            )
            .await?;

        Ok(())
    }

    /// Resolve the project directory for this repo.
    pub(crate) async fn resolve_project_dir(&self) -> anyhow::Result<PathBuf> {
        // Explicit env var always wins
        if let Ok(dir) = env::var("PROJECT_DIR")
            && !dir.is_empty()
        {
            return Ok(PathBuf::from(dir));
        }

        // Check projects list in global config — find path for this repo
        if let Ok(paths) = config::get_project_paths() {
            for path_str in &paths {
                let path = PathBuf::from(path_str);
                // Check if this project's .orch.yml has matching repo
                if let Ok(repo) = config::get_repo_for_project(&path) {
                    // Use tokio::fs::metadata for async-safe existence check
                    let path_for_check = path.clone();
                    if repo == self.repo && fs::metadata(&path_for_check).await.is_ok() {
                        return Ok(path);
                    }
                }
            }
        }

        // Legacy: check config project_dir
        if let Ok(dir) = config::get("project_dir")
            && !dir.is_empty()
        {
            let path = PathBuf::from(&dir);
            let path_for_check = path.clone();
            if fs::metadata(&path_for_check).await.is_ok() {
                return Ok(path);
            }
        }

        // Check for bare clone
        let parts: Vec<&str> = self.repo.split('/').collect();
        if parts.len() == 2 {
            let bare = self
                .orch_home
                .join("projects")
                .join(parts[0])
                .join(format!("{}.git", parts[1]));
            let bare_for_check = bare.clone();
            if task::spawn_blocking(move || bare_for_check.exists())
                .await
                .unwrap_or(false)
            {
                return Ok(bare);
            }
        }

        // No resolvable project directory: not in active config, no legacy
        // project_dir, no bare clone. Falling back to the service's own CWD
        // here would silently point worktree creation at an unrelated
        // directory (see issue #3416) — surface a clear error instead.
        Err(NoProjectDir {
            repo: self.repo.clone(),
            bare_clone: self
                .orch_home
                .join("projects")
                .join(self.repo.split('/').next().unwrap_or_default())
                .join(format!(
                    "{}.git",
                    self.repo.split('/').nth(1).unwrap_or_default()
                )),
        }
        .into())
    }
}

/// Update the GitHub agent label on an external task when it is rerouted to a new agent.
///
/// Removes the old `agent:<old_agent>` label, ensures the new `agent:<new_agent>` label exists
/// in the repo, then applies it. Returns `true` only when both `ensure_label` (repo label creation)
/// and `set_labels` (issue label assignment) succeed. `remove_label` failures are non-fatal.
async fn update_agent_label_after_reroute(
    #[cfg_attr(
        test,
        expect(unused_variables, reason = "only the label call outside tests uses it")
    )]
    repo: &str,
    task: &ExternalTask,
    backend: &Arc<dyn ExternalBackend>,
    old_agent: &str,
    new_agent: &str,
) -> bool {
    let old_label = format!("agent:{old_agent}");
    if let Err(e) = backend.remove_label(&task.id, &old_label).await {
        tracing::warn!(
            task_id = ?task.id,
            label = old_label,
            error = %e,
            "failed to remove old agent label during re-route"
        );
    }
    let new_label = format!("agent:{new_agent}");

    // ensure_label creates the repo-level label if missing (best-effort).
    // Skipped under #[cfg(test)] to avoid real GitHub API calls.
    #[cfg(not(test))]
    {
        match GhHttp::new() {
            Ok(gh) => {
                if gh
                    .ensure_label(
                        repo,
                        &new_label,
                        http::status_label_color(&new_label),
                        &format!("Agent: {new_agent}"),
                    )
                    .await
                    .is_err()
                {
                    tracing::warn!(
                        task_id = ?task.id,
                        label = %new_label,
                        "ensure_label failed during agent label update after failover"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    task_id = ?task.id,
                    err = %e,
                    "GhHttp::new() failed — skipping ensure_label"
                );
            }
        }
    }

    if let Err(e) = backend
        .set_labels(&task.id, slice::from_ref(&new_label))
        .await
    {
        tracing::warn!(
            task_id = ?task.id,
            label = %new_label,
            error = %e,
            "set_labels failed during agent label update after failover"
        );
        return false;
    }
    true
}

/// Weight signal for a run that ended in `needs_review`. `prepare_task` clears `last_error` at run
/// start, so a non-empty value means `handle_error` ended this run
fn needs_review_signal(last_error: &str, agent: &str) -> WeightSignal {
    let agent = agent.to_string();
    if classify_run_error_type(last_error) == "rate_limit" {
        WeightSignal::RateLimited { agent }
    } else if !last_error.is_empty() {
        WeightSignal::Failed { agent }
    } else {
        WeightSignal::Success { agent }
    }
}

#[cfg(test)]
mod tests;
