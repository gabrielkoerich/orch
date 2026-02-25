//! Agent command building + tmux invocation.
//!
//! Supports Claude, Codex, OpenCode (plus Kimi/MiniMax as Claude aliases).
//! Generates a runner shell script that tmux executes — agents need a real terminal.

use crate::tmux::TmuxManager;
use std::path::PathBuf;

/// Agent invocation configuration.
#[allow(dead_code)]
pub struct AgentInvocation {
    /// Agent name (claude, codex, opencode, kimi, minimax)
    pub agent: String,
    /// Model override (e.g., "sonnet", "opus", "gpt-5.2")
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
}

/// Build the runner script content that tmux will execute.
///
/// The script sets up environment, runs the agent, captures output and exit code.
pub fn build_runner_script(inv: &AgentInvocation) -> String {
    let state_dir = dirs::home_dir()
        .unwrap_or_default()
        .join(".orchestrator")
        .join(".orchestrator");

    let sys_file = state_dir.join(format!("prompt-{}-sys.txt", inv.task_id));
    let msg_file = state_dir.join(format!("prompt-{}-msg.txt", inv.task_id));
    let status_file = state_dir.join(format!("exit-{}.txt", inv.task_id));

    // Write prompt files
    std::fs::create_dir_all(&state_dir).ok();
    std::fs::write(&sys_file, &inv.system_prompt).ok();
    std::fs::write(&msg_file, &inv.agent_message).ok();

    let model_flag = inv
        .model
        .as_ref()
        .map(|m| format!("--model {m}"))
        .unwrap_or_default();

    let timeout_cmd = if inv.timeout_seconds > 0 {
        format!("timeout {}", inv.timeout_seconds)
    } else {
        String::new()
    };

    let disallowed = if !inv.disallowed_tools.is_empty() {
        format!("--disallowedTools '{}'", inv.disallowed_tools.join(","))
    } else {
        String::new()
    };

    let agent_cmd = match inv.agent.as_str() {
        "claude" | "kimi" | "minimax" => {
            // Claude-compatible agents
            let binary = &inv.agent;
            format!(
                r#"{timeout_cmd} {binary} -p {model_flag} \
  --permission-mode bypassPermissions \
  --output-format json \
  {disallowed} \
  --append-system-prompt "{sys_file}" \
  < "{msg_file}""#,
                timeout_cmd = timeout_cmd,
                binary = binary,
                model_flag = model_flag,
                disallowed = disallowed,
                sys_file = sys_file.display(),
                msg_file = msg_file.display(),
            )
        }
        "codex" => {
            format!(
                r#"cat "{msg_file}" | {timeout_cmd} codex {model_flag} \
  --ask-for-approval never \
  --sandbox workspace-write \
  exec --json -"#,
                msg_file = msg_file.display(),
                timeout_cmd = timeout_cmd,
                model_flag = model_flag,
            )
        }
        "opencode" => {
            format!(
                r#"{timeout_cmd} opencode run {model_flag} \
  --format json - < "{msg_file}""#,
                timeout_cmd = timeout_cmd,
                model_flag = model_flag,
                msg_file = msg_file.display(),
            )
        }
        other => {
            // Unknown agent — try as claude-compatible
            tracing::warn!(
                agent = other,
                "unknown agent, using claude-compatible invocation"
            );
            format!(
                r#"{timeout_cmd} {other} -p {model_flag} \
  --permission-mode bypassPermissions \
  --output-format json \
  --append-system-prompt "{sys_file}" \
  < "{msg_file}""#,
                timeout_cmd = timeout_cmd,
                other = other,
                model_flag = model_flag,
                sys_file = sys_file.display(),
                msg_file = msg_file.display(),
            )
        }
    };

    format!(
        r#"#!/usr/bin/env bash
set -euo pipefail

# Environment
export PATH="/opt/homebrew/bin:/usr/local/bin:$PATH"
export GIT_AUTHOR_NAME="{git_name}"
export GIT_COMMITTER_NAME="{git_name}"
export GIT_AUTHOR_EMAIL="{git_email}"
export GIT_COMMITTER_EMAIL="{git_email}"
export TASK_ID="{task_id}"
export OUTPUT_FILE="{output_file}"

cd "{work_dir}"

# Run agent
RESPONSE=$({agent_cmd} 2>"{state_dir}/stderr-{task_id}.txt") || CMD_STATUS=$?
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
        state_dir = state_dir.display(),
        status_file = status_file.display(),
    )
}

