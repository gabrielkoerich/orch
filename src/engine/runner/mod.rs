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

pub mod agent;
pub mod agents;
pub mod context;
pub mod git_ops;
pub mod response;
pub mod worktree;

use crate::backends::{ExternalBackend, ExternalId, ExternalTask, Status};
use crate::config;
use crate::db::{Db, InsertTaskMetric};
use crate::engine::router::{get_route_result, RouteResult};
use crate::security;
use crate::sidecar;
use crate::tmux::TmuxManager;
use chrono::Utc;
pub use response::WeightSignal;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::time::{timeout, Duration};

/// Task runner configuration.
pub struct TaskRunner {
    /// Repository slug (owner/repo)
    repo: String,
    /// Path to the orchestrator home directory
    orch_home: PathBuf,
    /// Database for storing metrics
    db: Option<Arc<Db>>,
}

impl TaskRunner {
    pub fn new(repo: String) -> Self {
        let orch_home =
            crate::home::orch_home().unwrap_or_else(|_| PathBuf::from("/tmp").join(".orch"));

        Self {
            repo,
            orch_home,
            db: None,
        }
    }

    /// Set the database reference for metrics recording.
    pub fn with_db(mut self, db: Arc<Db>) -> Self {
        self.db = Some(db);
        self
    }

    /// Maximum task execution time (30 minutes).
    const TASK_TIMEOUT: Duration = Duration::from_secs(30 * 60);

    /// Run a task through the full execution pipeline.
    ///
    /// This is the main entry point called by the engine dispatch loop.
    pub async fn run(
        &self,
        task_id: &str,
        agent: Option<&str>,
        model: Option<&str>,
    ) -> anyhow::Result<()> {
        // Record start time for metrics
        let started_at = Utc::now();

        tracing::info!(
            task_id,
            agent = agent.unwrap_or("default"),
            model = model.unwrap_or("default"),
            "starting task execution"
        );

        // Load sidecar state
        let attempts: u32 = sidecar::get(task_id, "attempts")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        // Guard: skip needs_review tasks
        let current_status = sidecar::get(task_id, "status").unwrap_or_default();
        if current_status == "needs_review" {
            tracing::info!(task_id, "skipping needs_review task");
            return Ok(());
        }

        // Check max attempts
        let max_attempts: u32 = config::get("workflow.max_attempts")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5);

        if attempts >= max_attempts {
            tracing::warn!(
                task_id,
                attempts,
                max_attempts,
                "exceeded max attempts, blocking task"
            );
            // Set sidecar status to blocked — run_with_context() will propagate
            // to GitHub. Owner can /retry to reset attempts and re-dispatch.
            sidecar::set(
                task_id,
                &[
                    "status=blocked".to_string(),
                    format!(
                        "last_error=exceeded max attempts ({attempts}/{max_attempts}). Use `/retry` to reset."
                    ),
                ],
            )?;
            return Ok(());
        }

        // Resolve project directory
        let project_dir = self.resolve_project_dir()?;

        // Load title from sidecar for branch naming (set by run_with_context before run())
        let title_for_branch = sidecar::get(task_id, "title").unwrap_or_default();

        // Set up worktree
        let wt = worktree::setup_worktree(task_id, &title_for_branch, &project_dir).await?;

        // Get routing result
        let route_result = get_route_result(task_id).ok();

        let agent_name = agent
            .map(String::from)
            .or_else(|| route_result.as_ref().map(|r| r.agent.clone()))
            .unwrap_or_else(|| "claude".to_string());

        let model_name = model
            .map(String::from)
            .or_else(|| route_result.as_ref().and_then(|r| r.model.clone()));

        // Build context
        // Note: we'd need a backend reference to build full context,
        // but for now we build what we can locally
        let task_context = context::load_task_context(task_id);
        let project_instructions = context::build_project_instructions(&wt.work_dir);
        let repo_tree = context::build_repo_tree(&wt.work_dir).await;

        let selected_skills = route_result
            .as_ref()
            .map(|r| r.selected_skills.clone())
            .unwrap_or_default();
        let skills_docs = context::build_skills_docs(&selected_skills);

        let git_diff = if attempts > 0 {
            context::build_git_diff(&wt.work_dir, &wt.default_branch).await
        } else {
            String::new()
        };

        // Load PR review context from sidecar (for re-dispatch after review changes requested)
        let pr_review_context = context::load_pr_review_context(task_id);

