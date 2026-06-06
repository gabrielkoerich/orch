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
//!   --output-format stream-json \
//!   --disallowedTools '...' \
//!   --append-system-prompt "{sys_file}" \
//!   < "{msg_file}"
//! ```
//!
//! ## Output format (`--output-format stream-json`)
//!
//! NDJSON event stream; the final event is always `"type": "result"`:
//! ```jsonl
//! {"type":"system","subtype":"init",...}
//! {"type":"assistant","message":{...}}
//! {"type":"tool_use","tool":{...}}
//! {"type":"tool_result","result":{...}}
//! {"type":"result","subtype":"success","is_error":false,"duration_ms":2006,"result":"...","usage":{"input_tokens":10,"output_tokens":78},"modelUsage":{...}}
//! ```
//!
//! Backward compatible with `--output-format json` (single JSON blob) — the
//! parser finds the last `"type":"result"` line whether it is NDJSON or a
//! pretty-printed single object.
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
    /// Extracts `result`, `usage`, and `is_error`.
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
            // Check stale session first — it requires a targeted recovery action
            // (reset UUID + retry) that is distinct from generic agent failures.
            if let Some(e) = super::patterns::detect_stale_session(result_text) {
                return Err(e);
            }
            // Check thinking block conflict before auth — it's a specific 400
            // that must not be counted as a model failure.
            if let Some(e) = super::patterns::detect_thinking_block_conflict(result_text) {
                return Err(e);
            }
            // Check auth before rate_limit — billing errors (credit balance)
            // are auth issues, not transient rate limits.
            // Use result_text only (not combined with raw JSON) so the error
            // message stays clean — the raw NDJSON stats blob must not leak
            // into the logged reason string.
            if let Some(e) = super::patterns::detect_auth_error(result_text) {
                return Err(e);
            }
            if let Some(e) = super::patterns::detect_rate_limit(result_text) {
                return Err(e);
            }
            if let Some(e) = super::patterns::detect_context_overflow(result_text) {
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

        // Parse the inner result text into AgentResponse.
        // Do NOT check for rate limits here — result_text is agent work product
        // (summary, code descriptions) when is_error=false. Rate-limit keywords
        // in work product cause false positives (see issue #1292).
        let response = match parser::parse(result_text) {
            Ok(r) => r,
            Err(_) => {
                // Structured parse failed — try plain-text synthesis.
                // Preserve already-extracted tokens rather than discarding them (issue #1387).
                super::synthesize_response_from_text(result_text).ok_or_else(|| {
                    AgentError::InvalidResponse {
                        raw: result_text.to_string(),
                    }
                })?
            }
        };

        Ok(ParsedResponse {
            response,
            input_tokens,
            output_tokens,
            duration_ms,
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
        })
    }
}

/// Helper: extract error from a JSON object if it is a `type:result` envelope
/// with `is_error=true`. Uses pattern detection for proper error classification
/// (auth, rate limit, context overflow) rather than always returning AgentFailed.
fn extract_error_from_envelope(
    obj: &serde_json::Map<String, serde_json::Value>,
    _raw: &str,
) -> Option<AgentError> {
    if obj.get("type").and_then(|v| v.as_str()) == Some("result")
        && obj
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    {
        let result_text = obj.get("result").and_then(|v| v.as_str()).unwrap_or("");
        // Use result_text only (not combined with raw JSON) so the error
        // message stays clean — the raw NDJSON stats blob must not leak
        // into the logged reason string.
        if let Some(e) = super::patterns::detect_thinking_block_conflict(result_text) {
            return Some(e);
        }
        if let Some(e) = super::patterns::detect_auth_error(result_text) {
            return Some(e);
        }
        if let Some(e) = super::patterns::detect_rate_limit(result_text) {
            return Some(e);
        }
        if let Some(e) = super::patterns::detect_context_overflow(result_text) {
            return Some(e);
        }
        return Some(AgentError::AgentFailed {
            message: result_text.to_string(),
        });
    }
    None
}

/// Helper: scan NDJSON lines in `text` for a `type:result` envelope with
/// `is_error=true` and extract the error message. Returns `None` if no error
/// envelope is found.
fn detect_error_envelope(text: &str) -> Option<AgentError> {
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(obj) = val.as_object() {
                if let Some(err) = extract_error_from_envelope(obj, line) {
                    return Some(err);
                }
            }
        }
    }
    None
}

pub(crate) fn extract_stream_json_result_text(raw: &str) -> Result<String, AgentError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(AgentError::InvalidResponse { raw: String::new() });
    }

    // Prefer the final result envelope whose result field is valid JSON.
    // This skips outer-session plain-text summaries (issue #1387).
    let envelope_line_opt = find_ndjson_result_line(trimmed).map(|(line, _)| line);

    if let Some(line) = envelope_line_opt {
        // Parse the found result line.
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(obj) = parsed.as_object() {
                // Check is_error FIRST.
                if let Some(err) = extract_error_from_envelope(obj, line) {
                    return Err(err);
                }
                // Extract result text from the JSON object.
                if let Some(result) = obj.get("result") {
                    if let Some(text) = result.as_str() {
                        return Ok(text.to_string());
                    }
                    if result.is_object() || result.is_array() {
                        return Ok(result.to_string());
                    }
                }
            }
        }
        // Not an error, but result field is not extractable: fall through to fallback.
    }

    // No JSON result line found (or it wasn't an error). Try opencode-style
    // text extraction which scans NDJSON for text/result events.
    if let Some(fallback) = crate::engine::runner::agents::opencode::extract_router_text(trimmed) {
        return Ok(fallback);
    }

    // No opencode-style text found. Try parsing the whole trimmed string as a
    // single JSON object (handles `--output-format json` plain output).
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(obj) = parsed.as_object() {
            // Check for error envelope in the single-line JSON.
            if let Some(err) = extract_error_from_envelope(obj, trimmed) {
                return Err(err);
            }
            // Extract result text from the JSON object.
            if let Some(result) = obj.get("result") {
                if let Some(text) = result.as_str() {
                    return Ok(text.to_string());
                }
                if result.is_object() || result.is_array() {
                    return Ok(result.to_string());
                }
            }
            return Ok(trimmed.to_string());
        }
    }

    // Not valid JSON at all. Before returning raw text, scan for error envelopes
    // (handles the case where find_ndjson_result_line skipped error-only output).
    if let Some(err) = detect_error_envelope(trimmed) {
        return Err(err);
    }

    Ok(trimmed.to_string())
}

