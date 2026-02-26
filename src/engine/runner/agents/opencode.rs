//! OpenCode agent runner.
//!
//! ## CLI invocation
//!
//! ```bash
//! opencode run --format json -m {model} - < "{msg_file}"
//! ```
//!
//! ## Output format (`run --format json`)
//!
//! NDJSON stream:
//! ```jsonl
//! {"type":"step_start","timestamp":...,"part":{"type":"step-start","snapshot":"..."}}
//! {"type":"text","timestamp":...,"part":{"type":"text","text":"hello"}}
//! {"type":"step_finish","timestamp":...,"part":{"type":"step-finish","reason":"stop","cost":0,"tokens":{"total":17512,"input":17509,"output":3}}}
//! ```
//!
//! ## Token extraction
//!
//! Tokens are in the `step_finish` event: `part.tokens.{input,output,total}`
//!
//! ## Free models
//!
//! Discoverable via: `opencode models | grep free`
//! Known free models:
//! - `opencode/minimax-m2.5-free`
//! - `opencode/trinity-large-preview-free`

use super::{AgentError, AgentRunner, ParsedResponse, PermissionRules};
use crate::parser;
use std::sync::Mutex;

/// Runner for OpenCode agent.
pub struct OpenCodeRunner {
    /// Cached free models (model list + timestamp).
    free_models_cache: Mutex<Option<(Vec<String>, std::time::Instant)>>,
}

impl OpenCodeRunner {
    pub fn new() -> Self {
        Self {
            free_models_cache: Mutex::new(None),
        }
    }

    /// Parse NDJSON stream into events.
    fn parse_ndjson(&self, raw: &str) -> Vec<serde_json::Value> {
        raw.lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| match serde_json::from_str(line) {
                Ok(val) => Some(val),
                Err(e) => {
                    tracing::debug!(line, error = %e, "opencode: skipping unparseable NDJSON line");
                    None
                }
            })
            .collect()
    }

    /// Extract text from `text` events.
    ///
    /// Concatenates all text events, then tries to find the structured
    /// JSON response. If the concatenated text doesn't parse as JSON,
    /// tries each text event individually (newest first) since earlier
    /// events are often progress messages.
    fn extract_text(&self, events: &[serde_json::Value]) -> Option<String> {
        let mut texts = Vec::new();

        for event in events {
            let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");

            if event_type == "text" {
                if let Some(part) = event.get("part") {
                    if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                        texts.push(text.to_string());
                    }
                }
            }
        }

        if texts.is_empty() {
            return None;
        }

        // Try full concatenation first (most complete)
        let full = texts.join("");
        if full.trim().starts_with('{') || full.contains("```json") {
            return Some(full);
        }

        // Fall back: find the last text event that looks like JSON
        for text in texts.iter().rev() {
            let trimmed = text.trim();
            if trimmed.contains('{') && trimmed.contains("status") {
                return Some(text.clone());
            }
        }

        // Last resort: full concatenation
        Some(full)
    }

    /// Extract token usage from `step_finish` events.
    fn extract_tokens(&self, events: &[serde_json::Value]) -> (Option<u64>, Option<u64>) {
        for event in events.iter().rev() {
            let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");

            if event_type == "step_finish" {
                if let Some(part) = event.get("part") {
                    if let Some(tokens) = part.get("tokens").and_then(|v| v.as_object()) {
                        let input = tokens.get("input").and_then(|v| v.as_u64());
                        let output = tokens.get("output").and_then(|v| v.as_u64());
                        return (input, output);
                    }
                }
            }
        }

        (None, None)
    }

    /// Check for error events in the stream.
    fn detect_error(&self, events: &[serde_json::Value]) -> Option<AgentError> {
        for event in events {
            let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");

            if event_type == "error" {
                let message = event
                    .get("message")
                    .and_then(|v| v.as_str())
                    .or_else(|| event.get("error").and_then(|v| v.as_str()))
                    .unwrap_or("unknown error");

                return Some(classify_opencode_message(message));
            }

            // Check step_finish for error reasons
            if event_type == "step_finish" {
                if let Some(part) = event.get("part") {
                    let reason = part.get("reason").and_then(|v| v.as_str()).unwrap_or("");
                    if reason == "error" || reason == "failed" {
                        let msg = part
                            .get("error")
                            .and_then(|v| v.as_str())
                            .unwrap_or("step failed");
                        return Some(classify_opencode_message(msg));
                    }
                }
            }
        }

        None
    }

    /// Discover free models via `opencode models | grep free`.
    /// Results are cached for 1 hour.
    fn discover_free_models_cached(&self) -> Vec<String> {
        let mut cache = self.free_models_cache.lock().unwrap_or_else(|e| e.into_inner());

        // Check cache freshness (1 hour)
        if let Some((ref models, ref ts)) = *cache {
            if ts.elapsed() < std::time::Duration::from_secs(3600) {
                return models.clone();
            }
        }

        // Discover fresh
        let models = discover_free_models();
        *cache = Some((models.clone(), std::time::Instant::now()));
        models
    }
}

