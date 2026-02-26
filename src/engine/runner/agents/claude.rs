//! Claude-compatible agent runner.
//!
//! Handles Claude, Kimi, and MiniMax agents. All three use the same
//! `claude` binary (or a shell wrapper that sets `ANTHROPIC_BASE_URL`).
//!
//! ## CLI invocation
//!
//! ```bash
//! claude -p --model {model} \
//!   --permission-mode bypassPermissions \
//!   --output-format json \
//!   --disallowedTools '...' \
//!   --append-system-prompt "{sys_file}" \
//!   < "{msg_file}"
//! ```
//!
//! ## Output format (`--output-format json`)
//!
//! Single JSON object:
//! ```json
//! {
//!   "type": "result",
//!   "subtype": "success",
//!   "is_error": false,
//!   "duration_ms": 2006,
//!   "result": "```json\n{\"status\": \"done\", ...}\n```",
//!   "usage": {"input_tokens": 10, "output_tokens": 78},
//!   "modelUsage": {"claude-haiku-4-5-20251001": {"inputTokens": 10, ...}},
//!   "permission_denials": []
//! }
//! ```
//!
//! ## Kimi/MiniMax wrappers
//!
//! Both are shell scripts that `exec` the claude binary with custom env:
//! ```bash
//! exec env ANTHROPIC_BASE_URL=... ANTHROPIC_API_KEY=... claude "$@"
//! ```
//!
//! ## Known error patterns
//!
//! - `is_error: true` with various message strings
//! - "credit balance too low" — billing/auth
//! - "overloaded" — capacity / rate limit (HTTP 529)
//! - "rate limit" / "429" — standard rate limiting
//! - "context_length_exceeded" — context overflow

#[cfg(test)]
use super::SandboxLevel;
use super::{AgentError, AgentRunner, ParsedResponse, PermissionRules};
use crate::parser;

/// Runner for Claude-compatible agents (Claude, Kimi, MiniMax).
pub struct ClaudeRunner {
    /// The agent binary name (claude, kimi, minimax, or custom).
    binary: String,
}

impl ClaudeRunner {
    pub fn new(agent_name: &str) -> Self {
        Self {
            binary: agent_name.to_string(),
        }
    }

    /// Parse the Claude `--output-format json` envelope.
    ///
    /// Extracts `result`, `usage`, `permission_denials`, and `is_error`.
    fn parse_envelope(&self, raw: &str) -> Result<ParsedResponse, AgentError> {
        let parsed_json: serde_json::Value = serde_json::from_str(raw).map_err(|_| {
            // Not valid JSON at all — try as raw text
            AgentError::InvalidResponse {
                raw: raw.to_string(),
            }
        })?;

        let envelope = parsed_json
            .as_object()
            .ok_or_else(|| AgentError::InvalidResponse {
                raw: raw.to_string(),
            })?;

        // Check if this is a Claude envelope (has "type" field)
        let is_envelope = envelope.get("type").and_then(|v| v.as_str()) == Some("result");

        if !is_envelope {
            // Not a Claude envelope — try direct parsing via generic parser
            return self.parse_direct(raw);
        }

        // Check is_error flag
        let is_error = envelope
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Extract the result text
        let result_text = envelope
            .get("result")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if is_error {
            // Classify the error from the result text.
            // Check auth before rate_limit — billing errors (credit balance)
            // are auth issues, not transient rate limits.
            let combined = format!("{result_text} {}", raw);
            if let Some(e) = super::patterns::detect_auth_error(&combined) {
                return Err(e);
            }
            if let Some(e) = super::patterns::detect_rate_limit(&combined) {
                return Err(e);
            }
            if let Some(e) = super::patterns::detect_context_overflow(&combined) {
                return Err(e);
            }
            return Err(AgentError::AgentFailed {
                message: result_text.to_string(),
            });
        }

        // Extract token usage from top-level `usage` object
        let (input_tokens, output_tokens) = extract_usage(envelope);

        // Extract duration
        let duration_ms = envelope.get("duration_ms").and_then(|v| v.as_u64());

        // Extract permission denials
        let permission_denials = envelope
            .get("permission_denials")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        // Parse the inner result text into AgentResponse
        let response = parser::parse(result_text).map_err(|_| {
            // Check if result_text itself contains error signals
            if let Some(e) = super::patterns::detect_rate_limit(result_text) {
                return e;
            }
            AgentError::InvalidResponse {
                raw: result_text.to_string(),
            }
        })?;

        Ok(ParsedResponse {
            response,
            input_tokens,
            output_tokens,
            duration_ms,
            permission_denials,
        })
    }

    /// Parse raw text directly (non-envelope response).
    fn parse_direct(&self, raw: &str) -> Result<ParsedResponse, AgentError> {
        let response = parser::parse(raw).map_err(|_| AgentError::InvalidResponse {
            raw: raw.to_string(),
        })?;

        Ok(ParsedResponse {
            response,
            input_tokens: None,
            output_tokens: None,
            duration_ms: None,
            permission_denials: vec![],
        })
    }
}

