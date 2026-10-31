//! Codex agent runner.
//!
//! ## CLI invocation
//!
//! ```bash
//! codex --model {model} \
//!   exec \
//!   -c 'sandbox_workspace_write.network_access=true' \
//!   -c 'shell_environment_policy.inherit=all' \
//!   --sandbox workspace-write \
//!   -c 'approval_policy="never"' \
//!   --json - < <(cat "{sys_file}" "{msg_file}")
//! ```
//!
//! Note: all `-c` overrides must appear **after** the `exec` subcommand.
//! Global `-c` flags before `exec` are parsed by the top-level codex CLI and do
//! NOT propagate into the exec subcommand's sandbox — placing
//! `sandbox_workspace_write.network_access=true` before `exec` silently fails,
//! leaving the sandbox network-blocked.
//!
//! ## Output format (`exec --json`)
//!
//! NDJSON stream (one JSON object per line):
//! ```jsonl
//! {"type":"thread.started","thread_id":"..."}
//! {"type":"turn.started"}
//! {"type":"item.completed","item":{"type":"agent_message","text":"..."}}
//! {"type":"item.completed","item":{"type":"command_execution","command":"..."}}
//! {"type":"turn.completed"}
//! ```
//!
//! ## Error events
//!
//! Rate limit:
//! ```jsonl
//! {"type":"error","message":"You've hit your usage limit..."}
//! {"type":"turn.failed","error":{"message":"..."}}
//! ```
//!
//! Model not found:
//! ```jsonl
//! {"type":"item.completed","item":{"type":"error","message":"Model metadata for `o3-mini` not found..."}}
//! {"type":"error","message":"{\"detail\":\"The 'o3-mini' model is not supported...\"}"}
//! {"type":"turn.failed","error":{"message":"..."}}
//! ```

use super::{
    AgentError, AgentRunner, ParsedResponse, PermissionRules, SandboxLevel, StreamEvent,
    json_error_message, json_str,
};
use crate::parser;

/// Runner for Codex agent.
#[derive(Debug)]
pub struct CodexRunner;

impl CodexRunner {
    /// Extract the agent's response text from NDJSON events.
    ///
    /// Looks for `item.completed` events with `item.type == "agent_message"`.
    /// Uses the **last** agent message that contains valid-looking JSON,
    /// since earlier messages are often progress updates.
    fn extract_agent_text(events: &[serde_json::Value]) -> Option<String> {
        let mut texts = Vec::new();

        for event in events {
            let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");

            if event_type == "item.completed"
                && let Some(item) = event.get("item")
            {
                let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");

                if item_type == "agent_message"
                    && let Some(text) = item.get("text").and_then(|v| v.as_str())
                {
                    texts.push(text.to_string());
                }
            }
        }

        if texts.is_empty() {
            return None;
        }

        // Prefer the last message that looks like JSON (contains `{` and `status`)
        for text in texts.iter().rev() {
            let trimmed = text.trim();
            if trimmed.contains('{') && trimmed.contains("status") {
                return Some(text.clone());
            }
        }

        // Fall back to the last message
        texts.pop()
    }

    /// Check for error events in the NDJSON stream.
    ///
    /// Scans ALL error events and returns the most specific one.
    /// Priority: RateLimit > ModelUnavailable > Auth > ContextOverflow > AgentFailed
    fn detect_error(events: &[serde_json::Value]) -> Option<AgentError> {
        super::patterns::detect_ndjson_error(
            events,
            |event| {
                let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match event_type {
                    "turn.failed" => Some(
                        event
                            .get("error")
                            .and_then(|e| e.get("message"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("turn failed")
                            .to_string(),
                    ),
                    "error" => Some(
                        event
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown error")
                            .to_string(),
                    ),
                    "item.completed" => {
                        let item = event.get("item")?;
                        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        if item_type == "error" {
                            Some(
                                item.get("message")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("item error")
                                    .to_string(),
                            )
                        } else {
                            None
                        }
                    }
                    _ => None,
                }
            },
            Self::classify_message,
        )
    }

    /// Classify an error message into an AgentError variant.
    fn classify_message(message: &str) -> AgentError {
        let lower = message.to_lowercase();

        // Rate limit — delegate to shared helper (covers "you've hit your", 429, 529, etc.)
        if let Some(e) = super::patterns::detect_rate_limit(message) {
            return e;
        }

        // Model not supported / not found (codex-specific patterns)
        if lower.contains("model metadata")
            || lower.contains("model is not supported")
            || (lower.contains("not found")
                && (lower.contains("model") || lower.contains("metadata")))
        {
            let model = extract_model_name(message).unwrap_or_default();
            return AgentError::ModelUnavailable {
                message: message.to_string(),
                model,
            };
        }

        // Auth errors — delegate to shared helper (includes HTTP 401/403, billing, etc.)
        if let Some(e) = super::patterns::detect_auth_error(message) {
            return e;
        }

        // Context overflow — delegate to shared helper
        if let Some(e) = super::patterns::detect_context_overflow(message) {
            return e;
        }

        // Connection/network errors — codex-specific transient patterns.
        // Return NetworkError (not AgentFailed) so fallback.rs can route these
        // into the dedicated network-retry loop with backoff and escalation.
        if lower.contains("reconnecting")
            || lower.contains("stream disconnected")
            || lower.contains("event stream lagged")
            || lower.contains("connection closed")
            || lower.contains("websocket")
            || lower.contains("econnreset")
        {
            return AgentError::NetworkError {
                message: format!("codex network error: {message}"),
            };
        }
        if let Some(e) = super::patterns::detect_network_error(message) {
            return e;
        }

        AgentError::AgentFailed {
            message: message.to_string(),
        }
    }
}

impl AgentRunner for CodexRunner {
    #[cfg(test)]
    fn name(&self) -> &'static str {
        "codex"
    }

    fn stream_events(&self, event: &serde_json::Value) -> Vec<StreamEvent> {
        stream_events(event)
    }

    fn extract_text(&self, raw: &str) -> Result<String, AgentError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(String::new());
        }

