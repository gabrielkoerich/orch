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
    /// Parse an NDJSON stream from Codex into structured events.
    fn parse_ndjson(&self, raw: &str) -> Vec<serde_json::Value> {
        raw.lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| match serde_json::from_str(line) {
                Ok(val) => Some(val),
                Err(e) => {
                    tracing::debug!(line, error = %e, "codex: skipping unparseable NDJSON line");
                    None
                }
            })
            .collect()
    }

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
                best = Some(match (&best, &err) {
                    // Prefer specific errors over generic AgentFailed
                    (Some(AgentError::AgentFailed { .. }), _) => err,
                    (None, _) => err,
                    // Keep existing specific error
                    _ => best.unwrap(),
                });
            }
        }

        best
    }

    /// Classify an error message into an AgentError variant.
    fn classify_message(&self, message: &str) -> AgentError {
        let lower = message.to_lowercase();

        // Rate limit / usage limit
        if lower.contains("usage limit")
            || lower.contains("rate limit")
            || lower.contains("429")
            || lower.contains("too many requests")
            || lower.contains("you've hit your")
        {
            return AgentError::RateLimit {
                message: message.to_string(),
                retry_after: None,
            };
        }

        // Model not supported / not found
        if lower.contains("model metadata")
            || lower.contains("model is not supported")
            || lower.contains("not found")
                && (lower.contains("model") || lower.contains("metadata"))
        {
            // Try to extract model name
            let model = extract_model_name(message).unwrap_or_default();
            return AgentError::ModelUnavailable {
                message: message.to_string(),
                model,
            };
        }

        // Auth errors
        if lower.contains("unauthorized")
            || lower.contains("invalid key")
            || lower.contains("invalid api")
            || lower.contains("401")
            || lower.contains("403")
        {
            return AgentError::Auth {
                message: message.to_string(),
            };
        }

        // Context overflow
        if lower.contains("context_length") || lower.contains("too many tokens") {
            return AgentError::ContextOverflow {
                message: message.to_string(),
                max_tokens: None,
            };
        }

        // Connection/network errors — treat as transient, worth retrying
        if lower.contains("reconnecting")
            || lower.contains("stream disconnected")
            || lower.contains("connection closed")
            || lower.contains("websocket")
            || lower.contains("econnreset")
            || lower.contains("econnrefused")
        {
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
    fn name(&self) -> &str {
        "codex"
    }

    fn is_available(&self) -> bool {
        which::which("codex").is_ok()
    }

    fn build_command(
        &self,
        model: Option<&str>,
        timeout_cmd: &str,
        _sys_file: &str,
        msg_file: &str,
        permissions: &PermissionRules,
    ) -> String {
        let model_flag = model.map(|m| format!("--model {m}")).unwrap_or_default();

        // Codex approval: autonomous → never, supervised → suggest
        let approval = if permissions.autonomous {
            "never"
        } else {
            "suggest"
        };

        // Codex sandbox: map from unified SandboxLevel
        let sandbox = match permissions.sandbox {
            SandboxLevel::WorkspaceWrite => "workspace-write",
            SandboxLevel::FullAccess => "danger-full-access",
            SandboxLevel::None => "workspace-write", // default safe
        };

        format!(
            r#"cat "{msg_file}" | {timeout_cmd} codex {model_flag} \
  --ask-for-approval {approval} \
  --sandbox {sandbox} \
  exec --json -"#,
            msg_file = msg_file,
            timeout_cmd = timeout_cmd,
            model_flag = model_flag,
            approval = approval,
            sandbox = sandbox,
        )
    }

    fn parse_response(&self, raw: &str) -> Result<ParsedResponse, AgentError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(AgentError::InvalidResponse { raw: String::new() });
        }

        let events = self.parse_ndjson(trimmed);

        if events.is_empty() {
            // Maybe it's direct JSON, not NDJSON
            if let Ok(resp) = parser::parse(trimmed) {
                return Ok(ParsedResponse {
                    response: resp,
                    input_tokens: None,
                    output_tokens: None,
                    duration_ms: None,
                    permission_denials: vec![],
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
            permission_denials: vec![],
        })
    }

    fn classify_error(&self, exit_code: i32, stdout: &str, stderr: &str) -> AgentError {
        // Try parsing NDJSON events from stdout for structured errors
        let events = self.parse_ndjson(stdout);
        if let Some(err) = self.detect_error(&events) {
            return err;
        }

        // Fall back to pattern matching
        let combined = format!("{stdout}\n{stderr}");
        super::patterns::classify_from_text(exit_code, &combined)
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
        assert!(cmd.contains("--model gpt-4o"));
        assert!(cmd.contains("exec --json -"));
        assert!(cmd.contains("--ask-for-approval never"));
        assert!(cmd.contains("--sandbox workspace-write"));
    }

    #[test]
    fn build_command_codex_full_access() {
        let perms = PermissionRules {
            autonomous: true,
            sandbox: SandboxLevel::FullAccess,
            disallowed_tools: vec![],
            blocked_paths: vec![],
        };
        let cmd = runner().build_command(None, "", "/tmp/sys.txt", "/tmp/msg.txt", &perms);
        assert!(cmd.contains("--sandbox danger-full-access"));
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
}
