//! Task runner — executes tasks using AI agents.
//!
//! Replaces `run_task.sh` with pure Rust. The runner:
//! 1. Sets up a git worktree for isolation
//! 2. Builds context (project instructions, skills, comments)
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
pub mod agents;
pub mod context;
pub mod diff;
pub mod direct;
pub mod fallback;
pub mod git_ops;
pub mod response;
pub mod response_handler;
pub mod session;
pub mod task_init;
pub mod worktree;

use crate::backends::{ExternalBackend, ExternalTask, Status};
use crate::config;
use crate::engine::router::RouteResult;
use crate::engine::tasks::is_internal_id;
use crate::security;
use crate::store;
use crate::store::InsertTaskMetric;
use crate::tmux::TmuxManager;
use anyhow::Context;
use chrono::Utc;
pub use response::WeightSignal;
use std::path::PathBuf;
use std::sync::Arc;

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
        Err(agents::AgentError::RateLimit { .. }) => "rate_limit",
        Err(agents::AgentError::InvalidResponse { .. }) => "parse_error",
        Err(_) => "failed",
        Ok(_)
            if matches!(
                status,
                "done" | "completed" | "in_progress" | "in_review" | "needs_review" | "routed"
            ) =>
        {
            "success"
        }
        Ok(_) if status == "blocked" => "blocked",
        Ok(_) => "failed",
    }
}