        let events = super::parse_ndjson(trimmed);
        if events.is_empty() {
            return Ok(trimmed.to_string());
        }

        // Propagate terminal errors (rate limit, auth) so the review pipeline
        // can record cooldowns and abort cleanly.
        if let Some(err) = Self::detect_error(&events) {
            return Err(err);
        }

        // Return agent_message text, or fall back to the raw output.
        Ok(Self::extract_agent_text(&events).unwrap_or_else(|| trimmed.to_string()))
    }

    fn build_command(
        &self,
        model: Option<&str>,
        timeout_cmd: &str,
        sys_file: &str,
        msg_file: &str,
        permissions: &PermissionRules,
    ) -> String {
        let model_flag = model
            .map(|m| format!("--model {}", super::shell_single_quote(m)))
            .unwrap_or_default();

        // Codex permission mode (codex-cli 0.133.0+):
        // - autonomous → --sandbox workspace-write -c 'approval_policy="never"'
        // - supervised → -c 'approval_policy="on-request"' --sandbox {sandbox}
        // - full access → --dangerously-bypass-approvals-and-sandbox
        //
        // Note: --ask-for-approval was removed in codex-cli 0.133.0.
        // Use -c 'approval_policy="..."' instead.
        // Note: --full-auto is deprecated as of codex 0.128.0 and does not
        // reliably honour --add-dir entries under the new seatbelt policy.
        let permission_flags = if permissions.autonomous {
            match permissions.sandbox {
                SandboxLevel::FullAccess | SandboxLevel::Os => {
                    "--dangerously-bypass-approvals-and-sandbox".to_string()
                }
                _ => r#"--sandbox workspace-write -c 'approval_policy="never"'"#.to_string(),
            }
        } else {
            let sandbox = match permissions.sandbox {
                SandboxLevel::WorkspaceWrite | SandboxLevel::None => "workspace-write",
                SandboxLevel::FullAccess | SandboxLevel::Os => "danger-full-access",
            };
            format!(r#"-c 'approval_policy="on-request"' --sandbox {sandbox}"#)
        };

        // Extra writable dirs (e.g. the git common dir when running inside a
        // worktree whose .git metadata lives outside the sandbox root).
        let add_dir_flags: String = permissions
            .extra_writable_dirs
            .iter()
            .map(|p| {
                format!(
                    "\\\n  --add-dir {}",
                    super::shell_single_quote(&p.to_string_lossy())
                )
            })
            .collect::<Vec<_>>()
            .join(" ");

        // All -c config overrides and permission flags must be placed AFTER the
        // `exec` subcommand. Global -c flags before `exec` are parsed by the
        // top-level CLI and do NOT propagate into the exec subcommand's sandbox
        // initialisation — in particular, `sandbox_workspace_write.network_access=true`
        // has no effect when placed before `exec`, causing outbound network to be
        // blocked despite the documented contract. Place everything after `exec`.
        // Feed both files via process substitution rather than a leading `cat |`
        // pipe stage — a leading pipe would make `cat`, not codex, the first
        // stage in the runner script's pipeline, so PIPESTATUS[0] there would
        // always report cat's (always-zero) exit code instead of codex's.
        format!(
            r#"{timeout_cmd} codex {model_flag} \
  exec \
  -c 'sandbox_workspace_write.network_access=true' \
  -c 'shell_environment_policy.inherit=all' \
  {permission_flags} {add_dir_flags} --json - < <(cat "{sys_file}" "{msg_file}")"#,
        )
    }

    fn parse_response(&self, raw: &str) -> Result<ParsedResponse, AgentError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(AgentError::InvalidResponse { raw: String::new() });
        }

        let events = super::parse_ndjson(trimmed);

        if events.is_empty() {
            // Maybe it's direct JSON, not NDJSON
            if let Ok(resp) = parser::parse(trimmed) {
                return Ok(ParsedResponse {
                    response: resp,
                    input_tokens: None,
                    output_tokens: None,
                    duration_ms: None,
                });
            }
            return Err(AgentError::InvalidResponse {
                raw: trimmed.to_string(),
            });
        }

        // Check for errors first
        if let Some(err) = Self::detect_error(&events) {
            return Err(err);
        }

        // Extract agent response text
        let agent_text =
            Self::extract_agent_text(&events).ok_or_else(|| AgentError::InvalidResponse {
                raw: trimmed.to_string(),
            })?;

        // Parse the agent text through our standard parser
        let response = match parser::parse(&agent_text) {
            Ok(r) => r,
            Err(_) => {
                // Structured parse failed — try plain-text synthesis before
                // giving up, same rescue path claude.rs uses (issue #3467).
                super::synthesize_response_from_text(&agent_text).ok_or_else(|| {
                    AgentError::InvalidResponse {
                        raw: agent_text.clone(),
                    }
                })?
            }
        };

        Ok(ParsedResponse {
            response,
            input_tokens: None,
            output_tokens: None,
            duration_ms: None,
        })
    }

    fn classify_error(&self, exit_code: i32, stdout: &str, stderr: &str) -> AgentError {
        // Try parsing NDJSON events from stdout for structured errors
        let events = super::parse_ndjson(stdout);
        if let Some(err) = Self::detect_error(&events) {
            return err;
        }

        // Fall back to pattern matching
        let combined = format!("{stdout}\n{stderr}");
        super::patterns::classify_from_text(exit_code, &combined)
    }

    fn router_command(
        &self,
        prompt: &str,
        model: Option<&str>,
    ) -> anyhow::Result<tokio::process::Command> {
        let mut cmd = tokio::process::Command::new("codex");
        cmd.arg("exec").arg("--json");
        if let Some(m) = model {
            cmd.arg("--model").arg(m);
        }
        cmd.arg(prompt);
        Ok(cmd)
    }
}

