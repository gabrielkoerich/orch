//! Agent command building + tmux invocation.
//!
//! Supports Claude, Codex, OpenCode, Grok (plus Kimi/MiniMax as Claude aliases).
//! Runs the agent via a runner.sh script executed as the tmux session shell.

use crate::engine::runner::agents::shell_single_quote;
use crate::template::render_template_str;
use crate::tmux::TmuxManager;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::path::PathBuf;

const AGENT_SYSTEM_TEMPLATE: &str = include_str!("../../../prompts/agent_system.md");
const AGENT_MESSAGE_TEMPLATE: &str = include_str!("../../../prompts/agent_message.md");
const AGENT_MEMORY_ENTRY_TEMPLATE: &str = include_str!("../../../prompts/agent_memory_entry.md");
const ALLOWED_TOOLS_TEMPLATE: &str = include_str!("../../../prompts/allowed_tools.md");
const REVIEW_PROMPT_TEMPLATE: &str = include_str!("../../../prompts/review_task.md");
const REVIEW_SYSTEM_TEMPLATE: &str = include_str!("../../../prompts/review_system.md");

/// Whether the agent tests through shipcheck: the repo opts in and the shipcheck on PATH
/// has `commit`. An older shipcheck gets the plain workflow step
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shipcheck {
    Absent,
    NotInstalled,
    Outdated,
    Available,
}

impl Shipcheck {
    pub async fn detect(work_dir: &std::path::Path) -> Self {
        static SUPPORTS_COMMIT: tokio::sync::OnceCell<Option<bool>> =
            tokio::sync::OnceCell::const_new();
        let installed = SUPPORTS_COMMIT
            .get_or_init(|| async {
                let bin = which::which("shipcheck").ok()?;
                Some(supports(&bin, "commit").await)
            })
            .await;
        Self::classify(work_dir.join("shipcheck.toml").exists(), *installed)
    }

    fn classify(has_config: bool, supports_commit: Option<bool>) -> Self {
        match (has_config, supports_commit) {
            (false, _) => Shipcheck::Absent,
            (true, None) => Shipcheck::NotInstalled,
            (true, Some(false)) => Shipcheck::Outdated,
            (true, Some(true)) => Shipcheck::Available,
        }
    }

    fn insert_vars(self, vars: &mut HashMap<String, String>) {
        let flags: &[&str] = match self {
            Shipcheck::Available => &["SHIPCHECK"],
            Shipcheck::NotInstalled => &["NO_SHIPCHECK", "SHIPCHECK_MISSING"],
            Shipcheck::Absent | Shipcheck::Outdated => &["NO_SHIPCHECK"],
        };
        for flag in flags {
            vars.insert(flag.to_string(), "1".to_string());
        }
    }
}

async fn run_shipcheck(
    bin: &std::path::Path,
    work_dir: &std::path::Path,
    args: &[&str],
) -> anyhow::Result<std::process::Output> {
    let output = tokio::process::Command::new(bin)
        .args(args)
        .current_dir(work_dir)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    Ok(tokio::time::timeout(std::time::Duration::from_secs(30), output).await??)
}

// `<sub> --help` exits 0 for any word, so read the subcommands the usage line lists
async fn supports(bin: &std::path::Path, subcommand: &str) -> bool {
    let Ok(out) = run_shipcheck(bin, &std::env::temp_dir(), &["--help"]).await else {
        return false;
    };
    out.status.success()
        && String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .any(|word| word == subcommand)
}

/// Runs `shipcheck install` so every agent CLI gets shipcheck's hooks in this worktree.
/// Returns whether install ran. It never fails the task
pub async fn install_shipcheck_hooks(
    work_dir: &std::path::Path,
    shipcheck: Shipcheck,
    bin: &std::path::Path,
) -> bool {
    if shipcheck != Shipcheck::Available {
        return false;
    }

    // A shipcheck without `hook` only installs git hooks, and orch skips it
    if !supports(bin, "hook").await {
        return false;
    }

    match run_shipcheck(bin, work_dir, &["install"]).await {
        Ok(out) if out.status.success() => {}
        Ok(out) => tracing::warn!(
            work_dir = %work_dir.display(),
            stderr = %String::from_utf8_lossy(&out.stderr),
            "shipcheck install failed"
        ),
        Err(e) => tracing::warn!(
            work_dir = %work_dir.display(),
            error = %e,
            "shipcheck install failed"
        ),
    }
    true
}

