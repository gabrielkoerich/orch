//! Codex agent runner.
//!
//! ## CLI invocation
//!
//! ```bash
//! cat "{msg_file}" | codex --model {model} \
//!   --ask-for-approval never \
//!   --sandbox workspace-write \
//!   exec --json -
//! ```
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

use super::{AgentError, AgentRunner, ParsedResponse, PermissionRules, SandboxLevel};
use crate::parser;

/// Runner for Codex agent.
pub struct CodexRunner;

impl CodexRunner {
    /// Extract the agent's response text from NDJSON events.
    ///
    /// Looks for `item.completed` events with `item.type == "agent_message"`.
    /// Uses the **last** agent message that contains valid-looking JSON,
    /// since earlier messages are often progress updates.
    fn extract_agent_text(&self, events: &[serde_json::Value]) -> Option<String> {
        let mut texts = Vec::new();

        for event in events {
            let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");

            if event_type == "item.completed" {
                if let Some(item) = event.get("item") {
                    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");

                    if item_type == "agent_message" {
                        if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                            texts.push(text.to_string());
                        }
                    }
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
    fn detect_error(&self, events: &[serde_json::Value]) -> Option<AgentError> {
        let mut best: Option<AgentError> = None;

        for event in events {
            let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");

            let classified = match event_type {
                "turn.failed" => {
                    let message = event
                        .get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("turn failed");
                    Some(self.classify_message(message))
                }
                "error" => {
                    let message = event
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");
                    Some(self.classify_message(message))
                }
                "item.completed" => {
                    if let Some(item) = event.get("item") {
                        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        if item_type == "error" {
                            let message = item
                                .get("message")
                                .and_then(|v| v.as_str())
                                .unwrap_or("item error");
                            Some(self.classify_message(message))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                _ => None,
            };

            if let Some(err) = classified {
                // Replace if no best yet, or if current best is only a generic AgentFailed.
                // Keep existing specific errors as-is.
                match &best {
                    None | Some(AgentError::AgentFailed { .. }) => {
                        best = Some(err);
                    }
                    Some(_) => {}
                }
            }
        }

        best
    }

    /// Classify an error message into an AgentError variant.
    fn classify_message(&self, message: &str) -> AgentError {
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

        // Connection/network errors — codex-specific transient patterns
        if lower.contains("reconnecting")
            || lower.contains("stream disconnected")
            || lower.contains("connection closed")
            || lower.contains("websocket")
            || lower.contains("econnreset")
        {
            return AgentError::AgentFailed {
                message: format!("codex failed: {message}"),
            };
        }
        if super::patterns::detect_network_error(message).is_some() {
            return AgentError::AgentFailed {
                message: format!("codex failed: {message}"),
            };
        }

        AgentError::AgentFailed {
            message: message.to_string(),
        }
    }
}

impl AgentRunner for CodexRunner {
    #[cfg(test)]
    fn name(&self) -> &str {
        "codex"
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
        if let Some(err) = self.detect_error(&events) {
            return Err(err);
        }

        // Return agent_message text, or fall back to the raw output.
        Ok(self
            .extract_agent_text(&events)
            .unwrap_or_else(|| trimmed.to_string()))
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

        // Codex permission mode:
        // - autonomous → --full-auto (auto-approval + workspace-write sandbox)
        // - supervised → --ask-for-approval suggest
        // - full access → --dangerously-bypass-approvals-and-sandbox
        let permission_flags = if permissions.autonomous {
            match permissions.sandbox {
                SandboxLevel::FullAccess => {
                    "--dangerously-bypass-approvals-and-sandbox".to_string()
                }
                _ => "--full-auto".to_string(),
            }
        } else {
            let sandbox = match permissions.sandbox {
                SandboxLevel::WorkspaceWrite | SandboxLevel::None => "workspace-write",
                SandboxLevel::FullAccess => "danger-full-access",
            };
            format!("--ask-for-approval suggest --sandbox {sandbox}")
        };

        format!(
            r#"cat "{sys_file}" "{msg_file}" | {timeout_cmd} codex {model_flag} \
  {permission_flags} \
  -c 'sandbox_workspace_write.network_access=true' \
  -c 'shell_environment_policy.inherit=all' \
  exec --json -"#,
            sys_file = sys_file,
            msg_file = msg_file,
            timeout_cmd = timeout_cmd,
            model_flag = model_flag,
            permission_flags = permission_flags,
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
        if let Some(err) = self.detect_error(&events) {
            return Err(err);
        }

        // Extract agent response text
        let agent_text =
            self.extract_agent_text(&events)
                .ok_or_else(|| AgentError::InvalidResponse {
                    raw: trimmed.to_string(),
                })?;

        // Parse the agent text through our standard parser
        let response = parser::parse(&agent_text).map_err(|_| AgentError::InvalidResponse {
            raw: agent_text.clone(),
        })?;

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
        if let Some(err) = self.detect_error(&events) {
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
    let runner = CodexRunner;
    let events = super::parse_ndjson(ndjson.trim());
    if events.is_empty() {
        return None;
    }

    // Check for errors first
    let is_error;
    let error_text;
    if let Some(err) = runner.detect_error(&events) {
        is_error = true;
        error_text = Some(err.to_string());
    } else {
        is_error = false;
        error_text = None;
    }

    let result_text = if is_error {
        error_text.unwrap_or_default()
    } else {
        runner.extract_agent_text(&events).unwrap_or_default()
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
        assert!(cmd.contains("exec --json -"));
        assert!(
            cmd.contains("--full-auto"),
            "default autonomous codex should use --full-auto, got: {cmd}"
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
        };
        let cmd = runner().build_command(None, "", "/tmp/sys.txt", "/tmp/msg.txt", &perms);
        assert!(
            cmd.contains("--dangerously-bypass-approvals-and-sandbox"),
            "full access codex should use dangerously-bypass, got: {cmd}"
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
        let err = runner().classify_message("Reconnecting 3/5... stream disconnected");
        assert!(
            matches!(err, AgentError::AgentFailed { .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn classify_codex_websocket_error() {
        let err = runner().classify_message("WebSocket connection closed unexpectedly");
        assert!(
            matches!(err, AgentError::AgentFailed { .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn classify_codex_econnreset() {
        let err = runner().classify_message("ECONNRESET: connection reset by peer");
        assert!(
            matches!(err, AgentError::AgentFailed { .. }),
            "got: {err:?}"
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
        assert!(text.is_empty());
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