/// Try to extract a model name from an error message.
///
/// Looks for text quoted with backticks or single quotes:
/// - "Model metadata for `o3-mini` not found"
/// - "The 'o3-mini' model is not supported"
fn extract_model_name(message: &str) -> Option<String> {
    extract_quoted(message, '`').or_else(|| extract_quoted(message, '\''))
}

/// Extract text between matching `quote` characters.
fn extract_quoted(text: &str, quote: char) -> Option<String> {
    let start = text.find(quote)?;
    let rest = &text[start + quote.len_utf8()..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_string())
}

/// Extract a structured `AgentResult` from Codex NDJSON output.
///
/// Finds `item.completed` events with `type:agent_message` for the result text.
/// Checks for `turn.failed`, `error`, and `item.completed` error events.
///
/// Returns `None` only if no parseable NDJSON events are found.
pub fn find_codex_result(ndjson: &str) -> Option<super::AgentResult> {
    let events = super::parse_ndjson(ndjson.trim());
    if events.is_empty() {
        return None;
    }

    // Check for errors first
    let is_error;
    let error_text;
    if let Some(err) = CodexRunner::detect_error(&events) {
        is_error = true;
        error_text = Some(err.to_string());
    } else {
        is_error = false;
        error_text = None;
    }

    let result_text = if is_error {
        error_text.unwrap_or_default()
    } else {
        CodexRunner::extract_agent_text(&events).unwrap_or_default()
    };

    if result_text.is_empty() && !is_error {
        return None;
    }

    Some(super::AgentResult {
        is_error,
        result_text,
        input_tokens: None,
        output_tokens: None,
        cost_usd: None,
        duration_ms: None,
        truncated_by_length: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runner() -> CodexRunner {
        CodexRunner
    }

    #[test]
    fn parse_codex_ndjson_success() {
        let raw = r#"{"type":"thread.started","thread_id":"t1"}
{"type":"turn.started"}
{"type":"item.completed","item":{"type":"agent_message","text":"{\"status\":\"done\",\"summary\":\"hello\",\"accomplished\":[],\"remaining\":[],\"files\":[]}"}}
{"type":"turn.completed"}"#;

        let parsed = runner().parse_response(raw).unwrap();
        assert_eq!(parsed.response.status, "done");
        assert_eq!(parsed.response.summary, "hello");
    }

    #[test]
    fn parse_codex_ndjson_rate_limit() {
        let raw = r#"{"type":"error","message":"You've hit your usage limit for this billing period"}
{"type":"turn.failed","error":{"message":"You've hit your usage limit"}}"#;

        let err = runner().parse_response(raw).unwrap_err();
        assert!(matches!(err, AgentError::RateLimit { .. }));
    }

    #[test]
    fn parse_codex_model_not_found() {
        let raw = r#"{"type":"item.completed","item":{"type":"error","message":"Model metadata for `o3-mini` not found"}}
{"type":"error","message":"{\"detail\":\"The 'o3-mini' model is not supported\"}"}
{"type":"turn.failed","error":{"message":"Model not supported"}}"#;

        let err = runner().parse_response(raw).unwrap_err();
        assert!(matches!(err, AgentError::ModelUnavailable { .. }));
    }

    /// Regression test for issue #3467: when codex's agent_message text is a
    /// plain-prose completion summary (not JSON), parser::parse fails but
    /// synthesize_response_from_text must rescue it instead of returning
    /// InvalidResponse, mirroring the fallback claude.rs already has.
    #[test]
    fn parse_codex_plain_prose_completion_synthesizes_response() {
        let raw = r#"{"type":"thread.started","thread_id":"t1"}
{"type":"turn.started"}
{"type":"item.completed","item":{"type":"agent_message","text":"Fixed the typo and corrected the deadline reference in the review."}}
{"type":"turn.completed"}"#;

        let parsed = runner()
            .parse_response(raw)
            .expect("plain-prose completion text must synthesize successfully");
        assert_eq!(parsed.response.status, "done");
    }

    #[test]
    fn parse_codex_empty_response() {
        let err = runner().parse_response("").unwrap_err();
        assert!(matches!(err, AgentError::InvalidResponse { .. }));
    }

    #[test]
    fn extract_model_name_backtick() {
        assert_eq!(
            extract_model_name("Model metadata for `o3-mini` not found"),
            Some("o3-mini".to_string())
        );
    }

    #[test]
    fn extract_model_name_single_quote() {
        assert_eq!(
            extract_model_name("The 'o3-mini' model is not supported"),
            Some("o3-mini".to_string())
        );
    }

    #[test]
    fn parse_codex_multiple_agent_messages() {
        let raw = r#"{"type":"thread.started","thread_id":"t1"}
{"type":"turn.started"}
{"type":"item.completed","item":{"type":"agent_message","text":"Working on it..."}}
{"type":"item.completed","item":{"type":"command_execution","command":"echo hello"}}
{"type":"item.completed","item":{"type":"agent_message","text":"{\"status\":\"done\",\"summary\":\"finished\",\"accomplished\":[\"did it\"],\"remaining\":[],\"files\":[\"a.rs\"]}"}}
{"type":"turn.completed"}"#;

        let parsed = runner().parse_response(raw).unwrap();
        // The parser concatenates all agent messages; the last valid JSON wins
        // Actually, parser::parse will try the concatenated text and find the JSON
        assert_eq!(parsed.response.status, "done");
    }

    #[test]
    fn classify_error_codex_ndjson() {
        let stdout = r#"{"type":"error","message":"You've hit your usage limit"}"#;
        let err = runner().classify_error(1, stdout, "");
        assert!(matches!(err, AgentError::RateLimit { .. }));
    }

    #[test]
    fn build_command_codex() {
        let perms = PermissionRules::default();
        let cmd = runner().build_command(
            Some("gpt-4o"),
            "timeout 1800",
            "/tmp/sys.txt",
            "/tmp/msg.txt",
            &perms,
        );
        assert!(cmd.contains("codex"));
        assert!(cmd.contains("--model 'gpt-4o'"));
        assert!(cmd.contains("exec"));
        assert!(cmd.contains("--json -"));
        assert!(
            cmd.contains("--sandbox workspace-write"),
            "default autonomous codex should use --sandbox workspace-write, got: {cmd}"
        );
        assert!(
            cmd.contains(r#"-c 'approval_policy="never"'"#),
            "default autonomous codex should use -c approval_policy=never, got: {cmd}"
        );
        assert!(
            !cmd.contains("--ask-for-approval"),
            "--ask-for-approval was removed in codex 0.133.0 and must not appear, got: {cmd}"
        );
        assert!(
            !cmd.contains("--full-auto"),
            "--full-auto is deprecated and must not appear in the command, got: {cmd}"
        );
        // Regression: network_access and shell_environment_policy overrides must appear
        // AFTER `exec`, not before it.  Global -c flags placed before `exec` are parsed
        // by the top-level codex CLI and do NOT propagate into the exec subcommand's
        // sandbox initialisation — the network would be blocked silently if misplaced.
        assert!(
            cmd.contains("sandbox_workspace_write.network_access=true"),
            "network_access override must be present, got: {cmd}"
        );
        let exec_pos = cmd.find("exec").expect("exec must appear in command");
        let network_pos = cmd
            .find("sandbox_workspace_write.network_access=true")
            .expect("network_access override must appear in command");
        assert!(
            network_pos > exec_pos,
            "sandbox_workspace_write.network_access=true must appear AFTER exec, got: {cmd}"
        );
        // Regression: the command must not start with a `cat sys msg |` pipe
        // stage. `build_runner_script` captures the agent's exit status via
        // `PIPESTATUS[0]` of `{agent_cmd} | tee ...` — a leading `cat |` would
        // make PIPESTATUS[0] always report cat's (always-zero) exit code
        // instead of codex's real exit code. sys/msg files must be fed via
        // input redirection instead.
        assert!(
            !cmd.trim_start().starts_with("cat "),
            "codex command must not start with a `cat |` pipe stage, got: {cmd}"
        );
        assert!(
            cmd.contains("< <(cat \"/tmp/sys.txt\" \"/tmp/msg.txt\")"),
            "sys/msg files must be fed via process substitution, got: {cmd}"
        );
    }

    #[test]
    fn build_command_codex_full_access() {
        let perms = PermissionRules {
            autonomous: true,
            sandbox: SandboxLevel::FullAccess,
            disallowed_tools: vec![],
            allowed_tools: vec![],
            allowed_edit_paths: vec![],
            deny_read_only: false,
            extra_writable_dirs: vec![],
        };
        let cmd = runner().build_command(None, "", "/tmp/sys.txt", "/tmp/msg.txt", &perms);
        assert!(
            cmd.contains("--dangerously-bypass-approvals-and-sandbox"),
            "full access codex should use dangerously-bypass, got: {cmd}"
        );
    }

    #[test]
    fn build_command_codex_in_the_os_sandbox_turns_its_own_off() {
        let perms = PermissionRules {
            autonomous: true,
            sandbox: SandboxLevel::Os,
            disallowed_tools: vec![],
            allowed_tools: vec![],
            allowed_edit_paths: vec![],
            deny_read_only: false,
            extra_writable_dirs: vec![],
        };
        let cmd = runner().build_command(None, "", "/tmp/sys.txt", "/tmp/msg.txt", &perms);
        assert!(
            cmd.contains("--dangerously-bypass-approvals-and-sandbox"),
            "codex cannot nest its Seatbelt in the OS sandbox, got: {cmd}"
        );
    }

    #[test]
    fn build_command_codex_extra_writable_dirs() {
        // When git metadata dirs are outside the worktree sandbox root, the
        // runner adds them to extra_writable_dirs and build_command must emit
        // --add-dir flags so Codex can create index.lock there.
        let perms = PermissionRules {
            autonomous: true,
            sandbox: SandboxLevel::WorkspaceWrite,
            disallowed_tools: vec![],
            allowed_tools: vec![],
            allowed_edit_paths: vec![],
            deny_read_only: false,
            extra_writable_dirs: vec![
                std::path::PathBuf::from("/Users/gb/Projects/bean/.git"),
                std::path::PathBuf::from("/Users/gb/Projects/bean/.git/worktrees/task-123"),
            ],
        };
        let cmd = runner().build_command(None, "", "/tmp/sys.txt", "/tmp/msg.txt", &perms);
        assert!(
            cmd.contains("--add-dir"),
            "should include --add-dir flag, got: {cmd}"
        );
        assert!(
            cmd.contains("/Users/gb/Projects/bean/.git'"),
            "should include git common dir path, got: {cmd}"
        );
        assert!(
            cmd.contains("/Users/gb/Projects/bean/.git/worktrees/task-123"),
            "should include git dir path, got: {cmd}"
        );
        assert_eq!(
            cmd.matches("--add-dir ").count(),
            2,
            "should include two --add-dir flags, got: {cmd}"
        );
        assert!(
            cmd.contains("--sandbox workspace-write"),
            "autonomous codex with extra dirs should use --sandbox workspace-write, got: {cmd}"
        );
        assert!(
            cmd.contains(r#"-c 'approval_policy="never"'"#),
            "autonomous codex with extra dirs should use -c approval_policy=never, got: {cmd}"
        );
        assert!(
            !cmd.contains("--ask-for-approval"),
            "--ask-for-approval was removed in codex 0.133.0 and must not appear, got: {cmd}"
        );
        assert!(
            !cmd.contains("--full-auto"),
            "--full-auto is deprecated and must not appear, got: {cmd}"
        );
        assert!(
            cmd.contains("exec") && cmd.contains("--json -"),
            "should include exec and --json -, got: {cmd}"
        );
        let exec_pos = cmd.find("exec").expect("exec must appear in command");
        let network_pos = cmd
            .find("sandbox_workspace_write.network_access=true")
            .expect("network_access override must appear in command");
        assert!(
            network_pos > exec_pos,
            "sandbox_workspace_write.network_access=true must appear AFTER exec, got: {cmd}"
        );
    }

    #[test]
    fn build_command_codex_no_extra_writable_dirs() {
        // With no extra_writable_dirs, --add-dir must not appear in the command.
        let perms = PermissionRules::default();
        let cmd = runner().build_command(None, "", "/tmp/sys.txt", "/tmp/msg.txt", &perms);
        assert!(
            !cmd.contains("--add-dir"),
            "should not include --add-dir when no extra dirs, got: {cmd}"
        );
    }

    // ── Fixture-based tests ─────────────────────────────────────

    #[test]
    fn fixture_codex_success() {
        let raw = include_str!("../../../../tests/fixtures/codex_success.jsonl");
        let parsed = runner().parse_response(raw).unwrap();
        assert_eq!(parsed.response.status, "done");
        assert!(parsed.response.summary.contains("Implemented"));
        assert_eq!(parsed.response.accomplished.len(), 2);
        assert_eq!(parsed.response.files, vec!["src/handler.rs"]);
    }

    #[test]
    fn fixture_codex_rate_limit() {
        let raw = include_str!("../../../../tests/fixtures/codex_rate_limit.jsonl");
        let err = runner().parse_response(raw).unwrap_err();
        assert!(matches!(err, AgentError::RateLimit { .. }), "got: {err:?}");
    }

    // ── Connection / transient error tests ────────────────────────

    #[test]
    fn classify_codex_stream_disconnected() {
        let err = CodexRunner::classify_message("Reconnecting 3/5... stream disconnected");
        assert!(
            matches!(err, AgentError::NetworkError { .. }),
            "expected NetworkError so fallback retries, got: {err:?}"
        );
    }

    #[test]
    fn classify_codex_websocket_error() {
        let err = CodexRunner::classify_message("WebSocket connection closed unexpectedly");
        assert!(
            matches!(err, AgentError::NetworkError { .. }),
            "expected NetworkError so fallback retries, got: {err:?}"
        );
    }

    #[test]
    fn classify_codex_event_stream_lagged() {
        let err = CodexRunner::classify_message(
            "in-process app-server event stream lagged; dropped 1 events",
        );
        assert!(
            matches!(err, AgentError::NetworkError { .. }),
            "expected NetworkError so fallback retries, got: {err:?}"
        );
    }

    #[test]
    fn classify_codex_econnreset() {
        let err = CodexRunner::classify_message("ECONNRESET: connection reset by peer");
        assert!(
            matches!(err, AgentError::NetworkError { .. }),
            "expected NetworkError so fallback retries, got: {err:?}"
        );
    }

    // ── Production error patterns ─────────────────────────────────

    #[test]
    fn parse_codex_chatgpt_model_unsupported() {
        // Real error: "The 'gpt-4.1' model is not supported when using Codex with a ChatGPT account."
        let raw = r#"{"type":"error","message":"The 'gpt-4.1' model is not supported when using Codex with a ChatGPT account."}
{"type":"turn.failed","error":{"message":"Model not supported"}}"#;

        let err = runner().parse_response(raw).unwrap_err();
        assert!(
            matches!(err, AgentError::ModelUnavailable { .. }),
            "got: {err:?}"
        );
        if let AgentError::ModelUnavailable { model, .. } = &err {
            assert_eq!(model, "gpt-4.1");
        }
    }

    #[test]
    fn parse_codex_usage_limit_after_reconnect() {
        // Real failure: stream disconnects, then usage limit hit.
        // detect_error scans ALL events and prefers RateLimit over AgentFailed.
        let raw = r#"{"type":"error","message":"Reconnecting 2/5"}
{"type":"error","message":"Falling back from WebSockets to HTTPS transport"}
{"type":"error","message":"You've hit your usage limit. Upgrade to Pro at https://..."}
{"type":"turn.failed","error":{"message":"You've hit your usage limit"}}"#;

        let err = runner().parse_response(raw).unwrap_err();
        assert!(
            matches!(err, AgentError::RateLimit { .. }),
            "expected RateLimit (most specific), got: {err:?}"
        );
    }

    #[test]
    fn classify_error_chatgpt_model_unsupported() {
        let stdout = r#"{"type":"error","message":"The 'gpt-4.1' model is not supported when using Codex with a ChatGPT account."}
{"type":"turn.failed","error":{"message":"Model not supported"}}"#;

        let err = runner().classify_error(1, stdout, "");
        assert!(
            matches!(err, AgentError::ModelUnavailable { .. }),
            "got: {err:?}"
        );
    }

    // ── Real output.json fixture tests ───────────────────────────

    /// Real failure: websocket disconnections followed by usage limit.
    /// RateLimit must take priority over AgentFailed (reconnect errors).
    #[test]
    fn fixture_codex_usage_limit() {
        let raw = include_str!("../../../../tests/fixtures/codex_usage_limit.jsonl");
        let err = runner().parse_response(raw).unwrap_err();
        assert!(
            matches!(err, AgentError::RateLimit { .. }),
            "expected RateLimit (priority over reconnect errors), got: {err:?}"
        );
    }

    /// Real failure: unsupported model with ChatGPT account.
    #[test]
    fn fixture_codex_model_unsupported() {
        let raw = include_str!("../../../../tests/fixtures/codex_model_unsupported.jsonl");
        let err = runner().parse_response(raw).unwrap_err();
        assert!(
            matches!(err, AgentError::ModelUnavailable { .. }),
            "expected ModelUnavailable, got: {err:?}"
        );
        if let AgentError::ModelUnavailable { model, .. } = &err {
            assert_eq!(model, "gpt-4.1");
        }
    }

    // ── extract_text ─────────────────────────────────────────────

    /// Codex: extract_text returns the agent_message text for review parsing.
    #[test]
    fn extract_text_returns_agent_message() {
        let raw = concat!(
            r#"{"type":"thread.started","thread_id":"t1"}"#,
            "\n",
            r#"{"type":"turn.started"}"#,
            "\n",
            r#"{"type":"item.completed","item":{"type":"reasoning","text":"Thinking..."}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"{\"decision\":\"approve\",\"notes\":\"LGTM\",\"test_results\":\"pass\",\"issues\":[]}"}}"#,
            "\n",
            r#"{"type":"turn.completed"}"#,
        );
        let text = runner().extract_text(raw).unwrap();
        assert_eq!(
            text,
            r#"{"decision":"approve","notes":"LGTM","test_results":"pass","issues":[]}"#
        );
    }

    /// Codex: extract_text propagates RateLimit for terminal errors.
    #[test]
    fn extract_text_rate_limit_propagates() {
        let raw = concat!(
            r#"{"type":"thread.started","thread_id":"t1"}"#,
            "\n",
            r#"{"type":"error","message":"You've hit your usage limit. Upgrade to continue."}"#,
            "\n",
            r#"{"type":"turn.failed","error":{"message":"usage limit exceeded"}}"#,
        );
        let err = runner().extract_text(raw).unwrap_err();
        assert!(
            matches!(err, AgentError::RateLimit { .. }),
            "expected RateLimit, got: {err:?}"
        );
    }

    /// Codex: extract_text propagates Auth error.
    #[test]
    fn extract_text_auth_error_propagates() {
        let raw = concat!(
            r#"{"type":"thread.started","thread_id":"t1"}"#,
            "\n",
            r#"{"type":"error","message":"401 Unauthorized: invalid api key"}"#,
            "\n",
            r#"{"type":"turn.failed","error":{"message":"auth failed"}}"#,
        );
        let err = runner().extract_text(raw).unwrap_err();
        assert!(
            matches!(err, AgentError::Auth { .. }),
            "expected Auth, got: {err:?}"
        );
    }

    /// Codex: extract_text on empty input returns empty string (not an error).
    #[test]
    fn extract_text_empty_input_returns_empty_ok() {
        let text = runner().extract_text("").unwrap();
        assert_eq!(text, "");
    }

    // ── find_codex_result ───────────────────────────────────────

    #[test]
    fn find_codex_result_success() {
        let ndjson = concat!(
            r#"{"type":"thread.started","thread_id":"t1"}"#,
            "\n",
            r#"{"type":"turn.started"}"#,
            "\n",
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"{\"decision\":\"approve\",\"notes\":\"LGTM\"}"}}"#,
            "\n",
            r#"{"type":"turn.completed"}"#,
        );
        let result = find_codex_result(ndjson).expect("should find result");
        assert!(!result.is_error);
        assert!(result.result_text.contains("approve"));
    }

    #[test]
    fn find_codex_result_error() {
        let ndjson = concat!(
            r#"{"type":"error","message":"You've hit your usage limit"}"#,
            "\n",
            r#"{"type":"turn.failed","error":{"message":"usage limit"}}"#,
        );
        let result = find_codex_result(ndjson).expect("should find error result");
        assert!(result.is_error);
        assert!(result.result_text.contains("rate limit"));
    }

    #[test]
    fn find_codex_result_empty_returns_none() {
        assert!(find_codex_result("").is_none());
        assert!(find_codex_result("   ").is_none());
    }

    #[test]
    fn find_codex_result_plain_text_returns_none() {
        assert!(find_codex_result("just some plain text").is_none());
    }

    #[test]
    fn find_codex_result_skips_reasoning_and_commands() {
        let ndjson = concat!(
            r#"{"type":"item.completed","item":{"type":"reasoning","text":"Thinking..."}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"type":"command_execution","command":"cargo test"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"All tests passed"}}"#,
        );
        let result = find_codex_result(ndjson).expect("should find result");
        assert!(!result.is_error);
        assert_eq!(result.result_text, "All tests passed");
    }
}

// Codex wraps every command as `/bin/zsh -lc '<command>'` or `-lc "<command>"`
static SHELL_WRAP: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r#"(?s)^\S*/(?:ba|z)?sh -l?c (?:'(.*)'|"(.*)")$"#)
        .expect("shell wrap regex is valid")
});

