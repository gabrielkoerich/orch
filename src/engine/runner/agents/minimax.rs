//! MiniMax agent runner — extends the Claude-compatible runner.
//!
//! MiniMax uses the `claude` CLI via a shell wrapper that sets
//! `ANTHROPIC_BASE_URL` to the MiniMax API. The underlying model
//! (MiniMax-M2.7) has different output behavior from Claude:
//!
//! - **No `"type":"result"` event** — the stream-json output ends with
//!   an `assistant` message, not a proper result envelope.
//! - **`stop_reason: null`** — the model doesn't set stop_reason.
//! - **Analysis-heavy** — tends to do many Read/Grep calls before editing.
//!
//! This runner overrides `parse_response` to handle these differences
//! while delegating command building and routing to `ClaudeRunner`.

use super::claude::ClaudeRunner;
use super::{truncate_at_char_boundary, AgentError, AgentRunner, ParsedResponse, PermissionRules};

/// Count Edit and Write tool_use events in NDJSON stream output.
fn count_edit_tools(raw: &str) -> usize {
    let mut count = 0;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let Some(content) = val
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        else {
            continue;
        };
        for item in content {
            if item.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if name == "Edit" || name == "Write" || name == "NotebookEdit" {
                    count += 1;
                }
            }
        }
    }
    count
}

/// Extract the last assistant text message from NDJSON stream output.
fn extract_last_assistant_text(raw: &str) -> Option<String> {
    let mut last_text = None;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let Some(content) = val
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        else {
            continue;
        };
        for item in content {
            if item.get("type").and_then(|v| v.as_str()) == Some("text") {
                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        last_text = Some(trimmed.to_string());
                    }
                }
            }
        }
    }
    last_text
}

/// Extract token usage from NDJSON stream by summing assistant message usage fields.
fn extract_stream_usage(raw: &str) -> (Option<u64>, Option<u64>) {
    let mut total_input: u64 = 0;
    let mut total_output: u64 = 0;
    let mut found = false;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        if let Some(usage) = val.get("message").and_then(|m| m.get("usage")) {
            if let Some(inp) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
                total_input = total_input.max(inp); // take max (cumulative reported)
                found = true;
            }
            if let Some(out) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                total_output = total_output.max(out);
                found = true;
            }
        }
    }
    if found {
        (Some(total_input), Some(total_output))
    } else {
        (None, None)
    }
}

/// Runner for MiniMax-via-Claude agents (delegates to ClaudeRunner for commands).
/// Named `MiniMaxClaudeRunner` to distinguish from a future native MiniMax CLI runner.
pub struct MiniMaxClaudeRunner {
    inner: ClaudeRunner,
}

impl MiniMaxClaudeRunner {
    pub fn new(binary: &str) -> Self {
        Self {
            inner: ClaudeRunner::new(binary),
        }
    }
}

impl AgentRunner for MiniMaxClaudeRunner {
    #[cfg(test)]
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn build_command(
        &self,
        model: Option<&str>,
        timeout_cmd: &str,
        sys_file: &str,
        msg_file: &str,
        permissions: &PermissionRules,
    ) -> String {
        self.inner
            .build_command(model, timeout_cmd, sys_file, msg_file, permissions)
    }

    fn parse_response(&self, raw: &str) -> Result<ParsedResponse, AgentError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(AgentError::InvalidResponse { raw: String::new() });
        }

        // First try the standard Claude parsing (works if the model emits result events)
        if let Ok(parsed) = self.inner.parse_response(raw) {
            return Ok(parsed);
        }

        // MiniMax models often don't emit "type":"result" events.
        // Build a response from the stream content instead.
        let edit_count = count_edit_tools(trimmed);
        let last_text = extract_last_assistant_text(trimmed);
        let (input_tokens, output_tokens) = extract_stream_usage(trimmed);

        let summary = last_text.unwrap_or_default();

        // Status is always "done" — the response handler checks the worktree
        // for actual code changes and re-routes if needed.
        let status = "done".to_string();

        tracing::info!(
            edit_count,
            summary_len = summary.len(),
            "minimax: built response from stream (no result event)"
        );

        let response = crate::parser::AgentResponse {
            status,
            summary: {
                let end = truncate_at_char_boundary(&summary, 500);
                summary[..end].to_string()
            },
            accomplished: vec![],
            remaining: vec![],
            files: vec![],
            error: None,
            input_tokens,
            output_tokens,
            learnings: vec![],
            delegations: vec![],
        };

        Ok(ParsedResponse {
            response,
            input_tokens,
            output_tokens,
            duration_ms: None,
        })
    }

    fn extract_text(&self, raw: &str) -> Result<String, AgentError> {
        // Try standard Claude extraction first
        if let Ok(text) = self.inner.extract_text(raw) {
            if !text.is_empty() {
                return Ok(text);
            }
        }

        // Fall back to extracting last assistant text from stream
        extract_last_assistant_text(raw).ok_or_else(|| AgentError::InvalidResponse {
            raw: raw.chars().take(300).collect(),
        })
    }

    fn classify_error(&self, exit_code: i32, stdout: &str, stderr: &str) -> AgentError {
        self.inner.classify_error(exit_code, stdout, stderr)
    }

    fn router_command(
        &self,
        prompt: &str,
        model: Option<&str>,
    ) -> anyhow::Result<tokio::process::Command> {
        self.inner.router_command(prompt, model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_edit_tools_finds_edits() {
        let raw = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{}}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Edit","input":{}}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Write","input":{}}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Grep","input":{}}]}}"#;
        assert_eq!(count_edit_tools(raw), 2);
    }

    #[test]
    fn count_edit_tools_zero_on_read_only() {
        let raw = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{}}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Grep","input":{}}]}}"#;
        assert_eq!(count_edit_tools(raw), 0);
    }

    #[test]
    fn extract_last_text_gets_final_message() {
        let raw = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"first message"}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{}}]}}
{"type":"assistant","message":{"content":[{"type":"text","text":"final summary"}]}}"#;
        assert_eq!(
            extract_last_assistant_text(raw),
            Some("final summary".to_string())
        );
    }

    #[test]
    fn minimax_parse_falls_back_to_stream() {
        let runner = MiniMaxClaudeRunner::new("minimax");
        // No result event, just assistant messages
        let raw = r#"{"type":"system","subtype":"init","model":"claude-opus-4-6"}
{"type":"assistant","message":{"content":[{"type":"text","text":"analyzing code"}],"model":"MiniMax-M2.7","usage":{"input_tokens":1000,"output_tokens":200}}}"#;
        let result = runner.parse_response(raw);
        assert!(result.is_ok());
        let parsed = result.unwrap();
        assert_eq!(parsed.response.status, "done");
        assert!(parsed.response.summary.contains("analyzing code"));
    }

    #[test]
    fn minimax_delegates_to_claude_when_result_exists() {
        let runner = MiniMaxClaudeRunner::new("minimax");
        let raw = r#"{"type":"result","subtype":"success","is_error":false,"result":"{\"status\":\"done\",\"summary\":\"fixed it\"}","duration_ms":5000}"#;
        let result = runner.parse_response(raw);
        assert!(result.is_ok());
        let parsed = result.unwrap();
        assert_eq!(parsed.response.summary, "fixed it");
    }
}
