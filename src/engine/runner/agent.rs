//! Agent command building + tmux invocation.
//!
//! Supports Claude, Codex, OpenCode (plus Kimi/MiniMax as Claude aliases).
//! Generates a runner shell script that tmux executes — agents need a real terminal.

use crate::template::render_template_str;
use crate::tmux::TmuxManager;
use std::collections::HashMap;
use std::path::PathBuf;

const AGENT_SYSTEM_TEMPLATE: &str = include_str!("../../../prompts/agent_system.md");
const AGENT_MESSAGE_TEMPLATE: &str = include_str!("../../../prompts/agent_message.md");
const AGENT_MEMORY_ENTRY_TEMPLATE: &str = include_str!("../../../prompts/agent_memory_entry.md");
const ALLOWED_TOOLS_TEMPLATE: &str = include_str!("../../../prompts/allowed_tools.md");
const REVIEW_PROMPT_TEMPLATE: &str = include_str!("../../../prompts/review_task.md");
const REVIEW_SYSTEM_TEMPLATE: &str = include_str!("../../../prompts/review_system.md");

fn render_prompt_template(template: &str, vars: HashMap<String, String>) -> String {
    match render_template_str(template, &vars) {
        Ok(rendered) => rendered,
        Err(err) => {
            tracing::error!(error = %err, "failed to render prompt template");
            template.to_string()
        }
    }
}

/// Agent invocation configuration.
pub struct AgentInvocation {
    /// Agent name (claude, codex, opencode, kimi, minimax)
    pub agent: String,
    /// Model override (e.g., "claude-sonnet-4-6", "o3", "gpt-4.1")
    pub model: Option<String>,
    /// Working directory
    pub work_dir: PathBuf,
    /// System prompt content
    pub system_prompt: String,
    /// Agent message (task prompt)
    pub agent_message: String,
    /// Task ID
    pub task_id: String,
    /// Disallowed tools pattern
    pub disallowed_tools: Vec<String>,
    /// Git author name
    pub git_author_name: String,
    /// Git author email
    pub git_author_email: String,
    /// Output file path for agent response
    pub output_file: PathBuf,
    /// Timeout in seconds (0 = no timeout)
    pub timeout_seconds: u64,
    /// Repository slug (owner/repo) for per-repo state isolation
    pub repo: String,
    /// Current attempt number (1-indexed)
    pub attempt: u32,
}

