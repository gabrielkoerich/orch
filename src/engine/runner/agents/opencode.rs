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
use crate::cmd::SyncCommandErrorContext;
use crate::parser;

use std::sync::Mutex;

/// Runner for OpenCode agent.
pub struct OpenCodeRunner {
    /// Cached free models (model list + timestamp).
    free_models_cache: Mutex<Option<(Vec<String>, std::time::Instant)>>,
}

pub(crate) fn parse_ndjson_events(raw: &str) -> Vec<serde_json::Value> {
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

pub(crate) fn extract_ndjson_text(events: &[serde_json::Value]) -> Option<String> {
    let mut texts = Vec::new();

    for event in events {
        let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");

        if event_type == "text" {
            if let Some(part) = event.get("part") {
                if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                    texts.push(text.to_string());
                    continue;
                }
            }
            if let Some(text) = event.get("text").and_then(|v| v.as_str()) {
                texts.push(text.to_string());
            }
        }
    }

    if texts.is_empty() {
        return None;
    }

    let full = texts.join("");
    if let Some(json) = extract_json_object(&full) {
        return Some(json);
    }

    for text in texts.iter().rev() {
        let trimmed = text.trim();
        if let Some(json) = extract_json_object(trimmed) {
            if json.contains("status") || json.contains("executor") {
                return Some(json);
            }
        }
        if trimmed.contains('{')
            && (trimmed.contains("status")
                || trimmed.contains("executor")
                || trimmed.contains("complexity"))
        {
            return Some(text.clone());
        }
    }

    Some(full)
}

fn extract_json_object(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
        return Some(trimmed.to_string());
    }

    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    if end <= start {
        return None;
    }

    let candidate = trimmed[start..=end].trim();
    serde_json::from_str::<serde_json::Value>(candidate)
        .ok()
        .map(|_| candidate.to_string())
}

pub(crate) fn extract_router_text(raw: &str) -> Option<String> {
    let events = parse_ndjson_events(raw.trim());
    if events.is_empty() {
        return None;
    }
    extract_ndjson_text(&events)
}

impl OpenCodeRunner {
    pub fn new() -> Self {
        Self {
            free_models_cache: Mutex::new(None),
        }
    }

    /// Parse NDJSON stream into events.
    fn parse_ndjson(&self, raw: &str) -> Vec<serde_json::Value> {
        parse_ndjson_events(raw)
    }

    /// Extract text from `text` events.
    ///
    /// Concatenates all text events, then tries to find the structured
    /// JSON response. If the concatenated text doesn't parse as JSON,
    /// tries each text event individually (newest first) since earlier
    /// events are often progress messages.
    ///
    /// Handles two text event formats emitted by different opencode versions:
    /// - Format 1 (current): `{"type":"text","part":{"type":"text","text":"..."}}`
    /// - Format 2 (newer):   `{"type":"text","text":"..."}`
    fn extract_text_from_events(&self, events: &[serde_json::Value]) -> Option<String> {
        extract_ndjson_text(events)
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
                // OpenCode error events have multiple shapes:
                // 1. {"type":"error","message":"..."}
                // 2. {"type":"error","error":"string message"}
                // 3. {"type":"error","error":{"name":"...","data":{"message":"..."}}}
                let message = event
                    .get("message")
                    .and_then(|v| v.as_str())
                    .or_else(|| event.get("error").and_then(|v| v.as_str()))
                    .or_else(|| {
                        event
                            .get("error")
                            .and_then(|e| e.get("data"))
                            .and_then(|d| d.get("message"))
                            .and_then(|m| m.as_str())
                    })
                    .or_else(|| {
                        // Last resort: stringify the error object
                        event
                            .get("error")
                            .and_then(|e| e.get("name"))
                            .and_then(|n| n.as_str())
                    })
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
        let mut cache = self
            .free_models_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        // Check cache freshness (1 hour)
        if let Some((ref models, ref ts)) = *cache {
            if ts.elapsed() < std::time::Duration::from_secs(3600) {
                return models.clone();
            }
        }

        // Discover fresh using async-aware process invocation
        let models = discover_free_models();
        *cache = Some((models.clone(), std::time::Instant::now()));
        models
    }
}

