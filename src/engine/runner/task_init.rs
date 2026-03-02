//! Task initialization — guard checks, worktree setup, and invocation building.
//!
//! Extracted from `runner/mod.rs`. Contains the first phase of `run()`:
//! guard checks, worktree creation, context building, and prompt assembly.

use crate::backends::{ExternalBackend, ExternalId, ExternalTask};
use crate::config;
use crate::engine::router::{get_route_result, RouteResult};
use crate::sidecar;
use std::path::{Path, PathBuf};

use super::{agent, context, git_ops, worktree};

/// Result of task preparation: everything needed to run an agent session.
pub struct TaskInitResult {
    pub agent_name: String,
    pub model_name: Option<String>,
    pub task_title: String,
    pub wt: worktree::WorktreeSetup,
    pub invocation: agent::AgentInvocation,
    pub attempt_dir: PathBuf,
    pub new_attempts: u32,
    pub route_result: Option<RouteResult>,
}

/// Check task guards and return the current attempt count.
///
/// Returns `None` if the task should be skipped (already logged/handled).
/// Returns `Some(attempts)` if the task should proceed.
pub fn check_guards(task_id: &str) -> anyhow::Result<Option<u32>> {
    let attempts: u32 = sidecar::get(task_id, "attempts")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    // Guard: skip needs_review tasks
    let current_status = sidecar::get(task_id, "status").unwrap_or_default();
    if current_status == "needs_review" {
        tracing::info!(task_id, "skipping needs_review task");
        return Ok(None);
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
        sidecar::set(
            task_id,
            &[
                "status=blocked".to_string(),
                format!(
                    "last_error=exceeded max attempts ({attempts}/{max_attempts}). Use `/retry` to reset."
                ),
            ],
        )?;
        return Ok(None);
    }

    Ok(Some(attempts))
}

/// Build a minimal `ExternalTask` from sidecar state for prompt building.
pub fn build_pseudo_task(task_id: &str) -> ExternalTask {
    let task_title =
        sidecar::get(task_id, "title").unwrap_or_else(|_| format!("Task #{task_id}"));
    let task_body = sidecar::get(task_id, "body").unwrap_or_default();
    ExternalTask {
        id: ExternalId(task_id.to_string()),
        title: task_title,
        body: task_body,
        state: "open".to_string(),
        labels: vec![],
        author: String::new(),
        created_at: String::new(),
        updated_at: String::new(),
        url: String::new(),
    }
}

/// Prepare the full task: set up worktree, build context and prompts, create invocation.
pub async fn prepare_task(
    task_id: &str,
    agent: Option<&str>,
    model: Option<&str>,
    backend: Option<&dyn ExternalBackend>,
    repo: &str,
    project_dir: &Path,
    attempts: u32,
) -> anyhow::Result<TaskInitResult> {
    // Load title from sidecar for branch naming (set by run_with_context before run())
    let title_for_branch = sidecar::get(task_id, "title").unwrap_or_default();

    // Set up worktree
    let wt = worktree::setup_worktree(task_id, &title_for_branch, project_dir).await?;

    // Rebase worktree on default branch to pick up latest changes.
    // This prevents non-fast-forward push failures when the task is re-dispatched.
    git_ops::rebase_on_default(&wt.work_dir, &wt.default_branch).await;

    // Get routing result
    let route_result = get_route_result(task_id).ok();

    let agent_name = agent
        .map(String::from)
        .or_else(|| route_result.as_ref().map(|r| r.agent.clone()))
        .unwrap_or_else(|| "claude".to_string());

    let model_name = model
        .map(String::from)
        .or_else(|| route_result.as_ref().and_then(|r| r.model.clone()));

    // Build a minimal ExternalTask for prompt building
    let pseudo_task = build_pseudo_task(task_id);
    let task_title = pseudo_task.title.clone();

    let selected_skills = route_result
        .as_ref()
        .map(|r| r.selected_skills.clone())
        .unwrap_or_default();

    // Build full context: memory, issue comments, and parent context are all loaded here
    let ctx = context::build_full_context(
        &pseudo_task,
        backend,
        &wt.work_dir,
        &wt.default_branch,
        attempts,
        &selected_skills,
    )
    .await;

    // Build prompts
    let system_prompt = agent::build_system_prompt(&pseudo_task, &ctx, route_result.as_ref());
    let agent_message = agent::build_agent_message(&pseudo_task, &ctx, attempts);

    // Git identity
    let git_name = config::get("git.name").unwrap_or_else(|_| format!("{agent_name}[bot]"));
    let git_email = config::get("git.email")
        .unwrap_or_else(|_| format!("{agent_name}[bot]@users.noreply.github.com"));

    // Output file in per-task attempt directory (attempt = attempts + 1, set below)
    let next_attempt = attempts + 1;
    let attempt_dir = crate::home::task_attempt_dir(repo, task_id, next_attempt)?;
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
    let invocation = agent::AgentInvocation {
        agent: agent_name.clone(),
        model: model_name.clone(),
        work_dir: wt.work_dir.clone(),
        system_prompt,
        agent_message,
        task_id: task_id.to_string(),
        disallowed_tools,
        git_author_name: git_name,
        git_author_email: git_email,
        output_file,
        timeout_seconds,
        repo: repo.to_string(),
        attempt: next_attempt,
    };

    // Increment attempts counter
    sidecar::set(task_id, &[format!("attempts={next_attempt}")])?;

    Ok(TaskInitResult {
        agent_name,
        model_name,
        task_title,
        wt,
        invocation,
        attempt_dir,
        new_attempts: next_attempt,
        route_result,
    })
}