/// Codex `--json`: each finished step is an `item.completed` event
fn stream_events(v: &serde_json::Value) -> Vec<StreamEvent> {
    match json_str(v, "/type") {
        "item.completed" => item_events(&v["item"]),
        "turn.completed" => vec![StreamEvent::Done(String::new())],
        "turn.failed" | "error" => vec![StreamEvent::Error(json_error_message(v))],
        _ => vec![],
    }
}

fn item_events(item: &serde_json::Value) -> Vec<StreamEvent> {
    let id = json_str(item, "/id").to_string();
    let call = |name: &str, input: serde_json::Value| StreamEvent::ToolCall {
        id: id.clone(),
        name: name.to_string(),
        input,
    };
    match json_str(item, "/type") {
        "agent_message" => vec![StreamEvent::Text(json_str(item, "/text").to_string())],
        "reasoning" => vec![StreamEvent::Thinking(json_str(item, "/text").to_string())],
        "command_execution" => {
            let command = json_str(item, "/command");
            let command = SHELL_WRAP
                .captures(command)
                .and_then(|c| c.get(1).or(c.get(2)))
                .map_or(command, |m| m.as_str());
            vec![
                call("Bash", serde_json::json!({ "command": command })),
                StreamEvent::ToolResult {
                    id: id.clone(),
                    name: "Bash".to_string(),
                    output: json_str(item, "/aggregated_output").to_string(),
                    is_error: item["exit_code"].as_i64().is_some_and(|c| c != 0),
                },
            ]
        }
        "file_change" => item["changes"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| call("Edit", serde_json::json!({ "path": c["path"] })))
            .collect(),
        "mcp_tool_call" => {
            let name = format!("{}.{}", json_str(item, "/server"), json_str(item, "/tool"));
            vec![call(&name, item["arguments"].clone())]
        }
        "web_search" => vec![call(
            "Search",
            serde_json::json!({ "query": json_str(item, "/query") }),
        )],
        "error" => vec![StreamEvent::Error(json_str(item, "/message").to_string())],
        _ => vec![],
    }
}