        let ctx = context::TaskContext {
            task_context,
            parent_context: String::new(), // Requires backend
            project_instructions,
            skills_docs,
            repo_tree,
            git_diff,
            issue_comments: String::new(), // Requires backend
            pr_review_context,
            memory: vec![], // Will be loaded on retries
        };

        // Build a minimal ExternalTask for prompt building
        let task_title =
            sidecar::get(task_id, "title").unwrap_or_else(|_| format!("Task #{task_id}"));
        let task_body = sidecar::get(task_id, "body").unwrap_or_default();
        let pseudo_task = ExternalTask {
            id: ExternalId(task_id.to_string()),
            title: task_title.clone(),
            body: task_body,
            state: "open".to_string(),
            labels: vec![],
            author: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            url: String::new(),
        };

        // Build prompts
        let system_prompt = agent::build_system_prompt(&pseudo_task, &ctx, route_result.as_ref());
        let agent_message = agent::build_agent_message(&pseudo_task, &ctx, attempts);

        // Git identity
        let git_name = config::get("git.name").unwrap_or_else(|_| format!("{agent_name}[bot]"));
        let git_email = config::get("git.email")
            .unwrap_or_else(|_| format!("{agent_name}[bot]@users.noreply.github.com"));

        // Output file in per-task attempt directory (attempt = attempts + 1, set below)
        let next_attempt = attempts + 1;
        let attempt_dir = crate::home::task_attempt_dir(&self.repo, task_id, next_attempt)?;
        let output_file = attempt_dir.join("output.json");

        // Build sandbox disallowed tools
        let mut disallowed_tools = vec!["Bash(rm *)".to_string(), "Bash(rm -*)".to_string()];

        // Sandbox: block access to main project dir
        if wt.work_dir != wt.main_project_dir {
            let main_str = wt.main_project_dir.to_string_lossy();
            disallowed_tools.extend([
                format!("Bash(cd {main_str}*)"),
                format!("Read({main_str}/*)"),
                format!("Write({main_str}/*)"),
                format!("Edit({main_str}/*)"),
            ]);
        }

