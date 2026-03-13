//! Task initialization — guard checks, worktree setup, and invocation building.
//!
//! Extracted from `runner/mod.rs`. Contains the first phase of `run()`:
//! guard checks, worktree creation, context building, and prompt assembly.

use crate::backends::{ExternalBackend, ExternalId, ExternalTask};
use crate::config;
use crate::engine::router::get_route_result;
use crate::store::TaskStore;
use crate::tmux::TmuxManager;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
}

/// Outcome of the guard check.
pub enum GuardOutcome {
    /// Task should proceed; returns current attempt count.
    Proceed(u32),
    /// Task should be skipped (already running, status=needs_review, etc.).
    Skip,
    /// Task has exceeded max attempts — caller should update GitHub to NeedsReview.
    MaxAttempts,
}

/// Check task guards and return the outcome.
///
/// Also checks for existing tmux sessions to prevent duplicate dispatch.
pub async fn check_guards(
    task_id: &str,
    repo: &str,
    store: &Option<Arc<TaskStore>>,
) -> anyhow::Result<GuardOutcome> {
    let attempts: u32 =
        crate::engine::cleanup::opt_store_or_sidecar(store, repo, task_id, "attempts")
            .await
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

    // Guard: check if tmux session already exists (prevents duplicate dispatch)
    let tmux = TmuxManager::new();
    let session_name = tmux.session_name(repo, task_id);
    if tmux.session_exists(&session_name).await {
        tracing::info!(
            task_id,
            session = %session_name,
            "skipping task: tmux session already exists"
        );
        return Ok(GuardOutcome::Skip);
    }

    // Check max attempts
    let max_attempts: u32 = config::get("workflow.max_attempts")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);

    if attempts >= max_attempts {
        tracing::warn!(task_id, attempts, max_attempts, "exceeded max attempts");
        let msg =
            format!("exceeded max attempts ({attempts}/{max_attempts}). Use `/retry` to reset.");
        crate::engine::cleanup::store_and_sidecar_set(
            store,
            repo,
            task_id,
            &[format!("last_error={msg}")],
            &[("last_error", serde_json::json!(msg))],
        )
        .await;
        return Ok(GuardOutcome::MaxAttempts);
    }

    Ok(GuardOutcome::Proceed(attempts))
}

/// Build a minimal `ExternalTask` from store/sidecar state for prompt building.
pub async fn build_pseudo_task(
    task_id: &str,
    store: &Option<Arc<TaskStore>>,
    repo: &str,
) -> ExternalTask {
    let task_title = crate::engine::cleanup::opt_store_or_sidecar(store, repo, task_id, "title")
        .await
        .unwrap_or_else(|| format!("Task #{task_id}"));
    let task_body = crate::engine::cleanup::opt_store_or_sidecar(store, repo, task_id, "body")
        .await
        .unwrap_or_default();
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
#[allow(clippy::too_many_arguments)]
pub async fn prepare_task(
    task_id: &str,
    agent: Option<&str>,
    model: Option<&str>,
    backend: Option<&dyn ExternalBackend>,
    repo: &str,
    project_dir: &Path,
    attempts: u32,
    store: &Option<Arc<TaskStore>>,
) -> anyhow::Result<TaskInitResult> {
    // Load title from store/sidecar for branch naming (set by run_with_context before run())
    let title_for_branch =
        crate::engine::cleanup::opt_store_or_sidecar(store, repo, task_id, "title")
            .await
            .unwrap_or_default();

    // Set up worktree
    let wt = worktree::setup_worktree(task_id, &title_for_branch, project_dir, store, repo).await?;

    // Rebase worktree on default branch to pick up latest changes.
    // This prevents non-fast-forward push failures when the task is re-dispatched.
    git_ops::rebase_on_default(&wt.work_dir, &wt.default_branch).await;

    // Get routing result
    let route_result = if let Some(ref s) = store {
        get_route_result(s, repo, task_id).await.ok()
    } else {
        None
    };

    let agent_name = agent
        .map(String::from)
        .or_else(|| route_result.as_ref().map(|r| r.agent.clone()))
        .unwrap_or_else(|| "claude".to_string());

    let model_name = model
        .map(String::from)
        .or_else(|| route_result.as_ref().and_then(|r| r.model.clone()));

    // Build a minimal ExternalTask for prompt building
    let pseudo_task = build_pseudo_task(task_id, store, repo).await;
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
        store,
        repo,
    )
    .await;

    // Build prompts
    let system_prompt = agent::build_system_prompt(
        &pseudo_task,
        &ctx,
        route_result.as_ref(),
        &wt.default_branch,
    );
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
    crate::engine::cleanup::store_and_sidecar_set(
        store,
        repo,
        task_id,
        &[format!("attempts={next_attempt}")],
        &[("attempts", serde_json::json!(next_attempt))],
    )
    .await;

    Ok(TaskInitResult {
        agent_name,
        model_name,
        task_title,
        wt,
        invocation,
        attempt_dir,
        new_attempts: next_attempt,
    })
}