impl AgentRunner for ClaudeRunner {
    #[cfg(test)]
    fn name(&self) -> &str {
        &self.binary
    }

    fn extract_text(&self, raw: &str) -> Result<String, super::AgentError> {
        // Empty input: return empty string (not an error), matching historical behavior.
        if raw.trim().is_empty() {
            return Ok(String::new());
        }
        extract_stream_json_result_text(raw)
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

        // Claude permission mode: autonomous → bypassPermissions, supervised → acceptEdits
        let permission_mode = if permissions.autonomous {
            "bypassPermissions"
        } else {
            "acceptEdits"
        };

        // Build tool restriction flags.
        // If allowed_tools is set (whitelist), use --allowedTools and ignore disallowed_tools.
        // Otherwise fall back to --disallowedTools (deny list).
        let tool_flag = if !permissions.allowed_tools.is_empty() {
            let tools = translate_allowed_tools(
                &permissions.allowed_tools,
                &permissions.allowed_edit_paths,
            );
            format!("--allowedTools '{}'", tools.join(","))
        } else if !permissions.disallowed_tools.is_empty() {
            format!(
                "--disallowedTools '{}'",
                permissions.disallowed_tools.join(",")
            )
        } else {
            String::new()
        };

        format!(
            r#"{timeout_cmd} {binary} -p --verbose {model_flag} \
  --permission-mode {permission_mode} \
  --output-format stream-json \
  {tool_flag} \
  --append-system-prompt "{sys_file}" \
  < "{msg_file}""#,
            timeout_cmd = timeout_cmd,
            binary = self.binary,
            model_flag = model_flag,
            permission_mode = permission_mode,
            tool_flag = tool_flag,
            sys_file = sys_file,
            msg_file = msg_file,
        )
    }

    fn parse_response(&self, raw: &str) -> Result<ParsedResponse, AgentError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(AgentError::InvalidResponse { raw: String::new() });
        }

        // For NDJSON (--output-format stream-json): find the best result line and its
        // token count. For single-line JSON (--output-format json), returns None.
        if let Some((result_line, tokens)) = find_ndjson_result_line(trimmed) {
            let result = self.parse_envelope(result_line)?;
            // If the envelope parse preserved meaningful tokens, use those.
            // Otherwise, fall back to the tokens from find_ndjson_result_line
            // (which selected the best result line by token count — issue #1387).
            let input_tokens = result.input_tokens.or(tokens.map(|(i, _)| i));
            let output_tokens = result.output_tokens.or(tokens.map(|(_, o)| o));
            if input_tokens != result.input_tokens || output_tokens != result.output_tokens {
                return Ok(ParsedResponse {
                    input_tokens,
                    output_tokens,
                    ..result
                });
            }
            return Ok(result);
        }

        // Fallback: treat the whole string as the envelope (e.g. pretty-printed JSON)
        self.parse_envelope(trimmed)
    }

    fn classify_error(&self, exit_code: i32, stdout: &str, stderr: &str) -> AgentError {
        // For claude-protocol agents the real error string lives in the `result`
        // field of the NDJSON result line (when is_error=true). Prepend it to the
        // combined text so the classifier sees it before the raw metadata tail.
        let error_result: Option<String> = (|| {
            let trimmed = stdout.trim();
            let (line, _) = find_ndjson_result_line(trimmed)?;
            let val: serde_json::Value = serde_json::from_str(line).ok()?;
            let obj = val.as_object()?;
            if obj.get("is_error").and_then(|v| v.as_bool()) != Some(true) {
                return None;
            }
            let result_str = obj.get("result")?.as_str()?;
            Some(result_str.to_string())
        })();

        // Also check for system-level error events (e.g., session limit mid-stream).
        // These appear as `type:system` NDJSON events with an error object, not as
        // `type:result` lines with `is_error:true`. This catches errors like session
        // limits, credential issues, and API errors that interrupt the agent mid-run.
        //
        // Example:
        //   {"type":"system","subtype":"error","error":{"type":"session_limit","message":"..."}}
        let system_error: Option<String> = (|| {
            let trimmed = stdout.trim();
            for line in trimmed.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                    if let Some(obj) = val.as_object() {
                        if obj.get("type").and_then(|t| t.as_str()) == Some("system") {
                            if let Some(error) = obj.get("error") {
                                if let Some(msg) = error.get("message").and_then(|m| m.as_str()) {
                                    return Some(msg.to_string());
                                }
                            }
                        }
                    }
                }
            }
            None
        })();

        let combined = match (&error_result, &system_error) {
            (Some(r), Some(s)) => format!("{s}\n{r}\n{stdout}\n{stderr}"),
            (Some(r), None) => format!("{r}\n{stdout}\n{stderr}"),
            (None, Some(s)) => format!("{s}\n{stdout}\n{stderr}"),
            (None, None) => format!("{stdout}\n{stderr}"),
        };
        super::patterns::classify_from_text(exit_code, &combined)
    }

    fn router_command(
        &self,
        prompt: &str,
        model: Option<&str>,
    ) -> anyhow::Result<tokio::process::Command> {
        let mut cmd = tokio::process::Command::new(&self.binary);
        cmd.env_remove("CLAUDECODE"); // allow nested invocation
        cmd.arg("--output-format")
            .arg("stream-json")
            .arg("--verbose")
            .arg("--print");
        if let Some(m) = model {
            cmd.arg("--model").arg(m);
        }
        cmd.arg(prompt);
        Ok(cmd)
    }
}

/// Known Claude Code native tool names.
/// These are passed as-is to `--allowedTools`.
const CLAUDE_NATIVE_TOOLS: &[&str] = &[
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
    "NotebookEdit",
    "EnterPlanMode",
    "ExitPlanMode",
    "EnterWorktree",
    "AskUserQuestion",
    "TodoWrite",
    "TodoRead",
];