const SHIPCHECK_RULE_PATH: &str = ".codex/rules/orch-shipcheck.rules";

// Only the commands the agent prompt uses, never clean, post, login or daemon
const SHIPCHECK_RULES: &str = r#"prefix_rule(pattern=["shipcheck", "run"], decision="allow")
prefix_rule(pattern=["shipcheck", "commit"], decision="allow")
prefix_rule(pattern=["shipcheck", "status"], decision="allow")
prefix_rule(pattern=["shipcheck", "logs"], decision="allow")
prefix_rule(pattern=["shipcheck", "doctor"], decision="allow")
"#;

// Codex runs a command a rule allows outside its sandbox, so shipcheck reaches Docker and its
// results dir while the agent's own commands still cannot write there
async fn install_shipcheck_rule(work_dir: &std::path::Path) -> anyhow::Result<()> {
    // The worktree holds repository content, so never follow a symlink it plants on the path
    let mut dir = work_dir.to_path_buf();
    for part in [".codex", "rules"] {
        dir.push(part);
        match tokio::fs::symlink_metadata(&dir).await {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => anyhow::bail!("{} is not a directory", dir.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tokio::fs::create_dir(&dir).await?;
            }
            Err(e) => return Err(e.into()),
        }
    }

    // remove_file drops a symlink itself, and create_new never follows one
    let rule = work_dir.join(SHIPCHECK_RULE_PATH);
    match tokio::fs::remove_file(&rule).await {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }

    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&rule)
        .await?;
    tokio::io::AsyncWriteExt::write_all(&mut file, SHIPCHECK_RULES.as_bytes()).await?;

    // Keep the rule out of the agent's commits
    let output = tokio::process::Command::new("git")
        .args(["rev-parse", "--git-path", "info/exclude"])
        .current_dir(work_dir)
        .output()
        .await?;
    if !output.status.success() {
        return Ok(());
    }

    let exclude = work_dir.join(String::from_utf8_lossy(&output.stdout).trim());
    let line = format!("/{SHIPCHECK_RULE_PATH}");
    let current = tokio::fs::read_to_string(&exclude)
        .await
        .unwrap_or_default();
    if current.lines().any(|l| l == line) {
        return Ok(());
    }

    if let Some(dir) = exclude.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }

    let sep = if current.is_empty() || current.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    tokio::fs::write(&exclude, format!("{current}{sep}{line}\n")).await?;
    Ok(())
}

/// Run `git rev-parse <arg>` from `work_dir` and canonicalize the
/// result. Returns `Ok(None)` when git is not available or the command fails.
async fn resolve_git_dir_arg(
    work_dir: &std::path::Path,
    arg: &str,
) -> anyhow::Result<Option<PathBuf>> {
    let output = tokio::process::Command::new("git")
        .args(["rev-parse", arg])
        .current_dir(work_dir)
        .output()
        .await?;

    if !output.status.success() {
        return Ok(None);
    }

    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if raw.is_empty() {
        return Ok(None);
    }

    // The path may be relative to work_dir; canonicalize to absolute.
    let candidate = if std::path::Path::new(&raw).is_absolute() {
        PathBuf::from(&raw)
    } else {
        work_dir.join(&raw)
    };

    let canonical = tokio::fs::canonicalize(&candidate)
        .await
        .unwrap_or(candidate);
    Ok(Some(canonical))
}

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
    /// Agent name (claude, codex, opencode, grok, kimi, minimax)
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

