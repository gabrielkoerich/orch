//! Task initialization — guard checks, worktree setup, and invocation building.
//!
//! Extracted from `runner/mod.rs`. Contains the first phase of `run()`:
//! guard checks, worktree creation, context building, and prompt assembly.

use crate::backends::{ExternalBackend, ExternalId, ExternalTask};
use crate::config;
use crate::engine::router::get_route_result;
use crate::store;
use crate::store::TaskStore;
use crate::tmux::TmuxManager;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{agent, context, git_ops, worktree};

/// Result of task preparation: everything needed to run an agent session.
pub struct TaskInitResult {
    pub agent_name: String,
    pub model_name: Option<String>,
    pub complexity: Option<String>,
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
    let attempts: u32 = store::opt_store_get_task(store, repo, task_id)
        .await
        .map(|t| t.attempts)
        .unwrap_or(0) as u32;

    // Guard: check if tmux session already exists (prevents duplicate dispatch)
    let tmux = TmuxManager::new();
    let session_name = tmux.session_name(repo, task_id);
    if tmux.session_blocks_dispatch(&session_name).await {
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
        if let Err(e) = store::store_set_result(
            store,
            repo,
            task_id,
            &[("last_error", serde_json::json!(msg))],
        )
        .await
        {
            tracing::warn!(task_id, err = %e, "failed to write max_attempts last_error to store");
        }
        return Ok(GuardOutcome::MaxAttempts);
    }

    Ok(GuardOutcome::Proceed(attempts))
}

/// Outcome of the token budget check.
#[derive(Debug, Clone)]
pub enum BudgetCheckOutcome {
    /// Budget is within limits; task can proceed.
    Proceed,
    /// Budget is exceeded; task should be blocked.
    Exceeded { total_tokens: u64, max_tokens: u64 },
    /// Store read failed; task should be blocked to avoid proceeding on uncertain budget state.
    StoreReadError,
}

/// Check if the task has exceeded its token budget before running.
///
/// This is a pre-flight check to prevent wasting tokens on tasks that have
/// already exceeded their budget. The budget is checked again after the run
/// in `handle_success` to catch tasks that exceed budget during execution.
pub async fn check_token_budget(
    task_id: &str,
    repo: &str,
    store: &Option<Arc<TaskStore>>,
) -> BudgetCheckOutcome {
    let max_tokens: u64 = config::get("workflow.max_tokens_per_task")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100_000);

    // If set to 0, token budget checks are disabled.
    if max_tokens == 0 {
        return BudgetCheckOutcome::Proceed;
    }

    let (total_tokens, cost) = match store::get_token_summary_result(store, repo, task_id).await {
        Ok(Some((tokens, cost))) => (tokens, cost),
        Ok(None) => (0, Default::default()),
        Err(e) => {
            tracing::error!(task_id, error = %e, "failed to read token summary from store — blocking task to prevent uncertain budget execution");
            return BudgetCheckOutcome::StoreReadError;
        }
    };

    if total_tokens > max_tokens {
        tracing::warn!(
            task_id,
            total_tokens,
            max_tokens,
            "pre-run check: token budget already exceeded"
        );
        let budget_msg = format!(
            "token budget exceeded: {}/{} tokens (${:.4})",
            total_tokens, max_tokens, cost.total_cost_usd
        );
        if let Err(e) = store::store_set_result(
            store,
            repo,
            task_id,
            &[
                ("last_error", serde_json::json!(budget_msg)),
                ("budget_exceeded", serde_json::json!(true)),
            ],
        )
        .await
        {
            tracing::warn!(task_id, err = %e, "failed to write budget_exceeded to store — budget enforcement may be incorrect on restart");
        }
        store::store_log_activity(
            store,
            repo,
            task_id,
            "budget_exceeded",
            None,
            None,
            None::<&str>,
            None::<&str>,
            Some(&serde_json::json!({
                "total_tokens": total_tokens,
                "max_tokens": max_tokens,
                "cost_usd": cost.total_cost_usd,
            })),
        )
        .await;
        return BudgetCheckOutcome::Exceeded {
            total_tokens,
            max_tokens,
        };
    }

    // Warn at 80% threshold
    let warning_threshold = (max_tokens as f64 * 0.8) as u64;
    if total_tokens > warning_threshold {
        let pct = (total_tokens as f64 / max_tokens as f64 * 100.0) as u32;
        tracing::warn!(
            task_id,
            total_tokens,
            max_tokens,
            pct,
            "pre-run check: approaching token budget"
        );
    }

    BudgetCheckOutcome::Proceed
}