impl AgentRunner for OpenCodeRunner {
    #[cfg(test)]
    fn name(&self) -> &str {
        "opencode"
    }

    fn extract_text(&self, raw: &str) -> Result<String, AgentError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(String::new());
        }

        let events = self.parse_ndjson(trimmed);
        if events.is_empty() {
            return Ok(trimmed.to_string());
        }

        // Propagate terminal errors so the review pipeline records cooldowns.
        if let Some(err) = self.detect_error(&events) {
            return Err(err);
        }

        // Concatenate all text events; fall back to raw output if none found.
        Ok(extract_ndjson_text(&events).unwrap_or_else(|| trimmed.to_string()))
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

        // OpenCode permission control via XDG_CONFIG_HOME override.
        //
        // We write our own opencode.json to .orch-opencode/opencode/opencode.json
        // and set XDG_CONFIG_HOME=.orch-opencode so opencode reads ONLY our config,
        // bypassing the user's global ~/.config/opencode/opencode.json (which has
        // "edit":"ask" and "external_directory":"ask" that block agent file edits
        // and git operations on the main repo's .git directory).
        let config_setup = if permissions.autonomous {
            let permission_json = translate_permissions_to_opencode(&permissions.allowed_tools);
            format!(
                r#"mkdir -p .orch-opencode/opencode || {{ printf '%s\n' 'failed to create opencode config directory: .orch-opencode/opencode' >&2; exit 1; }}
printf '%s\n' '{permission_json}' > .orch-opencode/opencode/opencode.json || {{ printf '%s\n' 'failed to write opencode config: .orch-opencode/opencode/opencode.json' >&2; exit 1; }}
"#
            )
        } else {
            String::new()
        };

        // When overriding XDG_CONFIG_HOME to isolate opencode's config, gh CLI
        // would also look in the override directory and fail to authenticate.
        // GH_CONFIG_DIR pins gh to its actual config regardless of XDG_CONFIG_HOME.
        let xdg_prefix = if permissions.autonomous {
            "XDG_CONFIG_HOME=.orch-opencode GH_CONFIG_DIR=$HOME/.config/gh "
        } else {
            ""
        };

        format!(
            r#"{config_setup}cat "{sys_file}" "{msg_file}" | {xdg_prefix}{timeout_cmd} opencode run {model_flag} \
  --format json -"#,
            config_setup = config_setup,
            xdg_prefix = xdg_prefix,
            sys_file = sys_file,
            msg_file = msg_file,
            timeout_cmd = timeout_cmd,
            model_flag = model_flag,
        )
    }

    fn parse_response(&self, raw: &str) -> Result<ParsedResponse, AgentError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(AgentError::InvalidResponse { raw: String::new() });
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
        let text =
            self.extract_text_from_events(&events)
                .ok_or_else(|| AgentError::InvalidResponse {
                    raw: trimmed.to_string(),
                })?;

        // Extract tokens
        let (input_tokens, output_tokens) = self.extract_tokens(&events);

        // Parse the text through standard parser
        let response =
            parser::parse(&text).map_err(|_| AgentError::InvalidResponse { raw: text.clone() })?;

        Ok(ParsedResponse {
            response,
            input_tokens,
            output_tokens,
            duration_ms: None,
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

    fn router_command(
        &self,
        prompt: &str,
        model: Option<&str>,
    ) -> anyhow::Result<tokio::process::Command> {
        let mut cmd = tokio::process::Command::new("opencode");
        cmd.arg("run").arg("--format").arg("json");
        if let Some(m) = model {
            cmd.arg("--model").arg(m);
        }
        cmd.arg(prompt);
        Ok(cmd)
    }
}

/// Map from unified allowed_tools names to OpenCode permission keys.
const TOOL_TO_OPENCODE: &[(&str, &str)] = &[
    ("Edit", "edit"),
    ("Write", "edit"),
    ("Read", "read"),
    ("Bash", "bash"),
    ("Grep", "grep"),
    ("Glob", "glob"),
    ("WebFetch", "webfetch"),
    ("WebSearch", "websearch"),
    ("Task", "task"),
    ("Skill", "skill"),
];

/// All OpenCode permission keys that can be controlled.
const OPENCODE_PERMISSION_KEYS: &[&str] = &[
    "read",
    "edit",
    "glob",
    "grep",
    "bash",
    "task",
    "skill",
    "webfetch",
    "websearch",
    "list",
    "todowrite",
    "todoread",
    "question",
    "codesearch",
    // Allow access to files outside the worktree (e.g. git's main repo .git dir
    // which is referenced by the worktree's .git symlink for fetch/push operations).
    "external_directory",
];

/// Translate allowed_tools into an OpenCode config JSON string.
///
/// Maps Claude tool names to OpenCode permission keys.
/// Tools in the allowed list get "allow", others get "deny".
/// CLI commands (git, npm, etc.) are covered by the "bash" permission.
pub(crate) fn translate_permissions_to_opencode(allowed_tools: &[String]) -> String {
    if allowed_tools.is_empty() {
        // No restrictions: allow everything
        return r#"{"permission":"allow"}"#.to_string();
    }

    // Collect which opencode keys should be "allow"
    let mut allowed_keys: Vec<&str> = Vec::new();
    for tool in allowed_tools {
        for (from, to) in TOOL_TO_OPENCODE {
            if tool == *from && !allowed_keys.contains(to) {
                allowed_keys.push(to);
            }
        }
        // CLI commands (lowercase) are all bash commands
        if tool
            .chars()
            .next()
            .map(|c| c.is_lowercase())
            .unwrap_or(false)
            && !allowed_keys.contains(&"bash")
        {
            allowed_keys.push("bash");
        }
    }

    // Build permission object
    let mut entries = Vec::new();
    for key in OPENCODE_PERMISSION_KEYS {
        let action = if *key == "external_directory" {
            // Always allow external directory access — agents need this for git
            // operations that touch the main repo's .git directory from a worktree.
            "allow"
        } else if allowed_keys.contains(key) {
            "allow"
        } else {
            "deny"
        };
        entries.push(format!(r#""{key}":"{action}""#));
    }

    format!(r#"{{"permission":{{{}}}}}"#, entries.join(","))
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
        };
    }

    if lower.contains("context") && (lower.contains("length") || lower.contains("overflow")) {
        return AgentError::ContextOverflow {
            message: message.to_string(),
        };
    }

    if lower.contains("unauthorized")
        || lower.contains("invalid key")
        || super::patterns::contains_http_status(&lower, "401")
    {
        return AgentError::Auth {
            message: message.to_string(),
        };
    }

    if lower.contains("rejected permission")
        || lower.contains("permission denied")
        || lower.contains("permissionerror")
    {
        return AgentError::PermissionDenied {
            message: message.to_string(),
        };
    }

    if lower.contains("model") && (lower.contains("not found") || lower.contains("not supported")) {
        // Try to extract model name from patterns like "Model not found: anthropic/claude-sonnet-4-6."
        let model = message
            .split(": ")
            .nth(1)
            .map(|s| s.trim_end_matches('.').to_string())
            .unwrap_or_default();
        return AgentError::ModelUnavailable {
            message: message.to_string(),
            model,
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

    // Try to discover dynamically using blocking I/O.
    // Note: this function is sync; callers in async contexts should wrap with
    // tokio::task::spawn_blocking() at the call site.
    let stdout = match std::process::Command::new("opencode")
        .args(["models"])
        .output_with_context()
    {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).to_string()
        }
        _ => return known,
    };
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

    /// Newer opencode versions emit text directly in the event (no "part" wrapper).
    /// The extract_text method must handle both formats.
    #[test]
    fn parse_opencode_ndjson_direct_text_format() {
        let raw = r#"{"type":"step_start","timestamp":1000,"sessionID":"ses_abc"}
{"type":"text","timestamp":1001,"sessionID":"ses_abc","text":"{\"status\":\"done\",\"summary\":\"fixed\",\"accomplished\":[],\"remaining\":[],\"files\":[]}"}
{"type":"step_finish","timestamp":1002,"sessionID":"ses_abc","part":{"type":"step-finish","reason":"stop","tokens":{"input":200,"output":10}}}"#;

        let parsed = runner().parse_response(raw).unwrap();
        assert_eq!(parsed.response.status, "done");
        assert_eq!(parsed.response.summary, "fixed");
        assert_eq!(parsed.input_tokens, Some(200));
        assert_eq!(parsed.output_tokens, Some(10));
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
    fn extract_router_text_prefers_json_payload() {
        let raw = r#"{"type":"text","timestamp":1001,"part":{"type":"text","text":"Thinking..."}}
{"type":"text","timestamp":1002,"part":{"type":"text","text":"{\"executor\":\"claude\",\"complexity\":\"medium\",\"reason\":\"fit\"}"}}"#;

        let text = extract_router_text(raw).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["executor"], "claude");
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
        assert!(cmd.contains("--model 'anthropic/claude-sonnet-4-20250514'"));
        assert!(cmd.contains("--format json"));
        // Autonomous mode should write permission config and set XDG_CONFIG_HOME
        assert!(
            cmd.contains("opencode.json"),
            "expected permission config setup, got: {cmd}"
        );
        assert!(
            cmd.contains("XDG_CONFIG_HOME=.orch-opencode"),
            "expected XDG_CONFIG_HOME override, got: {cmd}"
        );
        assert!(
            cmd.contains("GH_CONFIG_DIR=$HOME/.config/gh"),
            "expected GH_CONFIG_DIR to preserve gh auth, got: {cmd}"
        );
    }

    #[test]
    fn translate_permissions_with_allowed_tools() {
        let tools = vec![
            "Edit".to_string(),
            "Read".to_string(),
            "Bash".to_string(),
            "Grep".to_string(),
            "git".to_string(), // CLI command → "bash" key
        ];
        let json = translate_permissions_to_opencode(&tools);
        assert!(
            json.contains(r#""edit":"allow""#),
            "Edit should map to edit:allow"
        );
        assert!(
            json.contains(r#""read":"allow""#),
            "Read should map to read:allow"
        );
        assert!(
            json.contains(r#""bash":"allow""#),
            "Bash should map to bash:allow"
        );
        assert!(
            json.contains(r#""grep":"allow""#),
            "Grep should map to grep:allow"
        );
        // Tools not in allowed list should be denied
        assert!(
            json.contains(r#""webfetch":"deny""#),
            "webfetch should be deny"
        );
        assert!(
            json.contains(r#""websearch":"deny""#),
            "websearch should be deny"
        );
    }

    /// Test with the full real tool list from workflow.allowed_tools config.
    #[test]
    fn translate_permissions_full_config_list() {
        let tools: Vec<String> = vec![
            "Edit",
            "Write",
            "Read",
            "Bash",
            "Grep",
            "Glob",
            "WebFetch",
            "WebSearch",
            "Task",
            "Skill",
            "yq",
            "jq",
            "bash",
            "just",
            "git",
            "rg",
            "sed",
            "awk",
            "python3",
            "node",
            "npm",
            "bun",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        let json = translate_permissions_to_opencode(&tools);

        // All major permissions should be allowed
        for key in &[
            "edit",
            "read",
            "bash",
            "grep",
            "glob",
            "webfetch",
            "websearch",
            "task",
            "skill",
        ] {
            assert!(
                json.contains(&format!(r#""{key}":"allow""#)),
                "{key} should be allow, got: {json}"
            );
        }
    }

    #[test]
    fn translate_permissions_empty_allows_all() {
        let json = translate_permissions_to_opencode(&[]);
        assert_eq!(json, r#"{"permission":"allow"}"#);
    }

    #[test]
    fn classify_opencode_permission_denied() {
        let err = classify_opencode_message("user rejected permission for tool bash: cargo test");
        assert!(
            matches!(err, AgentError::PermissionDenied { .. }),
            "expected PermissionDenied, got: {err:?}"
        );
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

    /// Real failure: OpenCode rejects a tool permission in non-interactive mode.
    /// The error has `{"error":{"name":"PermissionError","data":{"message":"user rejected permission..."}}}`.
    /// Parser must extract the nested message and classify as PermissionDenied.
    #[test]
    fn fixture_opencode_permission_denied() {
        let raw = include_str!("../../../../tests/fixtures/opencode_permission_denied.jsonl");
        let err = runner().parse_response(raw).unwrap_err();
        assert!(
            matches!(err, AgentError::PermissionDenied { .. }),
            "expected PermissionDenied, got: {err:?}"
        );
    }

    /// Real failure: OpenCode rate limit via top-level message field.
    #[test]
    fn fixture_opencode_rate_limit() {
        let raw = include_str!("../../../../tests/fixtures/opencode_rate_limit.jsonl");
        let err = runner().parse_response(raw).unwrap_err();
        assert!(
            matches!(err, AgentError::RateLimit { .. }),
            "expected RateLimit, got: {err:?}"
        );
    }

    /// Real failure: OpenCode returns nested error for unknown model.
    /// The error object is `{"name":"UnknownError","data":{"message":"Model not found: X"}}`.
    /// Parser must extract the nested message and classify as ModelUnavailable.
    #[test]
    fn fixture_opencode_model_not_found() {
        let raw = include_str!("../../../../tests/fixtures/opencode_model_not_found.jsonl");
        let err = runner().parse_response(raw).unwrap_err();
        assert!(
            matches!(err, AgentError::ModelUnavailable { .. }),
            "expected ModelUnavailable, got: {err:?}"
        );
        if let AgentError::ModelUnavailable { model, .. } = &err {
            assert_eq!(model, "anthropic/claude-sonnet-4-6");
        }
    }

    // ── extract_text ─────────────────────────────────────────────

    /// OpenCode: extract_text concatenates text events for review parsing.
    #[test]
    fn extract_text_returns_text_events() {
        let raw = concat!(
            r#"{"type":"step_start","timestamp":1000,"sessionID":"ses_abc"}"#,
            "\n",
            r#"{"type":"text","timestamp":1001,"part":{"type":"text","text":"{\"decision\":\"approve\",\"notes\":\"LGTM\",\"test_results\":\"pass\",\"issues\":[]}"}}"#,
            "\n",
            r#"{"type":"step_finish","timestamp":1002,"part":{"type":"step-finish","reason":"stop","cost":0,"tokens":{"total":100,"input":90,"output":10}}}"#,
        );
        let text = runner().extract_text(raw).unwrap();
        assert_eq!(
            text,
            r#"{"decision":"approve","notes":"LGTM","test_results":"pass","issues":[]}"#
        );
    }

    /// OpenCode: extract_text with newer direct-text format.
    #[test]
    fn extract_text_direct_text_event_format() {
        let raw = concat!(
            r#"{"type":"step_start","timestamp":1000,"sessionID":"ses_abc"}"#,
            "\n",
            r#"{"type":"text","timestamp":1001,"sessionID":"ses_abc","text":"Here is my review:\n\n```json\n{\"decision\":\"request_changes\",\"notes\":\"Fix the bug\",\"test_results\":\"fail\",\"issues\":[]}\n```\n"}"#,
            "\n",
            r#"{"type":"step_finish","timestamp":1002,"sessionID":"ses_abc"}"#,
        );
        let text = runner().extract_text(raw).unwrap();
        assert!(
            text.contains("request_changes"),
            "expected review JSON, got: {text}"
        );
    }

    /// OpenCode: extract_text propagates RateLimit for terminal errors.
    #[test]
    fn extract_text_rate_limit_propagates() {
        let raw = concat!(
            r#"{"type":"step_start","timestamp":1000}"#,
            "\n",
            r#"{"type":"error","message":"rate limit exceeded: 429 Too Many Requests"}"#,
        );
        let err = runner().extract_text(raw).unwrap_err();
        assert!(
            matches!(err, AgentError::RateLimit { .. }),
            "expected RateLimit, got: {err:?}"
        );
    }

    /// OpenCode: extract_text propagates Auth error.
    #[test]
    fn extract_text_auth_error_propagates() {
        let raw = concat!(
            r#"{"type":"step_start","timestamp":1000}"#,
            "\n",
            r#"{"type":"error","message":"401 Unauthorized: invalid api key"}"#,
        );
        let err = runner().extract_text(raw).unwrap_err();
        assert!(
            matches!(err, AgentError::Auth { .. }),
            "expected Auth, got: {err:?}"
        );
    }

    /// OpenCode: extract_text on empty input returns empty string (not an error).
    #[test]
    fn extract_text_empty_input_returns_empty_ok() {
        let text = runner().extract_text("").unwrap();
        assert!(text.is_empty());
    }
}