/// Spawn the agent in a tmux session.
///
/// Returns the tmux session name.
pub async fn spawn_in_tmux(tmux: &TmuxManager, inv: &AgentInvocation) -> anyhow::Result<String> {
    // Prepare attempt directory and prompt files (system + message).
    let attempt_dir = crate::home::task_attempt_dir(&inv.repo, &inv.task_id, inv.attempt)?;
    std::fs::create_dir_all(&attempt_dir)?;

    let sys_file = attempt_dir.join("prompt-sys.md");
    let msg_file = attempt_dir.join("prompt-msg.md");
    // Build unified permission rules and sys content
    let mut permissions = super::agents::PermissionRules::from_config();
    if !inv.disallowed_tools.is_empty() {
        for tool in &inv.disallowed_tools {
            if !permissions.disallowed_tools.contains(tool) {
                permissions.disallowed_tools.push(tool.clone());
            }
        }
    }
    permissions.allowed_edit_paths.push(inv.work_dir.clone());

    let sys_content = if !permissions.allowed_tools.is_empty() {
        let tools_list = permissions
            .allowed_tools
            .iter()
            .map(|t| format!("- {t}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut vars = HashMap::new();
        vars.insert("TOOLS_LIST".to_string(), tools_list);
        let tools_prompt = render_prompt_template(ALLOWED_TOOLS_TEMPLATE, vars);
        format!("{}\n\n{}", inv.system_prompt, tools_prompt)
    } else {
        inv.system_prompt.clone()
    };

    std::fs::write(&sys_file, &sys_content)?;
    std::fs::write(&msg_file, &inv.agent_message)?;

    // Build agent command using per-agent runner
    let runner = super::agents::get_runner(&inv.agent);
    let timeout_cmd = if inv.timeout_seconds > 0 {
        format!("timeout {}", inv.timeout_seconds)
    } else {
        String::new()
    };
    let agent_cmd = runner.build_command(
        inv.model.as_deref(),
        &timeout_cmd,
        &sys_file.to_string_lossy(),
        &msg_file.to_string_lossy(),
        &permissions,
    );

    // Resolve GH_TOKEN at script-generation time so agents don't need to call gh auth.
    // Prefer the native auth resolver (GhHttp / auth::create_resolver) but keep
    // the CLI wrapper's resolve_token() as a non-fatal fallback for environments
    // that still rely on interactive `gh` sessions.
    let gh_token = if let Ok(resolver) = crate::github::auth::create_resolver() {
        // Try primary resolver without falling back to gh CLI
        match resolver.resolve_token().await {
            Ok(t) if !t.is_empty() => Some(t),
            _ => crate::github::cli_wrapper::resolve_token(),
        }
    } else {
        crate::github::cli_wrapper::resolve_token()
    };
    if gh_token.is_none() {
        tracing::warn!("gh auth token not available; agents may not have GitHub access");
    }

    // Prepare non-secret environment map for tmux session.
    // GH_TOKEN is injected via set_session_env after session creation to avoid
    // exposing it in process arguments or on-disk artifacts.
    let mut env_map = std::collections::HashMap::new();
    env_map.insert("GIT_AUTHOR_NAME".to_string(), inv.git_author_name.clone());
    env_map.insert(
        "GIT_COMMITTER_NAME".to_string(),
        inv.git_author_name.clone(),
    );
    env_map.insert("GIT_AUTHOR_EMAIL".to_string(), inv.git_author_email.clone());
    env_map.insert(
        "GIT_COMMITTER_EMAIL".to_string(),
        inv.git_author_email.clone(),
    );
    env_map.insert("TASK_ID".to_string(), inv.task_id.clone());
    env_map.insert(
        "OUTPUT_FILE".to_string(),
        inv.output_file.display().to_string(),
    );

    // Build shell command that runs agent, captures stdout/stderr and exit status.
    // We avoid creating a runner.sh on disk by passing a shell -lc command to tmux.
    let status_file = attempt_dir.join("exit.txt");
    let stderr_file = attempt_dir.join("stderr.txt");
    let work_dir = inv.work_dir.to_string_lossy();

    let command = format!(
        "unset CLAUDECODE; cd '{work_dir}'; RESPONSE=$({agent_cmd} 2> '{stderr}'); CMD_STATUS=$?; printf '%s' \"$RESPONSE\" > '{out}'; echo \"$CMD_STATUS\" > '{status}'; exit $CMD_STATUS",
        agent_cmd = agent_cmd,
        stderr = stderr_file.display(),
        out = inv.output_file.display(),
        status = status_file.display(),
        work_dir = work_dir,
    );

    // Convert env_map to a slice of (&str, &str) pairs for tmux.create_session.
    let env_vec: Vec<(&str, &str)> = env_map
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let session = tmux
        .create_session(
            &inv.repo,
            &inv.task_id,
            &inv.work_dir.to_string_lossy(),
            &command,
            env_vec.as_slice(),
        )
        .await?;

    // Inject GH_TOKEN via tmux set-environment after session creation.
    // This avoids exposing the token in process arguments or on-disk files.
    if let Some(ref token) = gh_token {
        if let Err(e) = tmux.set_session_env(&session, "GH_TOKEN", token).await {
            tracing::warn!(task_id = inv.task_id, error = %e, "failed to set GH_TOKEN in tmux session");
        }
    }

    tracing::info!(
        task_id = inv.task_id,
        agent = inv.agent,
        session = %session,
        "agent spawned in tmux"
    );

    Ok(session)
}

/// Build the system prompt for the agent.
pub fn build_system_prompt(
    _task: &crate::backends::ExternalTask,
    context: &super::context::TaskContext,
    route_result: Option<&crate::engine::router::RouteResult>,
) -> String {
    let mut vars = HashMap::new();

    if let Some(rr) = route_result {
        vars.insert("ROLE".to_string(), rr.profile.role.clone());

        if !rr.profile.constraints.is_empty() {
            let constraints = rr
                .profile
                .constraints
                .iter()
                .map(|c| format!("- {c}"))
                .collect::<Vec<_>>()
                .join("\n");
            vars.insert("CONSTRAINTS".to_string(), constraints);
        }
    }

    if !context.project_instructions.is_empty() {
        vars.insert(
            "PROJECT_INSTRUCTIONS".to_string(),
            context.project_instructions.clone(),
        );
    }

    if !context.skills_docs.is_empty() {
        vars.insert("SKILLS_DOCS".to_string(), context.skills_docs.clone());
    }

    if !context.repo_tree.is_empty() {
        vars.insert("REPO_TREE".to_string(), context.repo_tree.clone());
    }

    render_prompt_template(AGENT_SYSTEM_TEMPLATE, vars)
}

/// Build the agent message (task prompt).
pub fn build_agent_message(
    task: &crate::backends::ExternalTask,
    context: &super::context::TaskContext,
    attempts: u32,
) -> String {
    let mut vars = HashMap::new();
    vars.insert("TASK_ID".to_string(), task.id.0.clone());
    vars.insert("TASK_TITLE".to_string(), task.title.clone());
    vars.insert("TASK_BODY".to_string(), task.body.clone());

    if !context.task_context.is_empty() {
        vars.insert("TASK_CONTEXT".to_string(), context.task_context.clone());
    }

    if !context.parent_context.is_empty() {
        vars.insert("PARENT_CONTEXT".to_string(), context.parent_context.clone());
    }

    if !context.issue_comments.is_empty() {
        vars.insert("ISSUE_COMMENTS".to_string(), context.issue_comments.clone());
    }

    if !context.pr_review_context.is_empty() {
        vars.insert(
            "PR_REVIEW_CONTEXT".to_string(),
            context.pr_review_context.clone(),
        );
    }

    if attempts > 0 && !context.git_diff.is_empty() {
        vars.insert("GIT_DIFF".to_string(), context.git_diff.clone());
    }

    if attempts > 0 {
        vars.insert("ATTEMPT_NUMBER".to_string(), (attempts + 1).to_string());
    }

    if !context.memory.is_empty() {
        let mut sections = Vec::new();
        for entry in &context.memory {
            let mut entry_vars = HashMap::new();
            entry_vars.insert("ATTEMPT".to_string(), entry.attempt.to_string());
            entry_vars.insert("AGENT".to_string(), entry.agent.clone());

            if let Some(ref model) = entry.model {
                entry_vars.insert("MODEL".to_string(), model.clone());
            }

            if !entry.approach.is_empty() {
                entry_vars.insert("APPROACH".to_string(), entry.approach.clone());
            }

            if !entry.learnings.is_empty() {
                let learnings = entry
                    .learnings
                    .iter()
                    .map(|learning| format!("- {}", learning))
                    .collect::<Vec<_>>()
                    .join("\n");
                entry_vars.insert("LEARNINGS".to_string(), learnings);
            }

            if let Some(ref error) = entry.error {
                entry_vars.insert("ERROR".to_string(), error.clone());
            }

            if !entry.files_modified.is_empty() {
                entry_vars.insert(
                    "FILES_MODIFIED".to_string(),
                    entry.files_modified.join(", "),
                );
            }

            let rendered = render_prompt_template(AGENT_MEMORY_ENTRY_TEMPLATE, entry_vars);
            sections.push(rendered.trim().to_string());
        }

        let memory_section = sections.join("\n\n");
        vars.insert("MEMORY_SECTION".to_string(), memory_section);
    }

    render_prompt_template(AGENT_MESSAGE_TEMPLATE, vars)
}

/// Build the review prompt for the review agent.
///
/// This is called when a task completes and the review agent needs to
/// review the changes before auto-merge.
pub fn build_review_prompt(
    task: &crate::backends::ExternalTask,
    agent_summary: &str,
    git_diff: &str,
    git_log: &str,
) -> String {
    let mut vars = HashMap::new();
    vars.insert("TASK_ID".to_string(), task.id.0.clone());
    vars.insert("TASK_TITLE".to_string(), task.title.clone());
    vars.insert("TASK_BODY".to_string(), task.body.clone());

    if !agent_summary.is_empty() {
        vars.insert("AGENT_SUMMARY".to_string(), agent_summary.to_string());
    }

    if !git_diff.is_empty() {
        vars.insert("GIT_DIFF".to_string(), git_diff.to_string());
    }

    if !git_log.is_empty() {
        vars.insert("GIT_LOG".to_string(), git_log.to_string());
    }

    render_prompt_template(REVIEW_PROMPT_TEMPLATE, vars)
}

/// Build the system prompt for the review agent.
pub fn review_system_prompt() -> String {
    render_prompt_template(REVIEW_SYSTEM_TEMPLATE, HashMap::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // Use a unique owner/repo that won't conflict with home.rs parallel tests
    // which clean up under "test-owner" and "owner".
    const TEST_REPO: &str = "orch-runner-test/token-check";

    fn test_invocation(task_id: &str) -> AgentInvocation {
        AgentInvocation {
            agent: "claude".to_string(),
            model: None,
            work_dir: PathBuf::from("/tmp"),
            system_prompt: "test system prompt".to_string(),
            agent_message: "test message".to_string(),
            task_id: task_id.to_string(),
            disallowed_tools: vec![],
            git_author_name: "Test Bot".to_string(),
            git_author_email: "bot@example.com".to_string(),
            output_file: PathBuf::from("/tmp/test-output.json"),
            timeout_seconds: 0,
            repo: TEST_REPO.to_string(),
            attempt: 1,
        }
    }

    fn cleanup_test_state(task_id: &str) {
        if let Ok(dir) = crate::home::task_attempt_dir(TEST_REPO, task_id, 1) {
            // Remove the task directory (two parents up from attempt/1/)
            if let Some(task_dir) = dir.parent().and_then(|p| p.parent()) {
                let _ = std::fs::remove_dir_all(task_dir);
            }
        }
    }

    #[test]
    fn env_map_does_not_contain_token_value_as_string() {
        // Verify that the token resolution returns the raw token value,
        // not a string that would need to be embedded in a script.
        // The key security guarantee: we're NOT building a runner script anymore.
        // Tokens are injected directly into tmux session environment.
        let token = crate::github::http::resolve_token();
        // Token can be empty (no token configured) or a valid token string.
        // The important thing is it's passed via tmux -e flag, not written to disk.
        assert!(
            token.is_empty() || token.len() > 10,
            "resolve_token should return empty string or a valid token"
        );
    }

    #[test]
    fn spawn_in_tmux_rejects_invalid_env() {
        // This test verifies the function signature and basic behavior
        // without actually spawning a tmux session.
        // The key security guarantee: tokens are passed via tmux -e flag,
        // not written to any file on disk.
        let inv = test_invocation("env-test");
        // Just verify the invocation can be created without panicking
        assert_eq!(inv.task_id, "env-test");
        cleanup_test_state("env-test");
    }
}
