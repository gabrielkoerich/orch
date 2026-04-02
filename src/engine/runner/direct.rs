//! One-shot agent invocation without tmux.
//!
//! Used by the control session and the router LLM to run agents
//! in a fire-and-forget mode without a persistent tmux session.

use crate::engine::runner::agents::{get_runner, AgentError, PermissionRules};
use anyhow::Result;
use std::time::Duration;

/// Result of a one-shot agent invocation.
#[derive(Debug, Clone)]
pub struct DirectResult {
    pub text: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
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

impl std::fmt::Display for DirectCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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

impl std::error::Error for DirectCommandError {}

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
/// For claude-compatible agents (claude, kimi, minimax), appends `--session-id <uuid>`
/// when starting a new session, or `--resume <uuid>` when continuing an existing session.
/// Other agents ignore the parameter.
pub async fn run_direct_with_session(
    agent: &str,
    model: Option<&str>,
    system_prompt: &str,
    message: &str,
    timeout: Duration,
    work_dir: &str,
    session: Option<SessionContinuation<'_>>,
) -> Result<DirectResult> {
    let sys_file = format!("{work_dir}/system.md");
    let msg_file = format!("{work_dir}/message.txt");

    tokio::fs::write(&sys_file, system_prompt).await?;
    tokio::fs::write(&msg_file, message).await?;

    let runner = get_runner(agent);
    let permissions = PermissionRules::default();
    let timeout_cmd = format!("timeout {}", timeout.as_secs());

    let mut shell_cmd =
        runner.build_command(model, &timeout_cmd, &sys_file, &msg_file, &permissions);

    // Append --session-id (new) or --resume (existing) for claude-compatible agents
    if let Some(s) = session {
        if matches!(agent, "claude" | "kimi" | "minimax") {
            let flag = if s.resume { "--resume" } else { "--session-id" };
            // Insert flag before the stdin redirect (< "msg_file")
            if let Some(pos) = shell_cmd.rfind("< \"") {
                shell_cmd.insert_str(pos, &format!("{flag} {} \\\n  ", s.session_id));
            }
        }
    }

    run_shell_command(&shell_cmd, work_dir, timeout, agent).await
}

/// Execute an already-built `tokio::process::Command` one-shot, returning raw stdout.
///
/// Unlike `run_direct_command()`, this does NOT call `parse_response()`.
/// Use this when the caller needs the raw stdout to parse itself (e.g., router).
/// Returns `Err(DirectCommandError::NonZeroExit { stdout, .. })` so callers can
/// inspect stdout for rate-limit messages even on failure.
pub async fn run_direct_command_raw(
    cmd: &mut tokio::process::Command,
    timeout: Duration,
) -> std::result::Result<String, DirectCommandError> {
    let output = tokio::time::timeout(timeout + Duration::from_secs(5), cmd.output())
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
fn finish_output(output: std::process::Output, agent: &str) -> Result<DirectResult> {
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
            let text = if !parsed.response.summary.is_empty() {
                parsed.response.summary.clone()
            } else {
                stdout.clone()
            };
            Ok(DirectResult {
                text,
                input_tokens: parsed.input_tokens,
                output_tokens: parsed.output_tokens,
            })
        }
        Err(AgentError::InvalidResponse { .. }) => {
            let (text, input_tokens, output_tokens) = extract_from_envelope(&stdout);
            Ok(DirectResult {
                text,
                input_tokens,
                output_tokens,
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
) -> Result<DirectResult> {
    let output = tokio::time::timeout(
        timeout + Duration::from_secs(5),
        tokio::process::Command::new("bash")
            .arg("-c")
            .arg(shell_cmd)
            .current_dir(work_dir)
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("agent timed out after {}s", timeout.as_secs()))?
    .map_err(|e| anyhow::anyhow!("spawning agent: {e}"))?;

    finish_output(output, agent)
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

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(candidate) {
        if let Some(obj) = value.as_object() {
            if obj.get("type").and_then(|v| v.as_str()) == Some("result") {
                let text = obj
                    .get("result")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let input_tokens = obj
                    .get("usage")
                    .and_then(|u| u.get("input_tokens"))
                    .and_then(|v| v.as_u64());
                let output_tokens = obj
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(|v| v.as_u64());
                return (text, input_tokens, output_tokens);
            }
        }
    }
    (raw.to_string(), None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

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
                stderr: "".to_string()
            }
            .to_string(),
            "agent returned empty response"
        );
    }
}