/// Translate a unified allowed_tools list into Claude `--allowedTools` patterns.
///
/// - Items matching known Claude tool names pass through as-is
/// - Edit/Write are scoped to `allowed_edit_paths` when set
/// - Other items are treated as CLI commands and become `Bash(<cmd> *)` patterns
fn translate_allowed_tools(
    allowed: &[String],
    allowed_edit_paths: &[std::path::PathBuf],
) -> Vec<String> {
    let mut tools = Vec::new();
    for item in allowed {
        if (item == "Edit" || item == "Write") && !allowed_edit_paths.is_empty() {
            // Scope Edit/Write to allowed paths only
            for path in allowed_edit_paths {
                tools.push(format!("{}({}/*)", item, path.display()));
            }
        } else if CLAUDE_NATIVE_TOOLS.contains(&item.as_str()) {
            tools.push(item.clone());
        } else {
            // CLI command → Bash(<cmd> *) pattern
            tools.push(format!("Bash({item} *)"));
        }
    }
    tools
}

/// Find the best NDJSON result line and its token usage.
///
/// Returns `(line, tokens)` where `tokens` is `Some((input, output))` or `None`
/// if no usage data was found.
///
/// Selection priority:
/// 1. JSON result fields (object/array or string that parses as JSON) — always preferred
/// 2. Among plain-text results: the one with the most total tokens (preferring actual
///    work over outer-session confirmation summaries)
/// 3. Error results (is_error=true) — included even with no tokens, so the caller can
///    classify them properly
///
/// This skips outer-session plain-text summaries in favor of inner task results
/// with meaningful token counts (issue #1387).
pub(crate) fn find_ndjson_result_line(text: &str) -> Option<(&str, Option<(u64, u64)>)> {
    let lines: Vec<_> = text.lines().filter(|l| !l.trim().is_empty()).collect();

    // Pass 1: find a result line with a JSON result field.
    // JSON results are always preferred (inner task delegate output).
    for line in lines.iter().rev() {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(obj) = val.as_object() {
                if obj.get("type").and_then(|t| t.as_str()) == Some("result") {
                    if let Some(result) = obj.get("result") {
                        let is_json_result = match result {
                            serde_json::Value::Object(_) | serde_json::Value::Array(_) => true,
                            serde_json::Value::String(s) => {
                                serde_json::from_str::<serde_json::Value>(s).is_ok()
                            }
                            _ => false,
                        };
                        if is_json_result {
                            let tokens = extract_result_tokens(obj);
                            return Some((line, tokens));
                        }
                    }
                }
            }
        }
    }

    // Pass 2: no JSON result found — choose the plain-text result with the most tokens.
    // This handles outer-session confirmation summaries (e.g. "Already retrieved the result")
    // vs. actual work summaries: prefer the one with more tokens.
    // Error results (is_error=true) are included even with zero tokens, so the caller
    // can classify them properly.
    let mut best_line: Option<&str> = None;
    let mut best_total = 0u64;

    for line in &lines {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(obj) = val.as_object() {
                if obj.get("type").and_then(|t| t.as_str()) == Some("result") {
                    let tokens = extract_result_tokens(obj);
                    let total = tokens.map(|(i, o)| i.saturating_add(o)).unwrap_or(0);
                    if total >= best_total {
                        best_total = total;
                        best_line = Some(*line);
                    }
                }
            }
        }
    }

    best_line.map(|line| (line, extract_result_tokens_from_line(line)))
}

/// Extract input/output token counts from a parsed result envelope object.
fn extract_result_tokens(obj: &serde_json::Map<String, serde_json::Value>) -> Option<(u64, u64)> {
    // Try top-level `usage` first.
    if let Some(usage) = obj.get("usage").and_then(|v| v.as_object()) {
        let input = usage.get("input_tokens").and_then(|v| v.as_u64());
        let output = usage.get("output_tokens").and_then(|v| v.as_u64());
        if let (Some(i), Some(o)) = (input, output) {
            return Some((i, o));
        }
    }
    // Try modelUsage (aggregated across models).
    if let Some(model_usage) = obj.get("modelUsage").and_then(|v| v.as_object()) {
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
            return Some((total_input, total_output));
        }
    }
    None
}

/// Extract token counts from a raw NDJSON result line string.
fn extract_result_tokens_from_line(line: &str) -> Option<(u64, u64)> {
    let val = serde_json::from_str::<serde_json::Value>(line).ok()?;
    let obj = val.as_object()?;
    extract_result_tokens(obj)
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

/// Extract a structured `AgentResult` from Claude/Kimi/MiniMax NDJSON output.
///
/// Finds the last `"type":"result"` line whose `result` field contains valid JSON.
/// This skips outer-session plain-text summaries (issue #1387) and finds the
/// actual task-delegate result with structured JSON.
///
/// Extracts:
/// - `result` text (the inner content)
/// - `is_error` flag
/// - token usage from `usage` or `modelUsage`
/// - `duration_ms`
///
/// If no JSON result line exists (common with MiniMax), falls back to
/// extracting the last assistant text message from the stream.
///
/// Returns `None` only if the input contains no recognizable NDJSON events.
pub fn find_claude_result(ndjson: &str) -> Option<super::AgentResult> {
    let trimmed = ndjson.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Try to find the result envelope first
    if let Some((result_line, _tokens)) = find_ndjson_result_line(trimmed) {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(result_line) {
            if let Some(envelope) = parsed.as_object() {
                let is_error = envelope
                    .get("is_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                let result_text = envelope
                    .get("result")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                let (input_tokens, output_tokens) = extract_usage(envelope);
                let duration_ms = envelope.get("duration_ms").and_then(|v| v.as_u64());

                let cost_usd = envelope
                    .get("costUSD")
                    .or_else(|| envelope.get("cost_usd"))
                    .and_then(|v| v.as_f64());

                return Some(super::AgentResult {
                    is_error,
                    result_text,
                    input_tokens,
                    output_tokens,
                    cost_usd,
                    duration_ms,
                });
            }
        }
    }

    // Fallback for MiniMax/Kimi: extract last assistant text message
    let mut last_text = None;
    let mut input_tokens = None;
    let mut output_tokens = None;

    for line in trimmed.lines() {
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
        // Extract text from content array
        if let Some(content) = val
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        {
            for item in content {
                if item.get("type").and_then(|v| v.as_str()) == Some("text") {
                    if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                        let t = text.trim();
                        if !t.is_empty() {
                            last_text = Some(t.to_string());
                        }
                    }
                }
            }
        }
        // Extract usage (take max, as Claude reports cumulative)
        if let Some(usage) = val.get("message").and_then(|m| m.get("usage")) {
            if let Some(inp) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
                input_tokens = Some(input_tokens.unwrap_or(0u64).max(inp));
            }
            if let Some(out) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                output_tokens = Some(output_tokens.unwrap_or(0u64).max(out));
            }
        }
    }

    last_text.map(|text| super::AgentResult {
        is_error: false,
        result_text: text,
        input_tokens,
        output_tokens,
        cost_usd: None,
        duration_ms: None,
    })
}

