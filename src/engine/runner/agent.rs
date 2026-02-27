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

    // Write prompt files - fail if we can't write them
    std::fs::create_dir_all(&attempt_dir)?;
    std::fs::write(&sys_file, &inv.system_prompt)?;
    std::fs::write(&msg_file, &inv.agent_message)?;

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

    // Block main project dir when running in a worktree
    if inv.work_dir != inv.main_project_dir {
        permissions.blocked_paths.push(inv.main_project_dir.clone());
    }

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
export PATH="/opt/homebrew/bin:/usr/local/bin:$PATH"
export GIT_AUTHOR_NAME="{git_name}"
export GIT_COMMITTER_NAME="{git_name}"
export GIT_AUTHOR_EMAIL="{git_email}"
export GIT_COMMITTER_EMAIL="{git_email}"
export TASK_ID="{task_id}"
export OUTPUT_FILE="{output_file}"

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

    // Rules + workflow instructions
    prompt.push_str(
        r#"## Rules

- NEVER use `rm` to delete files. Use `trash` (macOS) or `trash-put` (Linux).
- NEVER commit directly to the main/master branch. You are on a feature branch.
- NEVER modify files outside your worktree. Everything outside your current working directory is read-only.
- If a skill is marked REQUIRED, you MUST follow its workflow exactly.
- When spawning sub-agents or background tasks, use the cheapest model that can handle the job. Reserve expensive models for complex reasoning and debugging.

## Worktree

You are running inside an isolated git worktree on a feature branch. Do NOT create worktrees or branches yourself — the orchestrator manages that.

Everything outside your current working directory is **read-only**. Never `cd ..` to modify the parent repo or any other directory. All your changes stay in this worktree.

## Workflow — CRITICAL

1. **On retry**: check `git diff main` and `git log main..HEAD` first to see what previous attempts already did. Build on existing work — do not start over.
2. **Commit step by step** as you work, not one big commit at the end. Use conventional commit messages (`feat:`, `fix:`, `docs:`, `refactor:`, `test:`, etc.).
3. **Lockfiles**: if you add, remove, or update dependencies, regenerate the lockfile before committing (`bun install`, `npm install`, `cargo update`, etc.). Always commit the updated lockfile with your changes.
4. **Test before done**: before marking work as done, run the project's test suite and type checker (`cargo test`, `npm test`, `pytest`, `tsc --noEmit`, etc.). Fix any failures. If tests fail and you cannot fix them, set status to `needs_review` and explain the failures.
5. **Push**: `git push origin HEAD` after committing.
6. **Create PR**: if no PR exists for this branch, create one using `gh pr create --base main` linking `Closes #<issue>`.

Do NOT skip any of these steps. Do NOT report "done" unless you have committed, pushed, and verified the PR exists. If you only make changes without committing and pushing, your work will be lost.

If git push fails (e.g., auth error, no remote), set status to `needs_review` with the error.

## Output Format

Your final output MUST be a JSON object with these fields:

```json
{
  "status": "done|in_progress|blocked|needs_review",
  "summary": "Brief summary of what was accomplished",
  "accomplished": ["list of things done"],
  "remaining": ["list of remaining items"],
  "files_changed": ["list of files modified"],
  "blockers": ["list of blockers, empty if none"],
  "reason": "reason if blocked or needs_review, empty string otherwise",
  "delegations": [{"title": "...", "body": "...", "labels": ["..."]}]
}
```

Note: `delegations` is optional — only include it when delegating subtasks.

Status rules:
- **done**: all work is committed, pushed, PR created, and tests pass. You must have produced a visible result (committed code, posted a comment, or completed the requested action). Pure research with no output is `in_progress`.
- **in_progress**: partial work was committed but more remains.
- **blocked**: waiting on dependencies, missing information, or delegated subtasks.
- **needs_review**: encountered errors you cannot resolve.

## Task Delegation

If a task is too complex for a single agent, you can delegate subtasks. Include a `delegations` array in your response:

```json
{
  "status": "blocked",
  "summary": "Decomposed into subtasks",
  "accomplished": ["Analyzed requirements"],
  "remaining": ["Waiting on subtasks"],
  "delegations": [
    {"title": "Subtask title", "body": "Detailed description of the subtask", "labels": ["label1"]},
    {"title": "Another subtask", "body": "Description", "labels": ["label2"]}
  ]
}
```

Delegation rules:
- Set status to `blocked` when delegating — you will be re-run after all subtasks complete.
- Each delegation becomes a separate GitHub issue routed to an agent independently.
- Provide clear, detailed descriptions in `body` so the subtask agent has full context.
- Only delegate when the task genuinely requires parallel workstreams or different expertise.
- Do not delegate trivial work — just do it yourself.
- Labels are optional — the orchestrator will route each subtask automatically.

## Visibility

Your output is parsed by the orchestrator and posted as a comment on the GitHub issue. Write clear, detailed summaries:
- **accomplished**: be specific (e.g., "Fixed memcmp offset from 40 to 48 in yieldRates.ts", not "Fixed bug")
- **remaining**: tell the owner what's left, what the next attempt should do
- **files_changed**: include every file you touched
- **reason**: include the exact command and error message, not just "permission denied"
- **blockers**: be actionable (e.g., "Need SSH key configured for git push", not "Permission denied")
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

    // PR review context (for re-dispatch after review changes requested)
    if !context.pr_review_context.is_empty() {
        msg.push_str("## PR Review Feedback\n\n");
        msg.push_str("A reviewer has requested changes on your PR. Please address the following feedback:\n\n");
        msg.push_str(&context.pr_review_context);
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

    // Memory from previous attempts
    if !context.memory.is_empty() {
        msg.push_str("\n## Previous Attempts Memory\n\n");
        msg.push_str(
            "Learnings from previous task attempts (to help you avoid repeating mistakes):\n\n",
        );

        for entry in &context.memory {
            msg.push_str(&format!(
                "### Attempt #{} (Agent: {})",
                entry.attempt, entry.agent
            ));

            if let Some(ref model) = entry.model {
                msg.push_str(&format!(", Model: {}", model));
            }
            msg.push('\n');

            if !entry.approach.is_empty() {
                msg.push_str(&format!("**Approach**: {}\n", entry.approach));
            }

            if !entry.learnings.is_empty() {
                msg.push_str("**Key Learnings**:\n");
                for learning in &entry.learnings {
                    msg.push_str(&format!("- {}\n", learning));
                }
            }

            if let Some(ref error) = entry.error {
                msg.push_str(&format!("**Error**: {}\n", error));
            }

            if !entry.files_modified.is_empty() {
                msg.push_str(&format!(
                    "**Files Modified**: {}\n",
                    entry.files_modified.join(", ")
                ));
            }

            msg.push('\n');
        }
    }

    msg
}