impl AgentRunner for OpenCodeRunner {
    fn name(&self) -> &str {
        "opencode"
    }

    fn is_available(&self) -> bool {
        which::which("opencode").is_ok()
    }

    fn build_command(
        &self,
        model: Option<&str>,
        timeout_cmd: &str,
        _sys_file: &str,
        msg_file: &str,
        _permissions: &PermissionRules,
    ) -> String {
        // OpenCode has no permission/sandbox flags — it relies on its own
        // built-in safety. Permission rules are enforced by the orchestrator
        // at the worktree level (filesystem isolation).
        let model_flag = model
            .map(|m| format!("--model {m}"))
            .unwrap_or_default();

        format!(
            r#"{timeout_cmd} opencode run {model_flag} \
  --format json - < "{msg_file}""#,
            timeout_cmd = timeout_cmd,
            model_flag = model_flag,
            msg_file = msg_file,
        )
    }

    fn parse_response(&self, raw: &str) -> Result<ParsedResponse, AgentError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(AgentError::InvalidResponse {
                raw: String::new(),
            });
        }

        let events = self.parse_ndjson(trimmed);

        if events.is_empty() {
            // Maybe direct JSON
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

        // Extract text
        let text = self.extract_text(&events).ok_or_else(|| {
            AgentError::InvalidResponse {
                raw: trimmed.to_string(),
            }
        })?;

        // Extract tokens
        let (input_tokens, output_tokens) = self.extract_tokens(&events);

        // Parse the text through standard parser
        let response = parser::parse(&text).map_err(|_| AgentError::InvalidResponse {
            raw: text.clone(),
        })?;

        Ok(ParsedResponse {
            response,
            input_tokens,
            output_tokens,
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

        let combined = format!("{stdout}\n{stderr}");
        super::patterns::classify_from_text(exit_code, &combined)
    }

    fn free_models(&self) -> Vec<String> {
        self.discover_free_models_cached()
    }

    fn available_models(&self) -> Vec<String> {
        // Known default models; could be extended with `opencode models` discovery
        vec![
            "anthropic/claude-sonnet-4-20250514".to_string(),
            "openai/gpt-4.1".to_string(),
        ]
    }
}

/// Classify an OpenCode error message.
fn classify_opencode_message(message: &str) -> AgentError {
    let lower = message.to_lowercase();

    if lower.contains("rate limit")
        || lower.contains("429")
        || lower.contains("usage limit")
        || lower.contains("too many requests")
    {
        return AgentError::RateLimit {
            message: message.to_string(),
            retry_after: None,
        };
    }

    if lower.contains("context") && (lower.contains("length") || lower.contains("overflow")) {
        return AgentError::ContextOverflow {
            message: message.to_string(),
            max_tokens: None,
        };
    }

    if lower.contains("unauthorized") || lower.contains("invalid key") || lower.contains("401") {
        return AgentError::Auth {
            message: message.to_string(),
        };
    }

    if lower.contains("model") && (lower.contains("not found") || lower.contains("not supported"))
    {
        return AgentError::ModelUnavailable {
            message: message.to_string(),
            model: String::new(),
        };
    }

    AgentError::AgentFailed {
        message: message.to_string(),
    }
}

/// Discover free models by running `opencode models` and filtering.
fn discover_free_models() -> Vec<String> {
    // Known free models as fallback
    let known = vec![
        "opencode/minimax-m2.5-free".to_string(),
        "opencode/trinity-large-preview-free".to_string(),
    ];

    // Try to discover dynamically
    let output = match std::process::Command::new("opencode")
        .args(["models"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return known,
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let discovered: Vec<String> = stdout
        .lines()
        .filter(|line| line.to_lowercase().contains("free"))
        .map(|line| line.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if discovered.is_empty() {
        known
    } else {
        discovered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runner() -> OpenCodeRunner {
        OpenCodeRunner::new()
    }

    #[test]
    fn parse_opencode_ndjson_success() {
        let raw = r#"{"type":"step_start","timestamp":1000,"part":{"type":"step-start","snapshot":"..."}}
{"type":"text","timestamp":1001,"part":{"type":"text","text":"{\"status\":\"done\",\"summary\":\"hello\",\"accomplished\":[],\"remaining\":[],\"files\":[]}"}}
{"type":"step_finish","timestamp":1002,"part":{"type":"step-finish","reason":"stop","cost":0,"tokens":{"total":17512,"input":17509,"output":3}}}"#;

        let parsed = runner().parse_response(raw).unwrap();
        assert_eq!(parsed.response.status, "done");
        assert_eq!(parsed.response.summary, "hello");
        assert_eq!(parsed.input_tokens, Some(17509));
        assert_eq!(parsed.output_tokens, Some(3));
    }

    #[test]
    fn parse_opencode_concatenated_text() {
        let raw = r#"{"type":"step_start","timestamp":1000,"part":{"type":"step-start"}}
{"type":"text","timestamp":1001,"part":{"type":"text","text":"Working on "}}
{"type":"text","timestamp":1002,"part":{"type":"text","text":"the task. "}}
{"type":"text","timestamp":1003,"part":{"type":"text","text":"{\"status\":\"done\",\"summary\":\"finished\",\"accomplished\":[],\"remaining\":[],\"files\":[]}"}}
{"type":"step_finish","timestamp":1004,"part":{"type":"step-finish","reason":"stop","tokens":{"input":100,"output":50}}}"#;

        let parsed = runner().parse_response(raw).unwrap();
        assert_eq!(parsed.response.status, "done");
        assert_eq!(parsed.input_tokens, Some(100));
        assert_eq!(parsed.output_tokens, Some(50));
    }

    #[test]
    fn parse_opencode_error_event() {
        let raw = r#"{"type":"error","message":"rate limit exceeded for this model"}"#;

        let err = runner().parse_response(raw).unwrap_err();
        assert!(matches!(err, AgentError::RateLimit { .. }));
    }

    #[test]
    fn parse_opencode_step_finish_error() {
        let raw = r#"{"type":"step_start","timestamp":1000,"part":{"type":"step-start"}}
{"type":"step_finish","timestamp":1001,"part":{"type":"step-finish","reason":"error","error":"context length exceeded"}}"#;

        let err = runner().parse_response(raw).unwrap_err();
        assert!(matches!(err, AgentError::ContextOverflow { .. }));
    }

    #[test]
    fn parse_opencode_empty() {
        let err = runner().parse_response("").unwrap_err();
        assert!(matches!(err, AgentError::InvalidResponse { .. }));
    }

    #[test]
    fn extract_tokens_from_step_finish() {
        let r = runner();
        let events: Vec<serde_json::Value> = vec![
            serde_json::json!({"type":"step_finish","part":{"type":"step-finish","tokens":{"input":1000,"output":500,"total":1500}}}),
        ];
        let (input, output) = r.extract_tokens(&events);
        assert_eq!(input, Some(1000));
        assert_eq!(output, Some(500));
    }

    #[test]
    fn classify_opencode_rate_limit() {
        let err = classify_opencode_message("429 Too Many Requests");
        assert!(matches!(err, AgentError::RateLimit { .. }));
    }

    #[test]
    fn classify_opencode_model_not_found() {
        let err = classify_opencode_message("model not found: gpt-5");
        assert!(matches!(err, AgentError::ModelUnavailable { .. }));
    }

    #[test]
    fn build_command_opencode() {
        let r = runner();
        let perms = PermissionRules::default();
        let cmd = r.build_command(
            Some("anthropic/claude-sonnet-4-20250514"),
            "timeout 1800",
            "/tmp/sys.txt",
            "/tmp/msg.txt",
            &perms,
        );
        assert!(cmd.contains("opencode run"));
        assert!(cmd.contains("--model anthropic/claude-sonnet-4-20250514"));
        assert!(cmd.contains("--format json"));
    }

    #[test]
    fn free_models_returns_known_defaults() {
        // When opencode isn't installed, should return known defaults
        let models = discover_free_models();
        assert!(!models.is_empty());
        // Should at least have the known free models
        assert!(models.iter().any(|m| m.contains("free")));
    }

    // ── Fixture-based tests ─────────────────────────────────────

    #[test]
    fn fixture_opencode_success() {
        let raw = include_str!("../../../../tests/fixtures/opencode_success.jsonl");
        let parsed = runner().parse_response(raw).unwrap();
        assert_eq!(parsed.response.status, "done");
        assert!(parsed.response.summary.contains("Fixed"));
        assert_eq!(parsed.response.accomplished.len(), 2);
        assert_eq!(parsed.input_tokens, Some(17500));
        assert_eq!(parsed.output_tokens, Some(500));
    }
}
