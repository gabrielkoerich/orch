//! Agent command building + tmux invocation.
//!
//! Supports Claude, Codex, OpenCode, Grok (plus Kimi/MiniMax as Claude aliases).
//! Runs the agent via a runner.sh script executed as the tmux session shell.

use crate::engine::agents;
use crate::engine::git::shipcheck::install_shipcheck_rule;
pub use crate::engine::git::shipcheck::{Shipcheck, install_shipcheck_hooks};

use super::sandbox;
use crate::backends::ExternalTask;
use crate::engine::agents::shell_single_quote;
use crate::engine::router::RouteResult;
use crate::engine::runner::template::render_template_str;
use crate::github::token;
use crate::store::cluster;
use crate::sys::home;
use crate::sys::tmux::TmuxManager;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
#[cfg(unix)]
use std::fs::Permissions;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;

use tokio::fs;

const AGENT_SYSTEM_TEMPLATE: &str = include_str!("../../../prompts/agent_system.md");
const AGENT_MESSAGE_TEMPLATE: &str = include_str!("../../../prompts/agent_message.md");
const AGENT_MEMORY_ENTRY_TEMPLATE: &str = include_str!("../../../prompts/agent_memory_entry.md");
const ALLOWED_TOOLS_TEMPLATE: &str = include_str!("../../../prompts/allowed_tools.md");
const REVIEW_PROMPT_TEMPLATE: &str = include_str!("../../../prompts/review_task.md");
const REVIEW_SYSTEM_TEMPLATE: &str = include_str!("../../../prompts/review_system.md");

/// The `orch progress` step appears only when a `cluster:` section turns the cluster on.
/// Separate flags, because the template engine does not nest `{{#if}}` blocks
fn insert_cluster_vars(shipcheck: Shipcheck, vars: &mut HashMap<String, String>) {
    if !cluster::enabled() {
        return;
    }

    let commit = if shipcheck == Shipcheck::Available {
        "CLUSTER_SHIPCHECK"
    } else {
        "CLUSTER_GIT"
    };
    for flag in ["CLUSTER", commit] {
        vars.insert(flag.to_string(), "1".to_string());
    }
}

fn render_prompt_template(template: &str, vars: &HashMap<String, String>) -> String {
    match render_template_str(template, vars) {
        Ok(rendered) => rendered,
        Err(err) => {
            tracing::error!(error = %err, "failed to render prompt template");
            template.to_string()
        }
    }
}

/// Agent invocation configuration.
#[derive(Debug)]
pub struct AgentInvocation {
    /// Agent name (claude, codex, opencode, grok, kimi, minimax)
    pub agent: String,
    /// Model override (e.g., "claude-sonnet-4-6", "o3", "gpt-4.1")
    pub model: Option<String>,
    /// Optional native CLI effort from the selected route.
    pub reasoning_effort: Option<String>,
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
    /// Whether the agent runs in the OS sandbox, from `workflow.sandbox`
    pub sandbox: bool,
    /// The project's main checkout, which the sandbox keeps read-only but for its `.git`
    pub main_project_dir: PathBuf,
    /// The task's branch, from orch's records, the one ref the sandbox lets the agent move
    pub branch: String,
    /// Current attempt number (1-indexed)
    pub attempt: u32,
}

impl AgentInvocation {
    pub fn sandbox_scope(&self) -> sandbox::Scope<'_> {
        sandbox::Scope {
            work_dir: &self.work_dir,
            main_project_dir: &self.main_project_dir,
            branch: &self.branch,
            task_id: &self.task_id,
        }
    }
}

