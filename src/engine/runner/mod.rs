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
pub mod context;
pub mod git_ops;
pub mod response;
pub mod worktree;

use crate::backends::{ExternalBackend, ExternalId, ExternalTask, Status};
use crate::config;
use crate::engine::router::{get_route_result, RouteResult};
use crate::security;
use crate::sidecar;
use crate::tmux::TmuxManager;
use response::RunResult;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::time::{timeout, Duration};

/// Task runner configuration.
pub struct TaskRunner {
    /// Repository slug (owner/repo)
    repo: String,
    /// Path to the orchestrator home directory
    orch_home: PathBuf,
}

impl TaskRunner {
    pub fn new(repo: String) -> Self {
        let orch_home =
            crate::home::orch_home().unwrap_or_else(|_| PathBuf::from("/tmp").join(".orch"));

        Self { repo, orch_home }
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
            tracing::warn!(task_id, attempts, max_attempts, "exceeded max attempts");
            return Ok(());
        }

        // Resolve project directory
        let project_dir = self.resolve_project_dir()?;

        // Set up worktree
        let wt = worktree::setup_worktree(task_id, "", &project_dir).await?;

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

        let ctx = context::TaskContext {
            task_context,
            parent_context: String::new(), // Requires backend
            project_instructions,
            skills_docs,
            repo_tree,
            git_diff,
            issue_comments: String::new(), // Requires backend
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

        // Output file
        let output_file = PathBuf::from(format!("/tmp/output-{task_id}.json"));

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

        // Read exit code (check new state dir, fall back to legacy)
        let status_file =
            sidecar::state_file(&format!("exit-{task_id}.txt")).unwrap_or_else(|_| {
                self.orch_home
                    .join("state")
                    .join(format!("exit-{task_id}.txt"))
            });
        let exit_code: i32 = std::fs::read_to_string(&status_file)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(-1);

        // Collect and classify response
        let result = response::collect_response(task_id, exit_code, &output_file);

        // Handle result
        match result {
            RunResult::Success(resp) => {
                tracing::info!(
                    task_id,
                    status = resp.status,
                    summary = resp.summary,
                    "agent completed successfully"
                );

                // Auto-commit
                if resp.status == "done" || resp.status == "in_progress" {
                    git_ops::auto_commit(
                        &wt.work_dir,
                        task_id,
                        &task_title,
                        &agent_name,
                        new_attempts,
                    )
                    .await
                    .ok();

                    // Push
                    git_ops::push_branch(&wt.work_dir, &wt.branch).await.ok();

                    // Create PR
                    git_ops::create_pr_if_needed(
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
                    .ok();
                }

                // Store result in sidecar
                sidecar::set(
                    task_id,
                    &[
                        format!("status={}", resp.status),
                        format!("summary={}", resp.summary),
                    ],
                )?;

                // Store token usage if available
                if let (Some(input), Some(output)) = (resp.input_tokens, resp.output_tokens) {
                    let model = model_name.as_deref().unwrap_or("haiku");
                    if let Err(e) = sidecar::store_token_usage(task_id, input, output, model) {
                        tracing::warn!(task_id, ?e, "failed to store token usage");
                    }
                }

                // Check token budget
                let max_tokens: u64 = config::get("max_tokens_per_task")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(100_000);

                let total_tokens = sidecar::get_total_tokens(task_id);
                if total_tokens > max_tokens {
                    tracing::warn!(task_id, total_tokens, max_tokens, "exceeded token budget");
                    sidecar::set(
                        task_id,
                        &[
                            "status=needs_review".to_string(),
                            format!(
                                "last_error=token budget exceeded: {}/{} tokens",
                                total_tokens, max_tokens
                            ),
                        ],
                    )?;
                    return Ok(());
                }

                // Check PR override: done → in_review
                if resp.status == "done"
                    && git_ops::check_pr_override(&wt.work_dir, &wt.branch).await
                {
                    tracing::info!(task_id, "overriding done → in_review (PR open)");
                    sidecar::set(task_id, &["status=in_review".to_string()])?;
                }
            }
            RunResult::Timeout => {
                tracing::error!(task_id, "agent timed out");
                sidecar::set(
                    task_id,
                    &[
                        "status=needs_review".to_string(),
                        "last_error=agent timed out".to_string(),
                    ],
                )?;
            }
            RunResult::UsageLimit(_snippet) => {
                tracing::warn!(task_id, "usage/rate limit hit");

                let chain = response::get_reroute_chain(task_id);
                let chain = response::update_reroute_chain(task_id, &agent_name, &chain);

                // Try to find available agents for fallback
                let available: Vec<String> = ["claude", "codex", "opencode"]
                    .iter()
                    .filter(|a| which::which(a).is_ok())
                    .map(|s| s.to_string())
                    .collect();

                if let Some(next) = response::pick_fallback_agent(&agent_name, &chain, &available) {
                    tracing::info!(
                        task_id,
                        from = agent_name,
                        to = next,
                        "rerouting due to usage limit"
                    );
                    sidecar::set(
                        task_id,
                        &[
                            format!("agent={next}"),
                            "agent_model=".to_string(),
                            "status=new".to_string(),
                            format!("last_error={agent_name} usage/rate limit, rerouted to {next}"),
                        ],
                    )?;
                } else {
                    sidecar::set(
                        task_id,
                        &[
                            "status=needs_review".to_string(),
                            format!("last_error={agent_name} usage limit, no fallback agents"),
                        ],
                    )?;
                }
            }
            RunResult::AuthError(_snippet) => {
                tracing::warn!(task_id, agent = agent_name, "auth/billing error");

                let available: Vec<String> = ["claude", "codex", "opencode"]
                    .iter()
                    .filter(|a| which::which(a).is_ok())
                    .map(|s| s.to_string())
                    .collect();

                if let Some(next) = response::pick_fallback_agent(&agent_name, "", &available) {
                    tracing::info!(
                        task_id,
                        from = agent_name,
                        to = next,
                        "switching agent due to auth error"
                    );
                    sidecar::set(
                        task_id,
                        &[
                            format!("agent={next}"),
                            "agent_model=".to_string(),
                            "status=new".to_string(),
                            format!(
                                "last_error={agent_name} auth/billing error, switched to {next}"
                            ),
                        ],
                    )?;
                } else {
                    sidecar::set(
                        task_id,
                        &[
                            "status=needs_review".to_string(),
                            format!("last_error={agent_name} auth error, no fallback agents"),
                        ],
                    )?;
                }
            }
            RunResult::MissingTooling(msg) => {
                tracing::warn!(task_id, msg = %msg, "missing tooling");
                sidecar::set(
                    task_id,
                    &[
                        "status=needs_review".to_string(),
                        format!("last_error={msg}"),
                    ],
                )?;
            }
            RunResult::Failed(msg) => {
                tracing::error!(task_id, msg = %msg, "task failed");
                sidecar::set(
                    task_id,
                    &[
                        "status=needs_review".to_string(),
                        format!("last_error={msg}"),
                    ],
                )?;
            }
        }

        // Kill tmux session if still alive
        if tmux.session_exists(&session).await {
            tmux.kill_session(&session).await.ok();
        }

        Ok(())
    }

    /// Run a task with full engine context (backend, tmux, capture).
    ///
    /// Called by the engine dispatch loop with richer context.
    pub async fn run_with_context(
        &self,
        task: &ExternalTask,
        backend: &Arc<dyn ExternalBackend>,
        _tmux: &Arc<TmuxManager>,
        route_result: Option<&RouteResult>,
    ) -> anyhow::Result<()> {
        let task_id = &task.id.0;
        let agent = route_result.map(|r| r.agent.as_str());
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

        // Post result to GitHub
        let status = sidecar::get(task_id, "status").unwrap_or_default();
        let summary = sidecar::get(task_id, "summary").unwrap_or_default();
        let last_error = sidecar::get(task_id, "last_error").unwrap_or_default();

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

        // Post comment (scan for secrets before posting to GitHub)
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
        let raw_comment = if !summary.is_empty() {
            format!("[{now}] {status}: {summary}")
        } else if !last_error.is_empty() {
            format!("[{now}] {status}: {last_error}")
        } else {
            format!("[{now}] {status}")
        };

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

        // Check config
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
