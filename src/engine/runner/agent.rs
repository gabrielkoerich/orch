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
#[allow(dead_code)]
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
    /// Branch name
    pub branch: String,
    /// Main project directory (for sandbox)
    pub main_project_dir: PathBuf,
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

/// Build the runner script content that tmux will execute.
///
/// The script sets up environment, runs the agent, captures output and exit code.
/// Delegates agent-specific command building to the `AgentRunner` trait, which
/// translates unified `PermissionRules` into each agent's native CLI flags.
pub fn build_runner_script(inv: &AgentInvocation) -> anyhow::Result<String> {
    // Use per-task attempt directory for artifacts (per-repo isolation)
    let attempt_dir = crate::home::task_attempt_dir(&inv.repo, &inv.task_id, inv.attempt)?;

    let sys_file = attempt_dir.join("prompt-sys.md");
    let msg_file = attempt_dir.join("prompt-msg.md");
    let status_file = attempt_dir.join("exit.txt");

    std::fs::create_dir_all(&attempt_dir)?;

    let timeout_cmd = if inv.timeout_seconds > 0 {
        format!("timeout {}", inv.timeout_seconds)
    } else {
        String::new()
    };

    // Build unified permission rules from config + invocation
    let mut permissions = super::agents::PermissionRules::from_config();

    // Merge invocation-specific disallowed tools into the unified rules
    if !inv.disallowed_tools.is_empty() {
        for tool in &inv.disallowed_tools {
            if !permissions.disallowed_tools.contains(tool) {
                permissions.disallowed_tools.push(tool.clone());
            }
        }
    }

    // Restrict edits to the working directory (worktree)
    permissions.allowed_edit_paths.push(inv.work_dir.clone());

    // Write prompt files with allowed tools appended to system prompt
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

    // Get the per-agent runner and delegate command building
    let runner = super::agents::get_runner(&inv.agent);
    let agent_cmd = runner.build_command(
        inv.model.as_deref(),
        &timeout_cmd,
        &sys_file.to_string_lossy(),
        &msg_file.to_string_lossy(),
        &permissions,
    );

    Ok(format!(
        r#"#!/usr/bin/env bash
set -euo pipefail

# Environment
[ -f "$HOME/.path" ] && source "$HOME/.path"
[ -f "$HOME/.private" ] && source "$HOME/.private"
export PATH="/opt/homebrew/bin:/usr/local/bin:$PATH"
export GIT_AUTHOR_NAME="{git_name}"
export GIT_COMMITTER_NAME="{git_name}"
export GIT_AUTHOR_EMAIL="{git_email}"
export GIT_COMMITTER_EMAIL="{git_email}"
export TASK_ID="{task_id}"
export OUTPUT_FILE="{output_file}"
unset CLAUDECODE  # allow nested claude invocations from orchestrator

cd "{work_dir}"

# Run agent
RESPONSE=$({agent_cmd} 2>"{attempt_dir}/stderr.txt") || CMD_STATUS=$?
CMD_STATUS=${{CMD_STATUS:-0}}

# Save response
printf '%s' "$RESPONSE" > "{output_file}"

# Save exit status
echo "$CMD_STATUS" > "{status_file}"

exit $CMD_STATUS
"#,
        git_name = inv.git_author_name,
        git_email = inv.git_author_email,
        task_id = inv.task_id,
        output_file = inv.output_file.display(),
        work_dir = inv.work_dir.display(),
        agent_cmd = agent_cmd,
        attempt_dir = attempt_dir.display(),
        status_file = status_file.display(),
    ))
}

/// Spawn the agent in a tmux session.
///
/// Returns the tmux session name.
pub async fn spawn_in_tmux(tmux: &TmuxManager, inv: &AgentInvocation) -> anyhow::Result<String> {
    let script_content = build_runner_script(inv)?;

    // Write runner script to per-task attempt dir
    let attempt_dir = crate::home::task_attempt_dir(&inv.repo, &inv.task_id, inv.attempt)?;
    let script_path = attempt_dir.join("runner.sh");
    std::fs::write(&script_path, &script_content)?;

    // Make executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))?;
    }

    let command = format!("bash {}", script_path.display());
    let session = tmux
        .create_session(
            &inv.repo,
            &inv.task_id,
            &inv.work_dir.to_string_lossy(),
            &command,
        )
        .await?;

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