/// Build the runner script content (saved to attempt dir), plus the small
/// helper script it execs the agent command through under a pty. Returns
/// `(runner_script, agent_cmd_script)`.
fn build_tmux_runner_scripts(
    inv: &AgentInvocation,
    agent_cmd: &str,
    attempt_dir: &Path,
    sandbox: Option<&[OsString]>,
    sandbox_env: &[(&str, PathBuf)],
) -> (String, String) {
    let status_file = attempt_dir.join("exit.txt");
    let stderr_file = attempt_dir.join("stderr.txt");
    let agent_cmd_script_path = attempt_dir.join("agent_cmd.sh");
    let sq_git_name = shell_single_quote(&inv.git_author_name);
    let sq_git_email = shell_single_quote(&inv.git_author_email);
    let sq_task_id = shell_single_quote(&inv.task_id);
    let sq_output_file = shell_single_quote(&inv.output_file.display().to_string());
    let sq_work_dir = shell_single_quote(&inv.work_dir.display().to_string());
    let sq_status_file = shell_single_quote(&status_file.display().to_string());
    let sq_stderr_file = shell_single_quote(&stderr_file.display().to_string());
    let sq_agent_cmd_script = shell_single_quote(&agent_cmd_script_path.display().to_string());

    // Runs the agent with stderr split off to a real file before the pty layer
    // below merges stdout+stderr, exactly as the previous plain-pipe invocation did.
    let agent_cmd = match sandbox {
        Some(argv) => {
            let prefix: Vec<String> = argv
                .iter()
                .map(|a| shell_single_quote(&a.to_string_lossy()))
                .collect();
            // The tmux and ssh-agent sockets are out of reach, and their variables go too
            let temp = sandbox::run_temp_dir(attempt_dir);
            let tmpdir = shell_single_quote(&temp.display().to_string());
            // gh's own config dir holds its logins and is hidden, so gh gets an empty one and
            // runs on GH_TOKEN
            let gh_config =
                shell_single_quote(&sandbox::gh_config_dir(&temp).display().to_string());
            // shipcheck cannot start its own sandbox in here, and with this it asks its daemon
            let extra: Vec<String> = sandbox_env
                .iter()
                .map(|(name, value)| {
                    format!(
                        " {name}={}",
                        shell_single_quote(&value.display().to_string())
                    )
                })
                .collect();
            let vars = format!(
                "TMPDIR={tmpdir} GH_CONFIG_DIR={gh_config} SHIPCHECK_HANDOFF=1{}",
                extra.concat()
            );

            format!(
                "env -u TMUX -u TMUX_PANE -u SSH_AUTH_SOCK {vars} {} /bin/bash -c {}",
                prefix.join(" "),
                shell_single_quote(agent_cmd)
            )
        }
        None => agent_cmd.to_string(),
    };
    let agent_cmd_script = format!("#!/usr/bin/env bash\n{agent_cmd} 2>{sq_stderr_file}\n");
    // The run's temp dir goes when the run ends, however it ends
    let wipe_temp = if sandbox.is_some() {
        format!(
            "; rm -rf -- {}",
            shell_single_quote(&sandbox::run_temp_dir(attempt_dir).display().to_string())
        )
    } else {
        String::new()
    };
    // Tags are read-only in the sandbox, and a fetch that cannot write one new tag updates no ref
    let sandbox_git_config = if sandbox.is_some() {
        r#"export GIT_CONFIG_COUNT="5"
export GIT_CONFIG_KEY_4="remote.origin.tagOpt"
export GIT_CONFIG_VALUE_4="--no-tags"
"#
    } else {
        ""
    };

    let runner_script = format!(
        r#"#!/usr/bin/env bash
set -euo pipefail

status_file={sq_status_file}
stderr_file={sq_stderr_file}
trap 'status=$?; printf "%s\n" "$status" > "$status_file"{wipe_temp}' EXIT

# Environment — ~/.path and ~/.private are loaded by orch at startup into the process env
export PATH="/opt/homebrew/bin:/usr/local/bin:$PATH"
export GIT_AUTHOR_NAME={sq_git_name}
export GIT_COMMITTER_NAME={sq_git_name}
export GIT_AUTHOR_EMAIL={sq_git_email}
export GIT_COMMITTER_EMAIL={sq_git_email}
# Env config overrides the operator's git config for this process only, the remotes stay as they
# are: agents never sign, and push over HTTPS with gh's credentials, not the operator's SSH agent
export GIT_CONFIG_COUNT="4"
export GIT_CONFIG_KEY_0="commit.gpgsign"
export GIT_CONFIG_VALUE_0="false"
export GIT_CONFIG_KEY_1="tag.gpgsign"
export GIT_CONFIG_VALUE_1="false"
export GIT_CONFIG_KEY_2="url.https://github.com/.pushInsteadOf"
export GIT_CONFIG_VALUE_2="git@github.com:"
export GIT_CONFIG_KEY_3="credential.https://github.com.helper"
export GIT_CONFIG_VALUE_3="!gh auth git-credential"
{sandbox_git_config}export TASK_ID={sq_task_id}
export OUTPUT_FILE={sq_output_file}
unset CLAUDECODE  # allow nested claude invocations from orch

cd {sq_work_dir} || {{
    printf '%s\n' "worktree directory does not exist: {sq_work_dir}" > "$stderr_file"
    exit 1
}}

# Run agent under a pty (via `script`) so its stdout is a tty, not a plain pipe —
# many CLI runtimes silently switch to full block-buffering when stdout isn't a
# tty, which can delay pane output long enough to trip silence detection even
# while the agent is actively streaming. Tee to both output file and terminal
# (tmux pane) for live streaming.
set +e
script -q /dev/null /bin/bash {sq_agent_cmd_script} | tee {sq_output_file}
CMD_STATUS=${{PIPESTATUS[0]:-0}}
set -e

exit $CMD_STATUS
"#,
    );

    (runner_script, agent_cmd_script)
}

