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

use std::path::PathBuf;

use tokio::process::Command;

use super::SandboxLevel;
use super::{
    AgentError, AgentRunner, ParsedResponse, PermissionRules, StreamEvent, json_error_message,
    json_str,
};
use crate::cooldown::RateLimitWindow;
use crate::engine::agents::opencode;
use crate::engine::agents::parser;

/// Runner for Claude-compatible agents (Claude, Kimi, MiniMax).
#[derive(Debug)]
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
    fn parse_envelope(raw: &str) -> Result<ParsedResponse, AgentError> {
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
            return Self::parse_direct(raw);
        }

        // Check is_error flag
        let is_error = envelope
            .get("is_error")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        // Extract the result text
        let error_text = if is_error {
            envelope_error_text(envelope)
        } else {
            String::new()
        };
        let result_text = if is_error {
            error_text.as_str()
        } else {
            envelope
                .get("result")
                .and_then(|v| v.as_str())
                .unwrap_or("")
        };

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
        let duration_ms = envelope
            .get("duration_ms")
            .and_then(serde_json::Value::as_u64);

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
            cost_usd: envelope_cost(envelope),
            duration_ms,
        })
    }

    /// Parse raw text directly (non-envelope response).
    fn parse_direct(raw: &str) -> Result<ParsedResponse, AgentError> {
        let response = parser::parse(raw).map_err(|_| AgentError::InvalidResponse {
            raw: raw.to_string(),
        })?;

        Ok(ParsedResponse {
            response,
            input_tokens: None,
            output_tokens: None,
            cost_usd: None,
            duration_ms: None,
        })
    }
}