        // Timeout
        let timeout_seconds: u64 = config::get("workflow.timeout_seconds")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1800);

        // Build agent invocation
        let model_for_invocation = model_name.clone();
        let invocation = agent::AgentInvocation {
            agent: agent_name.clone(),
            model: model_for_invocation,
            work_dir: wt.work_dir.clone(),
            system_prompt,
            agent_message,
            task_id: task_id.to_string(),
            branch: wt.branch.clone(),
            main_project_dir: wt.main_project_dir.clone(),
            disallowed_tools,
            git_author_name: git_name,
            git_author_email: git_email,
            output_file: output_file.clone(),
            timeout_seconds,
            repo: self.repo.clone(),
            attempt: next_attempt,
        };

        // Increment attempts
        let new_attempts = attempts + 1;
        sidecar::set(task_id, &[format!("attempts={new_attempts}")])?;

        // Spawn in tmux
        let tmux = TmuxManager::new();
        let session = agent::spawn_in_tmux(&tmux, &invocation).await?;

        // Wait for completion with timeout
        let poll_interval = Duration::from_secs(5);
        let wait_result = timeout(
            Self::TASK_TIMEOUT,
            tmux.wait_for_completion(&session, poll_interval),
        )
        .await;

        match wait_result {
            Ok(Ok(_output)) => {
                tracing::info!(task_id, "agent session completed");
            }
            Ok(Err(e)) => {
                tracing::error!(task_id, ?e, "error waiting for session");
            }
            Err(_) => {
                tracing::error!(task_id, "agent timed out after 30 minutes");
                tmux.kill_session(&session).await?;
            }
        }

        // Read exit code — check per-task attempt dir first, fall back to legacy
        let exit_code: i32 = {
            let attempt_exit = attempt_dir.join("exit.txt");
            let legacy_exit =
                sidecar::state_file(&format!("exit-{task_id}.txt")).unwrap_or_else(|_| {
                    self.orch_home
                        .join("state")
                        .join(format!("exit-{task_id}.txt"))
                });

            std::fs::read_to_string(&attempt_exit)
                .or_else(|_| std::fs::read_to_string(&legacy_exit))
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(-1)
        };

        // Get per-agent runner for parsing
        let agent_runner = agents::get_runner(&agent_name);

        // Read raw output + stderr
        let raw_stdout = response::read_output_file(task_id, &output_file, &self.repo);
        let stderr_path_attempt = attempt_dir.join("stderr.txt");
        let stderr_path_legacy = sidecar::state_file(&format!("stderr-{task_id}.txt"))
            .unwrap_or_else(|_| PathBuf::from(format!("/tmp/stderr-{task_id}.txt")));
        let raw_stderr = std::fs::read_to_string(&stderr_path_attempt)
            .or_else(|_| std::fs::read_to_string(&stderr_path_legacy))
            .unwrap_or_default();

        // Log raw output for debugging agent failures
        let stdout_len = raw_stdout.len();
        let stderr_len = raw_stderr.len();
        let stdout_tail: String = raw_stdout
            .chars()
            .rev()
            .take(500)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        let stderr_tail: String = raw_stderr
            .chars()
            .rev()
            .take(500)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        tracing::info!(
            task_id,
            exit_code,
            stdout_len,
            stderr_len,
            stdout_tail = %stdout_tail,
            stderr_tail = %stderr_tail,
            "agent raw output"
        );

        // Use agent-specific parsing when exit code is 0, fall back to classify_error
        let parse_result = if exit_code == 0 && !raw_stdout.is_empty() {
            agent_runner.parse_response(&raw_stdout)
        } else if exit_code != 0 {
            Err(agent_runner.classify_error(exit_code, &raw_stdout, &raw_stderr))
        } else {
            // Exit 0 but empty output — check stderr for clues
            let combined = format!("{raw_stdout}{raw_stderr}");
            Err(agents::patterns::classify_from_text(exit_code, &combined))
        };

        // Write structured result.json for deterministic testing and debugging
        {
            let result_json = match &parse_result {
                Ok(parsed) => {
                    serde_json::json!({
                        "outcome": "success",
                        "agent": agent_name,
                        "model": model_name.as_deref().unwrap_or("default"),
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
                        "model": model_name.as_deref().unwrap_or("default"),
                        "exit_code": exit_code,
                        "attempt": new_attempts,
                        "error_class": agents::error_class_name(agent_err),
                        "error_message": agent_err.to_string(),
                        "stderr_tail": &raw_stderr[raw_stderr.len().saturating_sub(2000)..],
                        "stdout_tail": &raw_stdout[raw_stdout.len().saturating_sub(2000)..],
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

        match parse_result {
            Ok(parsed) => {
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
                    if let Err(e) = git_ops::auto_commit(
                        &wt.work_dir,
                        task_id,
                        &task_title,
                        &agent_name,
                        new_attempts,
                    )
                    .await
                    {
                        tracing::error!(task_id, error = ?e, "auto commit failed");
                        sidecar::set(task_id, &[format!("last_error=auto commit failed: {e}")])?;
                    }

                    // Push (only create PR when push succeeds)
                    let can_create_pr = match git_ops::push_branch(
                        &wt.work_dir,
                        &wt.branch,
                        &wt.default_branch,
                    )
                    .await
                    {
                        Ok(true) => true,
                        Ok(false) => {
                            tracing::error!(
                                task_id,
                                "push skipped or failed, skipping PR creation"
                            );
                            sidecar::set(
                                task_id,
                                &["last_error=push skipped or failed".to_string()],
                            )?;
                            false
                        }
                        Err(e) => {
                            tracing::error!(task_id, error = ?e, "push failed");
                            sidecar::set(task_id, &[format!("last_error=push failed: {e}")])?;
                            false
                        }
                    };

                    if can_create_pr {
                        // Create PR
                        match git_ops::create_pr_if_needed(
                            &wt.work_dir,
                            &wt.branch,
                            &task_title,
                            &resp.summary,
                            &resp.accomplished,
                            &resp.remaining,
                            &resp.files,
                            task_id,
                            &agent_name,
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
                                sidecar::set(
                                    task_id,
                                    &[format!("last_error=create PR failed: {e}")],
                                )?;
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
                // If agent said "done" and a PR exists, set in_review instead
                let final_status = if resp.status == "done" && has_pr {
                    "in_review"
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
                    let model = model_name.as_deref().unwrap_or("haiku");
                    if let Err(e) = sidecar::store_token_usage(task_id, input, output, model) {
                        tracing::warn!(task_id, ?e, "failed to store token usage");
                    }
                }

                // Store learnings for memory (for future retries)
                response::store_learnings_from_response(
                    task_id,
                    new_attempts,
                    &agent_name,
                    model_name.as_deref(),
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
                    sidecar::set(
                        task_id,
                        &[
                            "status=needs_review".to_string(),
                            format!(
                                "last_error=token budget exceeded: {}/{} tokens (${:.4})",
                                total_tokens, max_tokens, cost.total_cost_usd
                            ),
                            "budget_exceeded=true".to_string(),
                        ],
                    )?;
                    return Ok(());
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
            }
            Err(agent_err) => {
                tracing::warn!(
                    task_id,
                    agent = agent_name,
                    error = %agent_err,
                    "agent error, attempting recovery"
                );

                // Map AgentError to RetryableError for the existing handle_failover()
                let (retryable, error_msg) = match &agent_err {
                    agents::AgentError::RateLimit { message, .. } => (
                        response::RetryableError::UsageLimit,
                        format!("{agent_name} rate limit: {message}"),
                    ),
                    agents::AgentError::Auth { message } => (
                        response::RetryableError::AuthError,
                        format!("{agent_name} auth error: {message}"),
                    ),
                    agents::AgentError::Timeout { elapsed } => (
                        response::RetryableError::Timeout,
                        format!("{agent_name} timed out after {}s", elapsed.as_secs()),
                    ),
                    agents::AgentError::MissingTool { tool } => (
                        response::RetryableError::MissingTooling,
                        format!("missing tool: {tool}"),
                    ),
                    agents::AgentError::ModelUnavailable { model, .. } => {
                        // Record model-specific cooldown (1 hour ban)
                        response::record_model_failure(&agent_name, model);

                        // Try next model before switching agent
                        let models = agent_runner.available_models();
                        let current_model = model_name.as_deref().unwrap_or("");
                        let next_model = models.iter().find(|m| {
                            m.as_str() != current_model
                                && m.as_str() != model
                                && !response::is_model_in_cooldown(&agent_name, m)
                        });
                        if let Some(next) = next_model {
                            tracing::info!(task_id, model = %next, "retrying with different model");
                            sidecar::set(
                                task_id,
                                &[
                                    format!("model={next}"),
                                    "status=new".to_string(),
                                    format!("last_error=model {model} unavailable, trying {next}"),
                                ],
                            )?;
                            // Skip normal failover — we're retrying same agent with different model
                            self.record_metrics(
                                task_id,
                                &agent_name,
                                &model_name,
                                &route_result,
                                &started_at,
                                attempts,
                            )
                            .await;
                            return Ok(());
                        }
                        (
                            response::RetryableError::Failed,
                            format!("model {model} unavailable"),
                        )
                    }
                    agents::AgentError::ContextOverflow { .. } => {
                        // Could truncate and retry, but for now treat as failed
                        (
                            response::RetryableError::Failed,
                            format!("{agent_name} context overflow"),
                        )
                    }
                    agents::AgentError::WaitingForInput { message } => {
                        // Requires human — skip failover, go straight to needs_review
                        sidecar::set(
                            task_id,
                            &[
                                "status=needs_review".to_string(),
                                format!("last_error=waiting for input: {message}"),
                            ],
                        )?;
                        self.record_metrics(
                            task_id,
                            &agent_name,
                            &model_name,
                            &route_result,
                            &started_at,
                            attempts,
                        )
                        .await;
                        return Ok(());
                    }
                    agents::AgentError::PermissionDenied { message } => (
                        response::RetryableError::Failed,
                        format!("permission denied: {message}"),
                    ),
                    agents::AgentError::InvalidResponse { .. } => (
                        response::RetryableError::Failed,
                        format!("{agent_name} invalid response"),
                    ),
                    agents::AgentError::AgentFailed { message } => (
                        response::RetryableError::Failed,
                        format!("{agent_name} failed: {message}"),
                    ),
                    agents::AgentError::Unknown { exit_code, message } => (
                        response::RetryableError::Failed,
                        format!("{agent_name} exit {exit_code}: {message}"),
                    ),
                };

                // Record rate limit in DB for rate-limit and auth errors
                if let Some(ref db) = self.db {
                    let error_type_str = match retryable {
                        response::RetryableError::UsageLimit => "rate",
                        response::RetryableError::AuthError => "budget",
                        _ => "error",
                    };
                    let _ = db
                        .record_rate_limit(&agent_name, error_type_str, Some(task_id))
                        .await;
                }

                // Try free models as last resort before giving up
                let chain = response::get_reroute_chain(task_id);
                let available: Vec<String> = ["claude", "codex", "opencode", "kimi", "minimax"]
                    .iter()
                    .filter(|a| crate::cmd_cache::command_exists(a))
                    .map(|s| s.to_string())
                    .collect();

                let all_agents_tried = {
                    let chain_set: std::collections::HashSet<&str> = if chain.is_empty() {
                        std::collections::HashSet::new()
                    } else {
                        chain.split(',').collect()
                    };
                    !available
                        .iter()
                        .any(|a| a != &agent_name && !chain_set.contains(a.as_str()))
                };

                if all_agents_tried {
                    // All agents exhausted — try free models via opencode
                    let free = agent_runner.free_models();
                    if !free.is_empty() {
                        let tried_models: String =
                            sidecar::get(task_id, "model_reroute_chain").unwrap_or_default();
                        let tried_set: std::collections::HashSet<&str> =
                            tried_models.split(',').filter(|s| !s.is_empty()).collect();

                        if let Some(free_model) =
                            free.iter().find(|m| !tried_set.contains(m.as_str()))
                        {
                            tracing::info!(task_id, model = %free_model, "last resort: trying free model via opencode");
                            let new_tried = if tried_models.is_empty() {
                                free_model.clone()
                            } else {
                                format!("{tried_models},{free_model}")
                            };
                            sidecar::set(
                                task_id,
                                &[
                                    "agent=opencode".to_string(),
                                    format!("model={free_model}"),
                                    "status=new".to_string(),
                                    format!("model_reroute_chain={new_tried}"),
                                    format!("last_error=all agents exhausted, trying free model {free_model}"),
                                ],
                            )?;
                            self.record_metrics(
                                task_id,
                                &agent_name,
                                &model_name,
                                &route_result,
                                &started_at,
                                attempts,
                            )
                            .await;
                            return Ok(());
                        }
                    }
                }

                let rerouted =
                    response::handle_failover(task_id, &agent_name, retryable, &error_msg);
                if !rerouted {
                    tracing::warn!(task_id, "failover exhausted, task marked needs_review");
                }

                // Store failure memory for retry learning
                response::store_failure_memory(
                    task_id,
                    new_attempts,
                    &agent_name,
                    model_name.as_deref(),
                    &error_msg,
                );
            }
        }

        // Kill tmux session if still alive
        if tmux.session_exists(&session).await {
            if let Err(e) = tmux.kill_session(&session).await {
                tracing::warn!(task_id, error = ?e, "failed to kill tmux session");
            }
        }

        // Record metrics
        self.record_metrics(
            task_id,
            &agent_name,
            &model_name,
            &route_result,
            &started_at,
            attempts,
        )
        .await;

        Ok(())
    }

    /// Record task execution metrics to the database.
    async fn record_metrics(
        &self,
        task_id: &str,
        agent_name: &str,
        model_name: &Option<String>,
        route_result: &Option<RouteResult>,
        started_at: &chrono::DateTime<Utc>,
        attempts: u32,
    ) {
        let completed_at = Utc::now();
        let duration_seconds = (completed_at - *started_at).num_milliseconds() as f64 / 1000.0;

        let final_status = sidecar::get(task_id, "status").unwrap_or_default();
        let outcome = match final_status.as_str() {
            "done" | "in_progress" | "in_review" => "success",
            "needs_review" => {
                let last_error = sidecar::get(task_id, "last_error").unwrap_or_default();
                if last_error.contains("timeout") {
                    "timeout"
                } else if last_error.contains("rate limit") || last_error.contains("usage") {
                    "rate_limit"
                } else if last_error.contains("auth") || last_error.contains("billing") {
                    "auth_error"
                } else {
                    "failed"
                }
            }
            "new" => "rerouted",
            _ => "unknown",
        };

        let complexity = route_result.as_ref().map(|r| r.complexity.clone());
        let files_changed = git_ops::count_changed_files(&PathBuf::from(
            sidecar::get(task_id, "worktree").unwrap_or_default(),
        ))
        .await
        .unwrap_or(0);

        if let Some(ref db) = self.db {
            let error_type: Option<String> = sidecar::get(task_id, "last_error").ok();

            // Read cost data from sidecar
            let usage = sidecar::get_token_usage(task_id);
            let cost = sidecar::get_cost_estimate(task_id);
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
                error_type: error_type.as_deref(),
                input_tokens,
                output_tokens,
                input_cost_usd: input_cost,
                output_cost_usd: output_cost,
                total_cost_usd: total_cost,
            };

            if let Err(e) = db.insert_task_metric(metric).await {
                tracing::warn!(task_id, ?e, "failed to record task metrics");
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

        // Store task info in sidecar for prompt building
        sidecar::set(
            task_id,
            &[
                format!("title={}", task.title),
                format!("body={}", task.body),
            ],
        )?;

        // Run the task
        self.run(task_id, agent, model).await?;

        // Process delegations if the agent requested subtasks
        let delegations_raw = sidecar::get(task_id, "delegations").unwrap_or_default();
        if !delegations_raw.is_empty() {
            if let Ok(delegations) =
                serde_json::from_str::<Vec<crate::parser::Delegation>>(&delegations_raw)
            {
                if !delegations.is_empty() {
                    self.process_delegations(task, &delegations, backend)
                        .await?;
                    // Clear delegations from sidecar after processing
                    sidecar::set(task_id, &["delegations=".to_string()])?;
                }
            }
        }

        // Post result to GitHub
        let status = sidecar::get(task_id, "status").unwrap_or_default();
        let summary = sidecar::get(task_id, "summary").unwrap_or_default();
        let last_error = sidecar::get(task_id, "last_error").unwrap_or_default();

        // Determine weight signal based on outcome
        let is_rate_limited = last_error.contains("usage")
            || last_error.contains("rate limit")
            || last_error.contains("rerouted");
        let weight_signal = if status == "new" && is_rate_limited {
            WeightSignal::RateLimited {
                agent: agent_name.clone(),
            }
        } else if status == "done" || status == "in_progress" || status == "in_review" {
            WeightSignal::Success {
                agent: agent_name.clone(),
            }
        } else {
            WeightSignal::None
        };

        // Write status to sidecar BEFORE updating GitHub (ensures atomicity)
        sidecar::set(
            task_id,
            &[
                format!("status={}", status),
                format!("summary={}", summary),
                format!("last_error={}", last_error),
                format!(
                    "status_confirmed_at={}",
                    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ")
                ),
            ],
        )?;

        // If task was rerouted (status=new after run), update GitHub agent label
        // so the router doesn't re-route back to the same failed agent.
        if status == "new" {
            let new_agent = sidecar::get(task_id, "agent").unwrap_or_default();
            if !new_agent.is_empty() && new_agent != agent_name {
                // Remove old agent label, add new one
                let old_label = format!("agent:{agent_name}");
                backend.remove_label(&task.id, &old_label).await.ok();
                let new_label = format!("agent:{new_agent}");
                backend.set_labels(&task.id, &[new_label]).await.ok();
                tracing::info!(
                    task_id,
                    from = %agent_name,
                    to = %new_agent,
                    "updated GitHub agent label after failover"
                );
            }
        }

        // Update GitHub status
        let new_status = match status.as_str() {
            "done" => Status::Done,
            "in_progress" => Status::InProgress,
            "in_review" => Status::InReview,
            "blocked" => Status::Blocked,
            "needs_review" => Status::NeedsReview,
            "new" => Status::New, // Rerouted
            _ => Status::NeedsReview,
        };
        backend.update_status(&task.id, new_status).await?;

        // Check for budget warnings and append to comment
        let budget_warning = sidecar::get(task_id, "budget_warning").unwrap_or_default();
        let budget_exceeded = sidecar::get(task_id, "budget_exceeded").unwrap_or_default();

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
            let cost = sidecar::get_cost_estimate(task_id);
            let total_tokens = sidecar::get_total_tokens(task_id);
            raw_comment.push_str(&format!(
                "\n\n> **Budget exceeded**: {} tokens used (${:.4}). Task paused for review.",
                total_tokens, cost.total_cost_usd
            ));
        } else if !budget_warning.is_empty() {
            raw_comment.push_str(&format!("\n\n> **Budget warning**: {budget_warning}"));
        }

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

        for delegation in delegations {
            // Build labels: status:new + any labels from the delegation
            let mut labels = delegation.labels.clone();
            labels.push("status:new".to_string());

            // Build child body with delegation reference
            let child_body = format!(
                "{}\n\n---\n_Delegated from #{}_",
                delegation.body, parent_id.0
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

        // Mark parent as blocked
        sidecar::set(&parent_id.0, &["status=blocked".to_string()])?;
        backend.update_status(parent_id, Status::Blocked).await?;

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
                    "Delegated {} subtask(s):\n\n{}\n\nParent task is blocked until all subtasks complete.",
                    delegations.len(),
                    summary
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