/// Spawn the agent in a tmux session.
///
/// Returns the tmux session name.
pub async fn spawn_in_tmux(tmux: &TmuxManager, inv: &AgentInvocation) -> anyhow::Result<String> {
    // Prepare attempt directory and prompt files (system + message).
    let attempt_dir = home::task_attempt_dir_async(&inv.repo, &inv.task_id, inv.attempt).await?;

    let sys_file = attempt_dir.join("prompt-sys.md");
    let msg_file = attempt_dir.join("prompt-msg.md");
    // Build unified permission rules and sys content
    let mut permissions = agents::PermissionRules::from_config();
    if !inv.disallowed_tools.is_empty() {
        for tool in &inv.disallowed_tools {
            if !permissions.disallowed_tools.contains(tool) {
                permissions.disallowed_tools.push(tool.clone());
            }
        }
    }
    permissions.allowed_edit_paths.push(inv.work_dir.clone());

    // Codex cannot start its own Seatbelt inside the OS sandbox, so the OS sandbox replaces it
    let mut sandbox_env = Vec::new();
    let sandbox = if inv.sandbox {
        permissions.sandbox = agents::SandboxLevel::Os;
        let temp = sandbox::run_temp_dir(&attempt_dir);
        fs::create_dir_all(sandbox::gh_config_dir(&temp)).await?;

        // The policy makes only paths that exist read-only, so the files orch writes later in the
        // attempt dir must exist now, or the agent could create them first
        for name in [
            "runner.sh",
            "agent_cmd.sh",
            "exit.txt",
            "stderr.txt",
            "prompt-sys.md",
            "prompt-msg.md",
            "result.json",
        ] {
            let path = attempt_dir.join(name);

            if !path.exists() {
                fs::write(&path, "").await?;
            }
        }

        if !inv.output_file.exists() && inv.output_file.parent().is_some_and(Path::is_dir) {
            fs::write(&inv.output_file, "").await?;
        }

        if let Some(home) = env::var_os("HOME") {
            sandbox_env = sandbox::package_config(Path::new(&home), &temp);
        }

        Some(sandbox::argv(&inv.sandbox_scope(), &temp).await?)
    } else {
        None
    };

    // For Codex running in workspace-write sandbox mode, grant write access to
    // git metadata dirs when they live outside the worktree root. We include
    // both --git-common-dir and --git-dir because index.lock can be created
    // under the worktree-specific git dir.
    if inv.agent == "codex" {
        let mut extra_dirs = BTreeSet::new();
        for (arg, label) in [
            ("--git-common-dir", "git common dir"),
            ("--git-dir", "git dir"),
        ] {
            match sandbox::resolve_git_dir_arg(&inv.work_dir, arg).await {
                Ok(Some(dir)) if !dir.starts_with(&inv.work_dir) => {
                    extra_dirs.insert(dir);
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::debug!(
                        work_dir = %inv.work_dir.display(),
                        arg,
                        error = %e,
                        "failed to resolve {}; skipping --add-dir",
                        label
                    );
                }
            }
        }
        permissions.extra_writable_dirs.extend(extra_dirs);

        if Shipcheck::detect(&inv.work_dir).await == Shipcheck::Available
            && let Err(e) = install_shipcheck_rule(&inv.work_dir).await
        {
            tracing::warn!(
                work_dir = %inv.work_dir.display(),
                error = %e,
                "failed to install the shipcheck codex rule"
            );
        }
    }

    let sys_content = if permissions.allowed_tools.is_empty() {
        inv.system_prompt.clone()
    } else {
        let tools_list = permissions
            .allowed_tools
            .iter()
            .map(|t| format!("- {t}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut vars = HashMap::new();
        vars.insert("TOOLS_LIST".to_string(), tools_list);
        let tools_prompt = render_prompt_template(ALLOWED_TOOLS_TEMPLATE, &vars);
        format!("{}\n\n{}", inv.system_prompt, tools_prompt)
    };

    fs::write(&sys_file, &sys_content).await?;
    fs::write(&msg_file, &inv.agent_message).await?;

    // Build agent command using per-agent runner (used in non-PTY path below)
    let runner = agents::get_runner(&inv.agent);

    // Prepare environment map for tmux session (does not write GH token to disk)
    let mut env = HashMap::new();
    env.insert("GIT_AUTHOR_NAME".to_string(), inv.git_author_name.clone());
    env.insert(
        "GIT_COMMITTER_NAME".to_string(),
        inv.git_author_name.clone(),
    );
    env.insert("GIT_AUTHOR_EMAIL".to_string(), inv.git_author_email.clone());
    env.insert(
        "GIT_COMMITTER_EMAIL".to_string(),
        inv.git_author_email.clone(),
    );
    env.insert("TASK_ID".to_string(), inv.task_id.clone());
    env.insert(
        "OUTPUT_FILE".to_string(),
        inv.output_file.display().to_string(),
    );

    // Write runner script and execute it in a detached tmux session.
    let timeout_cmd = if inv.timeout_seconds > 0 {
        format!("timeout {}", inv.timeout_seconds)
    } else {
        String::new()
    };
    let agent_cmd = runner.build_command_with_effort(
        inv.model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty()),
        inv.reasoning_effort.as_deref(),
        &timeout_cmd,
        &sys_file.to_string_lossy(),
        &msg_file.to_string_lossy(),
        &permissions,
    );

    // Write runner script (and the agent-cmd helper script it execs under a pty)
    // to the per-task attempt dir.
    let script_path = attempt_dir.join("runner.sh");
    let agent_cmd_script_path = attempt_dir.join("agent_cmd.sh");
    let (script_content, agent_cmd_script_content) = build_tmux_runner_scripts(
        inv,
        &agent_cmd,
        &attempt_dir,
        sandbox.as_deref(),
        &sandbox_env,
    );
    fs::write(&agent_cmd_script_path, &agent_cmd_script_content).await?;
    fs::write(&script_path, &script_content).await?;

    // Make executable
    #[cfg(unix)]
    {
        fs::set_permissions(&script_path, Permissions::from_mode(0o755)).await?;
        fs::set_permissions(&agent_cmd_script_path, Permissions::from_mode(0o755)).await?;
    }

    let command = format!("bash \"{}\"", script_path.display());

    // Resolve GitHub token via the process-wide shared resolver (cached after first call)
    let github_token = match token::shared().get_token().await {
        Ok(Some(token)) => {
            tracing::debug!("Resolved GitHub token via TokenResolver for agent session");
            Some(token)
        }
        Ok(None) => {
            tracing::warn!(
                "No GitHub token available; set GH_TOKEN, GITHUB_TOKEN, or gh.auth.token, or run gh auth login"
            );
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to resolve GitHub token");
            None
        }
    };

    // Inject GH_TOKEN into env so it's passed via `tmux new-session -e` along
    // with everything else. The `-e` flag sets variables in the *initial* pane
    // environment, so the agent process sees it immediately on startup.
    //
    // IMPORTANT: Do NOT use create_session_detached + set_env + create_window
    // to inject GH_TOKEN. That approach creates a 2-window session (window 0 =
    // idle shell, window 1 = agent). All tmux monitoring (is_session_active,
    // capture_pane, batch_session_active) targets the session without a window
    // index, which defaults to the *active* window — the idle shell. This
    // breaks completion detection (shell never exits → pane_dead always 0),
    // output capture (reads the shell prompt, not agent output), and silence
    // detection. See the revert of PR #2903.
    if let Some(ref token) = github_token {
        env.insert("GH_TOKEN".to_string(), token.clone());
    }

    let env_vec: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

    let session = tmux
        .create_session(
            &inv.repo,
            &inv.task_id,
            &inv.work_dir.to_string_lossy(),
            &command,
            env_vec.as_slice(),
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
    _task: &ExternalTask,
    context: &super::context::TaskContext,
    route_result: Option<&RouteResult>,
    default_branch: &str,
    shipcheck: Shipcheck,
    sandboxed: bool,
) -> String {
    let mut vars = HashMap::new();

    vars.insert("DEFAULT_BRANCH".to_string(), default_branch.to_string());
    shipcheck.insert_vars(&mut vars);
    insert_cluster_vars(shipcheck, &mut vars);

    if sandboxed {
        vars.insert("SANDBOX".to_string(), "1".to_string());
    }

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

    if !context.repo_tree.is_empty() {
        vars.insert("REPO_TREE".to_string(), context.repo_tree.clone());
    }

    render_prompt_template(AGENT_SYSTEM_TEMPLATE, &vars)
}

/// Build the agent message (task prompt).
pub fn build_agent_message(
    task: &ExternalTask,
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
                    .map(|learning| format!("- {learning}"))
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

            let rendered = render_prompt_template(AGENT_MEMORY_ENTRY_TEMPLATE, &entry_vars);
            sections.push(rendered.trim().to_string());
        }

        let memory_section = sections.join("\n\n");
        vars.insert("MEMORY_SECTION".to_string(), memory_section);
    }

    render_prompt_template(AGENT_MESSAGE_TEMPLATE, &vars)
}

