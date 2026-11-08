//! Task initialization — guard checks, worktree setup, and invocation building.
//!
//! Extracted from `runner/mod.rs`. Contains the first phase of `run()`:
//! guard checks, worktree creation, context building, and prompt assembly.

use crate::backends::{ExternalBackend, ExternalId, ExternalTask};
use crate::config;
use crate::engine::router::{RouteResultError, get_route_result};
use crate::store;
use crate::store::TaskStore;
use crate::tmux::TmuxManager;
use std::fmt;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{agent, context, git_ops, worktree};
use crate::engine::RouterConfig;
use crate::engine::RunTimeouts;
use crate::engine::cleanup;
use crate::home;

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

impl Debug for TaskInitResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskInitResult")
            .field("agent_name", &self.agent_name)
            .field("model_name", &self.model_name)
            .field("complexity", &self.complexity)
            .field("task_title", &self.task_title)
            .field("wt", &self.wt)
            .field("attempt_dir", &self.attempt_dir)
            .field("new_attempts", &self.new_attempts)
            .finish_non_exhaustive()
    }
}

/// Outcome of the guard check.
#[derive(Debug)]
pub enum GuardOutcome {
    /// Task should proceed; returns current attempt count.
    Proceed(u32),
    /// Task should be skipped (already running, status=needs_review, etc.).
    Skip,
    /// Task has exceeded max attempts, caller marks it blocked.
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
        .map_or(0, |t| u32::try_from(t.attempts).unwrap_or(0));

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
            &[
                ("last_error", serde_json::json!(msg)),
                ("block_reason", serde_json::json!(msg)),
            ],
        )
        .await
        {
            tracing::warn!(task_id, err = %e, "failed to write max_attempts last_error to store");
        }
        return Ok(GuardOutcome::MaxAttempts);
    }

    Ok(GuardOutcome::Proceed(attempts))
}

/// Build a minimal `ExternalTask` from store state for prompt building.
pub async fn build_pseudo_task(
    task_id: &str,
    store: &Option<Arc<TaskStore>>,
    repo: &str,
) -> ExternalTask {
    let (task_title, task_body) = store::opt_store_get_task(store, repo, task_id)
        .await
        .map_or_else(
            || (format!("Task #{task_id}"), String::new()),
            |t| (t.title, t.body),
        );
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

/// The router clears a stale model by writing an empty string, and `--model ""` makes the claude
/// API answer 400, so an empty name at any level means no model
fn resolve_model_name(
    param: Option<&str>,
    routed: Option<&str>,
    stored: Option<&str>,
) -> Option<String> {
    [param, routed, stored]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|m| !m.is_empty())
        .map(String::from)
}

