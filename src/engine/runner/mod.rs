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
//! - [`response_handler`] — success path: commit, push, PR, token storage, budget
//! - [`fallback`] — error classification and recovery strategies

pub mod agent;
pub mod agents;
pub mod context;
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
    pub input_tokens: i64,
    pub output_tokens: i64,
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
) -> &'static str {
    match parse_result {
        Err(agents::AgentError::Timeout { .. }) => "timeout",
        Err(agents::AgentError::RateLimit { .. }) => "rate_limit",
        Err(agents::AgentError::InvalidResponse { .. }) => "parse_error",
        Err(_) => "failed",
        Ok(_)
            if matches!(
                status,
                "done" | "in_progress" | "in_review" | "blocked" | "needs_review"
            ) =>
        {
            "success"
        }
        Ok(_) => "failed",
    }
}

fn classify_run_error_type(last_error: &str) -> &'static str {
    if last_error.is_empty() {
        // No error: agent completed successfully and created a PR waiting for review
        "success"
    } else if last_error.contains("timeout") {
        "timeout"
    } else if last_error.contains("rate limit") || last_error.contains("usage limit") {
        "rate_limit"
    } else if last_error.contains("auth") || last_error.contains("billing") {
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
) -> (i64, i64) {
    match parse_result {
        Ok(parsed) => (
            parsed
                .input_tokens
                .or(parsed.response.input_tokens)
                .unwrap_or(0) as i64,
            parsed
                .output_tokens
                .or(parsed.response.output_tokens)
                .unwrap_or(0) as i64,
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
}

fn parse_success_output(
    task_id: &str,
    agent_name: &str,
    agent_runner: &dyn agents::AgentRunner,
    raw_stdout: &str,
) -> Result<agents::ParsedResponse, agents::AgentError> {
    match agent_runner.parse_response(raw_stdout) {
        Ok(parsed) => Ok(parsed),
        Err(agents::AgentError::InvalidResponse { raw }) => {
            if let Some(response) = agents::synthesize_response_from_text(&raw) {
                tracing::warn!(
                    task_id,
                    agent = agent_name,
                    "parse failed, synthesizing response from plain text"
                );
                Ok(agents::ParsedResponse {
                    response,
                    input_tokens: None,
                    output_tokens: None,
                    duration_ms: None,
                })
            } else {
                Err(agents::AgentError::InvalidResponse { raw })
            }
        }
        Err(err) => Err(err),
    }
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
        let orch_home =
            crate::home::orch_home().unwrap_or_else(|_| PathBuf::from("/tmp").join(".orch"));

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
            "budget_warning" => Some(task.budget_warning),
            "budget_exceeded" => Some(task.budget_exceeded.to_string()),
            _ => None,
        }
    }

    async fn build_run_audit(&self, input: RunAuditInput<'_>) -> RunAudit {
        let last_error = self.get_field(input.task_id, "last_error").await;
        let error =
            input
                .error_override
                .or(last_error)
                .unwrap_or_else(|| match input.parse_result {
                    Ok(parsed) => parsed.response.error.clone().unwrap_or_default(),
                    Err(err) => err.to_string(),
                });

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
            outcome: classify_run_outcome(input.status, input.parse_result).to_string(),
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
                    let _ = b
                        .update_status(&id, crate::backends::Status::NeedsReview)
                        .await;

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
        let project_dir = self.resolve_project_dir()?;

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

        // Use agent-specific parsing when exit code is 0, fall back to classify_error
        let agent_runner = agents::get_runner(&init.agent_name);
        let parse_result = if session_output.exit_code == 0 && !session_output.raw_stdout.is_empty()
        {
            parse_success_output(
                task_id,
                &init.agent_name,
                &*agent_runner,
                &session_output.raw_stdout,
            )
        } else if session_output.exit_code != 0 {
            Err(agent_runner.classify_error_with_elapsed(
                session_output.exit_code,
                &session_output.raw_stdout,
                &session_output.raw_stderr,
                session_output.elapsed_secs,
            ))
        } else {
            // Exit 0 but empty output — check stderr for clues
            let combined = format!("{}{}", session_output.raw_stdout, session_output.raw_stderr);
            Err(agents::patterns::classify_from_text(
                session_output.exit_code,
                &combined,
            ))
        };

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
        );

        // Handle outcome: success or error recovery
        let final_status = match parse_result {
            Ok(ref parsed) => {
                let (status, budget_exceeded) = response_handler::handle_success(
                    task_id,
                    parsed.clone(),
                    &init.wt,
                    &init.task_title,
                    &init.agent_name,
                    init.model_name.as_deref(),
                    init.new_attempts,
                    &self.repo,
                    &self.store,
                )
                .await?;
                if budget_exceeded {
                    // Token budget exceeded — clean up tmux session and env secrets
                    session::cleanup_session(task_id, &tmux, &tmux_session).await;
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
                        })
                        .await;
                    return Ok(Some(RunExecution {
                        status,
                        exit_code: Some(session_output.exit_code),
                        audit,
                    }));
                }
                status
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
                        let audit = self
                            .build_run_audit(RunAuditInput {
                                task_id,
                                status: &status,
                                parse_result: &parse_result,
                                raw_stdout: &session_output.raw_stdout,
                                raw_stderr: &session_output.raw_stderr,
                                started_at,
                                error_override: Some(agent_err.to_string()),
                                elapsed_secs: session_output.elapsed_secs,
                            })
                            .await;
                        return Ok(Some(RunExecution {
                            status,
                            exit_code: Some(session_output.exit_code),
                            audit,
                        }));
                    }
                    fallback::ErrorHandleResult::Continue { status } => status,
                }
            }
        };

        // Kill tmux session if still alive
        session::cleanup_session(task_id, &tmux, &tmux_session).await;

        // For no-op done tasks (agent finished with no code changes and no PR),
        // clean up the worktree and branch immediately — tmux session is already
        // dead so the tmux guard won't block removal.
        if final_status == "done" {
            if let Some(ref st) = self.store {
                if let Err(e) =
                    crate::engine::cleanup::cleanup_task_worktree(task_id, &self.repo, st).await
                {
                    tracing::warn!(task_id, error = ?e, "worktree cleanup failed after no-op done");
                }
            }
        }

        // Return final status and the session exit code so callers can record it.
        let audit = self
            .build_run_audit(RunAuditInput {
                task_id,
                status: &final_status,
                parse_result: &parse_result,
                raw_stdout: &session_output.raw_stdout,
                raw_stderr: &session_output.raw_stderr,
                started_at,
                error_override: None,
                elapsed_secs: session_output.elapsed_secs,
            })
            .await;

        Ok(Some(RunExecution {
            status: final_status,
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

        // Store task info for prompt building
        store::store_set(
            &self.store,
            &self.repo,
            task_id,
            &[], // title/body already in store tasks table
        )
        .await;

        // Record run start in task_runs audit trail
        let run_audit_id = if let Some(ref store) = self.store {
            let attempt: i32 = self
                .get_field(task_id, "attempts")
                .await
                .and_then(|s| s.parse().ok())
                .unwrap_or(0)
                + 1;
            if let Ok(Some(store_id)) = store.resolve_task_id(&self.repo, task_id).await {
                store
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
                    .ok()
            } else {
                None
            }
        } else {
            None
        };

        // Run the task
        let run_result = self
            .run(task_id, agent, model, Some(&**backend), &started_at)
            .await?;

        // If the runner guard skipped the task, do not re-post stale data as a new comment.
        let (status, exit_code_opt, run_audit) = match run_result {
            Some(result) => (result.status, result.exit_code, result.audit),
            None => {
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
                    store::store_set(
                        &self.store,
                        &self.repo,
                        task_id,
                        &[("delegations", serde_json::json!([]))],
                    )
                    .await;
                }
                Ok(_) => {} // empty list, nothing to do
                Err(e) => {
                    tracing::error!(task_id, error = %e, "corrupt delegations JSON — clearing");
                    store::store_set(
                        &self.store,
                        &self.repo,
                        task_id,
                        &[("delegations", serde_json::json!([]))],
                    )
                    .await;
                    store::store_set(
                        &self.store,
                        &self.repo,
                        task_id,
                        &[(
                            "last_error",
                            serde_json::json!(format!("delegation parse failed: {e}")),
                        )],
                    )
                    .await;
                }
            }
        }

        // Post result to GitHub
        let summary = self.get_field(task_id, "summary").await.unwrap_or_default();
        let last_error = self
            .get_field(task_id, "last_error")
            .await
            .unwrap_or_default();

        // Determine weight signal based on outcome
        // Only treat explicit rate-limit messages (or usage limit) as rate-limited.
        // Do NOT consider generic "rerouted" text as evidence of a rate limit —
        // reroutes can happen for timeouts, auth errors, missing tools, etc.
        let is_rate_limited =
            last_error.contains("usage limit") || last_error.contains("rate limit");
        let is_rerouted = status == "new" || status == "routed";
        let weight_signal = if is_rerouted && is_rate_limited {
            WeightSignal::RateLimited {
                agent: agent_name.clone(),
            }
        } else if status == "done"
            || status == "needs_review"
            || status == "in_progress"
            || status == "in_review"
        {
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
                let _ = store
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
                    .await;
            }
        }

        // If task was rerouted (status=routed or new after run), update GitHub agent label
        // so the router doesn't re-route back to the same failed agent.
        if is_rerouted && !is_internal_id(task_id) {
            let new_agent = self.get_field(task_id, "agent").await.unwrap_or_default();
            if !new_agent.is_empty() && new_agent != agent_name {
                // Remove old agent label, ensure new one exists, then add it.
                // set_labels (add_labels) fails with 422 if the label doesn't exist
                // in the repo yet — ensure_label creates it first.
                let old_label = format!("agent:{agent_name}");
                backend.remove_label(&task.id, &old_label).await.ok();
                let new_label = format!("agent:{new_agent}");
                match crate::github::http::GhHttp::new() {
                    Ok(gh) => {
                        gh.ensure_label(
                            &self.repo,
                            &new_label,
                            crate::github::http::status_label_color(&new_label),
                            &format!("Agent: {new_agent}"),
                        )
                        .await
                        .ok();
                        backend.set_labels(&task.id, &[new_label]).await.ok();
                    }
                    Err(e) => {
                        tracing::warn!(task_id, err = %e, "GhHttp::new() failed — agent label not updated after failover");
                    }
                }
                tracing::info!(
                    task_id,
                    from = %agent_name,
                    to = %new_agent,
                    "updated GitHub agent label after failover"
                );
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

        // Check for budget warnings and append to comment
        let budget_warning = self
            .get_field(task_id, "budget_warning")
            .await
            .unwrap_or_default();
        let budget_exceeded = self
            .get_field(task_id, "budget_exceeded")
            .await
            .unwrap_or_default();

        // Post comment (scan for secrets before posting to GitHub)
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
        let mut raw_comment = if !summary.is_empty() {
            format!("[{now}] {status}: {summary}")
        } else if !last_error.is_empty() {
            format!("[{now}] {status}: {last_error}")
        } else {
            format!("[{now}] {status}")
        };

        // Append budget warnings to the GitHub comment
        if budget_exceeded == "true" {
            let cost = store::get_cost_estimate(&self.store, &self.repo, task_id).await;
            let total_tokens = store::get_total_tokens(&self.store, &self.repo, task_id).await;
            raw_comment.push_str(&format!(
                "\n\n> **Budget exceeded**: {} tokens used (${:.4}). Task paused for review.",
                total_tokens, cost.total_cost_usd
            ));
        } else if !budget_warning.is_empty() {
            raw_comment.push_str(&format!("\n\n> **Budget warning**: {budget_warning}"));
        }

        // Append visible attribution footer (matches PR body style)
        let model_str = model.map(|m| format!(" using `{m}`")).unwrap_or_default();
        raw_comment.push_str(&format!(
            "\n\n---\n*Created by {agent_name}[bot] via [Orch](https://github.com/gabrielkoerich/orch){model_str}*"
        ));

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
    fn resolve_project_dir(&self) -> anyhow::Result<PathBuf> {
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
                    if repo == self.repo && path.exists() {
                        return Ok(path);
                    }
                }
            }
        }

        // Legacy: check config project_dir
        if let Ok(dir) = config::get("project_dir") {
            if !dir.is_empty() {
                let path = PathBuf::from(&dir);
                if path.exists() {
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
            if bare.exists() {
                return Ok(bare);
            }
        }

        // Fall back to current directory
        Ok(std::env::current_dir()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::{ExternalId, ExternalTask, Mention, Status};
    use crate::engine::runner::response_handler::safe_utf8_tail;
    use async_trait::async_trait;
    use once_cell::sync::Lazy;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    static ENV_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

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

    // ── resolve_project_dir ──────────────────────────────────────────────────

    #[test]
    fn resolve_project_dir_uses_project_dir_env() {
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
        let result = runner.resolve_project_dir();
        std::env::remove_var("PROJECT_DIR");

        assert!(
            result.is_ok(),
            "resolve_project_dir should succeed with PROJECT_DIR set"
        );
        assert_eq!(result.unwrap(), dir.path());
    }

    #[test]
    fn resolve_project_dir_empty_project_dir_env_falls_through() {
        // An empty PROJECT_DIR should be ignored and fall through to other logic.
        let runner = TaskRunner::new("owner/testrepo-nonexistent".to_string());
        std::env::set_var("PROJECT_DIR", "");
        let result = runner.resolve_project_dir();
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
        std::fs::create_dir_all(&orch_home).unwrap();
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
        std::fs::create_dir_all(&orch_home).unwrap();
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

    fn weight_signal_for(status: &str, is_rate_limited: bool, agent: &str) -> WeightSignal {
        if (status == "new" || status == "routed") && is_rate_limited {
            WeightSignal::RateLimited {
                agent: agent.to_string(),
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
        let signal = weight_signal_for("needs_review", false, "claude");
        assert!(
            matches!(signal, WeightSignal::Success { agent } if agent == "claude"),
            "needs_review should produce WeightSignal::Success"
        );
    }

    #[test]
    fn weight_signal_success_for_all_done_statuses() {
        for status in ["done", "needs_review", "in_progress", "in_review"] {
            let signal = weight_signal_for(status, false, "codex");
            assert!(
                matches!(signal, WeightSignal::Success { agent } if agent == "codex"),
                "{status} should produce WeightSignal::Success"
            );
        }
    }

    #[test]
    fn weight_signal_rate_limited_overrides_success() {
        // Rate limit on "new" status (rerouted back) should be RateLimited.
        let signal = weight_signal_for("new", true, "claude");
        assert!(matches!(signal, WeightSignal::RateLimited { .. }));
    }

    #[test]
    fn weight_signal_blocked_status() {
        let signal = weight_signal_for("blocked", false, "claude");
        assert!(matches!(signal, WeightSignal::Blocked));
    }

    #[test]
    fn weight_signal_none_for_unknown_status() {
        let signal = weight_signal_for("routed", false, "claude");
        assert!(matches!(signal, WeightSignal::None));
    }
}