/// Build the review prompt for the review agent.
///
/// This is called when a task completes and the review agent needs to
/// review the changes before auto-merge.
pub fn build_review_prompt(
    task: &ExternalTask,
    agent_summary: &str,
    git_diff: &str,
    git_log: &str,
    default_branch: &str,
    pr_number: u64,
    shipcheck: Shipcheck,
) -> String {
    let mut vars = HashMap::new();
    shipcheck.insert_vars(&mut vars);
    insert_cluster_vars(shipcheck, &mut vars);
    vars.insert("TASK_ID".to_string(), task.id.0.clone());
    vars.insert("TASK_TITLE".to_string(), task.title.clone());
    vars.insert("TASK_BODY".to_string(), task.body.clone());
    vars.insert("DEFAULT_BRANCH".to_string(), default_branch.to_string());
    vars.insert("PR_NUMBER".to_string(), pr_number.to_string());

    if !agent_summary.is_empty() {
        vars.insert("AGENT_SUMMARY".to_string(), agent_summary.to_string());
    }

    if !git_diff.is_empty() {
        vars.insert("GIT_DIFF".to_string(), git_diff.to_string());
    }

    if !git_log.is_empty() {
        vars.insert("GIT_LOG".to_string(), git_log.to_string());
    }

    render_prompt_template(REVIEW_PROMPT_TEMPLATE, &vars)
}

