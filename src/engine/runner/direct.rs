//! One-shot agent invocation without tmux.
//!
//! Used by the control session and the router LLM to run agents
//! in a fire-and-forget mode without a persistent tmux session.

use crate::engine::runner::agents::{
    AgentError, PermissionRules, find_agent_result, get_runner, shell_single_quote,
    supports_session,
};
use anyhow::Result;
use std::error;
use std::ffi::OsString;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::fmt::Write as _;
use std::process::Output;
use std::result;
use std::time::Duration;
use tokio::fs;
use tokio::process::Command;
use tokio::time;

/// Result of a one-shot agent invocation.
#[derive(Debug, Clone)]
pub struct DirectResult {
    pub text: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
}

/// Error type for `run_direct_command_raw()` — carries stdout on non-zero exit
/// so callers can inspect it for rate-limit detection.
#[derive(Debug)]
pub enum DirectCommandError {
    Timeout {
        secs: u64,
    },
    Spawn {
        message: String,
    },
    NonZeroExit {
        exit_code: i32,
        stdout: String,
        stderr: String,
    },
    EmptyResponse {
        stderr: String,
    },
}

impl Display for DirectCommandError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout { secs } => write!(f, "agent timed out after {secs}s"),
            Self::Spawn { message } => write!(f, "spawning agent: {message}"),
            Self::NonZeroExit {
                exit_code, stderr, ..
            } => {
                write!(f, "agent failed (exit {exit_code}): {stderr}")
            }
            Self::EmptyResponse { .. } => write!(f, "agent returned empty response"),
        }
    }
}

impl error::Error for DirectCommandError {}

/// Run an agent one-shot without tmux.
///
/// Writes temp files to `work_dir`, builds the CLI command via
/// `get_runner(agent).build_command()`, executes with `timeout`,
/// then parses with `parse_response()`.
///
/// `work_dir` should be an isolated directory (e.g., `~/.orch/state/control/{pid}/`).
pub async fn run_direct(
    agent: &str,
    model: Option<&str>,
    system_prompt: &str,
    message: &str,
    timeout: Duration,
    work_dir: &str,
) -> Result<DirectResult> {
    run_direct_with_session(
        agent,
        model,
        system_prompt,
        message,
        timeout,
        work_dir,
        None,
    )
    .await
}

/// Configuration for continuing an agent session.
#[derive(Debug, Clone, Copy)]
pub struct SessionContinuation<'a> {
    pub session_id: &'a str,
    pub resume: bool,
}

/// Run an agent one-shot with an optional `--session-id` or `--resume` for conversation continuity.
///
/// For agents that support sessions (claude, kimi, minimax, grok), appends
/// `--session-id <uuid>` when starting a new session, or `--resume <uuid>` when
/// continuing one. Other agents ignore the parameter.
pub async fn run_direct_with_session(
    agent: &str,
    model: Option<&str>,
    system_prompt: &str,
    message: &str,
    timeout: Duration,
    work_dir: &str,
    session: Option<SessionContinuation<'_>>,
) -> Result<DirectResult> {
    let place = DirectPlace {
        files_dir: work_dir,
        cwd: work_dir,
        permissions: PermissionRules::deny_read_only(),
        sandbox: Vec::new(),
    };
    run_direct_at(
        agent,
        model,
        system_prompt,
        message,
        timeout,
        &place,
        session,
    )
    .await
}

/// Where a direct call runs, what its agent may do, and the sandbox around it
#[derive(Debug, Clone)]
pub struct DirectPlace<'a> {
    /// Where the prompt files go, kept out of `cwd` so a checkout gets nothing written to it
    pub files_dir: &'a str,
    /// The folder the agent works in
    pub cwd: &'a str,
    pub permissions: PermissionRules,
    /// A sandbox-jail prefix for the command, empty to run without one
    pub sandbox: Vec<OsString>,
}