#[cfg(test)]
mod stream_tests {
    use super::*;
    use serde_json::json;

    fn events(line: &str) -> Vec<StreamEvent> {
        CodexRunner.stream_events(&serde_json::from_str(line).unwrap())
    }

    #[test]
    fn message_and_reasoning() {
        let msg = r#"{"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"Fixing."}}"#;
        assert_eq!(events(msg), vec![StreamEvent::Text("Fixing.".into())]);
        let think =
            r#"{"type":"item.completed","item":{"id":"i1","type":"reasoning","text":"**Plan**"}}"#;
        assert_eq!(
            events(think),
            vec![StreamEvent::Thinking("**Plan**".into())]
        );
    }

    #[test]
    fn command_unwraps_shell_and_carries_result() {
        let line = r#"{"type":"item.completed","item":{"id":"i2","type":"command_execution","command":"/bin/zsh -lc 'git status --short'","aggregated_output":"M a.rs\n","exit_code":1}}"#;
        assert_eq!(
            events(line),
            vec![
                StreamEvent::ToolCall {
                    id: "i2".into(),
                    name: "Bash".into(),
                    input: json!({"command": "git status --short"}),
                },
                StreamEvent::ToolResult {
                    id: "i2".into(),
                    name: "Bash".into(),
                    output: "M a.rs\n".into(),
                    is_error: true,
                },
            ]
        );
    }