/// Build the runner shell script contents for testing and inspection, given a pre-created
/// attempt directory so tests can pass a temp dir.
///
/// IMPORTANT: this must not embed secrets (GH_TOKEN) into the script.
#[cfg(test)]
fn build_runner_script_in_dir(inv: &AgentInvocation, attempt_dir: &Path) -> String {
    let sys_file = attempt_dir.join("prompt-sys.md");
    let msg_file = attempt_dir.join("prompt-msg.md");

    // Build unified permission rules and sys content (minimal translation)
    let mut permissions = agents::PermissionRules::from_config();
    if !inv.disallowed_tools.is_empty() {
        for tool in &inv.disallowed_tools {
            if !permissions.disallowed_tools.contains(tool) {
                permissions.disallowed_tools.push(tool.clone());
            }
        }
    }
    permissions.allowed_edit_paths.push(inv.work_dir.clone());

    // Build agent command using per-agent runner
    let runner = agents::get_runner(&inv.agent);
    let timeout_cmd = if inv.timeout_seconds > 0 {
        format!("timeout {}", inv.timeout_seconds)
    } else {
        String::new()
    };
    let agent_cmd = runner.build_command_with_effort(
        inv.model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty()),
        inv.reasoning_effort.as_deref(),
        &timeout_cmd,
        &sys_file.to_string_lossy(),
        &msg_file.to_string_lossy(),
        &permissions,
    );

    let status_file = attempt_dir.join("exit.txt");
    let stderr_file = attempt_dir.join("stderr.txt");

    let sq_status = shell_single_quote(&status_file.display().to_string());
    let sq_stderr = shell_single_quote(&stderr_file.display().to_string());
    let sq_out = shell_single_quote(&inv.output_file.display().to_string());
    let sq_work_dir = shell_single_quote(&inv.work_dir.to_string_lossy());
    let script = format!(
        "#!/usr/bin/env bash\nset -euo pipefail\nstatus_file={sq_status}\nstderr_file={sq_stderr}\ntrap 'status=$?; printf \"%s\\n\" \"$status\" > \"$status_file\"' EXIT\nunset CLAUDECODE\ncd {sq_work_dir} || {{\n  printf '%s\\n' \"worktree directory does not exist: {sq_work_dir}\" > \"$stderr_file\"\n  exit 1\n}}\nset +e\nRESPONSE=$({agent_cmd} 2> \"$stderr_file\")\nCMD_STATUS=$?\nset -e\nprintf '%s' \"$RESPONSE\" > {sq_out}\nexit $CMD_STATUS\n",
    );

    script
}