impl AgentRunner for ClaudeRunner {
    fn name(&self) -> &str {
        &self.binary
    }

    fn is_available(&self) -> bool {
        which::which(&self.binary).is_ok()
    }

    fn build_command(
        &self,
        model: Option<&str>,
        timeout_cmd: &str,
        sys_file: &str,
        msg_file: &str,
        permissions: &PermissionRules,
    ) -> String {
        let model_flag = model.map(|m| format!("--model {m}")).unwrap_or_default();

        // Claude permission mode: autonomous → bypassPermissions, supervised → acceptEdits
        let permission_mode = if permissions.autonomous {
            "bypassPermissions"
        } else {
            "acceptEdits"
        };

        // Build disallowed tools list from unified rules + blocked paths
        let mut all_disallowed = permissions.disallowed_tools.clone();
        for path in &permissions.blocked_paths {
            let p = path.to_string_lossy();
            all_disallowed.extend([
                format!("Bash(cd {p}*)"),
                format!("Read({p}/*)"),
                format!("Write({p}/*)"),
                format!("Edit({p}/*)"),
            ]);
        }

        let disallowed_flag = if !all_disallowed.is_empty() {
            format!("--disallowedTools '{}'", all_disallowed.join(","))
        } else {
            String::new()
        };

        format!(
            r#"{timeout_cmd} {binary} -p {model_flag} \
  --permission-mode {permission_mode} \
  --output-format json \
  {disallowed_flag} \
  --append-system-prompt "{sys_file}" \
  < "{msg_file}""#,
            timeout_cmd = timeout_cmd,
            binary = self.binary,
            model_flag = model_flag,
            permission_mode = permission_mode,
            disallowed_flag = disallowed_flag,
            sys_file = sys_file,
            msg_file = msg_file,
        )
    }

    fn parse_response(&self, raw: &str) -> Result<ParsedResponse, AgentError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(AgentError::InvalidResponse { raw: String::new() });
        }

        self.parse_envelope(trimmed)
    }

    fn classify_error(&self, exit_code: i32, stdout: &str, stderr: &str) -> AgentError {
        let combined = format!("{stdout}\n{stderr}");
        super::patterns::classify_from_text(exit_code, &combined)
    }
}

