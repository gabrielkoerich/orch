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
pub mod fallback;
pub mod git_ops;
pub mod response;
pub mod response_handler;
pub mod session;
pub mod task_init;
pub mod worktree;

use crate::backends::{ExternalBackend, ExternalTask, Status};
use crate::config;
use crate::db::{Db, InsertTaskMetric};
use crate::engine::router::RouteResult;
use crate::engine::tasks::is_internal_id;
use crate::security;
use crate::sidecar;
use crate::tmux::TmuxManager;
use chrono::Utc;
pub use response::WeightSignal;
use std::path::PathBuf;
use std::sync::Arc;

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
    ) -> anyhow::Result<Option<String>> {
        tracing::info!(
            task_id,
            agent = agent.unwrap_or("default"),
            model = model.unwrap_or("default"),
            "starting task execution"
        );

        // Check task guards; returns outcome indicating whether to proceed.
        let attempts = match task_init::check_guards(task_id, &self.repo).await {
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
                    let route_reason = sidecar::get(task_id, "route_reason").unwrap_or_default();
                    if route_reason.starts_with("label agent:") {
                        let agent_label = route_reason.trim_start_matches("label ");
                        let gh = crate::github::http::GhHttp::new();
                        if let Err(e) = gh.remove_label(&self.repo, task_id, agent_label).await {
                            tracing::warn!(task_id, label = agent_label, err = %e, "failed to remove agent label after max attempts");
                        } else {
                            tracing::info!(task_id, label = agent_label, "removed forced agent label after max attempts — /retry will use free routing");
                        }
                    }
                }
                return Ok(Some("needs_review".to_string()));
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
            agent_runner.parse_response(&session_output.raw_stdout)
        } else if session_output.exit_code != 0 {
            Err(agent_runner.classify_error(
                session_output.exit_code,
                &session_output.raw_stdout,
                &session_output.raw_stderr,
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
            Ok(parsed) => {
                let (status, budget_exceeded) = response_handler::handle_success(
                    task_id,
                    parsed,
                    &init.wt,
                    &init.task_title,
                    &init.agent_name,
                    init.model_name.as_deref(),
                    init.new_attempts,
                )
                .await?;
                if budget_exceeded {
                    // Token budget exceeded — sidecar already updated, return without
                    // tmux cleanup or metrics (preserves original behavior)
                    return Ok(Some(status));
                }
                status
            }
            Err(agent_err) => {
                match fallback::handle_error(
                    task_id,
                    &agent_err,
                    &init.agent_name,
                    &*agent_runner,
                    init.model_name.as_deref(),
                    init.new_attempts,
                    self.db.as_ref(),
                )
                .await?
                {
                    fallback::ErrorHandleResult::EarlyReturn { status } => {
                        // Task rerouted — return (metrics recorded in run_with_context)
                        return Ok(Some(status));
                    }
                    fallback::ErrorHandleResult::Continue { status } => status,
                }
            }
        };

        // Kill tmux session if still alive
        session::cleanup_session(task_id, &tmux, &tmux_session).await;

        Ok(Some(final_status))
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
            "needs_review" => {
                let last_error = error_type.unwrap_or("");
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
            _ => "unknown",
        };

        let complexity = route_result.as_ref().map(|r| r.complexity.clone());
        let files_changed = git_ops::count_changed_files(&PathBuf::from(
            sidecar::get(task_id, "worktree").unwrap_or_default(),
        ))
        .await
        .unwrap_or(0);

        if let Some(ref db) = self.db {
            // Only set error_type for non-success outcomes
            let db_error_type: Option<String> = if outcome == "success" {
                None
            } else {
                Some(outcome.to_string())
            };

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
                error_type: db_error_type.as_deref(),
                input_tokens,
                output_tokens,
                input_cost_usd: input_cost,
                output_cost_usd: output_cost,
                total_cost_usd: total_cost,
            };

            if let Err(e) = db.insert_task_metric(metric).await {
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

        // Store task info in sidecar for prompt building
        sidecar::set(
            task_id,
            &[
                format!("title={}", task.title),
                format!("body={}", task.body),
            ],
        )?;

        // Run the task
        let run_status = self.run(task_id, agent, model, Some(&**backend)).await?;

        // If the runner guard skipped the task, do not re-post stale data as a new comment.
        if run_status.is_none() {
            tracing::info!(task_id, "guard skipped task — not posting stale result");
            return Ok(WeightSignal::None);
        }
        let status = run_status.unwrap();

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
        } else if status == "blocked" {
            WeightSignal::Blocked
        } else {
            WeightSignal::None
        };

        // Record metrics
        {
            let attempts: u32 = sidecar::get(task_id, "attempts")
                .ok()
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

        for delegation in delegations {
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

        // Mark parent as blocked
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
        let old_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", temp_home.path());

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

        if let Some(old_home) = old_home {
            std::env::set_var("HOME", old_home);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn process_delegations_single_subtask() {
        let _guard = ENV_LOCK.lock().unwrap();
        let temp_home = TempDir::new().unwrap();
        let old_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", temp_home.path());

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

        if let Some(old_home) = old_home {
            std::env::set_var("HOME", old_home);
        } else {
            std::env::remove_var("HOME");
        }
    }
}