/// Like [`run_direct_with_session`], in the place `place` describes
pub async fn run_direct_at(
    agent: &str,
    model: Option<&str>,
    system_prompt: &str,
    message: &str,
    timeout: Duration,
    place: &DirectPlace<'_>,
    session: Option<SessionContinuation<'_>>,
) -> Result<DirectResult> {
    let work_dir = place.files_dir;
    let sys_file = format!("{work_dir}/system.md");
    let msg_file = format!("{work_dir}/message.txt");

    fs::write(&sys_file, system_prompt).await?;
    fs::write(&msg_file, message).await?;

    let runner = get_runner(agent);
    let timeout_cmd = format!("timeout {}", timeout.as_secs());

    let mut shell_cmd = runner.build_command(
        model,
        &timeout_cmd,
        &sys_file,
        &msg_file,
        &place.permissions,
    );

    if let Some(s) = session {
        shell_cmd = insert_session_flag(agent, shell_cmd, s);
    }

    run_shell_command(
        &shell_cmd,
        place.cwd,
        timeout,
        agent,
        &place.sandbox,
        work_dir,
    )
    .await
}

/// Append `--session-id` or `--resume` for agents that keep a conversation.
///
/// Claude-family commands redirect the prompt with `< "file"`, so the flag is
/// inserted before that redirect. Grok takes `--prompt-file` and has no
/// redirect, so the flag is appended.
fn insert_session_flag(
    agent: &str,
    mut shell_cmd: String,
    session: SessionContinuation<'_>,
) -> String {
    if !supports_session(agent) {
        return shell_cmd;
    }
    let flag = if session.resume {
        "--resume"
    } else {
        "--session-id"
    };
    let quoted = shell_single_quote(session.session_id);
    if let Some(pos) = shell_cmd.rfind("< \"") {
        shell_cmd.insert_str(pos, &format!("{flag} {quoted} \\\n  "));
    } else if agent == "grok" {
        let _ = write!(shell_cmd, " \\\n  {flag} {quoted}");
    } else {
        tracing::warn!(
            agent,
            flag,
            session_id = session.session_id,
            "could not insert session flag: '< \"' pattern not found in shell command"
        );
    }
    shell_cmd
}