/// Extract input/output tokens from the Claude envelope's usage objects.
fn extract_usage(
    envelope: &serde_json::Map<String, serde_json::Value>,
) -> (Option<u64>, Option<u64>) {
    // Try top-level `usage` first
    if let Some(usage) = envelope.get("usage").and_then(|v| v.as_object()) {
        let input = usage.get("input_tokens").and_then(|v| v.as_u64());
        let output = usage.get("output_tokens").and_then(|v| v.as_u64());
        if input.is_some() || output.is_some() {
            return (input, output);
        }
    }

    // Try modelUsage (aggregated across models)
    if let Some(model_usage) = envelope.get("modelUsage").and_then(|v| v.as_object()) {
        let mut total_input = 0u64;
        let mut total_output = 0u64;
        let mut found = false;

        for (_model_name, model_stats) in model_usage {
            if let Some(stats) = model_stats.as_object() {
                if let Some(i) = stats
                    .get("inputTokens")
                    .or_else(|| stats.get("input_tokens"))
                    .and_then(|v| v.as_u64())
                {
                    total_input += i;
                    found = true;
                }
                if let Some(o) = stats
                    .get("outputTokens")
                    .or_else(|| stats.get("output_tokens"))
                    .and_then(|v| v.as_u64())
                {
                    total_output += o;
                    found = true;
                }
            }
        }

        if found {
            return (Some(total_input), Some(total_output));
        }
    }

    (None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runner() -> ClaudeRunner {
        ClaudeRunner::new("claude")
    }

    #[test]
    fn parse_claude_envelope_success() {
        let raw = r#"{
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "duration_ms": 2006,
            "result": "{\"status\":\"done\",\"summary\":\"hello\",\"accomplished\":[],\"remaining\":[],\"files\":[]}",
            "usage": {"input_tokens": 10, "output_tokens": 78},
            "permission_denials": []
        }"#;

        let parsed = runner().parse_response(raw).unwrap();
        assert_eq!(parsed.response.status, "done");
        assert_eq!(parsed.response.summary, "hello");
        assert_eq!(parsed.input_tokens, Some(10));
        assert_eq!(parsed.output_tokens, Some(78));
        assert_eq!(parsed.duration_ms, Some(2006));
        assert!(parsed.permission_denials.is_empty());
    }

    #[test]
    fn parse_claude_envelope_with_markdown_result() {
        let raw = r#"{
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "duration_ms": 5000,
            "result": "```json\n{\"status\": \"done\", \"summary\": \"fixed bug\"}\n```",
            "usage": {"input_tokens": 100, "output_tokens": 50},
            "permission_denials": ["Bash(rm *)"]
        }"#;

        let parsed = runner().parse_response(raw).unwrap();
        assert_eq!(parsed.response.status, "done");
        assert_eq!(parsed.response.summary, "fixed bug");
        assert_eq!(parsed.permission_denials, vec!["Bash(rm *)"]);
    }

    #[test]
    fn parse_claude_envelope_error() {
        let raw = r#"{
            "type": "result",
            "subtype": "error",
            "is_error": true,
            "result": "credit balance too low",
            "usage": {},
            "permission_denials": []
        }"#;

        let err = runner().parse_response(raw).unwrap_err();
        assert!(matches!(err, AgentError::Auth { .. }));
    }

    #[test]
    fn parse_claude_envelope_rate_limit_error() {
        let raw = r#"{
            "type": "result",
            "subtype": "error",
            "is_error": true,
            "result": "rate limit exceeded, please try again later",
            "usage": {},
            "permission_denials": []
        }"#;

        let err = runner().parse_response(raw).unwrap_err();
        assert!(matches!(err, AgentError::RateLimit { .. }));
    }

    #[test]
    fn parse_direct_json_no_envelope() {
        let raw =
            r#"{"status":"done","summary":"hello","accomplished":[],"remaining":[],"files":[]}"#;

        let parsed = runner().parse_response(raw).unwrap();
        assert_eq!(parsed.response.status, "done");
        assert!(parsed.input_tokens.is_none());
    }

    #[test]
    fn parse_empty_response() {
        let err = runner().parse_response("").unwrap_err();
        assert!(matches!(err, AgentError::InvalidResponse { .. }));
    }

    #[test]
    fn classify_error_rate_limit() {
        let err = runner().classify_error(1, "", "429 Too Many Requests");
        assert!(matches!(err, AgentError::RateLimit { .. }));
    }

    #[test]
    fn classify_error_timeout() {
        let err = runner().classify_error(124, "", "");
        assert!(matches!(err, AgentError::Timeout { .. }));
    }

    #[test]
    fn extract_usage_from_model_usage() {
        let response_json: serde_json::Value = serde_json::json!({
            "modelUsage": {
                "claude-haiku-4-5-20251001": {
                    "inputTokens": 1000,
                    "outputTokens": 500
                }
            }
        });
        let envelope = response_json.as_object().unwrap();
        let (input, output) = extract_usage(envelope);
        assert_eq!(input, Some(1000));
        assert_eq!(output, Some(500));
    }

    #[test]
    fn build_command_with_model() {
        let r = runner();
        let perms = PermissionRules {
            autonomous: true,
            sandbox: SandboxLevel::WorkspaceWrite,
            disallowed_tools: vec!["Bash(rm *)".to_string()],
            blocked_paths: vec![],
        };
        let cmd = r.build_command(
            Some("opus"),
            "timeout 1800",
            "/tmp/sys.txt",
            "/tmp/msg.txt",
            &perms,
        );
        assert!(cmd.contains("--model opus"));
        assert!(cmd.contains("claude -p"));
        assert!(cmd.contains("--output-format json"));
        assert!(cmd.contains("--permission-mode bypassPermissions"));
        assert!(cmd.contains("--disallowedTools 'Bash(rm *)'"));
    }

    #[test]
    fn build_command_supervised_mode() {
        let r = runner();
        let perms = PermissionRules {
            autonomous: false,
            sandbox: SandboxLevel::WorkspaceWrite,
            disallowed_tools: vec![],
            blocked_paths: vec![],
        };
        let cmd = r.build_command(None, "", "/tmp/sys.txt", "/tmp/msg.txt", &perms);
        assert!(cmd.contains("--permission-mode acceptEdits"));
    }

    #[test]
    fn build_command_with_blocked_paths() {
        let r = runner();
        let perms = PermissionRules {
            autonomous: true,
            sandbox: SandboxLevel::WorkspaceWrite,
            disallowed_tools: vec![],
            blocked_paths: vec![std::path::PathBuf::from("/home/user/project")],
        };
        let cmd = r.build_command(None, "", "/tmp/sys.txt", "/tmp/msg.txt", &perms);
        assert!(cmd.contains("Bash(cd /home/user/project*)"));
        assert!(cmd.contains("Read(/home/user/project/*)"));
    }

    #[test]
    fn kimi_runner_name() {
        let r = ClaudeRunner::new("kimi");
        assert_eq!(r.name(), "kimi");
    }

    // ── Fixture-based tests ─────────────────────────────────────

    #[test]
    fn fixture_claude_success() {
        let raw = include_str!("../../../../tests/fixtures/claude_success.json");
        let parsed = runner().parse_response(raw).unwrap();
        assert_eq!(parsed.response.status, "done");
        assert!(parsed.response.summary.contains("Implemented"));
        assert_eq!(parsed.response.accomplished.len(), 2);
        assert_eq!(parsed.input_tokens, Some(15000));
        assert_eq!(parsed.output_tokens, Some(3500));
        assert_eq!(parsed.duration_ms, Some(45000));
    }

    #[test]
    fn fixture_claude_rate_limit() {
        let raw = include_str!("../../../../tests/fixtures/claude_error_rate_limit.json");
        let err = runner().parse_response(raw).unwrap_err();
        assert!(matches!(err, AgentError::RateLimit { .. }), "got: {err:?}");
    }
}