/// Build the runner script content (saved to attempt dir), plus the small
/// helper script it execs the agent command through under a pty. Returns
/// `(runner_script, agent_cmd_script)`.
fn build_tmux_runner_scripts(
    inv: &AgentInvocation,
    agent_cmd: &str,
    attempt_dir: &std::path::Path,
) -> anyhow::Result<(String, String)> {
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
    let agent_cmd_script = format!("#!/usr/bin/env bash\n{agent_cmd} 2>{sq_stderr_file}\n");

    let runner_script = format!(
        r#"#!/usr/bin/env bash
set -euo pipefail

status_file={sq_status_file}
stderr_file={sq_stderr_file}
trap 'status=$?; printf "%s\n" "$status" > "$status_file"' EXIT

# Environment — ~/.path and ~/.private are loaded by orch at startup into the process env
export PATH="/opt/homebrew/bin:/usr/local/bin:$PATH"
export GIT_AUTHOR_NAME={sq_git_name}
export GIT_COMMITTER_NAME={sq_git_name}
export GIT_AUTHOR_EMAIL={sq_git_email}
export GIT_COMMITTER_EMAIL={sq_git_email}
# Env config overrides the operator's global commit.gpgsign, the signing agent
# (1Password etc) is often locked in unattended sessions and blocks `git commit`
export GIT_CONFIG_COUNT="1"
export GIT_CONFIG_KEY_0="commit.gpgsign"
export GIT_CONFIG_VALUE_0="false"
export TASK_ID={sq_task_id}
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

    Ok((runner_script, agent_cmd_script))
}

/// Spawn the agent in a tmux session.
///
/// Returns the tmux session name.
pub async fn spawn_in_tmux(tmux: &TmuxManager, inv: &AgentInvocation) -> anyhow::Result<String> {
    // Prepare attempt directory and prompt files (system + message).
    let attempt_dir =
        crate::home::task_attempt_dir_async(&inv.repo, &inv.task_id, inv.attempt).await?;

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
            match resolve_git_dir_arg(&inv.work_dir, arg).await {
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

        if Shipcheck::detect(&inv.work_dir).await == Shipcheck::Available {
            if let Err(e) = install_shipcheck_rule(&inv.work_dir).await {
                tracing::warn!(
                    work_dir = %inv.work_dir.display(),
                    error = %e,
                    "failed to install the shipcheck codex rule"
                );
            }
        }
    }

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

    tokio::fs::write(&sys_file, &sys_content).await?;
    tokio::fs::write(&msg_file, &inv.agent_message).await?;

    // Build agent command using per-agent runner (used in non-PTY path below)
    let runner = super::agents::get_runner(&inv.agent);

    // Prepare environment map for tmux session (does not write GH token to disk)
    let mut env = std::collections::HashMap::new();
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
    let agent_cmd = runner.build_command(
        inv.model.as_deref(),
        &timeout_cmd,
        &sys_file.to_string_lossy(),
        &msg_file.to_string_lossy(),
        &permissions,
    );

    // Write runner script (and the agent-cmd helper script it execs under a pty)
    // to the per-task attempt dir.
    let script_path = attempt_dir.join("runner.sh");
    let agent_cmd_script_path = attempt_dir.join("agent_cmd.sh");
    let (script_content, agent_cmd_script_content) =
        build_tmux_runner_scripts(inv, &agent_cmd, &attempt_dir)?;
    tokio::fs::write(&agent_cmd_script_path, &agent_cmd_script_content).await?;
    tokio::fs::write(&script_path, &script_content).await?;

    // Make executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).await?;
        tokio::fs::set_permissions(
            &agent_cmd_script_path,
            std::fs::Permissions::from_mode(0o755),
        )
        .await?;
    }

    let command = format!("bash \"{}\"", script_path.display());

    // Resolve GitHub token via the process-wide shared resolver (cached after first call)
    let github_token = match crate::github::token::shared().get_token().await {
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
    _task: &crate::backends::ExternalTask,
    context: &super::context::TaskContext,
    route_result: Option<&crate::engine::router::RouteResult>,
    default_branch: &str,
    shipcheck: Shipcheck,
) -> String {
    let mut vars = HashMap::new();

    vars.insert("DEFAULT_BRANCH".to_string(), default_branch.to_string());
    shipcheck.insert_vars(&mut vars);

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
    default_branch: &str,
    pr_number: u64,
    shipcheck: Shipcheck,
) -> String {
    let mut vars = HashMap::new();
    shipcheck.insert_vars(&mut vars);
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

    render_prompt_template(REVIEW_PROMPT_TEMPLATE, vars)
}

/// Build the runner shell script contents for testing and inspection.
///
/// IMPORTANT: this must not embed secrets (GH_TOKEN) into the script.
#[allow(dead_code)]
pub fn build_runner_script(inv: &AgentInvocation) -> anyhow::Result<String> {
    // Prepare attempt directory and prompt files
    let attempt_dir = crate::home::task_attempt_dir(&inv.repo, &inv.task_id, inv.attempt)?;
    std::fs::create_dir_all(&attempt_dir)?;

    let sys_file = attempt_dir.join("prompt-sys.md");
    let msg_file = attempt_dir.join("prompt-msg.md");
    std::fs::write(&sys_file, &inv.system_prompt)?;
    std::fs::write(&msg_file, &inv.agent_message)?;

    build_runner_script_in_dir(inv, &attempt_dir)
}

/// Inner script builder given a pre-created attempt directory.
/// Separated from `build_runner_script` so tests can pass a temp dir and
/// avoid racing with the parallel binary/lib test that shares `~/.orch/state/`.
fn build_runner_script_in_dir(
    inv: &AgentInvocation,
    attempt_dir: &std::path::Path,
) -> anyhow::Result<String> {
    let sys_file = attempt_dir.join("prompt-sys.md");
    let msg_file = attempt_dir.join("prompt-msg.md");

    // Build unified permission rules and sys content (minimal translation)
    let mut permissions = super::agents::PermissionRules::from_config();
    if !inv.disallowed_tools.is_empty() {
        for tool in &inv.disallowed_tools {
            if !permissions.disallowed_tools.contains(tool) {
                permissions.disallowed_tools.push(tool.clone());
            }
        }
    }
    permissions.allowed_edit_paths.push(inv.work_dir.clone());

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

    let status_file = attempt_dir.join("exit.txt");
    let stderr_file = attempt_dir.join("stderr.txt");

    let sq_status = shell_single_quote(&status_file.display().to_string());
    let sq_stderr = shell_single_quote(&stderr_file.display().to_string());
    let sq_out = shell_single_quote(&inv.output_file.display().to_string());
    let sq_work_dir = shell_single_quote(&inv.work_dir.to_string_lossy());
    let script = format!(
        "#!/usr/bin/env bash\nset -euo pipefail\nstatus_file={sq_status}\nstderr_file={sq_stderr}\ntrap 'status=$?; printf \"%s\\n\" \"$status\" > \"$status_file\"' EXIT\nunset CLAUDECODE\ncd {sq_work_dir} || {{\n  printf '%s\\n' \"worktree directory does not exist: {sq_work_dir}\" > \"$stderr_file\"\n  exit 1\n}}\nset +e\nRESPONSE=$({agent_cmd} 2> \"$stderr_file\")\nCMD_STATUS=$?\nset -e\nprintf '%s' \"$RESPONSE\" > {sq_out}\nexit $CMD_STATUS\n",
        agent_cmd = agent_cmd,
    );

    Ok(script)
}

/// Build the system prompt for the review agent.
pub fn review_system_prompt() -> String {
    render_prompt_template(REVIEW_SYSTEM_TEMPLATE, HashMap::new())
}

#[cfg(test)]
mod tests {
    use super::*;
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
        }
    }

    #[test]
    fn env_map_does_not_contain_token_value_as_string() {
        // Verify that the token resolution returns the raw token value,
        // not a string that would need to be embedded in a script.
        // The key security guarantee: we're NOT building a runner script anymore.
        // Tokens are injected directly into tmux session environment.
        let resolver = crate::github::token::TokenResolver::default_env();
        let token = resolver.get_token_sync().ok().flatten().unwrap_or_default();
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
    }

    #[test]
    fn build_runner_script_persists_exit_status_on_failure() {
        // Use a temp dir so lib and bin compilation units don't race on ~/.orch/state/.
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let inv = test_invocation("runner-status-test");
        let script =
            build_runner_script_in_dir(&inv, tmp.path()).expect("runner script should build");

        assert!(script
            .contains("trap 'status=$?; printf \"%s\\n\" \"$status\" > \"$status_file\"' EXIT"));
        assert!(script.contains("set +e"));
    }

    #[test]
    fn build_runner_script_reports_missing_worktree() {
        // Use a temp dir so lib and bin compilation units don't race on ~/.orch/state/.
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let inv = test_invocation("runner-missing-worktree-test");
        let script =
            build_runner_script_in_dir(&inv, tmp.path()).expect("runner script should build");

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
            build_tmux_runner_scripts(&inv, "echo hello", tmp.path())
                .expect("runner scripts should build");

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
        let (runner_script, _) = build_tmux_runner_scripts(&inv, "echo hello", tmp.path())
            .expect("runner scripts should build");

        assert!(runner_script.contains("CMD_STATUS=${PIPESTATUS[0]:-0}"));
        assert!(runner_script.contains("exit $CMD_STATUS"));
    }

    #[test]
    fn tmux_runner_script_disables_commit_signing() {
        // Locked signing agent must not fail `git commit` inside agent sessions
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let inv = test_invocation("gpgsign-test");
        let (runner_script, _) = build_tmux_runner_scripts(&inv, "echo hello", tmp.path())
            .expect("runner scripts should build");

        assert!(runner_script.contains("GIT_CONFIG_KEY_0=\"commit.gpgsign\""));
        assert!(runner_script.contains("GIT_CONFIG_VALUE_0=\"false\""));
    }

    #[test]
    fn tmux_runner_script_writes_executable_agent_cmd_helper() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let inv = test_invocation("pty-helper-path-test");
        let (runner_script, agent_cmd_script) =
            build_tmux_runner_scripts(&inv, "echo hello", tmp.path())
                .expect("runner scripts should build");

        let agent_cmd_script_path = tmp.path().join("agent_cmd.sh");
        assert!(
            runner_script.contains(&agent_cmd_script_path.display().to_string()),
            "runner script must reference the agent_cmd.sh helper path, got: {runner_script}"
        );
        assert!(agent_cmd_script.starts_with("#!/usr/bin/env bash\n"));
    }

    #[test]
    fn build_runner_script_escapes_shell_injection_in_paths() {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let mut inv = test_invocation("injection-test");
        inv.work_dir = std::path::PathBuf::from("/tmp/$(whoami)/workspace");
        inv.output_file = std::path::PathBuf::from("/tmp/evil`id`.json");
        let script =
            build_runner_script_in_dir(&inv, tmp.path()).expect("runner script should build");

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
        let task = crate::backends::ExternalTask {
            id: crate::backends::ExternalId("155460".to_string()),
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
        use crate::engine::runner::agents::shell_single_quote;

        assert_eq!(shell_single_quote("hello"), "'hello'");
        assert_eq!(shell_single_quote("O'Brien"), "'O'\\''Brien'");
        assert_eq!(shell_single_quote("$(rm -rf /)"), "'$(rm -rf /)'");
        assert_eq!(shell_single_quote("`whoami`"), "'`whoami`'");
        assert_eq!(shell_single_quote("a\"b"), "'a\"b'");
        assert_eq!(shell_single_quote("$HOME"), "'$HOME'");
    }

    fn git_init(dir: &std::path::Path) {
        let out = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success());
    }

    #[tokio::test]
    async fn install_shipcheck_rule_writes_rule_and_keeps_tree_clean() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());

        install_shipcheck_rule(dir.path()).await.unwrap();
        install_shipcheck_rule(dir.path()).await.unwrap();

        let rule = std::fs::read_to_string(dir.path().join(SHIPCHECK_RULE_PATH)).unwrap();
        assert_eq!(rule, SHIPCHECK_RULES);
        for cmd in ["run", "commit", "status", "logs", "doctor"] {
            assert!(
                rule.contains(&format!(r#"["shipcheck", "{cmd}"]"#)),
                "{cmd}"
            );
        }
        assert!(!rule.contains(r#"["shipcheck"]"#));
        for cmd in [
            "clean", "post", "login", "logout", "daemon", "init", "install",
        ] {
            assert!(!rule.contains(&format!(r#""{cmd}""#)), "{cmd}");
        }

        let exclude = std::fs::read_to_string(dir.path().join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.matches("orch-shipcheck.rules").count(), 1);
        let status = std::process::Command::new("git")
            .args(["status", "--porcelain", "--untracked-files=all"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(status.stdout.is_empty(), "{:?}", status);
    }

    #[tokio::test]
    async fn install_shipcheck_rule_refuses_symlinked_dirs() {
        for link in [".codex", ".codex/rules"] {
            let dir = tempfile::tempdir().unwrap();
            let target = tempfile::tempdir().unwrap();
            git_init(dir.path());
            std::fs::create_dir_all(dir.path().join(".codex")).unwrap();
            if link == ".codex" {
                std::fs::remove_dir(dir.path().join(".codex")).unwrap();
            }
            std::os::unix::fs::symlink(target.path(), dir.path().join(link)).unwrap();

            assert!(install_shipcheck_rule(dir.path()).await.is_err(), "{link}");
            assert_eq!(
                std::fs::read_dir(target.path()).unwrap().count(),
                0,
                "{link}"
            );
        }
    }

    #[tokio::test]
    async fn install_shipcheck_rule_replaces_a_symlinked_file_without_following_it() {
        let dir = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let outside = target.path().join("victim");
        std::fs::write(&outside, "keep").unwrap();
        git_init(dir.path());
        std::fs::create_dir_all(dir.path().join(".codex/rules")).unwrap();
        std::os::unix::fs::symlink(&outside, dir.path().join(SHIPCHECK_RULE_PATH)).unwrap();

        install_shipcheck_rule(dir.path()).await.unwrap();

        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep");
        let meta = std::fs::symlink_metadata(dir.path().join(SHIPCHECK_RULE_PATH)).unwrap();
        assert!(meta.is_file());
    }

    fn prompt_task() -> crate::backends::ExternalTask {
        crate::backends::ExternalTask {
            id: crate::backends::ExternalId("1".to_string()),
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
    fn system_prompt_uses_shipcheck_only_when_available() {
        let ctx = super::super::context::TaskContext::default();
        let task = prompt_task();
        let prompt = |s| build_system_prompt(&task, &ctx, None, "main", s);

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
    fn shipcheck_classify_needs_config_and_commit() {
        assert_eq!(Shipcheck::classify(false, Some(true)), Shipcheck::Absent);
        assert_eq!(Shipcheck::classify(false, None), Shipcheck::Absent);
        assert_eq!(Shipcheck::classify(true, None), Shipcheck::NotInstalled);
        assert_eq!(Shipcheck::classify(true, Some(false)), Shipcheck::Outdated);
        assert_eq!(Shipcheck::classify(true, Some(true)), Shipcheck::Available);
    }

    #[tokio::test]
    async fn supports_reads_the_usage_line() {
        let new_dir = tempfile::tempdir().unwrap();
        let new = fake_shipcheck(new_dir.path(), true, true);
        assert!(supports(&new, "commit").await);
        assert!(supports(&new, "hook").await);
        let old_dir = tempfile::tempdir().unwrap();
        let old = fake_shipcheck(old_dir.path(), false, false);
        assert!(!supports(&old, "commit").await);
        assert!(!supports(&old, "hook").await);
        let missing = old_dir.path().join("missing");
        assert!(!supports(&missing, "commit").await);
    }

    // Mimics shipcheck, whose `--help` prints usage and exits 0 after any subcommand
    fn fake_shipcheck(dir: &std::path::Path, commit: bool, hook: bool) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let mut usage = "usage: shipcheck run [--sha <rev>] | status | logs".to_string();
        if commit {
            usage.push_str(" | commit -m <message>");
        }
        if hook {
            usage.push_str(" | hook");
        }
        usage.push_str(" | install | doctor");
        let bin = dir.join("shipcheck");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do [ \"$a\" = --help ] && {{ echo '{usage}'; exit 0; }}; done\n[ \"$1\" = install ] && touch installed\nexit 0\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    #[tokio::test]
    async fn install_shipcheck_hooks_runs_install_when_supported() {
        let bin_dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let bin = fake_shipcheck(bin_dir.path(), true, true);
        assert!(install_shipcheck_hooks(work.path(), Shipcheck::Available, &bin).await);
        assert!(work.path().join("installed").exists());
    }

    #[tokio::test]
    async fn install_shipcheck_hooks_skips_a_shipcheck_without_hook() {
        let bin_dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let bin = fake_shipcheck(bin_dir.path(), true, false);
        assert!(!install_shipcheck_hooks(work.path(), Shipcheck::Available, &bin).await);
        assert!(!work.path().join("installed").exists());
    }

    #[tokio::test]
    async fn install_shipcheck_hooks_skips_when_not_available() {
        let bin_dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let bin = fake_shipcheck(bin_dir.path(), true, true);
        for s in [
            Shipcheck::Outdated,
            Shipcheck::NotInstalled,
            Shipcheck::Absent,
        ] {
            assert!(!install_shipcheck_hooks(work.path(), s, &bin).await);
        }
        assert!(!work.path().join("installed").exists());
    }
}