/// Execute an already-built `tokio::process::Command` one-shot, returning raw stdout.
///
/// Unlike `run_direct_command()`, this does NOT call `parse_response()`.
/// Use this when the caller needs the raw stdout to parse itself (e.g., router).
/// Returns `Err(DirectCommandError::NonZeroExit { stdout, .. })` so callers can
/// inspect stdout for rate-limit messages even on failure.
pub async fn run_direct_command_raw(
    cmd: &mut Command,
    timeout: Duration,
) -> result::Result<String, DirectCommandError> {
    cmd.kill_on_drop(true);
    let output = time::timeout(timeout + Duration::from_secs(5), cmd.output())
        .await
        .map_err(|_| DirectCommandError::Timeout {
            secs: timeout.as_secs(),
        })?
        .map_err(|e| DirectCommandError::Spawn {
            message: e.to_string(),
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code().unwrap_or(-1);

    if !output.status.success() {
        return Err(DirectCommandError::NonZeroExit {
            exit_code,
            stdout,
            stderr,
        });
    }
    if stdout.is_empty() {
        return Err(DirectCommandError::EmptyResponse { stderr });
    }
    Ok(stdout)
}

/// Shared output-handling logic: parse stdout, classify errors.
fn finish_output(output: &Output, agent: &str) -> Result<DirectResult> {
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code().unwrap_or(-1);

    let runner = get_runner(agent);

    if !output.status.success() {
        let err = runner.classify_error(exit_code, &stdout, &stderr);
        anyhow::bail!("{err}");
    }

    match runner.parse_response(&stdout) {
        Ok(parsed) => {
            let text = reply_text(&stdout, &parsed.response.summary);
            Ok(DirectResult {
                text,
                input_tokens: parsed.input_tokens,
                output_tokens: parsed.output_tokens,
                cost_usd: parsed.cost_usd,
            })
        }
        Err(AgentError::InvalidResponse { .. }) => {
            let (text, input_tokens, output_tokens) = extract_from_envelope(&stdout);
            Ok(DirectResult {
                text,
                input_tokens,
                output_tokens,
                cost_usd: find_agent_result(agent, &stdout).and_then(|r| r.cost_usd),
            })
        }
        Err(err) => anyhow::bail!("{err}"),
    }
}

async fn run_shell_command(
    shell_cmd: &str,
    work_dir: &str,
    timeout: Duration,
    agent: &str,
    sandbox: &[OsString],
    temp: &str,
) -> Result<DirectResult> {
    let mut cmd = match sandbox.split_first() {
        Some((program, args)) => {
            // The agent's temp files go to a dir the sandbox lets it write, and no SSH agent is offered
            let mut c = Command::new(program);
            c.args(args)
                .arg("/bin/bash")
                .env_remove("SSH_AUTH_SOCK")
                .env("TMPDIR", temp);
            c
        }
        None => Command::new("bash"),
    };
    cmd.arg("-c")
        .arg(shell_cmd)
        .current_dir(work_dir)
        .kill_on_drop(true);
    let output = time::timeout(timeout + Duration::from_secs(5), cmd.output())
        .await
        .map_err(|_| anyhow::anyhow!("agent timed out after {}s", timeout.as_secs()))?
        .map_err(|e| anyhow::anyhow!("spawning agent: {e}"))?;

    finish_output(&output, agent)
}

/**
 * The text a direct call answers with. A summary synthesized from free text holds only its start,
 * so a reply in a result envelope comes back whole, and the summary stays for an agent that
 * answered with the JSON task response
 **/
fn reply_text(stdout: &str, summary: &str) -> String {
    let (text, _, _) = extract_from_envelope(stdout);
    let in_envelope = text != stdout;
    let head = text.trim_start();
    let task_json = head.starts_with('{') || head.starts_with("```json");
    if in_envelope && !text.trim().is_empty() && !task_json {
        return text;
    }

    if summary.is_empty() {
        stdout.to_string()
    } else {
        summary.to_string()
    }
}

/// Extract text and token usage from a Claude JSON envelope or raw output.
///
/// Handles both single JSON blobs (`--output-format json`) and NDJSON streams
/// (`--output-format stream-json`): finds the last line with `"type":"result"`.
pub(crate) fn extract_from_envelope(raw: &str) -> (String, Option<u64>, Option<u64>) {
    // Find the result line: last NDJSON line with "type":"result", or the full string
    let candidate = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .rev()
        .find(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|v| {
                    v.get("type")
                        .and_then(|t| t.as_str())
                        .map(|t| t == "result")
                })
                .unwrap_or(false)
        })
        .unwrap_or(raw);

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(candidate)
        && let Some(obj) = value.as_object()
        && obj.get("type").and_then(|v| v.as_str()) == Some("result")
    {
        let text = obj
            .get("result")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let input_tokens = obj
            .get("usage")
            .and_then(|u| u.get("input_tokens"))
            .and_then(serde_json::Value::as_u64);
        let output_tokens = obj
            .get("usage")
            .and_then(|u| u.get("output_tokens"))
            .and_then(serde_json::Value::as_u64);
        return (text, input_tokens, output_tokens);
    }
    (raw.to_string(), None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_flag_appends_for_grok_prompt_file() {
        let cmd = "grok --prompt-file '/tmp/msg.txt'".to_string();
        let updated = insert_session_flag(
            "grok",
            cmd,
            SessionContinuation {
                session_id: "abc",
                resume: false,
            },
        );
        assert!(updated.contains("--session-id 'abc'"), "{updated}");
        assert!(updated.contains("--prompt-file"), "{updated}");
    }

    #[test]
    fn session_flag_inserts_before_stdin_redirect() {
        let cmd = "claude -p < \"/tmp/msg.txt\"".to_string();
        let updated = insert_session_flag(
            "claude",
            cmd,
            SessionContinuation {
                session_id: "abc",
                resume: true,
            },
        );
        assert!(updated.contains("--resume 'abc'"), "{updated}");
        let resume = updated.find("--resume").unwrap();
        let redirect = updated.find("< \"").unwrap();
        assert!(resume < redirect, "{updated}");
    }

    #[test]
    fn session_flag_ignored_for_codex() {
        let cmd = "codex exec".to_string();
        let updated = insert_session_flag(
            "codex",
            cmd.clone(),
            SessionContinuation {
                session_id: "abc",
                resume: false,
            },
        );
        assert_eq!(updated, cmd);
    }

    #[test]
    fn a_long_reply_comes_back_whole() {
        let long = format!(
            "Plan:\n{}",
            "- a step that says usage: hello <name>\n".repeat(40)
        );
        let raw = serde_json::json!({"type": "result", "result": long}).to_string();
        assert_eq!(reply_text(&raw, "Plan:"), long, "not cut to the summary");
    }

    #[test]
    fn a_json_task_reply_keeps_its_summary() {
        let raw = serde_json::json!({"type": "result", "result": "{\"status\":\"done\",\"summary\":\"ok\"}"}).to_string();
        assert_eq!(reply_text(&raw, "ok"), "ok", "the task summary");
    }

    #[test]
    fn test_extract_from_envelope_claude_json() {
        let raw = r#"{"type":"result","result":"Hello world","usage":{"input_tokens":10,"output_tokens":5}}"#;
        let (text, input, output) = extract_from_envelope(raw);
        assert_eq!(text, "Hello world");
        assert_eq!(input, Some(10));
        assert_eq!(output, Some(5));
    }

    #[test]
    fn test_extract_from_envelope_ndjson_stream() {
        let raw = concat!(
            r#"{"type":"system","subtype":"init"}"#,
            "\n",
            r#"{"type":"assistant","message":{}}"#,
            "\n",
            r#"{"type":"result","result":"streamed result","usage":{"input_tokens":20,"output_tokens":8}}"#,
            "\n"
        );
        let (text, input, output) = extract_from_envelope(raw);
        assert_eq!(text, "streamed result");
        assert_eq!(input, Some(20));
        assert_eq!(output, Some(8));
    }

    #[test]
    fn test_extract_from_envelope_not_envelope() {
        let raw = "plain text response";
        let (text, input, output) = extract_from_envelope(raw);
        assert_eq!(text, "plain text response");
        assert_eq!(input, None);
        assert_eq!(output, None);
    }

    #[test]
    fn test_extract_from_envelope_wrong_type() {
        let raw = r#"{"type":"error","message":"something"}"#;
        let (text, input, output) = extract_from_envelope(raw);
        assert_eq!(text, raw);
        assert_eq!(input, None);
        assert_eq!(output, None);
    }

    #[test]
    fn test_direct_result_debug() {
        let r = DirectResult {
            text: "hi".to_string(),
            input_tokens: Some(1),
            output_tokens: Some(2),
            cost_usd: None,
        };
        assert!(format!("{r:?}").contains("hi"));
    }

    #[test]
    fn test_direct_command_error_display() {
        assert_eq!(
            DirectCommandError::Timeout { secs: 30 }.to_string(),
            "agent timed out after 30s"
        );
        assert_eq!(
            DirectCommandError::Spawn {
                message: "no binary".to_string()
            }
            .to_string(),
            "spawning agent: no binary"
        );
        assert_eq!(
            DirectCommandError::NonZeroExit {
                exit_code: 1,
                stdout: "out".to_string(),
                stderr: "err".to_string()
            }
            .to_string(),
            "agent failed (exit 1): err"
        );
        assert_eq!(
            DirectCommandError::EmptyResponse {
                stderr: String::new()
            }
            .to_string(),
            "agent returned empty response"
        );
    }
}