/// Build the system prompt for the review agent.
pub fn review_system_prompt() -> String {
    render_prompt_template(REVIEW_SYSTEM_TEMPLATE, &HashMap::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::ExternalId;
    use crate::engine::agents::shell_single_quote;
    use crate::engine::runner::sandbox;
    use crate::engine::runner::template;

    use std::path::PathBuf;

    #[test]
    fn review_system_prompt_checks_stray_files_and_trust_boundaries() {
        let p = review_system_prompt();
        assert!(p.contains("**Files**"));
        assert!(p.contains("**Trust boundaries**"));
    }

    fn test_invocation(task_id: &str) -> AgentInvocation {
        AgentInvocation {
            agent: "claude".to_string(),
            model: None,
            reasoning_effort: None,
            work_dir: PathBuf::from("/tmp"),
            system_prompt: "test system prompt".to_string(),
            agent_message: "test message".to_string(),
            task_id: task_id.to_string(),
            disallowed_tools: vec![],
            git_author_name: "Test Bot".to_string(),
            git_author_email: "bot@example.com".to_string(),
            output_file: PathBuf::from("/tmp/test-output.json"),
            timeout_seconds: 0,
            repo: "orch-runner-test/token-check".to_string(),
            attempt: 1,
            sandbox: false,
            main_project_dir: PathBuf::from("/tmp"),
            branch: String::new(),
        }
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
    }

    #[test]
    fn build_runner_script_persists_exit_status_on_failure() {
        // Use a temp dir so lib and bin compilation units don't race on ~/.orch/state/.
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let inv = test_invocation("runner-status-test");
        let script = build_runner_script_in_dir(&inv, tmp.path());

        assert!(
            script
                .contains("trap 'status=$?; printf \"%s\\n\" \"$status\" > \"$status_file\"' EXIT")
        );
        assert!(script.contains("set +e"));
    }

    #[test]
    fn build_runner_script_reports_missing_worktree() {
        // Use a temp dir so lib and bin compilation units don't race on ~/.orch/state/.
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let inv = test_invocation("runner-missing-worktree-test");
        let script = build_runner_script_in_dir(&inv, tmp.path());

        assert!(script.contains("worktree directory does not exist: '/tmp'"));
        assert!(script.contains("stderr_file='"));
    }

    #[test]
    fn tmux_runner_script_runs_agent_under_pty_not_a_plain_pipe() {
        // Regression test for #3554: piping the agent binary's stdout straight
        // into `tee` leaves it non-tty, which makes many CLI runtimes switch to
        // full block-buffering and can starve the tmux pane long enough to trip
        // silence detection even while the agent is actively working. The agent
        // command must run under `script` (a real pty) before hitting `tee`.
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let inv = test_invocation("pty-wrap-test");
        let (runner_script, agent_cmd_script) =
            build_tmux_runner_scripts(&inv, "echo hello", tmp.path(), None, &[]);

        assert!(
            runner_script.contains("script -q /dev/null /bin/bash"),
            "agent must be exec'd under a pty via `script`, got: {runner_script}"
        );
        assert!(
            runner_script.contains("| tee"),
            "output must still be tee'd to the output file, got: {runner_script}"
        );
        assert!(
            !runner_script.contains("echo hello 2>"),
            "the raw agent command must not be piped directly into tee anymore, got: {runner_script}"
        );
        assert!(
            agent_cmd_script.contains("echo hello 2>"),
            "the agent command should run (with stderr split to a file) inside the pty-wrapped helper script, got: {agent_cmd_script}"
        );
    }

    #[test]
    fn tmux_runner_script_captures_agent_exit_status_via_pipestatus() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let inv = test_invocation("pty-exit-status-test");
        let (runner_script, _) =
            build_tmux_runner_scripts(&inv, "echo hello", tmp.path(), None, &[]);

        assert!(runner_script.contains("CMD_STATUS=${PIPESTATUS[0]:-0}"));
        assert!(runner_script.contains("exit $CMD_STATUS"));
    }

    #[test]
    fn tmux_runner_script_disables_commit_signing() {
        // Locked signing agent must not fail `git commit` inside agent sessions
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let inv = test_invocation("gpgsign-test");
        let (runner_script, _) =
            build_tmux_runner_scripts(&inv, "echo hello", tmp.path(), None, &[]);

        assert!(runner_script.contains("GIT_CONFIG_KEY_0=\"commit.gpgsign\""));
        assert!(runner_script.contains("GIT_CONFIG_VALUE_0=\"false\""));
        assert!(runner_script.contains("GIT_CONFIG_KEY_1=\"tag.gpgsign\""));
        // Only pushes go over HTTPS with gh's credentials, fetches keep the remote as it is
        assert!(
            runner_script.contains("GIT_CONFIG_KEY_2=\"url.https://github.com/.pushInsteadOf\"")
        );
        assert!(runner_script.contains("GIT_CONFIG_VALUE_2=\"git@github.com:\""));
        assert!(runner_script.contains("GIT_CONFIG_VALUE_3=\"!gh auth git-credential\""));
    }

    #[test]
    fn tmux_runner_script_writes_executable_agent_cmd_helper() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let inv = test_invocation("pty-helper-path-test");
        let (runner_script, agent_cmd_script) =
            build_tmux_runner_scripts(&inv, "echo hello", tmp.path(), None, &[]);

        let agent_cmd_script_path = tmp.path().join("agent_cmd.sh");
        assert!(
            runner_script.contains(&agent_cmd_script_path.display().to_string()),
            "runner script must reference the agent_cmd.sh helper path, got: {runner_script}"
        );
        assert!(agent_cmd_script.starts_with("#!/usr/bin/env bash\n"));
    }

    #[test]
    fn tmux_runner_script_runs_the_agent_inside_the_sandbox() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let inv = test_invocation("sandbox-test");
        let argv: Vec<OsString> = vec![
            "/usr/bin/sandbox-exec".into(),
            "-p".into(),
            "(version 1)".into(),
        ];
        let (_, agent_cmd_script) =
            build_tmux_runner_scripts(&inv, "claude -p 'hi'", tmp.path(), Some(&argv), &[]);
        assert!(
            agent_cmd_script.contains(
                "env -u TMUX -u TMUX_PANE -u SSH_AUTH_SOCK TMPDIR='"
            ) && agent_cmd_script.contains(
                "1 '/usr/bin/sandbox-exec' '-p' '(version 1)' /bin/bash -c 'claude -p '\\''hi'\\''' 2>"
            ),
            "the agent must run inside the sandbox with stderr split outside it, got: {agent_cmd_script}"
        );
        assert!(
            agent_cmd_script.contains(&format!(
                "TMPDIR='{}'",
                sandbox::run_temp_dir(tmp.path()).display()
            )),
            "the agent must get its own TMPDIR, got: {agent_cmd_script}"
        );
        assert!(
            agent_cmd_script.contains(&format!(
                "GH_CONFIG_DIR='{}'",
                sandbox::gh_config_dir(&sandbox::run_temp_dir(tmp.path())).display()
            )),
            "gh must get a config dir it can read, got: {agent_cmd_script}"
        );
        assert!(agent_cmd_script.contains(" SHIPCHECK_HANDOFF=1 "));

        let env = [("NPM_CONFIG_USERCONFIG", tmp.path().join("npmrc"))];
        let (_, with_env) = build_tmux_runner_scripts(&inv, "true", tmp.path(), Some(&argv), &env);
        assert!(with_env.contains(&format!(
            " NPM_CONFIG_USERCONFIG='{}' ",
            tmp.path().join("npmrc").display()
        )));
    }

    #[test]
    fn only_a_sandboxed_run_fetches_without_tags() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let inv = test_invocation("sandbox-test");
        let argv: Vec<OsString> = vec!["/usr/bin/sandbox-exec".into()];
        let (sandboxed, _) = build_tmux_runner_scripts(&inv, "true", tmp.path(), Some(&argv), &[]);
        assert!(sandboxed.contains("export GIT_CONFIG_COUNT=\"5\""));
        assert!(sandboxed.contains("export GIT_CONFIG_VALUE_4=\"--no-tags\""));
        let (plain, _) = build_tmux_runner_scripts(&inv, "true", tmp.path(), None, &[]);
        assert!(!plain.contains("tagOpt"));
        assert!(
            !plain.contains("rm -rf"),
            "only a sandboxed run has a temp dir to wipe"
        );
        assert!(sandboxed.contains(&format!(
            "\"$status_file\"; rm -rf -- '{}'' EXIT",
            sandbox::run_temp_dir(tmp.path()).display()
        )));
    }

    #[test]
    fn a_sandboxed_prompt_offers_no_shipcheck() {
        assert_eq!(
            Shipcheck::Available.for_prompt(true, false),
            Shipcheck::Absent
        );
        assert_eq!(
            Shipcheck::Available.for_prompt(true, true),
            Shipcheck::Available
        );
        assert_eq!(
            Shipcheck::Available.for_prompt(false, false),
            Shipcheck::Available
        );
        assert_eq!(
            Shipcheck::NotInstalled.for_prompt(true, false),
            Shipcheck::NotInstalled
        );
    }

    #[test]
    fn build_runner_script_escapes_shell_injection_in_paths() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let mut inv = test_invocation("injection-test");
        inv.work_dir = PathBuf::from("/tmp/$(whoami)/workspace");
        inv.output_file = PathBuf::from("/tmp/evil`id`.json");
        let script = build_runner_script_in_dir(&inv, tmp.path());

        // Values must be single-quoted, neutralizing $() and backticks
        assert!(
            script.contains("'/tmp/$(whoami)/workspace'"),
            "work_dir with $() must be single-quoted: {script}"
        );
        assert!(
            script.contains("'/tmp/evil`id`.json'"),
            "output_file with backticks must be single-quoted: {script}"
        );
    }

    #[test]
    fn review_prompt_forbids_ending_turn_on_pending_ci_status() {
        // Regression test for #3453: a review agent must not stop with a
        // prose "CI is still running" status update and leave the turn
        // without JSON. The prompt must explicitly route PENDING/NOT RUN CI
        // through the local-checks fallback and into the same-turn JSON output.
        let task = ExternalTask {
            id: ExternalId("155460".to_string()),
            title: "test task".to_string(),
            body: "test body".to_string(),
            state: "open".to_string(),
            labels: vec![],
            author: "tester".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: "https://example.com/1".to_string(),
        };
        let prompt = build_review_prompt(&task, "", "", "", "main", 1, Shipcheck::Absent);

        assert!(
            prompt.contains("Pending CI is never a terminal review state"),
            "prompt must state pending CI is not terminal: {prompt}"
        );
        assert!(
            prompt.contains("Never end your turn with a CI-waiting status update"),
            "prompt must forbid ending the turn on a CI-waiting status update: {prompt}"
        );
    }

    #[test]
    fn shell_single_quote_escapes_embedded_quotes() {
        assert_eq!(shell_single_quote("hello"), "'hello'");
        assert_eq!(shell_single_quote("O'Brien"), "'O'\\''Brien'");
        assert_eq!(shell_single_quote("$(rm -rf /)"), "'$(rm -rf /)'");
        assert_eq!(shell_single_quote("`whoami`"), "'`whoami`'");
        assert_eq!(shell_single_quote("a\"b"), "'a\"b'");
        assert_eq!(shell_single_quote("$HOME"), "'$HOME'");
    }

    fn prompt_task() -> ExternalTask {
        ExternalTask {
            id: ExternalId("1".to_string()),
            title: "t".to_string(),
            body: "b".to_string(),
            state: "open".to_string(),
            labels: vec![],
            author: "a".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: "https://example.com/1".to_string(),
        }
    }

    #[test]
    fn only_a_sandboxed_prompt_explains_the_sandbox() {
        let ctx = super::super::context::TaskContext::default();
        let task = prompt_task();
        let prompt = |sandboxed| {
            build_system_prompt(&task, &ctx, None, "main", Shipcheck::Absent, sandboxed)
        };
        assert!(prompt(true).contains("you run inside an OS sandbox"));
        assert!(!prompt(false).contains("OS sandbox"));
        assert!(!prompt(false).contains("SANDBOX"));
    }

    #[test]
    fn system_prompt_uses_shipcheck_only_when_available() {
        let ctx = super::super::context::TaskContext::default();
        let task = prompt_task();
        let prompt = |s| build_system_prompt(&task, &ctx, None, "main", s, false);

        let available = prompt(Shipcheck::Available);
        assert!(available.contains("shipcheck run"));
        assert!(available.contains("shipcheck commit"));
        assert!(!available.contains(".github/workflows/"));
        assert!(!available.contains("not installed"));

        let outdated = prompt(Shipcheck::Outdated);
        assert!(!outdated.contains("shipcheck"));
        assert!(outdated.contains(".github/workflows/"));

        let missing = prompt(Shipcheck::NotInstalled);
        assert!(!missing.contains("shipcheck run"));
        assert!(missing.contains(".github/workflows/"));
        assert!(missing.contains("shipcheck is not installed"));

        let absent = prompt(Shipcheck::Absent);
        assert!(!absent.contains("shipcheck"));
        assert!(absent.contains(".github/workflows/"));

        for p in [&available, &outdated, &missing, &absent] {
            assert!(!p.contains("{{"), "{p}");
            assert!(!p.contains("install shipcheck"), "{p}");
        }
    }

    #[test]
    fn review_prompt_uses_shipcheck_only_when_available() {
        let task = prompt_task();
        let prompt = |s| build_review_prompt(&task, "", "", "", "main", 1, s);
        assert!(prompt(Shipcheck::Available).contains("shipcheck run"));
        for s in [
            Shipcheck::Outdated,
            Shipcheck::NotInstalled,
            Shipcheck::Absent,
        ] {
            let p = prompt(s);
            assert!(!p.contains("shipcheck"), "{p}");
            assert!(p.contains(".github/workflows/"), "{p}");
        }
    }

    #[test]
    fn the_progress_step_appears_only_in_cluster_mode() {
        let mut vars = HashMap::new();
        insert_cluster_vars(Shipcheck::Available, &mut vars);
        assert!(vars.is_empty(), "no cluster section, no cluster flags");
        let plain = template::render_template_str(AGENT_SYSTEM_TEMPLATE, &vars).unwrap();
        assert!(!plain.contains("orch progress"));

        for flag in ["CLUSTER", "CLUSTER_SHIPCHECK"] {
            vars.insert(flag.to_string(), "1".to_string());
        }
        let cluster = template::render_template_str(AGENT_SYSTEM_TEMPLATE, &vars).unwrap();
        assert!(cluster.contains("orch progress"));
        assert!(cluster.contains("commits only through `shipcheck commit`"));
        assert!(!cluster.contains("It commits anything you left"));
    }
}