/// Prepare the full task: set up worktree, build context and prompts, create invocation.
#[expect(
    clippy::too_many_arguments,
    reason = "task setup needs the routing result, backend, project paths and store together"
)]
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
    // Load title (and model fallback) from store.
    // `model` param and `route_result.model` can both be None when a prior
    // ModelUnavailable failover wrote the new model to tasks.model but did not
    // update task_routes.model.  Reading tasks.model here ensures model_name is
    // never None when the task already has a model assigned — which matters for
    // BillingCycleExhausted handling in fallback.rs (model-known → model-level
    // cooldown only; model-unknown → agent-wide 24 h cooldown).
    let initial_task = store::opt_store_get_task(store, repo, task_id).await;
    let title_for_branch = initial_task
        .as_ref()
        .map(|t| t.title.clone())
        .unwrap_or_default();
    let stored_model = initial_task.and_then(|t| t.model);

    // Set up worktree
    let wt = worktree::setup_worktree(task_id, &title_for_branch, project_dir, store, repo).await?;

    // Rebase worktree on default branch to pick up latest changes.
    // This prevents non-fast-forward push failures when the task is re-dispatched.
    git_ops::rebase_on_default(&wt.work_dir, &wt.default_branch).await;

    // Get routing result — distinguish between "no route" and store errors
    let route_result = if let Some(s) = store {
        match get_route_result(s, repo, task_id).await {
            Ok(r) => Some(r),
            // NoAgent means the router hasn't set a route for this task yet;
            // fall back to the default agent (caller may re-route later).
            Err(RouteResultError::NoAgent { .. }) => {
                tracing::warn!(task_id, "routed task has no agent — using fallback");
                None
            }
            // Any other error indicates a store/IO problem — surface it so the
            // caller doesn't silently proceed with an incorrect fallback.
            Err(e) => {
                tracing::error!(task_id, error = %e, "failed to get route result from store — blocking task");
                // Persist the error to the task result so operators can see it.
                let err_msg = format!("failed to read routing result: {e}");
                if let Err(set_err) = store::store_set_result(
                    store,
                    repo,
                    task_id,
                    &[("last_error", serde_json::json!(err_msg.clone()))],
                )
                .await
                {
                    tracing::warn!(task_id, err = %set_err, "failed to write route read error to store");
                }
                // Attempt to clean up the worktree we created to avoid leaving
                // partial artifacts behind.
                if let Some(s) = store {
                    let _ = cleanup::cleanup_task_worktree(task_id, repo, s).await;
                }
                return Err(anyhow::anyhow!(err_msg));
            }
        }
    } else {
        None
    };

    let agent_name = agent
        .map(String::from)
        .or_else(|| route_result.as_ref().map(|r| r.agent.clone()))
        .unwrap_or_else(|| "claude".to_string());

    let model_name = resolve_model_name(
        model,
        route_result.as_ref().and_then(|r| r.model.as_deref()),
        stored_model.as_deref(),
    );

    let complexity = route_result.as_ref().map(|r| r.complexity.clone());

    // Build a minimal ExternalTask for prompt building
    let pseudo_task = build_pseudo_task(task_id, store, repo).await;
    let task_title = pseudo_task.title.clone();

    // Build full context: memory, issue comments, and parent context are all loaded here
    let ctx = context::build_full_context(
        &pseudo_task,
        backend,
        &wt.work_dir,
        &wt.default_branch,
        attempts,
        store,
        repo,
    )
    .await;

    let sandboxed = super::sandbox::enabled(&wt.main_project_dir);
    let shipcheck = agent::Shipcheck::detect(&wt.work_dir).await;
    agent::install_shipcheck_hooks(&wt.work_dir, shipcheck, Path::new("shipcheck")).await;

    // Build prompts
    let system_prompt = agent::build_system_prompt(
        &pseudo_task,
        &ctx,
        route_result.as_ref(),
        &wt.default_branch,
        shipcheck.for_prompt(sandboxed),
    );
    let agent_message = agent::build_agent_message(&pseudo_task, &ctx, attempts);

    // Git identity
    let git_name = config::get("git.name").unwrap_or_else(|_| format!("{agent_name}[bot]"));
    let git_email = config::get("git.email")
        .unwrap_or_else(|_| format!("{agent_name}[bot]@users.noreply.github.com"));

    // Output file in per-task attempt directory (attempt = attempts + 1, set below)
    let next_attempt = attempts + 1;
    let attempt_dir = match home::task_attempt_dir_async(repo, task_id, next_attempt).await {
        Ok(dir) => dir,
        Err(e) => {
            if let Some(s) = store {
                let _ = cleanup::cleanup_task_worktree(task_id, repo, s).await;
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
        // Pushing and opening the PR are orch's steps. An agent that tries either hits a
        // permission prompt or a rejection and reports it as its own failure (#3783)
        "Bash(gh pr create*)".to_string(),
    ];
    disallowed_tools.extend(git_ops::remote_change_tools());

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

    if let Ok(orch_home) = home::orch_home() {
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

    // Stuck recovery reads the same limits, so a live session is never taken before this runs out
    // A model taken from another tier runs with that tier's timeout
    let timeout_tier = match (complexity.as_deref(), model_name.as_deref()) {
        (Some(c), Some(m)) if !m.is_empty() => {
            Some(RouterConfig::from_config().tier_of_model(&agent_name, m, c))
        }
        (c, _) => c.map(String::from),
    };
    let timeout_seconds = RunTimeouts::from_config().for_complexity(timeout_tier.as_deref());

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
        sandbox: sandboxed,
        main_project_dir: wt.main_project_dir.clone(),
        branch: wt.branch.clone(),
    };

    // Increment attempts counter — must succeed so max_attempts guard cannot be bypassed.
    // Also clears last_error, so a failover's error never reads as this run's outcome
    if let Err(e) = store::store_set_result(
        store,
        repo,
        task_id,
        &[
            ("attempts", serde_json::json!(next_attempt)),
            ("last_error", serde_json::json!("")),
        ],
    )
    .await
    {
        if let Some(s) = store {
            let _ = cleanup::cleanup_task_worktree(task_id, repo, s).await;
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

#[cfg(test)]
mod tests {
    use std::env;
    use std::mem;
    use std::process::Command;

    use super::*;
    use crate::engine::runner::{WeightSignal, needs_review_signal};
    use crate::engine::tick::display_status_for;
    use crate::store::NewTask;

    const FAILOVER_ERROR: &str = "claude timed out after 1804s, switching to codex for re-route";

    /// Seeds a no-code task with `seed_error`, then runs `prepare_task` for its next run
    async fn prepare_run(seed_error: &str) -> (Arc<TaskStore>, i64) {
        let orch_home = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        unsafe { env::set_var("ORCH_HOME", orch_home.path()) };
        mem::forget(orch_home);

        let project = tempfile::tempdir().unwrap();
        for args in [
            &["init", "-b", "main"][..],
            &["config", "user.email", "t@t.com"],
            &["config", "user.name", "T"],
            &["commit", "--allow-empty", "-m", "base"],
        ] {
            let out = Command::new("git")
                .args(args)
                .current_dir(project.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed");
        }

        let task_store = Arc::new(TaskStore::open_memory().await.unwrap());
        let store_id = task_store
            .create(&NewTask {
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "Check the report".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        task_store
            .set_fields(store_id, &[("last_error", serde_json::json!(seed_error))])
            .await
            .unwrap();
        let task_id = format!("internal:{store_id}");
        let store = Some(Arc::clone(&task_store));

        prepare_task(
            &task_id,
            Some("claude"),
            Some("sonnet"),
            None,
            "owner/repo",
            project.path(),
            0,
            &store,
        )
        .await
        .expect("prepare_task should succeed");
        (task_store, store_id)
    }

    async fn stored_last_error(store: &TaskStore, store_id: i64) -> String {
        store.get(store_id).await.unwrap().last_error
    }

    #[tokio::test]
    async fn no_code_task_is_done_after_a_failover_when_the_fallback_run_succeeds() {
        let (store, id) = prepare_run(FAILOVER_ERROR).await;

        let last_error = stored_last_error(&store, id).await;
        assert_eq!(last_error, "", "starting a run clears the failover error");
        // The fallback agent finishes with no commits and no PR, so nothing writes last_error
        let signal = needs_review_signal(&last_error, "codex");
        assert!(matches!(signal, WeightSignal::Success { .. }));
        assert_eq!(display_status_for(&signal, true, false), "done");
    }

    #[tokio::test]
    async fn no_code_task_is_done_after_a_first_run_with_no_commits() {
        let (store, id) = prepare_run("").await;

        let signal = needs_review_signal(&stored_last_error(&store, id).await, "claude");
        assert!(matches!(signal, WeightSignal::Success { .. }));
        assert_eq!(display_status_for(&signal, true, false), "done");
    }

    #[tokio::test]
    async fn timed_out_run_with_no_fallback_stays_in_needs_review() {
        let (store, id) = prepare_run(FAILOVER_ERROR).await;

        // handle_error stores this when the run times out and no fallback agent is left
        store
            .set_fields(
                id,
                &[(
                    "last_error",
                    serde_json::json!("claude timed out after 1804s, no fallback agents"),
                )],
            )
            .await
            .unwrap();

        let signal = needs_review_signal(&stored_last_error(&store, id).await, "claude");
        assert!(matches!(signal, WeightSignal::Failed { .. }));
        assert_eq!(display_status_for(&signal, true, false), "needs_review");
    }

    #[test]
    fn resolve_model_name_skips_a_model_cleared_to_empty() {
        assert_eq!(resolve_model_name(None, None, Some("")), None);
        assert_eq!(resolve_model_name(None, Some(""), Some("")), None);
        assert_eq!(
            resolve_model_name(None, None, Some("sonnet")).as_deref(),
            Some("sonnet")
        );
        assert_eq!(
            resolve_model_name(Some("opus"), Some("sonnet"), Some("haiku")).as_deref(),
            Some("opus")
        );
    }

    #[tokio::test]
    async fn max_attempts_writes_block_reason() {
        let task_store = Arc::new(TaskStore::open_memory().await.unwrap());
        let store_id = task_store
            .create(&NewTask {
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "Test".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        task_store
            .set_fields(store_id, &[("attempts", serde_json::json!(1000))])
            .await
            .unwrap();
        let store = Some(Arc::clone(&task_store));

        let outcome = check_guards(&format!("internal:{store_id}"), "owner/repo", &store)
            .await
            .unwrap();

        assert!(matches!(outcome, GuardOutcome::MaxAttempts));
        let task = task_store.get(store_id).await.unwrap();
        assert!(
            task.block_reason
                .is_some_and(|r| r.contains("exceeded max attempts"))
        );
    }
}