/// Concatenate text content from all `type:assistant` NDJSON messages in
/// the stream.  Used as a fallback when the `type:result` envelope text
/// does not contain valid AgentResponse JSON but an earlier assistant turn
/// might have emitted it.
pub fn collect_assistant_messages_text(ndjson: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    for line in ndjson.trim().lines() {
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
        if let Some(content) = val
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        {
            for item in content {
                if item.get("type").and_then(|v| v.as_str()) == Some("text") {
                    if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                        let t = text.trim();
                        if !t.is_empty() {
                            parts.push(t.to_string());
                        }
                    }
                }
            }
        }
    }
    parts.join("\n")
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
    fn parse_envelope_no_false_positive_from_work_product_result() {
        // Regression test for issue #1292: when is_error=false but the result
        // text contains rate-limit keywords (agent describing work it did),
        // parse_response must NOT return a RateLimit error.
        //
        // Fix for issue #1377: when parser::parse fails on plain-text output,
        // synthesize_response_from_text is tried and tokens are preserved.
        let raw = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":1000,"result":"Added detect_rate_limit and quota exceeded patterns","usage":{"input_tokens":10,"output_tokens":5}}"#.to_string();
        // Plain-text result with rate-limit keywords: parser::parse fails, but
        // synthesis succeeds — result must be Ok (tokens preserved) not RateLimit.
        let parsed = runner()
            .parse_response(&raw)
            .expect("plain-text work product must synthesize successfully");
        // Tokens must be preserved from the envelope (issue #1377)
        assert_eq!(
            parsed.input_tokens,
            Some(10),
            "input_tokens must be preserved"
        );
        assert_eq!(
            parsed.output_tokens,
            Some(5),
            "output_tokens must be preserved"
        );
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
    fn classify_error_detects_session_limit_from_mid_stream_system_event() {
        // Regression test for issue #3272: Claude session limit errors appear as
        // `type:system` NDJSON events mid-stream, not as `type:result` lines with
        // `is_error:true`. When significant work product exists before and after
        // the system event, it is outside both head and tail scan windows in
        // `classify_from_text`. The `classify_error` function must extract the
        // error message from the system event and prepend it to the combined text.
        let header_events = "{\"type\":\"conversation\",\"subtype\":\"init\"}\n\
             {\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"I'll work on this task.\"}]}}\n"
            .repeat(80);
        let system_event = r#"{"type":"system","subtype":"error","error":{"type":"session_limit","message":"You've hit your session limit · resets 4:10am (America/Sao_Paulo)"}}"#;
        let work_product = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Here is the implementation...\"}]}}\n"
            .repeat(80);
        let stdout = format!("{header_events}{system_event}\n{work_product}");
        // Verify the system event is deep enough to miss head and tail scans.
        assert!(
            stdout.len() > 6000,
            "test stdout must exceed combined head+tail windows, got {} bytes",
            stdout.len()
        );
        let err = runner().classify_error(1, &stdout, "");
        assert!(
            matches!(err, AgentError::RateLimit { .. }),
            "session limit system event mid-stream must be detected, got: {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("session limit"),
            "error message should contain the rate limit reason, got: {msg}"
        );
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
            allowed_tools: vec![],
            allowed_edit_paths: vec![],
            deny_read_only: false,
            extra_writable_dirs: vec![],
        };
        let cmd = r.build_command(
            Some("opus"),
            "timeout 1800",
            "/tmp/sys.txt",
            "/tmp/msg.txt",
            &perms,
        );
        assert!(cmd.contains("--model 'opus'"));
        assert!(cmd.contains("claude -p"));
        assert!(cmd.contains("--output-format stream-json"));
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
            allowed_tools: vec![],
            allowed_edit_paths: vec![],
            deny_read_only: false,
            extra_writable_dirs: vec![],
        };
        let cmd = r.build_command(None, "", "/tmp/sys.txt", "/tmp/msg.txt", &perms);
        assert!(cmd.contains("--permission-mode acceptEdits"));
    }

    #[test]
    fn build_command_allowed_edit_paths_scopes_edit_write() {
        let r = runner();
        let perms = PermissionRules {
            autonomous: true,
            sandbox: SandboxLevel::WorkspaceWrite,
            disallowed_tools: vec![],
            allowed_tools: vec![
                "Edit".to_string(),
                "Write".to_string(),
                "Read".to_string(),
                "Bash".to_string(),
            ],
            allowed_edit_paths: vec![std::path::PathBuf::from("/home/user/worktree")],
            deny_read_only: false,
            extra_writable_dirs: vec![],
        };
        let cmd = r.build_command(None, "", "/tmp/sys.txt", "/tmp/msg.txt", &perms);

        // Edit/Write should be scoped to the worktree path
        assert!(
            cmd.contains("Edit(/home/user/worktree/*)"),
            "expected scoped Edit, got: {cmd}"
        );
        assert!(
            cmd.contains("Write(/home/user/worktree/*)"),
            "expected scoped Write, got: {cmd}"
        );

        // Read and Bash should not be scoped
        assert!(
            cmd.contains(",Read,") || cmd.contains("'Read,") || cmd.contains(",Read'"),
            "expected unrestricted Read, got: {cmd}"
        );
        assert!(
            cmd.contains(",Bash,") || cmd.contains("'Bash,") || cmd.contains(",Bash'"),
            "expected unrestricted Bash, got: {cmd}"
        );
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

    // ── NDJSON / stream-json fixtures ────────────────────────────

    #[test]
    fn fixture_claude_stream_success() {
        let raw = include_str!("../../../../tests/fixtures/claude_stream_success.jsonl");
        let parsed = runner().parse_response(raw).unwrap();
        assert_eq!(parsed.response.status, "done");
        assert!(parsed.response.summary.contains("Implemented"));
        assert_eq!(parsed.response.accomplished.len(), 2);
        assert_eq!(parsed.input_tokens, Some(15000));
        assert_eq!(parsed.output_tokens, Some(3500));
        assert_eq!(parsed.duration_ms, Some(45000));
    }

    #[test]
    fn fixture_claude_stream_error() {
        let raw = include_str!("../../../../tests/fixtures/claude_stream_error.jsonl");
        let err = runner().parse_response(raw).unwrap_err();
        assert!(matches!(err, AgentError::Auth { .. }), "got: {err:?}");
    }

    #[test]
    fn fixture_claude_stream_rate_limit() {
        let raw = include_str!("../../../../tests/fixtures/claude_stream_rate_limit.jsonl");
        let err = runner().parse_response(raw).unwrap_err();
        assert!(matches!(err, AgentError::RateLimit { .. }), "got: {err:?}");
    }

    #[test]
    fn fixture_claude_stream_thinking_block_conflict() {
        let raw =
            include_str!("../../../../tests/fixtures/claude_stream_thinking_block_conflict.jsonl");
        let err = runner().parse_response(raw).unwrap_err();
        assert!(
            matches!(err, AgentError::ThinkingBlockConflict { .. }),
            "expected ThinkingBlockConflict, got: {err:?}"
        );
    }

    #[test]
    fn classify_error_thinking_block_conflict_from_stderr() {
        // The error can also surface through classify_error() when the claude
        // process exits non-zero with the message in stdout/stderr.
        let stderr = "claude failed: API Error: 400 messages.1.content.3: `thinking` or `redacted_thinking` blocks in the latest assistant message cannot be modified. These blocks must remain as they were in the original response.";
        let err = runner().classify_error(1, "", stderr);
        assert!(
            matches!(err, AgentError::ThinkingBlockConflict { .. }),
            "expected ThinkingBlockConflict, got: {err:?}"
        );
    }

    #[test]
    fn thinking_block_conflict_does_not_match_unrelated_text() {
        // Ensure the pattern is specific enough to not false-positive on
        // work product that mentions "thinking" or "blocks".
        let unrelated = "I was thinking about how to handle blocks of code in the response.";
        let err = runner().classify_error(1, "", unrelated);
        assert!(
            !matches!(err, AgentError::ThinkingBlockConflict { .. }),
            "should not classify unrelated text as ThinkingBlockConflict, got: {err:?}"
        );
    }

    #[test]
    fn parse_ndjson_with_intermediate_events() {
        // Parser must ignore intermediate events and only look at the result line
        let raw = concat!(
            r#"{"type":"system","subtype":"init"}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[]}}"#,
            "\n",
            r#"{"type":"tool_use","tool":{"name":"Read","input":{}}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":1000,"result":"{\"status\":\"done\",\"summary\":\"streamed\",\"accomplished\":[],\"remaining\":[],\"files\":[]}","usage":{"input_tokens":5,"output_tokens":10}}"#,
            "\n"
        );
        let parsed = runner().parse_response(raw).unwrap();
        assert_eq!(parsed.response.status, "done");
        assert_eq!(parsed.response.summary, "streamed");
        assert_eq!(parsed.input_tokens, Some(5));
        assert_eq!(parsed.output_tokens, Some(10));
    }

    #[test]
    fn parse_ndjson_skips_malformed_lines() {
        // Malformed intermediate lines should be ignored; result line still parsed
        let raw = concat!(
            "not valid json\n",
            r#"{"type":"partial_update","data":"something"}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":500,"result":"{\"status\":\"done\",\"summary\":\"ok\",\"accomplished\":[],\"remaining\":[],\"files\":[]}","usage":{"input_tokens":1,"output_tokens":2}}"#,
            "\n"
        );
        let parsed = runner().parse_response(raw).unwrap();
        assert_eq!(parsed.response.status, "done");
        assert_eq!(parsed.response.summary, "ok");
    }

    #[test]
    fn parse_single_line_json_backward_compat() {
        // Single-line JSON (--output-format json) still works
        let raw = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":2000,"result":"{\"status\":\"done\",\"summary\":\"backward compat\",\"accomplished\":[],\"remaining\":[],\"files\":[]}","usage":{"input_tokens":50,"output_tokens":25}}"#;
        let parsed = runner().parse_response(raw).unwrap();
        assert_eq!(parsed.response.status, "done");
        assert_eq!(parsed.response.summary, "backward compat");
        assert_eq!(parsed.input_tokens, Some(50));
        assert_eq!(parsed.output_tokens, Some(25));
    }

    // ── Kimi / MiniMax (Claude-compatible wrappers) ───────────────

    #[test]
    fn kimi_parses_claude_envelope() {
        let r = ClaudeRunner::new("kimi");
        let raw = r#"{
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "duration_ms": 3000,
            "result": "{\"status\":\"done\",\"summary\":\"kimi did it\",\"accomplished\":[\"task\"],\"remaining\":[],\"files\":[\"a.rs\"]}",
            "usage": {"input_tokens": 200, "output_tokens": 100}
        }"#;

        let parsed = r.parse_response(raw).unwrap();
        assert_eq!(parsed.response.status, "done");
        assert_eq!(parsed.response.summary, "kimi did it");
    }

    #[test]
    fn minimax_parses_claude_envelope() {
        let r = ClaudeRunner::new("minimax");
        assert_eq!(r.name(), "minimax");
        let raw = r#"{
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": "{\"status\":\"done\",\"summary\":\"minimax did it\",\"accomplished\":[],\"remaining\":[],\"files\":[]}",
            "usage": {}
        }"#;

        let parsed = r.parse_response(raw).unwrap();
        assert_eq!(parsed.response.status, "done");
    }

    #[test]
    fn kimi_build_command_uses_kimi_binary() {
        let r = ClaudeRunner::new("kimi");
        let perms = PermissionRules {
            autonomous: true,
            sandbox: SandboxLevel::WorkspaceWrite,
            disallowed_tools: vec![],
            allowed_tools: vec![],
            allowed_edit_paths: vec![],
            deny_read_only: false,
            extra_writable_dirs: vec![],
        };
        let cmd = r.build_command(
            Some("sonnet"),
            "timeout 1800",
            "/tmp/sys.txt",
            "/tmp/msg.txt",
            &perms,
        );
        assert!(cmd.contains("kimi -p"), "expected kimi binary, got: {cmd}");
        assert!(cmd.contains("--model 'sonnet'"));
    }

    #[test]
    fn build_command_with_allowed_tools() {
        let r = runner();
        let perms = PermissionRules {
            autonomous: true,
            sandbox: SandboxLevel::WorkspaceWrite,
            disallowed_tools: vec!["Bash(rm *)".to_string()], // should be ignored
            allowed_tools: vec![
                "Edit".to_string(),
                "Write".to_string(),
                "Read".to_string(),
                "Bash".to_string(),
                "Grep".to_string(),
                "git".to_string(),
                "npm".to_string(),
            ],
            allowed_edit_paths: vec![],
            deny_read_only: false,
            extra_writable_dirs: vec![],
        };
        let cmd = r.build_command(None, "", "/tmp/sys.txt", "/tmp/msg.txt", &perms);

        // Should use --allowedTools instead of --disallowedTools
        assert!(
            cmd.contains("--allowedTools"),
            "expected --allowedTools flag, got: {cmd}"
        );
        assert!(
            !cmd.contains("--disallowedTools"),
            "should not have --disallowedTools when allowed_tools is set, got: {cmd}"
        );

        // Native tools pass through as-is
        assert!(cmd.contains("Edit"), "missing Edit tool");
        assert!(cmd.contains("Write"), "missing Write tool");
        assert!(cmd.contains("Bash"), "missing Bash tool");

        // CLI commands become Bash(<cmd> *) patterns
        assert!(
            cmd.contains("Bash(git *)"),
            "missing Bash(git *) pattern, got: {cmd}"
        );
        assert!(
            cmd.contains("Bash(npm *)"),
            "missing Bash(npm *) pattern, got: {cmd}"
        );
    }

    #[test]
    fn build_command_allowed_tools_empty_falls_back_to_disallowed() {
        let r = runner();
        let perms = PermissionRules {
            autonomous: true,
            sandbox: SandboxLevel::WorkspaceWrite,
            disallowed_tools: vec!["Bash(rm *)".to_string()],
            allowed_tools: vec![], // empty = use disallowed_tools
            allowed_edit_paths: vec![],
            deny_read_only: false,
            extra_writable_dirs: vec![],
        };
        let cmd = r.build_command(None, "", "/tmp/sys.txt", "/tmp/msg.txt", &perms);

        assert!(
            cmd.contains("--disallowedTools"),
            "should fall back to --disallowedTools when allowed_tools is empty, got: {cmd}"
        );
        assert!(
            !cmd.contains("--allowedTools"),
            "should not have --allowedTools when empty, got: {cmd}"
        );
    }

    #[test]
    fn translate_allowed_tools_categorizes_correctly() {
        let allowed = vec![
            "Edit".to_string(),
            "Bash".to_string(),
            "Grep".to_string(),
            "git".to_string(),
            "cargo".to_string(),
            "WebFetch".to_string(),
        ];
        // No allowed_edit_paths → Edit passes through as-is
        let result = translate_allowed_tools(&allowed, &[]);
        assert_eq!(
            result,
            vec![
                "Edit",
                "Bash",
                "Grep",
                "Bash(git *)",
                "Bash(cargo *)",
                "WebFetch"
            ]
        );
    }

    #[test]
    fn translate_allowed_tools_scopes_edit_write() {
        let allowed = vec!["Edit".to_string(), "Write".to_string(), "Read".to_string()];
        let paths = vec![std::path::PathBuf::from("/tmp/worktree")];
        let result = translate_allowed_tools(&allowed, &paths);
        assert_eq!(
            result,
            vec!["Edit(/tmp/worktree/*)", "Write(/tmp/worktree/*)", "Read"]
        );
    }

    #[test]
    fn classify_error_overloaded_529() {
        // Claude 529 overloaded error
        let err = runner().classify_error(1, "", "overloaded_error: 529");
        assert!(matches!(err, AgentError::RateLimit { .. }), "got: {err:?}");
    }

    #[test]
    fn classify_error_context_length() {
        let err = runner().classify_error(1, "", "context_length_exceeded: max 200000 tokens");
        assert!(
            matches!(err, AgentError::ContextOverflow { .. }),
            "got: {err:?}"
        );
    }

    // ── extract_text ─────────────────────────────────────────────

    /// Claude stream-json: extract_text returns the inner result text for review parsing.
    #[test]
    fn extract_text_stream_json_returns_result_field() {
        let raw = concat!(
            r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Reviewing..."}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"{\"decision\":\"approve\",\"notes\":\"LGTM\",\"test_results\":\"pass\",\"issues\":[]}","usage":{"input_tokens":100,"output_tokens":20}}"#,
        );
        let text = runner().extract_text(raw).unwrap();
        assert_eq!(
            text,
            r#"{"decision":"approve","notes":"LGTM","test_results":"pass","issues":[]}"#
        );
    }

    /// Claude stream-json with markdown-wrapped ReviewResponse in result field.
    #[test]
    fn extract_text_stream_json_markdown_result() {
        let review_json =
            r#"```json\n{"decision":"request_changes","notes":"Fix it","issues":[]}\n```"#;
        let result_line = format!(
            r#"{{"type":"result","subtype":"success","is_error":false,"result":"{review_json}","usage":{{"input_tokens":10,"output_tokens":5}}}}"#
        );
        let raw = format!(
            "{}\n{}",
            r#"{"type":"system","subtype":"init","session_id":"s1"}"#, result_line
        );
        let text = runner().extract_text(&raw).unwrap();
        assert!(
            text.contains("request_changes"),
            "expected review JSON in result, got: {text}"
        );
    }

    /// Claude stream-json error (is_error=true, rate limit): extract_text propagates RateLimit.
    #[test]
    fn extract_text_stream_json_rate_limit_propagates() {
        let raw = concat!(
            r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
            "\n",
            r#"{"type":"result","subtype":"error","is_error":true,"result":"rate limit exceeded: 429 Too Many Requests","usage":{"input_tokens":0,"output_tokens":0}}"#,
        );
        let err = runner().extract_text(raw).unwrap_err();
        assert!(
            matches!(err, AgentError::RateLimit { .. }),
            "expected RateLimit, got: {err:?}"
        );
    }

    #[test]
    fn extract_stream_json_fallbacks_to_opencode_extractor_on_leading_init() {
        // Some Claude wrappers emit a leading system/init envelope and then
        // place the real JSON payload inside an assistant message content array
        // (no final "type":"result" line). Ensure our extractor falls back
        // to the opencode-style NDJSON scanner and returns the inner JSON.
        let raw = concat!(
            r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
            "\n",
            // The inner JSON must be escaped inside the outer JSON string
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"{\"status\":\"done\",\"summary\":\"fallback\",\"accomplished\":[],\"remaining\":[],\"files\":[]}"}]}}"#,
        );

        let text = extract_stream_json_result_text(raw).expect("should fallback and extract JSON");
        let parsed: serde_json::Value =
            serde_json::from_str(&text).expect("extracted text must be JSON");
        assert_eq!(parsed["status"], "done");
        assert_eq!(parsed["summary"], "fallback");
    }

    /// Claude stream-json error (is_error=true, auth): extract_text propagates Auth.
    #[test]
    fn extract_text_stream_json_auth_error_propagates() {
        let raw = concat!(
            r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
            "\n",
            r#"{"type":"result","subtype":"error","is_error":true,"result":"401 Unauthorized: invalid api key","usage":{"input_tokens":0,"output_tokens":0}}"#,
        );
        let err = runner().extract_text(raw).unwrap_err();
        assert!(
            matches!(err, AgentError::Auth { .. }),
            "expected Auth, got: {err:?}"
        );
    }

    /// extract_text on empty input returns empty string (not an error).
    #[test]
    fn extract_text_empty_input_returns_empty_ok() {
        let text = runner().extract_text("").unwrap();
        assert!(text.is_empty());
    }

    // ── find_claude_result ──────────────────────────────────────

    #[test]
    fn find_claude_result_success_with_tokens() {
        let ndjson = concat!(
            r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working..."}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"{\"decision\":\"approve\",\"notes\":\"LGTM\"}","usage":{"input_tokens":1500,"output_tokens":200},"duration_ms":5000}"#,
        );
        let result = find_claude_result(ndjson).expect("should find result");
        assert!(!result.is_error);
        assert!(result.result_text.contains("approve"));
        assert_eq!(result.input_tokens, Some(1500));
        assert_eq!(result.output_tokens, Some(200));
        assert_eq!(result.duration_ms, Some(5000));
    }

    #[test]
    fn find_claude_result_error_flag() {
        let ndjson = r#"{"type":"result","subtype":"error","is_error":true,"result":"rate limit exceeded","usage":{}}"#;
        let result = find_claude_result(ndjson).expect("should find result");
        assert!(result.is_error);
        assert!(result.result_text.contains("rate limit"));
    }

    #[test]
    fn find_claude_result_empty_returns_none() {
        assert!(find_claude_result("").is_none());
        assert!(find_claude_result("   ").is_none());
    }

    #[test]
    fn find_claude_result_no_result_line_falls_back_to_assistant() {
        // MiniMax/Kimi pattern: no type:result, just assistant messages
        let ndjson = concat!(
            r#"{"type":"system","subtype":"init","model":"MiniMax-M2.7"}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"analysis complete"}],"usage":{"input_tokens":800,"output_tokens":150}}}"#,
        );
        let result = find_claude_result(ndjson).expect("should fall back to assistant text");
        assert!(!result.is_error);
        assert_eq!(result.result_text, "analysis complete");
        assert_eq!(result.input_tokens, Some(800));
        assert_eq!(result.output_tokens, Some(150));
    }

    #[test]
    fn find_claude_result_plain_text_returns_none() {
        // Plain text (not NDJSON) should return None
        assert!(find_claude_result("just some plain text output").is_none());
    }

    #[test]
    fn find_claude_result_model_usage() {
        let ndjson = r#"{"type":"result","subtype":"success","is_error":false,"result":"done","modelUsage":{"claude-sonnet-4-20250514":{"inputTokens":5000,"outputTokens":1000}},"duration_ms":3000}"#;
        let result = find_claude_result(ndjson).expect("should extract modelUsage");
        assert_eq!(result.input_tokens, Some(5000));
        assert_eq!(result.output_tokens, Some(1000));
    }

    // ── Regression tests for issue #1387 ──────────────────────────────────────

    /// Regression test: when an NDJSON stream has multiple type:result lines —
    /// one with JSON in the result field (inner task delegate) and one with
    /// plain text (outer session summary) — find_ndjson_result_line must
    /// skip the plain-text one and return the JSON one.
    #[test]
    fn find_ndjson_result_line_skips_plain_text_result() {
        // Build test NDJSON using serde_json to avoid string-escaping confusion.
        let inner_result = serde_json::json!({"status": "done", "summary": "fix applied"});
        let json_result_line = serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "duration_ms": 273044,
            "result": inner_result.to_string(),
            "usage": {"input_tokens": 12, "output_tokens": 1577}
        })
        .to_string();
        let plain_result_line = serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "duration_ms": 2352,
            "result": "All 2352 tests passed.",
            "usage": {"input_tokens": 3, "output_tokens": 16}
        })
        .to_string();
        // Concatenate without using concat! to avoid string-escaping headaches.
        let mut ndjson = json_result_line.clone();
        ndjson.push('\n');
        ndjson.push_str(r#"{"type":"system","subtype":"task_notification"}"#);
        ndjson.push('\n');
        ndjson.push_str(r#"{"type":"system","subtype":"init"}"#);
        ndjson.push('\n');
        ndjson.push_str(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"summary here"}]}}"#,
        );
        ndjson.push('\n');
        ndjson.push_str(&plain_result_line);

        let line = find_ndjson_result_line(&ndjson)
            .expect("should find a result line")
            .0;
        // Must be the JSON result (inner), not the plain-text one (outer).
        let parsed: serde_json::Value =
            serde_json::from_str(line).expect("returned line must be valid JSON");
        let result_val = parsed.get("result").expect("result field must exist");
        assert!(
            result_val.is_string(),
            "result field must be a JSON string (not plain text), got: {result_val}"
        );
        let result_str = result_val.as_str().unwrap();
        assert!(
            result_str.starts_with('{'),
            "result must be JSON (object), got: {result_str}"
        );
        assert!(
            !result_str.contains("All 2352 tests passed"),
            "must NOT return outer plain-text result: {result_str}"
        );
    }

    /// Regression test: when BOTH result lines have plain-text results (no JSON),
    /// find_ndjson_result_line must return the one with more tokens — not the last.
    /// This is the actual task 1384 scenario: inner result has 12+1577 tokens,
    /// outer confirmation has 3+16 tokens. Must pick the inner one (issue #1387).
    #[test]
    fn find_ndjson_result_line_prefers_more_tokens_on_plain_text() {
        // Build NDJSON where both results are plain text.
        let inner_result_line = serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "duration_ms": 273044,
            "result": "All 2352 tests pass. The fix is clean and correct.",
            "usage": {"input_tokens": 12, "output_tokens": 1577}
        })
        .to_string();
        let outer_result_line = serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "duration_ms": 2352,
            "result": "Already retrieved the result — all 2352 tests passed.",
            "usage": {"input_tokens": 3, "output_tokens": 16}
        })
        .to_string();

        // Inner result FIRST, outer result LAST (the actual task 1384 ordering).
        let ndjson = format!(
            "{0}\n{1}\n{2}",
            inner_result_line, r#"{"type":"system","subtype":"init"}"#, outer_result_line
        );

        let (line, tokens) = find_ndjson_result_line(&ndjson).expect("should find a result line");

        // Must return the inner result (more tokens), not the outer one.
        assert!(
            line.contains("The fix is clean"),
            "expected inner result (more tokens), got: {line}"
        );
        // Tokens must match the inner result.
        assert_eq!(
            tokens,
            Some((12, 1577)),
            "tokens must be from the inner result"
        );
    }

    /// Regression test: when parse_response encounters plain-text results
    /// and falls back to synthesis, tokens from the envelope must be preserved.
    #[test]
    fn parse_response_preserves_tokens_on_synthesis_fallback() {
        // Both results are plain text (no JSON in result field).
        // parse_response should synthesize but preserve the envelope tokens.
        let ndjson = concat!(
            r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":273044,"result":"All 2352 tests pass. The fix is clean and correct.","usage":{"input_tokens":12,"output_tokens":1577}}"#,
            "\n",
            r#"{"type":"system","subtype":"init"}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":2352,"result":"Already retrieved the result — all 2352 tests passed.","usage":{"input_tokens":3,"output_tokens":16}}"#,
        );

        let parsed = runner()
            .parse_response(ndjson)
            .expect("should parse successfully");

        // Must preserve tokens from the inner (higher-token) result.
        assert_eq!(
            parsed.input_tokens,
            Some(12),
            "input_tokens must be preserved (not lost on synthesis fallback)"
        );
        assert_eq!(
            parsed.output_tokens,
            Some(1577),
            "output_tokens must be preserved (not lost on synthesis fallback)"
        );
    }

    #[test]
    fn parse_response_uses_inner_json_result_preserving_tokens() {
        // Simulates task 1384: outer session says "tests passed" (plain text),
        // but inner task delegate result is JSON with tokens.
        let ndjson = concat!(
            r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":273044,"result":"{\"status\":\"done\",\"summary\":\"fix applied\",\"accomplished\":[\"fix\"],\"remaining\":[],\"files\":[]}","usage":{"input_tokens":12,"output_tokens":1577}}"#,
            "\n",
            r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"tests passed"}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":2352,"result":"All 2352 tests passed.","usage":{"input_tokens":3,"output_tokens":16}}"#,
        );

        let parsed = runner()
            .parse_response(ndjson)
            .expect("should parse successfully");

        // Must use the inner JSON result (fix applied), not outer plain text.
        assert_eq!(
            parsed.response.status, "done",
            "expected inner JSON result status, got: {:?}",
            parsed.response.status
        );
        assert_eq!(
            parsed.response.summary.as_str(),
            "fix applied",
            "expected inner JSON result summary, got: {:?}",
            parsed.response.summary
        );
        // Tokens must be from the inner result with tokens, not the outer plain-text one.
        assert_eq!(
            parsed.input_tokens,
            Some(12),
            "input_tokens must be from inner result (not outer plain-text result)"
        );
        assert_eq!(
            parsed.output_tokens,
            Some(1577),
            "output_tokens must be from inner result (not outer plain-text result)"
        );
    }

    /// Regression test: when find_ndjson_result_line returns None (no JSON
    /// result found), extract_stream_json_result_text must fall back to
    /// opencode-style text extraction, not return raw NDJSON.
    #[test]
    fn extract_text_falls_back_to_opencode_style_when_no_json_result() {
        // An NDJSON stream with no type:result at all — just a text event.
        let ndjson = concat!(
            r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"All tasks complete"}]}}"#,
        );

        let text = runner().extract_text(ndjson).expect("should extract text");
        // Must extract the assistant message text, not raw NDJSON.
        assert!(
            text.contains("All tasks complete"),
            "expected extracted assistant text, got: {text}"
        );
        assert!(!text.contains("type"), "must not return raw NDJSON: {text}");
    }
}