fn classify_run_error_type(last_error: &str) -> &'static str {
    if last_error.is_empty() {
        // No error: agent completed successfully and created a PR waiting for review
        "success"
    } else if last_error.contains("timeout") {
        "timeout"
    } else if last_error.contains("billing cycle") {
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
        if lower.contains("cargo build failed")
            || lower.contains("cargo test")
            || lower.contains("cargo clippy")
            || lower.contains("clippy")
            || lower.contains("test suite failed")
            || lower.contains("nextest")
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
            if let Ok(response) = crate::parser::parse(&assistant_text) {
                tracing::debug!(task_id, agent = agent_name, "found AgentResponse JSON in earlier assistant message (terminal_reason:completed)");
                return Ok(agents::ParsedResponse {
                    response,
                    input_tokens: None,
                    output_tokens: None,
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
    if let Some(agent_result) = agents::find_agent_result(agent_name, raw_stdout) {
        // Treat is_error as authoritative for the success path
        if agent_result.is_error {
            return Err(agents::AgentError::AgentFailed {
                message: agent_result.result_text,
            });
        }

        // Try to parse the inner result text as standard JSON
        match crate::parser::parse(&agent_result.result_text) {
            Ok(response) => {
                return Ok(agents::ParsedResponse {
                    response,
                    input_tokens: agent_result.input_tokens,
                    output_tokens: agent_result.output_tokens,
                    duration_ms: agent_result.duration_ms,
                });
            }
            Err(_) => {
                // Strategy 2: scan all earlier assistant messages in the NDJSON
                // stream for valid AgentResponse JSON. Agents often emit the
                // JSON in an intermediate turn and then summarise in prose as
                // their final output, so the `type:result` text may be pure
                // narrative while a prior assistant turn contains the JSON.
                let assistant_text =
                    agents::collect_assistant_messages_text(agent_name, raw_stdout);
                if !assistant_text.is_empty() {
                    if let Ok(response) = crate::parser::parse(&assistant_text) {
                        tracing::debug!(
                            task_id,
                            agent = agent_name,
                            "found AgentResponse JSON in earlier assistant message"
                        );
                        return Ok(agents::ParsedResponse {
                            response,
                            input_tokens: agent_result.input_tokens,
                            output_tokens: agent_result.output_tokens,
                            duration_ms: agent_result.duration_ms,
                        });
                    }
                }

                // Only fall back to text synthesis when parsing fails
                if let Some(response) =
                    agents::synthesize_response_from_text(&agent_result.result_text)
                {
                    tracing::warn!(
                        task_id,
                        agent = agent_name,
                        "parse failed on agent result, synthesizing response from plain text"
                    );
                    return Ok(agents::ParsedResponse {
                        response,
                        input_tokens: agent_result.input_tokens,
                        output_tokens: agent_result.output_tokens,
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
                    && raw_stdout.contains("\"terminal_reason\":\"completed\"")
                    && !agent_result.result_text.is_empty()
                {
                    let summary_end =
                        agents::truncate_at_char_boundary(&agent_result.result_text, 500);
                    let summary = agent_result.result_text[..summary_end].to_string();
                    tracing::warn!(
                        task_id,
                        agent = agent_name,
                        "NDJSON envelope indicates success but result lacks AgentResponse \
                         schema — synthesizing done response"
                    );
                    return Ok(agents::ParsedResponse {
                        response: crate::parser::AgentResponse {
                            status: "done".to_string(),
                            summary,
                            ..Default::default()
                        },
                        input_tokens: agent_result.input_tokens,
                        output_tokens: agent_result.output_tokens,
                        duration_ms: agent_result.duration_ms,
                    });
                }
            }
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
                let duration_ms = agent_result.as_ref().and_then(|r| r.duration_ms);
                Ok(agents::ParsedResponse {
                    response,
                    input_tokens,
                    output_tokens,
                    duration_ms,
                })
            } else {
                Err(agents::AgentError::InvalidResponse { raw })
            }
        }
        Err(err) => Err(err),
    }
}

/// Parse session output, handling the kimi/minimax/glm exit-1-with-completed-edge case.
///
/// Tries `parse_success_output` first when stdout is non-empty. If that succeeds,
/// returns success regardless of exit code. If it fails and `exit_code != 0`,
/// checks for `"terminal_reason":"completed"` or cost-telemetry-only stdout
/// (`"costUSD":`) in stdout before falling back to `classify_error` — so valid
/// sessions that exit 1 produce `InvalidResponse` (soft error, re-route) instead
/// of a garbled `Unknown`/`Auth` error (hard failure with spurious cooldown).
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
                // If parse_success_output already returned AgentFailed (e.g. is_error=true
                // in NDJSON envelope), propagate it directly — the session had an explicit
                // error and we should not try to re-classify it or intercept it.
                if matches!(err, agents::AgentError::AgentFailed { .. }) {
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

/// Task runner configuration.
pub struct TaskRunner {
    /// Repository slug (owner/repo)
    repo: String,
    /// Path to the orch home directory
    orch_home: PathBuf,
    /// Unified task store for run audit trail
    store: Option<Arc<crate::store::TaskStore>>,
}

impl TaskRunner {
    pub fn new(repo: String) -> Self {
        let orch_home = crate::home::orch_home().unwrap_or_else(|e| {
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
    pub fn with_store(mut self, store: Arc<crate::store::TaskStore>) -> Self {
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
            let error = match input.error_override.as_deref().filter(|s| !s.is_empty()) {
                Some(error_override) => error_override.to_string(),
                None => match last_error {
                    Some(last_error) => last_error,
                    None => match input.parse_result {
                        Ok(parsed) => parsed.response.error.clone().unwrap_or_default(),
                        Err(err) => err.to_string(),
                    },
                },
            };

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

        let total_cost_usd = match &self.store {
            Some(_) => {
                store::get_cost_estimate(&self.store, &self.repo, input.task_id)
                    .await
                    .total_cost_usd
            }
            None => 0.0,
        };

        let (input_tokens, output_tokens) = extract_run_tokens(input.parse_result);

        let duration_secs = input
            .elapsed_secs
            .map(|s| s as f64)
            .unwrap_or_else(|| run_duration_secs(input.started_at));

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
                // Update GitHub status to NeedsReview so engine stops re-dispatching.
                if let Some(b) = backend {
                    let id = crate::backends::ExternalId(task_id.to_string());
                    if let Err(e) = b
                        .update_status(&id, crate::backends::Status::NeedsReview)
                        .await
                    {
                        tracing::warn!(task_id, err = %e, "failed to update backend status to NeedsReview after max attempts");
                    }

                    // If this was label-forced to a specific agent, remove the agent label
                    // so that /retry can route to a different agent instead of looping.
                    let route_reason = self
                        .get_field(task_id, "route_reason")
                        .await
                        .unwrap_or_default();
                    if route_reason.starts_with("label agent:") {
                        let agent_label = route_reason.trim_start_matches("label ");
                        let gh = crate::github::http::GhHttp::new()?;
                        if let Err(e) = gh.remove_label(&self.repo, task_id, agent_label).await {
                            tracing::warn!(task_id, label = agent_label, err = %e, "failed to remove agent label after max attempts");
                        } else {
                            tracing::info!(task_id, label = agent_label, "removed forced agent label after max attempts — /retry will use free routing");
                        }
                    }
                }
                return Ok(Some(RunExecution {
                    status: "needs_review".to_string(),
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

        // Run agent session: spawn in tmux, wait for completion, collect output
        let (tmux, tmux_session, session_output) = session::run_agent_session(
            task_id,
            &init.invocation,
            &init.attempt_dir,
            &self.orch_home,
        )
        .await;

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
                crate::store::TaskStatus::New
                    | crate::store::TaskStatus::Routed
                    | crate::store::TaskStatus::NeedsReview
            );
            if silence_reset {
                let status_str = stored.status.as_str();
                tracing::info!(
                    task_id,
                    status = status_str,
                    "task already reset by silence detection — skipping fallback processing"
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
                        error_override: Some(format!("silence detection set task to {status_str}")),
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
                        // (see src/engine/runner/fallback.rs which persists a compact
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
        if final_status.as_str() == "done" {
            if let Some(ref st) = self.store {
                if let Err(e) =
                    crate::engine::cleanup::cleanup_task_worktree(task_id, &self.repo, st).await
                {
                    tracing::warn!(task_id, error = ?e, "worktree cleanup failed after no-op done");
                }
            }
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
            status: final_status.to_string(),
            exit_code: Some(session_output.exit_code),
            audit,
        }))
    }

    /// Record task execution metrics to the database.
    #[allow(clippy::too_many_arguments)]
    async fn record_metrics(
        &self,
        task_id: &str,
        agent_name: &str,
        model_name: &Option<String>,
        route_result: &Option<RouteResult>,
        started_at: &chrono::DateTime<Utc>,
        attempts: u32,
        final_status: &str,
        error_type: Option<&str>,
    ) {
        let completed_at = Utc::now();
        let duration_seconds = (completed_at - *started_at).num_milliseconds() as f64 / 1000.0;

        // Classify outcome based on the final status and optional error_type/last_error
        let outcome = match final_status {
            "done" | "in_progress" | "in_review" => "success",
            "new" => "rerouted",
            "needs_review" => classify_run_error_type(error_type.unwrap_or("")),
            _ => "unknown",
        };

        let complexity = route_result.as_ref().map(|r| r.complexity.clone());
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

            // Read cost data from store
            let usage = store::get_token_usage(&self.store, &self.repo, task_id).await;
            let cost = store::get_cost_estimate(&self.store, &self.repo, task_id).await;
            let input_tokens = if usage.input_tokens > 0 {
                Some(usage.input_tokens as i64)
            } else {
                None
            };
            let output_tokens = if usage.output_tokens > 0 {
                Some(usage.output_tokens as i64)
            } else {
                None
            };
            let input_cost = if cost.input_cost_usd > 0.0 {
                Some(cost.input_cost_usd)
            } else {
                None
            };
            let output_cost = if cost.output_cost_usd > 0.0 {
                Some(cost.output_cost_usd)
            } else {
                None
            };
            let total_cost = if cost.total_cost_usd > 0.0 {
                Some(cost.total_cost_usd)
            } else {
                None
            };

            let metric = InsertTaskMetric {
                repo: &self.repo,
                task_id,
                agent: agent_name,
                model: model_name.as_deref(),
                complexity: complexity.as_deref(),
                outcome,
                duration_seconds,
                started_at,
                completed_at: &completed_at,
                attempts: attempts as i32 + 1,
                files_changed: files_changed as i32,
                error_type: db_error_type.as_deref(),
                input_tokens,
                output_tokens,
                input_cost_usd: input_cost,
                output_cost_usd: output_cost,
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
            if let Ok(Some(store_id)) = store.resolve_task_id(&self.repo, task_id).await {
                match store
                    .start_run(&crate::store::StartRun {
                        task_id: store_id,
                        attempt,
                        run_type: "agent",
                        agent: &agent_name,
                        model: model.unwrap_or(""),
                        command: &format!("{} --model {}", agent_name, model.unwrap_or("default")),
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
            } else {
                None
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
                if let Some(run_id) = run_audit_id {
                    if let Some(ref store) = self.store {
                        if let Err(complete_err) = store
                            .complete_run(&crate::store::CompleteRun {
                                run_id,
                                exit_code: Some(-1),
                                stdout: "",
                                stderr: "",
                                parsed: "",
                                outcome: "failed",
                                error: &e.to_string(),
                                tokens: crate::store::RunTokenUsage::default(),
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
                    }
                }
                return Err(e);
            }
        };

        // If the runner guard skipped the task, do not re-post stale data as a new comment.
        let (status, exit_code_opt, run_audit) = match run_result {
            Some(result) => (result.status, result.exit_code, result.audit),
            None => {
                if let Some(run_id) = run_audit_id {
                    if let Some(ref store) = self.store {
                        if let Err(e) = store
                            .complete_run(&crate::store::CompleteRun {
                                run_id,
                                exit_code: Some(-1),
                                stdout: "",
                                stderr: "",
                                parsed: "",
                                outcome: "aborted",
                                error: "run skipped by guard",
                                tokens: crate::store::RunTokenUsage::default(),
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
                    }
                }
                tracing::info!(task_id, "guard skipped task — not posting stale result");
                return Ok(WeightSignal::None);
            }
        };

        // Process delegations if the agent requested subtasks
        let delegations_raw = self
            .get_field(task_id, "delegations")
            .await
            .unwrap_or_default();
        if !delegations_raw.is_empty() {
            match serde_json::from_str::<Vec<crate::parser::Delegation>>(&delegations_raw) {
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
            // Check if silence detection (tick phase1b) already re-routed this task while
            // the session was running. Silence detection sets status to Routed/New and
            // stores the fallback agent — we must NOT overwrite those fields here.
            // If the stored status is still InProgress, silence detection did not fire
            // and we can safely clear agent/model for normal reroute.
            let silence_already_reset = store::opt_store_get_task(&self.store, &self.repo, task_id)
                .await
                .map(|t| !matches!(t.status, crate::store::TaskStatus::InProgress))
                .unwrap_or(false);

            if !silence_already_reset {
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
            } else {
                tracing::debug!(
                    task_id,
                    "silence detection already set fallback agent — skipping agent/model clear"
                );
            }
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
                // Non-rate-limit reroutes (silence, timeout, parse error) should not trigger
                // weight degradation — they're not provider-side limits.
                WeightSignal::None
            }
        } else if status == "needs_review" {
            // Rate limit errors that escalate to needs_review must NOT reset backoff
            // counters (issue #2153).  These errors indicate the agent is genuinely
            // unavailable — resetting would wipe the cooldown and cause the next dispatch
            // to retry the same rate-limited agent immediately.
            //
            // Detection: classify_run_error_type() confirms "rate limit" in last_error.
            // We detect rate limits from the last_error text (consistent with the
            // classify_run_error_type() pattern used in record_metrics).
            let has_rate_limit_error =
                last_error.contains("rate limit") || last_error.contains("usage limit");
            if has_rate_limit_error {
                // Record in the metrics store so the router's health check detects it.
                if let Some(ref store) = self.store {
                    if let Err(e) = store
                        .record_rate_limit(&agent_name, "rate_limit", Some(task_id))
                        .await
                    {
                        tracing::warn!(task_id, agent = %agent_name, err = %e, "failed to persist rate_limit event — router health check may miss this signal");
                    }
                }
                // Also record via the cooldown system so model_for_complexity() skips
                // this model.  Use the stored last_error text as the message since
                // classify_run_error_type() already confirmed it contains "rate limit".
                crate::engine::cooldown::record_agent_failure_with_message(
                    &agent_name,
                    &last_error,
                )
                .await;
                if let Some(m) = model {
                    crate::engine::cooldown::record_model_failure(&agent_name, m).await;
                }
                WeightSignal::RateLimited {
                    agent: agent_name.clone(),
                }
            } else {
                // Non-rate-limit "needs_review" (e.g. agent successfully completed but
                // review agent is needed) — reset backoff so the next failure starts fresh.
                crate::engine::cooldown::record_agent_success(&agent_name, model.unwrap_or(""))
                    .await;
                WeightSignal::Success {
                    agent: agent_name.clone(),
                }
            }
        } else if status == "done" || status == "in_progress" || status == "in_review" {
            // Reset exponential backoff counters so the next failure starts fresh.
            crate::engine::cooldown::record_agent_success(&agent_name, model.unwrap_or("")).await;
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
                &model.map(|s| s.to_string()),
                &route_result.cloned(),
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
        if let Some(run_id) = run_audit_id {
            if let Some(ref store) = self.store {
                if let Err(e) = store
                    .complete_run(&crate::store::CompleteRun {
                        run_id,
                        exit_code: exit_code_opt,
                        stdout: &run_audit.stdout,
                        stderr: &run_audit.stderr,
                        parsed: &run_audit.parsed_response,
                        outcome: &run_audit.outcome,
                        error: &run_audit.error,
                        tokens: crate::store::RunTokenUsage {
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
            }
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
            "needs_review" => Status::NeedsReview,
            "routed" => Status::Routed, // Rerouted (agent/model already set)
            "new" => Status::New,       // Legacy reroute path
            _ => Status::NeedsReview,
        };
        // Store-first: update SQLite (must succeed), then mirror to backend.
        if let Some(ref store) = self.store {
            if let Ok(Some(store_id)) = store.resolve_task_id(&self.repo, &task.id.0).await {
                store
                    .update_status(
                        store_id,
                        crate::engine::tasks::status_to_task_status(new_status),
                    )
                    .await
                    .context("store-first status update failed in runner")?;
            }
        }
        if let Err(e) = backend.update_status(&task.id, new_status).await {
            tracing::warn!(
                task_id,
                ?new_status,
                err = %e,
                "failed to mirror runner status to backend — store is authoritative"
            );
        }

        // Post comment (scan for secrets before posting to GitHub)
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
        let mut raw_comment = if !summary.is_empty() {
            format!("[{now}] {status}: {summary}")
        } else if !last_error.is_empty() {
            format!("[{now}] {status}: {last_error}")
        } else {
            format!("[{now}] {status}")
        };

        // Append visible attribution footer (matches PR body style)
        let footer = crate::engine::attribution_footer("Created", &agent_name, model);
        raw_comment.push_str(&footer);

        // Scan for leaked secrets and redact if needed
        let comment = if security::has_leaks(&raw_comment) {
            let leaks = security::scan(&raw_comment);
            let rules: Vec<&str> = leaks.iter().map(|l| l.rule).collect();
            let warning = format!(
                "\n\n> ⚠️ **Security Notice**: {} potential secret(s) detected and redacted: {}",
                leaks.len(),
                rules.join(", ")
            );
            let redacted = security::redact(&raw_comment);
            format!("{redacted}{warning}")
        } else {
            raw_comment
        };
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
        delegations: &[crate::parser::Delegation],
        backend: &Arc<dyn ExternalBackend>,
    ) -> anyhow::Result<()> {
        let parent_id = &parent_task.id;

        // Fetch existing open tasks once for dedup — prevents creating duplicate
        // GitHub issues when the same delegations are produced on repeated runs.
        let existing_titles: std::collections::HashSet<String> =
            match backend.list_all_tasks().await {
                Ok(tasks) => tasks.into_iter().map(|t| t.title).collect(),
                Err(e) => {
                    tracing::warn!(err = %e, "failed to fetch tasks for dedup — will create all");
                    std::collections::HashSet::new()
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
                crate::engine::orch_footer()
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
        if let Some(ref store) = self.store {
            if let Ok(Some(store_id)) = store.resolve_task_id(&self.repo, &parent_id.0).await {
                store
                    .update_status(store_id, crate::store::TaskStatus::Blocked)
                    .await
                    .context("store-first: failed to block parent in store")?;
            }
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
                    crate::engine::orch_footer()
                ),
            )
            .await?;

        Ok(())
    }

    /// Resolve the project directory for this repo.
    async fn resolve_project_dir(&self) -> anyhow::Result<PathBuf> {
        // Explicit env var always wins
        if let Ok(dir) = std::env::var("PROJECT_DIR") {
            if !dir.is_empty() {
                return Ok(PathBuf::from(dir));
            }
        }

        // Check projects list in global config — find path for this repo
        if let Ok(paths) = config::get_project_paths() {
            for path_str in &paths {
                let path = PathBuf::from(path_str);
                // Check if this project's .orch.yml has matching repo
                if let Ok(repo) = config::get_repo_for_project(&path) {
                    // Use tokio::fs::metadata for async-safe existence check
                    let path_for_check = path.clone();
                    if repo == self.repo && tokio::fs::metadata(&path_for_check).await.is_ok() {
                        return Ok(path);
                    }
                }
            }
        }

        // Legacy: check config project_dir
        if let Ok(dir) = config::get("project_dir") {
            if !dir.is_empty() {
                let path = PathBuf::from(&dir);
                let path_for_check = path.clone();
                if tokio::fs::metadata(&path_for_check).await.is_ok() {
                    return Ok(path);
                }
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
            if tokio::task::spawn_blocking(move || bare_for_check.exists())
                .await
                .unwrap_or(false)
            {
                return Ok(bare);
            }
        }

        // Fall back to current directory
        Ok(std::env::current_dir()?)
    }
}

/// Update the GitHub agent label on an external task when it is rerouted to a new agent.
///
/// Removes the old `agent:<old_agent>` label, ensures the new `agent:<new_agent>` label exists
/// in the repo, then applies it. Returns `true` only when both `ensure_label` (repo label creation)
/// and `set_labels` (issue label assignment) succeed. `remove_label` failures are non-fatal.
async fn update_agent_label_after_reroute(
    #[cfg_attr(test, allow(unused_variables))] repo: &str,
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
        match crate::github::http::GhHttp::new() {
            Ok(gh) => {
                if gh
                    .ensure_label(
                        repo,
                        &new_label,
                        crate::github::http::status_label_color(&new_label),
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
        .set_labels(&task.id, std::slice::from_ref(&new_label))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::{ExternalId, ExternalTask, Mention, Status};
    use crate::engine::runner::agents::patterns::safe_tail as safe_utf8_tail;
    use crate::parser::AgentResponse;
    use crate::store::{NewTask, TaskStore};
    use async_trait::async_trait;
    use chrono::Utc;
    use once_cell::sync::Lazy;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    static ENV_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

    fn ok_parse_result(status: &str) -> Result<agents::ParsedResponse, agents::AgentError> {
        Ok(agents::ParsedResponse {
            response: AgentResponse {
                status: status.to_string(),
                summary: String::new(),
                accomplished: vec![],
                remaining: vec![],
                files: vec![],
                error: None,
                input_tokens: None,
                output_tokens: None,
                learnings: vec![],
                delegations: vec![],
            },
            input_tokens: None,
            output_tokens: None,
            duration_ms: None,
        })
    }

    // ── safe_utf8_tail ───────────────────────────────────────────────────────

    #[test]
    fn safe_utf8_tail_short_string_returned_as_is() {
        assert_eq!(safe_utf8_tail("hello", 100), "hello");
        assert_eq!(safe_utf8_tail("", 10), "");
    }

    #[test]
    fn safe_utf8_tail_truncates_ascii() {
        let s = "abcdefghij"; // 10 bytes
        assert_eq!(safe_utf8_tail(s, 5), "fghij");
        assert_eq!(safe_utf8_tail(s, 10), "abcdefghij");
    }

    #[test]
    fn safe_utf8_tail_handles_multibyte_boundary() {
        // "日本語" is 3 chars × 3 bytes each = 9 bytes total
        let s = "日本語";
        assert_eq!(s.len(), 9);
        // Cutting at 8 bytes lands in the middle of "語" (3 bytes at 6..9).
        // safe_utf8_tail should walk forward to the next boundary at 9 (end) rather
        // than panicking on an invalid slice.
        let tail = safe_utf8_tail(s, 8);
        assert!(
            s.ends_with(tail),
            "tail must be a valid suffix of the original"
        );
        assert!(
            std::str::from_utf8(tail.as_bytes()).is_ok(),
            "tail must be valid UTF-8"
        );
    }

    #[test]
    fn safe_utf8_tail_handles_mixed_ascii_and_multibyte() {
        let s = "hello日本語world"; // ascii + CJK + ascii
        for max_bytes in [1, 3, 5, 7, 11, 13, 20, 100] {
            let tail = safe_utf8_tail(s, max_bytes);
            assert!(
                std::str::from_utf8(tail.as_bytes()).is_ok(),
                "max_bytes={max_bytes}: tail must be valid UTF-8"
            );
            assert!(
                s.ends_with(tail),
                "max_bytes={max_bytes}: tail must be a suffix of original"
            );
        }
    }

    #[test]
    fn safe_utf8_tail_exact_boundary() {
        let s = "abcdef"; // 6 bytes
        assert_eq!(safe_utf8_tail(s, 6), "abcdef"); // exactly fits
        assert_eq!(safe_utf8_tail(s, 0), ""); // empty tail
    }

    #[test]
    fn parse_success_output_synthesizes_done_from_plain_text() {
        let runner = agents::get_runner("claude");
        let parsed = parse_success_output(
            "123",
            "claude",
            &*runner,
            "No open positions, no trade executions, no conditions change.",
        )
        .unwrap();

        assert_eq!(parsed.response.status, "done");
        assert_eq!(
            parsed.response.summary,
            "No open positions, no trade executions, no conditions change."
        );
    }

    #[test]
    fn parse_success_output_synthesizes_needs_review_from_error_text() {
        let runner = agents::get_runner("claude");
        let parsed =
            parse_success_output("123", "claude", &*runner, "Failed to update branch").unwrap();

        assert_eq!(parsed.response.status, "needs_review");
        assert_eq!(
            parsed.response.error.as_deref(),
            Some("agent returned plain text instead of JSON")
        );
    }

    #[test]
    fn parse_success_output_keeps_empty_text_invalid() {
        let runner = agents::get_runner("claude");
        let err = parse_success_output("123", "claude", &*runner, "").unwrap_err();

        assert!(matches!(err, agents::AgentError::InvalidResponse { .. }));
    }

    #[test]
    fn parse_success_output_preserves_tokens_from_ndjson() {
        // Simulates NDJSON output from --output-format stream-json where
        // the result event contains token usage. parse_response fails on
        // NDJSON (expects single JSON blob), but find_agent_result extracts
        // tokens from the NDJSON envelope so cost tracking is preserved.
        let ndjson = r#"{"type":"system","subtype":"init","session_id":"abc"}
{"type":"assistant","message":{"content":[{"type":"text","text":"Done."}],"usage":{"input_tokens":100,"output_tokens":50}}}
{"type":"result","subtype":"success","is_error":false,"duration_ms":5000,"result":"All changes committed successfully","usage":{"input_tokens":1234,"output_tokens":567}}"#;
        let runner = agents::get_runner("claude");
        let parsed = parse_success_output("456", "claude", &*runner, ndjson).unwrap();

        assert_eq!(parsed.response.status, "done");
        assert_eq!(parsed.input_tokens, Some(1234));
        assert_eq!(parsed.output_tokens, Some(567));
        assert_eq!(parsed.duration_ms, Some(5000));
    }

    #[test]
    fn parse_success_output_preserves_tokens_from_minimax_ndjson() {
        // MiniMax/Kimi use assistant event fallback when there's no result event.
        let ndjson = r#"{"type":"system","subtype":"init"}
{"type":"assistant","message":{"content":[{"type":"text","text":"No open positions."}],"usage":{"input_tokens":2000,"output_tokens":300}}}"#;
        let runner = agents::get_runner("minimax");
        let parsed = parse_success_output("789", "minimax", &*runner, ndjson).unwrap();

        assert_eq!(parsed.response.status, "done");
        assert_eq!(parsed.input_tokens, Some(2000));
        assert_eq!(parsed.output_tokens, Some(300));
    }

    #[test]
    fn classify_run_error_type_recognizes_specific_patterns() {
        let cases = [
            ("push failed: remote rejected", "push_failed"),
            ("No commits between main and branch", "no_commits"),
            ("create PR failed: 422", "pr_failed"),
            ("invalid response from agent", "parse_error"),
            ("parse error: missing json", "parse_error"),
            ("exceeded max attempts while rerouting", "max_attempts"),
            ("cargo test failed", "ci_failure"),
            ("cargo build failed: error[E0308]", "ci_failure"),
            ("cargo clippy -- -D warnings failed", "ci_failure"),
            ("clippy failed on warning", "ci_failure"),
            ("nextest run exited 1", "ci_failure"),
            ("test suite failed: 3 tests failed", "ci_failure"),
        ];

        for (input, expected) in cases {
            assert_eq!(classify_run_error_type(input), expected, "input={input}");
        }
    }

    #[test]
    fn classify_run_error_type_falls_back_for_unknown_errors() {
        assert_eq!(classify_run_error_type("something unexpected"), "failed");
    }

    #[test]
    fn classify_run_error_type_connection_error_mentioning_pull_request_is_not_pr_failed() {
        // A generic GitHub API or connection error whose message happens to contain the
        // substring "pull request" must NOT be classified as pr_failed.
        assert_eq!(
            classify_run_error_type("failed to fetch pull request details from GitHub"),
            "failed"
        );
        assert_eq!(
            classify_run_error_type("connection reset while reading pull request data"),
            "failed"
        );
    }

    #[test]
    fn classify_run_error_type_url_with_test_substring_is_not_ci_failure() {
        // A connection error whose URL path contains "/test-org/" must NOT be ci_failure.
        assert_eq!(
            classify_run_error_type(
                "failed to connect to api.github.com/repos/test-org/repo/pulls/123"
            ),
            "failed"
        );
        // Bare "test" word in an unrelated error must not trigger ci_failure.
        assert_eq!(
            classify_run_error_type("context deadline exceeded testing connection"),
            "failed"
        );
    }

    #[test]
    fn classify_run_error_type_usage_without_limit_is_not_rate_limit() {
        // "usage" alone (e.g. token usage reporting) must NOT be rate_limit.
        assert_eq!(
            classify_run_error_type("token usage: 1245 tokens"),
            "failed"
        );
        assert_eq!(
            classify_run_error_type("incorrect usage of git command"),
            "failed"
        );
        // Only "usage limit" should trigger rate_limit.
        assert_eq!(
            classify_run_error_type("exceeded usage limit for this billing period"),
            "rate_limit"
        );
    }

    #[test]
    fn classify_run_error_type_billing_cycle_is_not_auth_error() {
        // "billing cycle" quota-exhaustion messages must be classified as billing_cycle_exhausted,
        // not auth_error — aligning with cooldown.rs CreditExhaustionReason::BillingCycleExhausted.
        assert_eq!(
            classify_run_error_type(
                "You've reached your usage limit for this billing cycle. \
                 Your quota will be refreshed in the next cycle."
            ),
            "billing_cycle_exhausted"
        );
        assert_eq!(
            classify_run_error_type("quota exhausted for billing cycle"),
            "billing_cycle_exhausted"
        );
        // Plain "billing" without "cycle" that used to map to auth_error now falls through to "failed".
        assert_eq!(
            classify_run_error_type("billing account suspended"),
            "failed"
        );
    }

    #[test]
    fn classify_run_outcome_routed_is_success() {
        // "routed" is a canonical non-success status (task will be re-dispatched).
        // It should be classified as "success" so the audit trail distinguishes it
        // from genuine failures (#2801).
        let parse_result = ok_parse_result("routed");
        assert_eq!(
            classify_run_outcome("routed", &parse_result, false),
            "success"
        );
    }

    #[test]
    fn classify_run_outcome_blocked_is_blocked() {
        let parse_result = ok_parse_result("blocked");
        assert_eq!(
            classify_run_outcome("blocked", &parse_result, false),
            "blocked"
        );
    }

    #[test]
    fn classify_run_outcome_non_canonical_is_failed() {
        // Non-canonical statuses should be classified as "failed" in the audit.
        // The explicit error message is provided by build_run_audit.
        let parse_result = ok_parse_result("fix_deployed");
        assert_eq!(
            classify_run_outcome("fix_deployed", &parse_result, false),
            "failed"
        );
    }

    #[tokio::test]
    async fn build_run_audit_success_ignores_stale_last_error() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let task_id = store
            .create(&NewTask {
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "Test".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        store
            .set_fields(
                task_id,
                &[("last_error", serde_json::json!("no error info available"))],
            )
            .await
            .unwrap();

        let runner = TaskRunner::new("owner/repo".to_string()).with_store(store);
        let parse_result = ok_parse_result("done");
        let started_at = Utc::now();
        let audit = runner
            .build_run_audit(RunAuditInput {
                task_id: &task_id.to_string(),
                status: "done",
                parse_result: &parse_result,
                raw_stdout: "",
                raw_stderr: "",
                started_at: &started_at,
                error_override: None,
                elapsed_secs: Some(1),
                push_failed: false,
            })
            .await;

        assert_eq!(audit.outcome, "success");
        assert!(audit.error.is_empty());
    }

    #[tokio::test]
    async fn build_run_audit_non_success_uses_fallback_error_when_empty() {
        let runner = TaskRunner::new("owner/repo".to_string());
        let parse_result = ok_parse_result("new");
        let started_at = Utc::now();
        let audit = runner
            .build_run_audit(RunAuditInput {
                task_id: "1",
                status: "new",
                parse_result: &parse_result,
                raw_stdout: "",
                raw_stderr: "",
                started_at: &started_at,
                error_override: None,
                elapsed_secs: Some(1),
                push_failed: false,
            })
            .await;

        assert_eq!(audit.outcome, "failed");
        // "new" is a canonical non-success status — no error message needed.
        assert_eq!(audit.error, "");
    }

    #[tokio::test]
    async fn build_run_audit_non_canonical_status_reports_explicit_error() {
        // Non-canonical statuses (not in the allowlist) with no parse error
        // should get an explicit "unrecognized status: X" message instead of
        // the opaque "no error info available" placeholder (#2801).
        let runner = TaskRunner::new("owner/repo".to_string());
        let parse_result = ok_parse_result("fix_deployed");
        let started_at = Utc::now();
        let audit = runner
            .build_run_audit(RunAuditInput {
                task_id: "1",
                status: "fix_deployed",
                parse_result: &parse_result,
                raw_stdout: "",
                raw_stderr: "",
                started_at: &started_at,
                error_override: None,
                elapsed_secs: Some(1),
                push_failed: false,
            })
            .await;

        assert_eq!(audit.outcome, "failed");
        assert_eq!(audit.error, "unrecognized status: fix_deployed");
    }

    #[tokio::test]
    async fn build_run_audit_canonical_non_success_status_no_error_message() {
        // Canonical non-success statuses (e.g. "routed") should not produce
        // an error message — they're expected outcomes, not failures.
        for status in &["routed"] {
            let runner = TaskRunner::new("owner/repo".to_string());
            let parse_result = ok_parse_result(status);
            let started_at = Utc::now();
            let audit = runner
                .build_run_audit(RunAuditInput {
                    task_id: "1",
                    status,
                    parse_result: &parse_result,
                    raw_stdout: "",
                    raw_stderr: "",
                    started_at: &started_at,
                    error_override: None,
                    elapsed_secs: Some(1),
                    push_failed: false,
                })
                .await;

            assert_eq!(audit.outcome, "success", "status={status}");
            assert_eq!(
                audit.error, "",
                "status={status}: no error for canonical status"
            );
        }
    }

    #[tokio::test]
    async fn build_run_audit_blocked_uses_summary_when_error_missing() {
        let runner = TaskRunner::new("owner/repo".to_string());
        let parse_result = Ok(agents::ParsedResponse {
            response: AgentResponse {
                status: "blocked".to_string(),
                summary: "Could not proceed due to missing credentials".to_string(),
                accomplished: vec![],
                remaining: vec![],
                files: vec![],
                error: None,
                input_tokens: None,
                output_tokens: None,
                learnings: vec![],
                delegations: vec![],
            },
            input_tokens: None,
            output_tokens: None,
            duration_ms: None,
        });
        let started_at = Utc::now();
        let audit = runner
            .build_run_audit(RunAuditInput {
                task_id: "1",
                status: "blocked",
                parse_result: &parse_result,
                raw_stdout: "",
                raw_stderr: "",
                started_at: &started_at,
                error_override: None,
                elapsed_secs: Some(1),
                push_failed: false,
            })
            .await;

        assert_eq!(audit.outcome, "blocked");
        assert_eq!(audit.error, "Could not proceed due to missing credentials");
    }

    // ── resolve_project_dir ──────────────────────────────────────────────────

    #[tokio::test]
    async fn resolve_project_dir_uses_project_dir_env() {
        // When PROJECT_DIR env var is set, it should always win.
        let runner = TaskRunner::new("owner/repo".to_string());

        // Use a temp dir so the path exists
        let dir = tempfile::tempdir().unwrap();
        let dir_str = dir.path().to_string_lossy().to_string();

        // Temporarily set the env var — note: tests run in parallel so we use a
        // dedicated key that only this test touches. std::env::set_var is not
        // thread-safe in general, but the subsequent call is synchronous and
        // isolated to the temp dir created above.
        std::env::set_var("PROJECT_DIR", &dir_str);
        let result = runner.resolve_project_dir().await;
        std::env::remove_var("PROJECT_DIR");

        assert!(
            result.is_ok(),
            "resolve_project_dir should succeed with PROJECT_DIR set"
        );
        assert_eq!(result.unwrap(), dir.path());
    }

    #[tokio::test]
    async fn resolve_project_dir_empty_project_dir_env_falls_through() {
        // An empty PROJECT_DIR should be ignored and fall through to other logic.
        let runner = TaskRunner::new("owner/testrepo-nonexistent".to_string());
        std::env::set_var("PROJECT_DIR", "");
        let result = runner.resolve_project_dir().await;
        std::env::remove_var("PROJECT_DIR");

        // Should succeed (falls back to current dir) without panicking.
        assert!(result.is_ok());
    }

    // ── mock backend for process_delegations ─────────────────────────────────

    struct TrackingBackend {
        sub_tasks_created: Arc<Mutex<Vec<(String, String)>>>, // (title, parent_id)
        status_updates: Arc<Mutex<Vec<(String, Status)>>>,
        comments: Arc<Mutex<Vec<(String, String)>>>, // (id, body)
    }

    impl TrackingBackend {
        fn new() -> Self {
            Self {
                sub_tasks_created: Arc::new(Mutex::new(vec![])),
                status_updates: Arc::new(Mutex::new(vec![])),
                comments: Arc::new(Mutex::new(vec![])),
            }
        }
    }

    #[async_trait]
    impl crate::backends::ExternalBackend for TrackingBackend {
        fn name(&self) -> &str {
            "tracking"
        }
        async fn create_task(
            &self,
            _t: &str,
            _b: &str,
            _l: &[String],
        ) -> anyhow::Result<ExternalId> {
            Ok(ExternalId("new".to_string()))
        }
        async fn get_task(&self, id: &ExternalId) -> anyhow::Result<ExternalTask> {
            Ok(ExternalTask {
                id: id.clone(),
                title: "".to_string(),
                body: "".to_string(),
                state: "open".to_string(),
                labels: vec![],
                author: "".to_string(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                url: "".to_string(),
            })
        }
        async fn list_by_status(&self, _s: Status) -> anyhow::Result<Vec<ExternalTask>> {
            Ok(vec![])
        }
        async fn list_routable(&self) -> anyhow::Result<Vec<ExternalTask>> {
            Ok(vec![])
        }
        async fn post_comment(&self, id: &ExternalId, body: &str) -> anyhow::Result<()> {
            self.comments
                .lock()
                .unwrap()
                .push((id.0.clone(), body.to_string()));
            Ok(())
        }
        async fn set_labels(&self, _id: &ExternalId, _l: &[String]) -> anyhow::Result<()> {
            Ok(())
        }
        async fn remove_label(&self, _id: &ExternalId, _l: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn get_sub_issues(&self, _id: &ExternalId) -> anyhow::Result<Vec<ExternalId>> {
            Ok(vec![])
        }
        async fn create_sub_task(
            &self,
            parent: &ExternalId,
            title: &str,
            _body: &str,
            _l: &[String],
        ) -> anyhow::Result<ExternalId> {
            self.sub_tasks_created
                .lock()
                .unwrap()
                .push((title.to_string(), parent.0.clone()));
            Ok(ExternalId(format!("child-{}", title)))
        }
        async fn ensure_status_label(&self, _l: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn has_open_issue_with_title(&self, _t: &str, _l: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn health_check(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn is_pr_merged(&self, _b: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn get_authenticated_user(&self) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
        async fn get_mentions(&self, _s: &str) -> anyhow::Result<Vec<Mention>> {
            Ok(vec![])
        }
        async fn update_status(&self, id: &ExternalId, status: Status) -> anyhow::Result<()> {
            self.status_updates
                .lock()
                .unwrap()
                .push((id.0.clone(), status));
            Ok(())
        }
    }

    fn make_task(id: &str) -> ExternalTask {
        ExternalTask {
            id: ExternalId(id.to_string()),
            title: format!("Task {id}"),
            body: "".to_string(),
            state: "open".to_string(),
            labels: vec![],
            author: "bot".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: "".to_string(),
        }
    }

    // ── process_delegations ───────────────────────────────────────────────────

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn process_delegations_creates_subtasks_and_blocks_parent() {
        let _guard = ENV_LOCK.lock().unwrap();
        let temp_home = TempDir::new().unwrap();
        let orch_home = temp_home.path().join(".orch");
        tokio::fs::create_dir_all(&orch_home).await.unwrap();
        let old_orch_home = std::env::var("ORCH_HOME").ok();
        std::env::set_var("ORCH_HOME", &orch_home);

        let runner = TaskRunner::new("owner/repo".to_string());
        let parent = make_task("99");
        let backend = Arc::new(TrackingBackend::new());
        let backend_dyn: Arc<dyn crate::backends::ExternalBackend> = backend.clone();

        let delegations = vec![
            crate::parser::Delegation {
                title: "Subtask A".to_string(),
                body: "Do A".to_string(),
                labels: vec!["enhancement".to_string()],
            },
            crate::parser::Delegation {
                title: "Subtask B".to_string(),
                body: "Do B".to_string(),
                labels: vec![],
            },
        ];

        runner
            .process_delegations(&parent, &delegations, &backend_dyn)
            .await
            .unwrap();

        // Two sub-tasks should have been created
        let created = backend.sub_tasks_created.lock().unwrap();
        assert_eq!(created.len(), 2);
        assert_eq!(created[0].1, "99", "parent id should be 99");
        assert!(
            created.iter().any(|(t, _)| t == "Subtask A"),
            "Subtask A should be created"
        );
        assert!(
            created.iter().any(|(t, _)| t == "Subtask B"),
            "Subtask B should be created"
        );
        drop(created);

        // Parent should be marked blocked
        let updates = backend.status_updates.lock().unwrap();
        assert!(
            updates
                .iter()
                .any(|(id, s)| id == "99" && *s == Status::Blocked),
            "parent should be blocked"
        );
        drop(updates);

        // A comment summarising the delegations should be posted
        let comments = backend.comments.lock().unwrap();
        assert!(
            !comments.is_empty(),
            "a delegation summary comment should be posted"
        );
        let comment_body = &comments[0].1;
        assert!(
            comment_body.contains("Subtask A"),
            "summary should mention Subtask A"
        );
        assert!(
            comment_body.contains("Subtask B"),
            "summary should mention Subtask B"
        );

        if let Some(old) = old_orch_home {
            std::env::set_var("ORCH_HOME", old);
        } else {
            std::env::remove_var("ORCH_HOME");
        }
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn process_delegations_single_subtask() {
        let _guard = ENV_LOCK.lock().unwrap();
        let temp_home = TempDir::new().unwrap();
        let orch_home = temp_home.path().join(".orch");
        tokio::fs::create_dir_all(&orch_home).await.unwrap();
        let old_orch_home = std::env::var("ORCH_HOME").ok();
        std::env::set_var("ORCH_HOME", &orch_home);

        let runner = TaskRunner::new("owner/repo".to_string());
        let parent = make_task("101");
        let backend = Arc::new(TrackingBackend::new());
        let backend_dyn: Arc<dyn crate::backends::ExternalBackend> = backend.clone();

        let delegations = vec![crate::parser::Delegation {
            title: "Only Child".to_string(),
            body: "Do only child".to_string(),
            labels: vec!["feature".to_string()],
        }];

        runner
            .process_delegations(&parent, &delegations, &backend_dyn)
            .await
            .unwrap();

        let created = backend.sub_tasks_created.lock().unwrap();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].0, "Only Child");
        drop(created);

        let updates = backend.status_updates.lock().unwrap();
        assert!(
            updates
                .iter()
                .any(|(id, s)| id == "101" && *s == Status::Blocked),
            "parent should be blocked"
        );
        drop(updates);

        let comments = backend.comments.lock().unwrap();
        let body = &comments[0].1;
        assert!(
            body.contains("1 subtask"),
            "comment should count one subtask"
        );

        if let Some(old) = old_orch_home {
            std::env::set_var("ORCH_HOME", old);
        } else {
            std::env::remove_var("ORCH_HOME");
        }
    }

    // ── weight signal logic ───────────────────────────────────────────────────

    /// Test helper that mirrors the weight signal logic from `run_with_context`.
    /// Reroutes only emit `RateLimited` when the error type is genuinely "rate_limit".
    fn weight_signal_for(status: &str, error_type: &str, agent: &str) -> WeightSignal {
        let is_rerouted = status == "new" || status == "routed";
        let is_rate_limit_error = error_type == "rate_limit";
        if is_rerouted {
            if is_rate_limit_error {
                WeightSignal::RateLimited {
                    agent: agent.to_string(),
                }
            } else {
                WeightSignal::None
            }
        } else if status == "done"
            || status == "needs_review"
            || status == "in_progress"
            || status == "in_review"
        {
            WeightSignal::Success {
                agent: agent.to_string(),
            }
        } else if status == "blocked" {
            WeightSignal::Blocked
        } else {
            WeightSignal::None
        }
    }

    #[test]
    fn weight_signal_success_includes_needs_review() {
        // The normal happy-path for code tasks: agent reports done, has a PR,
        // so handle_success returns "needs_review". This must map to Success.
        let signal = weight_signal_for("needs_review", "success", "claude");
        assert!(
            matches!(signal, WeightSignal::Success { agent } if agent == "claude"),
            "needs_review should produce WeightSignal::Success"
        );
    }

    #[test]
    fn weight_signal_success_for_all_done_statuses() {
        for status in ["done", "needs_review", "in_progress", "in_review"] {
            let signal = weight_signal_for(status, "success", "codex");
            assert!(
                matches!(signal, WeightSignal::Success { agent } if agent == "codex"),
                "{status} should produce WeightSignal::Success"
            );
        }
    }

    #[test]
    fn weight_signal_rate_limited_for_reroute_with_rate_limit_error() {
        // Reroute due to rate limit should emit RateLimited.
        let signal = weight_signal_for("new", "rate_limit", "claude");
        assert!(matches!(signal, WeightSignal::RateLimited { agent } if agent == "claude"));
    }

    #[test]
    fn weight_signal_rate_limited_for_routed_status_with_rate_limit_error() {
        let signal = weight_signal_for("routed", "rate_limit", "claude");
        assert!(matches!(signal, WeightSignal::RateLimited { agent } if agent == "claude"));
    }

    #[test]
    fn weight_signal_none_for_reroute_without_rate_limit_error() {
        // Reroute due to silence detection, timeout, or other non-rate-limit errors
        // should NOT emit RateLimited (fixes false cooldown cascades).
        for error_type in ["timeout", "silence detected", "parse_error", "failed"] {
            for status in ["new", "routed"] {
                let signal = weight_signal_for(status, error_type, "claude");
                assert!(
                    matches!(signal, WeightSignal::None),
                    "{status} with error_type={error_type} should produce WeightSignal::None, not RateLimited"
                );
            }
        }
    }

    #[test]
    fn weight_signal_blocked_status() {
        let signal = weight_signal_for("blocked", "success", "claude");
        assert!(matches!(signal, WeightSignal::Blocked));
    }

    // ── parse_success_output token extraction ────────────────────────────────

    /// Mock runner that always returns InvalidResponse with truncated text,
    /// simulating what minimax/codex do when they can't parse structured JSON.
    struct FailingMockRunner;

    impl agents::AgentRunner for FailingMockRunner {
        #[cfg(test)]
        fn name(&self) -> &str {
            "opencode"
        }

        fn build_command(
            &self,
            _model: Option<&str>,
            _timeout_cmd: &str,
            _sys_file: &str,
            _msg_file: &str,
            _permissions: &agents::PermissionRules,
        ) -> String {
            String::new()
        }

        fn parse_response(&self, raw: &str) -> Result<agents::ParsedResponse, agents::AgentError> {
            // Simulate what minimax does: truncate to 300 chars
            Err(agents::AgentError::InvalidResponse {
                raw: raw.chars().take(30).collect(),
            })
        }

        fn extract_text(&self, raw: &str) -> Result<String, agents::AgentError> {
            Err(agents::AgentError::InvalidResponse {
                raw: raw.to_string(),
            })
        }

        fn classify_error(
            &self,
            exit_code: i32,
            _stdout: &str,
            _stderr: &str,
        ) -> agents::AgentError {
            agents::AgentError::Unknown {
                exit_code,
                message: "mock error".into(),
            }
        }

        fn router_command(
            &self,
            _prompt: &str,
            _model: Option<&str>,
        ) -> anyhow::Result<tokio::process::Command> {
            anyhow::bail!("not implemented for mock")
        }
    }

    #[test]
    fn parse_success_output_extracts_tokens_from_raw_stdout_not_error_raw() {
        // OpenCode NDJSON output with token metadata in step_finish
        let ndjson = r#"{"type":"text","timestamp":1,"part":{"type":"text","text":"Here is my response: {\"status\":\"done\",\"summary\":\"fixed\"}}"}}
{"type":"step_finish","timestamp":2,"part":{"type":"step-finish","reason":"stop","cost":0,"tokens":{"total":95177,"input":94920,"output":257}}}"#;

        let runner = FailingMockRunner;
        let result = parse_success_output("1550", "opencode", &runner, ndjson);

        // Should succeed (synthesized from text) with tokens preserved
        let parsed = result.expect("should synthesize response from text");
        assert_eq!(parsed.input_tokens, Some(94920));
        assert_eq!(parsed.output_tokens, Some(257));
    }

    #[test]
    fn parse_success_output_no_tokens_when_ndjson_has_none() {
        // Plain text with an explicit completion signal but no token metadata.
        // Uses "No open positions" which is in the explicit_done list.
        let text = "No open positions, no trade executions, no conditions change.";

        let runner = FailingMockRunner;
        let result = parse_success_output("1551", "opencode", &runner, text);

        // Should synthesize a "done" response, but without tokens (no NDJSON envelope).
        let parsed = result.expect("should synthesize response from explicit-done text");
        assert_eq!(parsed.input_tokens, None);
        assert_eq!(parsed.output_tokens, None);
    }

    #[test]
    fn parse_success_output_uses_agent_result_directly() {
        // Here we test that if find_agent_result succeeds and returns valid JSON, we use it directly
        // WITHOUT calling agent_runner.parse_response at all.
        let ndjson = r#"{"type":"result","subtype":"success","is_error":false,"result":"{\"status\":\"done\",\"summary\":\"my summary\",\"accomplished\":[],\"remaining\":[],\"files\":[]}"}"#;

        // FailingMockRunner always returns InvalidResponse, so if we called it, we'd synthesize "done" instead of getting "my summary".
        let runner = FailingMockRunner;
        let result = parse_success_output("999", "claude", &runner, ndjson);

        let parsed = result.expect("should parse successfully via find_agent_result");
        assert_eq!(parsed.response.status, "done");
        assert_eq!(parsed.response.summary, "my summary");
    }

    #[test]
    fn parse_success_output_treats_agent_result_is_error_as_authoritative() {
        // Here we test that if find_agent_result returns is_error=true, we return AgentFailed
        // instead of falling back to synthesis or agent_runner.parse_response.
        let ndjson = r#"{"type":"result","subtype":"success","is_error":true,"result":"I failed because of reasons"}"#;

        let runner = FailingMockRunner;
        let result = parse_success_output("999", "claude", &runner, ndjson);

        let err = result.expect_err("should return error");
        match err {
            agents::AgentError::AgentFailed { message } => {
                assert_eq!(message, "I failed because of reasons");
            }
            _ => panic!("expected AgentFailed, got {err:?}"),
        }
    }

    #[test]
    fn nonzero_exit_with_ndjson_success_is_parsed_as_success() {
        // Simulate an agent that exits with non-zero code but emits NDJSON result
        // with is_error=false and a JSON result payload. The runner should accept
        // this as success because the NDJSON result is authoritative.
        let ndjson = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":1234,"result":"{\"status\":\"done\",\"summary\":\"All good\"}","usage":{"input_tokens":10,"output_tokens":5}}"#;

        // Use a runner that would normally fail parsing to ensure we're using
        // the find_agent_result path. FailingMockRunner.parse_response returns
        // InvalidResponse, but parse_success_output should short-circuit.
        let runner = FailingMockRunner;

        // Construct a fake SessionOutput
        let session_output = session::SessionOutput {
            exit_code: 1,
            raw_stdout: ndjson.to_string(),
            raw_stderr: String::new(),
            elapsed_secs: Some(1),
        };

        // Call the helper we added via the task runner logic by reusing the
        // parse_success_output directly since it encapsulates the same behavior.
        let parsed =
            parse_success_output("task-ndjson", "claude", &runner, &session_output.raw_stdout)
                .expect("should parse NDJSON success");
        assert_eq!(parsed.response.status, "done");
        assert_eq!(parsed.response.summary, "All good");
        assert_eq!(parsed.input_tokens, Some(10));
        assert_eq!(parsed.output_tokens, Some(5));
    }

    // ── agent label update after reroute ─────────────────────────────────────

    /// Backend that wraps TrackingBackend but can be configured to fail label ops.
    struct LabelFailingBackend {
        inner: TrackingBackend,
        /// If set, set_labels returns this error.
        set_labels_err: Option<String>,
    }

    impl LabelFailingBackend {
        fn new() -> Self {
            Self {
                inner: TrackingBackend::new(),
                set_labels_err: None,
            }
        }
        fn with_set_labels_err(mut self, err: impl Into<String>) -> Self {
            self.set_labels_err = Some(err.into());
            self
        }
    }

    impl Default for LabelFailingBackend {
        fn default() -> Self {
            Self::new()
        }
    }

    #[async_trait]
    impl crate::backends::ExternalBackend for LabelFailingBackend {
        fn name(&self) -> &str {
            "label_failing"
        }
        async fn create_task(
            &self,
            _t: &str,
            _b: &str,
            _l: &[String],
        ) -> anyhow::Result<ExternalId> {
            Ok(ExternalId("new".to_string()))
        }
        async fn get_task(&self, id: &ExternalId) -> anyhow::Result<ExternalTask> {
            self.inner.get_task(id).await
        }
        async fn list_by_status(&self, _s: Status) -> anyhow::Result<Vec<ExternalTask>> {
            Ok(vec![])
        }
        async fn list_routable(&self) -> anyhow::Result<Vec<ExternalTask>> {
            Ok(vec![])
        }
        async fn post_comment(&self, id: &ExternalId, body: &str) -> anyhow::Result<()> {
            self.inner.post_comment(id, body).await
        }
        async fn set_labels(&self, id: &ExternalId, labels: &[String]) -> anyhow::Result<()> {
            if let Some(ref err) = self.set_labels_err {
                return Err(anyhow::anyhow!("{}", err));
            }
            self.inner.set_labels(id, labels).await
        }
        async fn remove_label(&self, id: &ExternalId, label: &str) -> anyhow::Result<()> {
            self.inner.remove_label(id, label).await
        }
        async fn get_sub_issues(&self, _id: &ExternalId) -> anyhow::Result<Vec<ExternalId>> {
            Ok(vec![])
        }
        async fn create_sub_task(
            &self,
            parent: &ExternalId,
            title: &str,
            body: &str,
            labels: &[String],
        ) -> anyhow::Result<ExternalId> {
            self.inner
                .create_sub_task(parent, title, body, labels)
                .await
        }
        async fn ensure_status_label(&self, label: &str) -> anyhow::Result<()> {
            self.inner.ensure_status_label(label).await
        }
        async fn update_status(&self, id: &ExternalId, status: Status) -> anyhow::Result<()> {
            self.inner.update_status(id, status).await
        }
        async fn get_authenticated_user(&self) -> anyhow::Result<Option<String>> {
            self.inner.get_authenticated_user().await
        }
        async fn get_mentions(&self, _s: &str) -> anyhow::Result<Vec<Mention>> {
            Ok(vec![])
        }
        async fn health_check(&self) -> anyhow::Result<()> {
            self.inner.health_check().await
        }
    }

    /// `update_agent_label_after_reroute` returns `false` when `set_labels` fails.
    ///
    /// This tests the extracted helper directly, bypassing the full `run_with_context`
    /// path (which would fail earlier on worktree setup in a unit-test environment).
    #[tokio::test]
    async fn update_agent_label_returns_false_on_set_labels_failure() {
        let task = ExternalTask {
            id: ExternalId("test-reroute-1".to_string()),
            title: "Test".to_string(),
            body: "body".to_string(),
            state: "open".to_string(),
            labels: vec![],
            author: "bot".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: "".to_string(),
        };
        let backend: Arc<dyn crate::backends::ExternalBackend> =
            Arc::new(LabelFailingBackend::new().with_set_labels_err("network error"));

        let result =
            update_agent_label_after_reroute("owner/repo", &task, &backend, "claude", "codex")
                .await;

        assert!(
            !result,
            "update_agent_label_after_reroute should return false when set_labels fails"
        );
    }

    /// `update_agent_label_after_reroute` returns `true` when `set_labels` succeeds.
    #[tokio::test]
    async fn update_agent_label_returns_true_on_success() {
        let task = ExternalTask {
            id: ExternalId("test-reroute-2".to_string()),
            title: "Test".to_string(),
            body: "body".to_string(),
            state: "open".to_string(),
            labels: vec![],
            author: "bot".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: "".to_string(),
        };
        // LabelFailingBackend with no injected error delegates to TrackingBackend (always Ok).
        let backend: Arc<dyn crate::backends::ExternalBackend> =
            Arc::new(LabelFailingBackend::new());

        let result =
            update_agent_label_after_reroute("owner/repo", &task, &backend, "claude", "codex")
                .await;

        // ensure_label is skipped in #[cfg(test)], set_labels succeeds via mock.
        assert!(
            result,
            "update_agent_label_after_reroute should return true when set_labels succeeds"
        );
    }

    // ── NDJSON envelope fallback (issue #3078) ────────────────────────────────

    /// Pattern A: agent emits domain JSON as result (no `status` field).
    /// NDJSON envelope says is_error=false, subtype=success.
    /// Was previously returning InvalidResponse → task re-ran.
    #[test]
    fn parse_success_output_ndjson_envelope_fallback_domain_json_result() {
        let domain_result = r#"{"file_path":"md/journal/2026-05-08-morning-briefing.md","summary":"Morning briefing filed","health":{"status":"ok"},"priorities":[]}"#;
        let ndjson = format!(
            r#"{{"type":"result","subtype":"success","is_error":false,"terminal_reason":"completed","result":{domain_result:?},"usage":{{"input_tokens":3000,"output_tokens":400}}}}"#
        );

        let runner = agents::get_runner("glm");
        let parsed = parse_success_output("149200", "glm", &*runner, &ndjson)
            .expect("should synthesize done from NDJSON envelope when result is domain JSON");

        assert_eq!(parsed.response.status, "done");
        assert!(
            parsed.response.summary.contains("file_path") || !parsed.response.summary.is_empty(),
            "summary should be populated from domain JSON result text"
        );
        assert_eq!(parsed.input_tokens, Some(3000));
        assert_eq!(parsed.output_tokens, Some(400));
    }

    /// Pattern B: agent emits plain prose as result, no completion keywords.
    /// NDJSON envelope says is_error=false, subtype=success.
    /// Was previously returning InvalidResponse → task re-ran.
    #[test]
    fn parse_success_output_ndjson_envelope_fallback_plain_prose_result() {
        let prose = "PR #1008 now has zero diff vs main. The reverted ledger changes are gone and the branch is clean.";
        let ndjson = format!(
            r#"{{"type":"result","subtype":"success","is_error":false,"terminal_reason":"completed","result":{prose:?},"usage":{{"input_tokens":2500,"output_tokens":300}}}}"#
        );

        let runner = agents::get_runner("claude");
        let parsed = parse_success_output("149164", "claude", &*runner, &ndjson)
            .expect("should synthesize done from NDJSON envelope when result is plain prose");

        assert_eq!(parsed.response.status, "done");
        assert_eq!(parsed.response.summary, prose);
        assert_eq!(parsed.input_tokens, Some(2500));
        assert_eq!(parsed.output_tokens, Some(300));
    }

    /// Verify that is_error=true in NDJSON envelope still returns AgentFailed,
    /// not "done" — the envelope fallback must not override explicit errors.
    #[test]
    fn parse_success_output_ndjson_envelope_fallback_skipped_when_is_error_true() {
        let ndjson = r#"{"type":"result","subtype":"error","is_error":true,"result":"Something went wrong with no status"}"#;

        let runner = agents::get_runner("claude");
        let err = parse_success_output("149999", "claude", &*runner, ndjson)
            .expect_err("is_error=true should return AgentFailed");

        assert!(matches!(err, agents::AgentError::AgentFailed { .. }));
    }

    // ── terminal_reason:completed + exit 1 detection (issue #3087) ─────────────

    /// kimi/minimax sometimes emit valid NDJSON with terminal_reason=completed
    /// but exit 1. In that case parse_success_output may fail (e.g. empty result
    /// text that synthesize can't parse), then parse_session_output would
    /// normally fall back to classify_error — grabbing a garbled JSON tail and
    /// producing a spurious Unknown/Auth error. The fix intercepts this and
    /// returns InvalidResponse (soft error, re-route) instead.
    #[test]
    fn parse_session_output_exit1_with_terminal_reason_completed_returns_invalid_response() {
        // Empty result text causes synthesize_response_from_text to return None
        // (empty/whitespace-only input), so parse_success_output returns
        // InvalidResponse. With the fix, parse_session_output intercepts the
        // "terminal_reason":"completed" signal before classify_error and
        // returns InvalidResponse (re-route) instead of a garbled error.
        let ndjson = r#"{"type":"result","subtype":"success","is_error":false,"terminal_reason":"completed","result":"","usage":{"input_tokens":1000,"output_tokens":100}}"#;
        let session = session::SessionOutput {
            exit_code: 1,
            raw_stdout: ndjson.to_string(),
            raw_stderr: String::new(),
            elapsed_secs: Some(30),
        };

        let runner = agents::get_runner("claude");
        let result = parse_session_output("149298", "claude", &*runner, &session);

        let err = result.expect_err("should be an error, not Ok");
        assert!(
            matches!(err, agents::AgentError::InvalidResponse { .. }),
            "should be InvalidResponse, got {err:?}"
        );
    }

    // ── GLM cost-telemetry exit-1 detection (issue #3094) ──────────────────────

    /// GLM exits with code 1 but stdout is only cost telemetry JSON (contains
    /// "costUSD":). This must return InvalidResponse (soft re-route) instead of
    /// a hard Failed classification with spurious agent cooldown.
    #[test]
    fn parse_session_output_exit1_with_cost_telemetry_returns_invalid_response() {
        // GLM emits cost/usage JSON on clean exit; exit code 1 is a false failure.
        let telemetry = r#"{"type":"result","subtype":"success","is_error":false,"result":"","costUSD":0.042,"usage":{"input_tokens":5000,"output_tokens":800}}"#;
        let session = session::SessionOutput {
            exit_code: 1,
            raw_stdout: telemetry.to_string(),
            raw_stderr: String::new(),
            elapsed_secs: Some(120),
        };

        let runner = agents::get_runner("glm");
        let result = parse_session_output("149347", "glm", &*runner, &session);

        let err = result.expect_err("should be an error, not Ok");
        assert!(
            matches!(err, agents::AgentError::InvalidResponse { .. }),
            "glm cost-telemetry exit-1 should return InvalidResponse, got {err:?}"
        );
    }

    /// GLM with costUSD AND is_error:true — must propagate AgentFailed, not
    /// silently swallow the error via the cost-telemetry guard.
    #[test]
    fn parse_session_output_exit1_cost_telemetry_with_is_error_true_propagates_failure() {
        let telemetry = r#"{"type":"result","subtype":"error","is_error":true,"result":"tool failed","costUSD":0.01,"usage":{"input_tokens":100,"output_tokens":10}}"#;
        let session = session::SessionOutput {
            exit_code: 1,
            raw_stdout: telemetry.to_string(),
            raw_stderr: String::new(),
            elapsed_secs: Some(15),
        };

        let runner = agents::get_runner("glm");
        let result = parse_session_output("149348", "glm", &*runner, &session);

        let err = result.expect_err("should be an error");
        assert!(
            matches!(err, agents::AgentError::AgentFailed { .. }),
            "is_error=true with costUSD must still return AgentFailed, got {err:?}"
        );
    }

    /// terminal_reason=completed must NOT override actual error conditions.
    /// When is_error=true in the NDJSON envelope, AgentFailed must be returned
    /// even if terminal_reason=completed is also present. Only kimi/minimax
    /// agents use find_claude_result which extracts is_error, so use "kimi"
    /// as agent_name.
    #[test]
    fn parse_session_output_exit1_preserves_error_when_is_error_true() {
        let ndjson = r#"{"type":"result","subtype":"error","is_error":true,"terminal_reason":"completed","result":"failed"}"#;
        let session = session::SessionOutput {
            exit_code: 1,
            raw_stdout: ndjson.to_string(),
            raw_stderr: String::new(),
            elapsed_secs: Some(30),
        };

        let runner = agents::get_runner("kimi");
        let result = parse_session_output("149299", "kimi", &*runner, &session);

        let err = result.expect_err("should be an error");
        assert!(
            matches!(err, agents::AgentError::AgentFailed { .. }),
            "is_error=true should return AgentFailed, got {err:?}"
        );
    }
}