    #[test]
    fn command_unwraps_double_quoted_shell() {
        let line = r#"{"type":"item.completed","item":{"id":"i4","type":"command_execution","command":"/bin/zsh -lc \"rg --files .github\"","aggregated_output":"","exit_code":0}}"#;
        assert_eq!(
            events(line)[0],
            StreamEvent::ToolCall {
                id: "i4".into(),
                name: "Bash".into(),
                input: json!({"command": "rg --files .github"}),
            }
        );
    }

    #[test]
    fn file_change_is_an_edit_per_path() {
        let line = r#"{"type":"item.completed","item":{"id":"i3","type":"file_change","changes":[{"path":"a.rs","kind":"update"},{"path":"b.rs","kind":"add"}]}}"#;
        let calls: Vec<String> = events(line)
            .into_iter()
            .map(|e| match e {
                StreamEvent::ToolCall { name, input, .. } => format!("{name} {}", input["path"]),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(calls, vec![r#"Edit "a.rs""#, r#"Edit "b.rs""#]);
    }

    #[test]
    fn errors_and_turn_end() {
        assert_eq!(
            events(r#"{"type":"error","message":"Reconnecting... 2/5"}"#),
            vec![StreamEvent::Error("Reconnecting... 2/5".into())]
        );
        assert_eq!(
            events(r#"{"type":"turn.failed","error":{"message":"quota exceeded"}}"#),
            vec![StreamEvent::Error("quota exceeded".into())]
        );
        assert_eq!(
            events(r#"{"type":"turn.completed","usage":{}}"#),
            vec![StreamEvent::Done(String::new())]
        );
    }

    #[test]
    fn started_items_are_dropped() {
        let started =
            r#"{"type":"item.started","item":{"type":"command_execution","command":"ls"}}"#;
        let dropped = events(started);
        assert!(dropped.is_empty(), "{dropped:?}");
        let dropped = events(r#"{"type":"thread.started","thread_id":"t"}"#);
        assert!(dropped.is_empty(), "{dropped:?}");
    }
}