/// Spawn the agent in a tmux session.
///
/// Returns the tmux session name.
pub async fn spawn_in_tmux(tmux: &TmuxManager, inv: &AgentInvocation) -> anyhow::Result<String> {
    let script_content = build_runner_script(inv);

    // Write runner script
    let state_dir = dirs::home_dir()
        .unwrap_or_default()
        .join(".orchestrator")
        .join(".orchestrator");
    std::fs::create_dir_all(&state_dir)?;

    let script_path = state_dir.join(format!("runner-{}.sh", inv.task_id));
    std::fs::write(&script_path, &script_content)?;

    // Make executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))?;
    }

    let command = format!("bash {}", script_path.display());
    let session = tmux
        .create_session(&inv.task_id, &inv.work_dir.to_string_lossy(), &command)
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
    let mut prompt = String::new();

    // Role from routing
    if let Some(rr) = route_result {
        prompt.push_str(&format!("You are a {} agent.\n\n", rr.profile.role));
        if !rr.profile.constraints.is_empty() {
            prompt.push_str("## Constraints\n\n");
            for c in &rr.profile.constraints {
                prompt.push_str(&format!("- {c}\n"));
            }
            prompt.push('\n');
        }
    }

    // Project instructions
    if !context.project_instructions.is_empty() {
        prompt.push_str("## Project Instructions\n\n");
        prompt.push_str(&context.project_instructions);
        prompt.push('\n');
    }

    // Skills docs
    if !context.skills_docs.is_empty() {
        prompt.push_str("## Available Skills\n\n");
        prompt.push_str(&context.skills_docs);
        prompt.push('\n');
    }

    // Repo tree
    if !context.repo_tree.is_empty() {
        prompt.push_str("## Repository Structure\n\n```\n");
        prompt.push_str(&context.repo_tree);
        prompt.push_str("\n```\n\n");
    }

    // Output format instructions
    prompt.push_str(
        r#"## Output Format

You MUST output a JSON object with the following fields:

```json
{
  "status": "done|in_progress|blocked|needs_review",
  "summary": "Brief summary of what was accomplished",
  "accomplished": ["list of things done"],
  "remaining": ["list of remaining items"],
  "files": ["list of files changed"],
  "reason": "reason if blocked or needs_review"
}
```
"#,
    );

    prompt
}

/// Build the agent message (task prompt).
pub fn build_agent_message(
    task: &crate::backends::ExternalTask,
    context: &super::context::TaskContext,
    attempts: u32,
) -> String {
    let mut msg = String::new();

    msg.push_str(&format!(
        "# Task #{}: {}\n\n{}\n\n",
        task.id.0, task.title, task.body
    ));

    // Previous context
    if !context.task_context.is_empty() {
        msg.push_str("## Previous Context\n\n");
        msg.push_str(&context.task_context);
        msg.push('\n');
    }

    // Parent context
    if !context.parent_context.is_empty() {
        msg.push_str(&context.parent_context);
    }

    // Issue comments
    if !context.issue_comments.is_empty() {
        msg.push_str("## Recent Comments\n\n");
        msg.push_str(&context.issue_comments);
        msg.push('\n');
    }

    // Git diff for retries
    if attempts > 0 && !context.git_diff.is_empty() {
        msg.push_str("## Current Changes (from previous attempt)\n\n```diff\n");
        msg.push_str(&context.git_diff);
        msg.push_str("\n```\n\n");
    }

    if attempts > 0 {
        msg.push_str(&format!(
            "\nThis is attempt #{} (previous attempts may have made partial progress).\n",
            attempts + 1
        ));
    }

    msg
}