/// Error text of an `is_error` envelope: `result`, else the joined `errors[]`, else `subtype`.
/// Some agents (e.g. billing failures) put the message only in `errors[]`
fn envelope_error_text(obj: &serde_json::Map<String, serde_json::Value>) -> String {
    let result = obj.get("result").and_then(|v| v.as_str()).unwrap_or("");
    if !result.is_empty() {
        return result.to_string();
    }
    let errors = obj
        .get("errors")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|e| e.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    if !errors.is_empty() {
        return errors;
    }
    obj.get("subtype")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
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
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    {
        let result_text = envelope_error_text(obj);
        let result_text = result_text.as_str();
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
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line)
            && let Some(obj) = val.as_object()
            && let Some(err) = extract_error_from_envelope(obj, line)
        {
            return Some(err);
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
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(line)
            && let Some(obj) = parsed.as_object()
        {
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
        // Not an error, but result field is not extractable: fall through to fallback.
    }

    // No JSON result line found (or it wasn't an error). Try opencode-style
    // text extraction which scans NDJSON for text/result events.
    if let Some(fallback) = opencode::extract_router_text(trimmed) {
        return Ok(fallback);
    }

    // No opencode-style text found. Try parsing the whole trimmed string as a
    // single JSON object (handles `--output-format json` plain output).
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(trimmed)
        && let Some(obj) = parsed.as_object()
    {
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

    fn stream_events(&self, event: &serde_json::Value) -> Vec<StreamEvent> {
        stream_events(event)
    }

    fn router_transcript(&self) -> super::RouterTranscript {
        super::RouterTranscript::Messages
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
        self.build_command_with_effort(model, None, timeout_cmd, sys_file, msg_file, permissions)
    }

    fn build_command_with_effort(
        &self,
        model: Option<&str>,
        effort: Option<&str>,
        timeout_cmd: &str,
        sys_file: &str,
        msg_file: &str,
        permissions: &PermissionRules,
    ) -> String {
        let effort_flag = effort
            .map(|e| format!(" --effort {}", super::shell_single_quote(e)))
            .unwrap_or_default();
        let model_flag = model
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(|m| format!("--model {}", super::shell_single_quote(m)))
            .unwrap_or_default();

        // Claude permission mode: autonomous → bypassPermissions, supervised → acceptEdits
        // dontAsk refuses every tool not on --allowedTools instead of asking, so a read-only
        // chat runs its allowlist and nothing else
        let permission_mode = if permissions.sandbox == SandboxLevel::ReadOnly {
            "dontAsk"
        } else if permissions.autonomous {
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

        // A read-only chat ignores the operator's own settings files, whose allow rules would widen
        // its allowlist, loads no MCP server, and has only the tool kinds its allowlist names
        let (restrict_flag, prompt_flag) = if permissions.sandbox == SandboxLevel::ReadOnly {
            let mut kinds: Vec<&str> = permissions
                .allowed_tools
                .iter()
                .filter(|t| CLAUDE_NATIVE_TOOLS.contains(&t.as_str()))
                .map(String::as_str)
                .collect();
            if permissions
                .allowed_tools
                .iter()
                .any(|t| t.starts_with("Bash("))
            {
                kinds.push("Bash");
            }
            (
                format!(
                    "--restricted --strict-mcp-config --tools '{}' \\\n  ",
                    kinds.join(",")
                ),
                "--append-system-prompt-file",
            )
        } else {
            (String::new(), "--append-system-prompt")
        };

        format!(
            r#"{timeout_cmd} {binary} -p --verbose {model_flag}{effort_flag} \
  --permission-mode {permission_mode} \
  --output-format stream-json \
  {restrict_flag}{tool_flag} \
  {prompt_flag} "{sys_file}" \
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
            let result = Self::parse_envelope(result_line)?;
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
        Self::parse_envelope(trimmed)
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
            if obj.get("is_error").and_then(serde_json::Value::as_bool) != Some(true) {
                return None;
            }
            Some(envelope_error_text(obj)).filter(|t| !t.is_empty())
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
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(line)
                    && let Some(obj) = val.as_object()
                    && obj.get("type").and_then(|t| t.as_str()) == Some("system")
                    && let Some(error) = obj.get("error")
                    && let Some(msg) = error.get("message").and_then(|m| m.as_str())
                {
                    return Some(msg.to_string());
                }
            }
            None
        })();

        // A kill signal (SIGTERM or SIGKILL) with no result envelope and no error event
        // is an interrupted run, not a provider failure, so skip the text scan
        let has_result_line = find_ndjson_result_line(stdout.trim()).is_some();
        if matches!(exit_code, 143 | 137)
            && !has_result_line
            && error_result.is_none()
            && system_error.is_none()
        {
            return AgentError::Interrupted {
                message: format!("agent killed by signal (exit {exit_code})"),
            };
        }

        let combined = match (&error_result, &system_error) {
            (Some(r), Some(s)) => format!("{s}\n{r}\n{stdout}\n{stderr}"),
            (Some(r), None) => format!("{r}\n{stdout}\n{stderr}"),
            (None, Some(s)) => format!("{s}\n{stdout}\n{stderr}"),
            (None, None) => format!("{stdout}\n{stderr}"),
        };
        // Envelope billing text is prepended into the same 3000-byte head as hook
        // UUIDs, so classify the envelope before the combined scan
        if let Some(message) = &error_result
            && let Some(e) = super::patterns::detect_auth_error(message)
        {
            return e;
        }
        let classified = super::patterns::classify_from_text(exit_code, &combined);

        // `classify_from_text`'s generic catch-all truncates to the last 300
        // bytes of the combined text, which can cut a perfectly well-formed
        // `error_result` line off mid-field-name (e.g. `is_error` becomes
        // `i_error`) once enough NDJSON precedes it (issue #3577). When we
        // already extracted a clean, complete error message from the result
        // envelope but none of the specific pattern detectors matched it,
        // prefer that clean message over the byte-truncated fallback.
        if let (AgentError::Unknown { .. }, Some(message)) = (&classified, &error_result)
            && !message.is_empty()
        {
            return AgentError::AgentFailed {
                message: message.clone(),
            };
        }
        classified
    }

    fn router_command(&self, prompt: &str, model: Option<&str>) -> anyhow::Result<Command> {
        let mut cmd = Command::new(&self.binary);
        cmd.env_remove("CLAUDECODE"); // allow nested invocation
        cmd.arg("--output-format")
            .arg("stream-json")
            .arg("--verbose")
            .arg("--print");
        if let Some(m) = model.map(str::trim).filter(|m| !m.is_empty()) {
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
fn translate_allowed_tools(allowed: &[String], allowed_edit_paths: &[PathBuf]) -> Vec<String> {
    let mut tools = Vec::new();
    for item in allowed {
        if (item == "Edit" || item == "Write") && !allowed_edit_paths.is_empty() {
            // Scope Edit/Write to allowed paths only
            for path in allowed_edit_paths {
                tools.push(format!("{}({}/*)", item, path.display()));
            }
        } else if CLAUDE_NATIVE_TOOLS.contains(&item.as_str()) || item.starts_with("Bash(") {
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
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line)
            && let Some(obj) = val.as_object()
            && obj.get("type").and_then(|t| t.as_str()) == Some("result")
            && let Some(result) = obj.get("result")
        {
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

    // Pass 2: no JSON result found — choose the plain-text result with the most tokens.
    // This handles outer-session confirmation summaries (e.g. "Already retrieved the result")
    // vs. actual work summaries: prefer the one with more tokens.
    // Error results (is_error=true) are included even with zero tokens, so the caller
    // can classify them properly.
    let mut best_line: Option<&str> = None;
    let mut best_total = 0u64;

    for line in &lines {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line)
            && let Some(obj) = val.as_object()
            && obj.get("type").and_then(|t| t.as_str()) == Some("result")
        {
            let tokens = extract_result_tokens(obj);
            let total = tokens.map_or(0, |(i, o)| i.saturating_add(o));
            if total >= best_total {
                best_total = total;
                best_line = Some(*line);
            }
        }
    }

    best_line.map(|line| (line, extract_result_tokens_from_line(line)))
}

/// Extract input/output token counts from a parsed result envelope object.
fn extract_result_tokens(obj: &serde_json::Map<String, serde_json::Value>) -> Option<(u64, u64)> {
    // Try top-level `usage` first.
    if let Some(usage) = obj.get("usage").and_then(|v| v.as_object()) {
        let input = usage
            .get("input_tokens")
            .and_then(serde_json::Value::as_u64);
        let output = usage
            .get("output_tokens")
            .and_then(serde_json::Value::as_u64);
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
                    .and_then(serde_json::Value::as_u64)
                {
                    total_input += i;
                    found = true;
                }
                if let Some(o) = stats
                    .get("outputTokens")
                    .or_else(|| stats.get("output_tokens"))
                    .and_then(serde_json::Value::as_u64)
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

/// The run cost the envelope reports. Claude Code names it `total_cost_usd`, older builds used
/// `costUSD` or `cost_usd`.
fn envelope_cost(envelope: &serde_json::Map<String, serde_json::Value>) -> Option<f64> {
    envelope
        .get("total_cost_usd")
        .or_else(|| envelope.get("costUSD"))
        .or_else(|| envelope.get("cost_usd"))
        .and_then(serde_json::Value::as_f64)
}

/// Extract input/output tokens from the Claude envelope's usage objects.
fn extract_usage(
    envelope: &serde_json::Map<String, serde_json::Value>,
) -> (Option<u64>, Option<u64>) {
    // Try top-level `usage` first
    if let Some(usage) = envelope.get("usage").and_then(|v| v.as_object()) {
        let input = usage
            .get("input_tokens")
            .and_then(serde_json::Value::as_u64);
        let output = usage
            .get("output_tokens")
            .and_then(serde_json::Value::as_u64);
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
                    .and_then(serde_json::Value::as_u64)
                {
                    total_input += i;
                    found = true;
                }
                if let Some(o) = stats
                    .get("outputTokens")
                    .or_else(|| stats.get("output_tokens"))
                    .and_then(serde_json::Value::as_u64)
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

const SHOW_CURSOR: &str = "\x1b[?25h";
const HIDE_CURSOR: &str = "\x1b[?25l";

/// Drop `script` tty junk: show-cursor (`ESC[?25h`) and `^D`/backspace redraw lines
pub fn strip_tty_control_lines(raw: &str) -> String {
    raw.lines()
        .filter(|line| !is_tty_control_line(line))
        .map(strip_trailing_cursor)
        .collect::<Vec<_>>()
        .join("\n")
}

fn strip_trailing_cursor(line: &str) -> &str {
    let mut s = line.trim_end();
    loop {
        if let Some(next) = s
            .strip_suffix(SHOW_CURSOR)
            .or_else(|| s.strip_suffix(HIDE_CURSOR))
        {
            s = next.trim_end();
            continue;
        }
        return s;
    }
}

fn is_tty_control_line(line: &str) -> bool {
    let trimmed = strip_trailing_cursor(line).trim();
    if trimmed.is_empty() {
        return !line.trim().is_empty();
    }
    trimmed.chars().all(|c| matches!(c, '^' | 'D' | '\u{0008}'))
}

/// Type of the last content block on the last assistant NDJSON message
fn last_assistant_content_type(ndjson: &str) -> Option<String> {
    let mut last = None;
    for line in ndjson.lines() {
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
        if let Some(item) = content.last()
            && let Some(t) = item.get("type").and_then(|v| v.as_str())
        {
            last = Some(t.to_string());
        }
    }
    last
}

/// True when the stream has no result envelope and ended on a thinking block
/// or a tool call, so the model never produced a finished review decision
pub fn review_session_exited_mid_thought(ndjson: &str) -> bool {
    if find_ndjson_result_line(ndjson).is_some() {
        return false;
    }
    matches!(
        last_assistant_content_type(ndjson).as_deref(),
        Some("thinking" | "redacted_thinking" | "tool_use")
    )
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
    let stripped = strip_tty_control_lines(ndjson);
    let trimmed = stripped.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Try to find the result envelope first
    if let Some((result_line, _tokens)) = find_ndjson_result_line(trimmed)
        && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(result_line)
        && let Some(envelope) = parsed.as_object()
    {
        let is_error = envelope
            .get("is_error")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        let result_text = if is_error {
            // Error envelopes put the real message in errors[] (billing,
            // rate limits) with `result` empty — don't lose it (issue #3713)
            envelope_error_text(envelope)
        } else {
            envelope
                .get("result")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };

        let (input_tokens, output_tokens) = extract_usage(envelope);
        let duration_ms = envelope
            .get("duration_ms")
            .and_then(serde_json::Value::as_u64);

        let cost_usd = envelope_cost(envelope);

        return Some(super::AgentResult {
            is_error,
            result_text,
            input_tokens,
            output_tokens,
            cost_usd,
            duration_ms,
            truncated_by_length: false,
        });
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
                if item.get("type").and_then(|v| v.as_str()) == Some("text")
                    && let Some(text) = item.get("text").and_then(|v| v.as_str())
                {
                    let t = text.trim();
                    if !t.is_empty() {
                        last_text = Some(t.to_string());
                    }
                }
            }
        }
        // Extract usage (take max, as Claude reports cumulative)
        if let Some(usage) = val.get("message").and_then(|m| m.get("usage")) {
            if let Some(inp) = usage
                .get("input_tokens")
                .and_then(serde_json::Value::as_u64)
            {
                input_tokens = Some(input_tokens.unwrap_or(0u64).max(inp));
            }
            if let Some(out) = usage
                .get("output_tokens")
                .and_then(serde_json::Value::as_u64)
            {
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
        truncated_by_length: false,
    })
}

/// Keep a vendor label printable: letters, digits, `_` and `-`, at most 32 characters
fn sanitize_window_label(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        .take(32)
        .collect()
}

/// Every window of every `rate_limit_event`, oldest first. The event status belongs to the headline
/// window, the other `unifiedWindows` entries use their own status or `unknown`
pub fn rate_limit_windows(ndjson: &str) -> Vec<RateLimitWindow> {
    let mut windows = Vec::new();
    for line in ndjson
        .lines()
        .filter(|line| line.contains("rate_limit_event"))
    {
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        if val.get("type").and_then(|v| v.as_str()) != Some("rate_limit_event") {
            continue;
        }
        let Some(info) = val.get("rate_limit_info") else {
            continue;
        };
        let Some(status) = info.get("status").and_then(|v| v.as_str()) else {
            continue;
        };
        let unified = info.get("unifiedWindows").and_then(|v| v.as_object());
        let headline_raw = info
            .get("rateLimitType")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let headline = unified.and_then(|map| map.get(headline_raw));
        windows.push(RateLimitWindow {
            status: sanitize_window_label(status),
            window_type: sanitize_window_label(headline_raw),
            utilization: headline
                .and_then(|w| w.get("utilization"))
                .or_else(|| info.get("utilization"))
                .and_then(serde_json::Value::as_f64),
            resets_at: info
                .get("resetsAt")
                .or_else(|| headline.and_then(|w| w.get("resetsAt")))
                .and_then(serde_json::Value::as_i64),
        });
        for (name, w) in unified.into_iter().flatten() {
            if name == headline_raw {
                continue;
            }
            windows.push(RateLimitWindow {
                status: sanitize_window_label(
                    w.get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown"),
                ),
                window_type: sanitize_window_label(name),
                utilization: w.get("utilization").and_then(serde_json::Value::as_f64),
                resets_at: w.get("resetsAt").and_then(serde_json::Value::as_i64),
            });
        }
    }
    windows
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
                if item.get("type").and_then(|v| v.as_str()) == Some("text")
                    && let Some(text) = item.get("text").and_then(|v| v.as_str())
                {
                    let t = text.trim();
                    if !t.is_empty() {
                        parts.push(t.to_string());
                    }
                }
            }
        }
    }
    parts.join("\n")
}

/// Claude `stream-json`: calls and text are blocks in `assistant` messages, results in `user` ones
pub(crate) fn stream_events(v: &serde_json::Value) -> Vec<StreamEvent> {
    let blocks = || v["message"]["content"].as_array().into_iter().flatten();
    match json_str(v, "/type") {
        "system" if json_str(v, "/subtype") == "init" => vec![StreamEvent::Started {
            model: json_str(v, "/model").to_string(),
        }],
        "assistant" => blocks()
            .filter_map(|b| match json_str(b, "/type") {
                "text" => Some(StreamEvent::Text(json_str(b, "/text").to_string())),
                "thinking" => Some(StreamEvent::Thinking(json_str(b, "/thinking").to_string())),
                "tool_use" => Some(StreamEvent::ToolCall {
                    id: json_str(b, "/id").to_string(),
                    name: json_str(b, "/name").to_string(),
                    input: b["input"].clone(),
                }),
                _ => None,
            })
            .collect(),
        "user" => blocks()
            .filter(|b| json_str(b, "/type") == "tool_result")
            .map(|b| StreamEvent::ToolResult {
                id: json_str(b, "/tool_use_id").to_string(),
                name: String::new(),
                output: match &b["content"] {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Array(parts) => parts
                        .iter()
                        .filter_map(|p| p["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => String::new(),
                },
                is_error: b["is_error"].as_bool() == Some(true),
            })
            .collect(),
        "error" => vec![StreamEvent::Error(json_error_message(v))],
        "result" => {
            let failed = v["is_error"].as_bool() == Some(true)
                || json_str(v, "/subtype").starts_with("error");
            if failed {
                return vec![StreamEvent::Error(json_str(v, "/result").to_string())];
            }

            let turns = v["num_turns"].as_u64().unwrap_or(0);
            let secs = v["duration_ms"].as_u64().unwrap_or(0) / 1000;
            let time = if secs >= 60 {
                format!("{}m {}s", secs / 60, secs % 60)
            } else {
                format!("{secs}s")
            };
            vec![StreamEvent::Done(format!("{turns} turns · {time}"))]
        }
        _ => vec![],
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod stream_tests;