/// Build a minimal `ExternalTask` from store state for prompt building.
pub async fn build_pseudo_task(
    task_id: &str,
    store: &Option<Arc<TaskStore>>,
    repo: &str,
) -> ExternalTask {
    let (task_title, task_body) = store::opt_store_get_task(store, repo, task_id)
        .await
        .map(|t| (t.title, t.body))
        .unwrap_or_else(|| (format!("Task #{task_id}"), String::new()));
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
    // Load title from store for branch naming (set by run_with_context before run())
    let title_for_branch = store::opt_store_get_task(store, repo, task_id)
        .await
        .map(|t| t.title)
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

    let complexity = route_result.as_ref().map(|r| r.complexity.clone());

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
    let attempt_dir = match crate::home::task_attempt_dir_async(repo, task_id, next_attempt).await {
        Ok(dir) => dir,
        Err(e) => {
            if let Some(s) = store {
                let _ = crate::engine::cleanup::cleanup_task_worktree(task_id, repo, s).await;
            }
            return Err(e);
        }
    };
    let output_file = attempt_dir.join("output.json");

    // Build sandbox disallowed tools
    let mut disallowed_tools = vec![
        "Bash(rm *)".to_string(),
        "Bash(rm -*)".to_string(),
        "Bash(git push*)".to_string(),
    ];

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

    if let Ok(orch_home) = crate::home::orch_home() {
        for path in [
            orch_home.join("config.yml"),
            orch_home.join("config.example.yml"),
        ] {
            let path_str = path.to_string_lossy();
            disallowed_tools.extend([
                format!("Read({path_str})"),
                format!("Write({path_str})"),
                format!("Edit({path_str})"),
            ]);
        }
    }

    // Sandbox: block .orch.yml project config (settled architecture: agents must not modify it)
    {
        let orch_yml = wt.work_dir.join(".orch.yml");
        let orch_yml_str = orch_yml.to_string_lossy();
        disallowed_tools.extend([
            format!("Write({orch_yml_str})"),
            format!("Edit({orch_yml_str})"),
        ]);
    }

    // Timeout: read base from workflow.timeout_seconds (min 1800s), then apply per-complexity
    // override from workflow.timeout_by_complexity.{simple,medium,complex} if configured.
    // The per-complexity value is also floored at the base to prevent accidental regression.
    let base_timeout: u64 = config::get("workflow.timeout_seconds")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1800)
        .max(1800);
    let timeout_seconds: u64 = match complexity.as_deref() {
        Some("complex") => config::get("workflow.timeout_by_complexity.complex")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(base_timeout)
            .max(base_timeout),
        Some("medium") => config::get("workflow.timeout_by_complexity.medium")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(base_timeout)
            .max(base_timeout),
        _ => base_timeout,
    };

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

    // Increment attempts counter — must succeed so max_attempts guard cannot be bypassed
    if let Err(e) = store::store_set_result(
        store,
        repo,
        task_id,
        &[("attempts", serde_json::json!(next_attempt))],
    )
    .await
    {
        if let Some(s) = store {
            let _ = crate::engine::cleanup::cleanup_task_worktree(task_id, repo, s).await;
        }
        return Err(anyhow::anyhow!("failed to persist attempts counter: {e}"));
    }

    Ok(TaskInitResult {
        agent_name,
        model_name,
        complexity,
        task_title,
        wt,
        invocation,
        attempt_dir,
        new_attempts: next_attempt,
    })
}
