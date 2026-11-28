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
//! Discovered dynamically at startup with 1-hour cache.

use super::{
    AgentError, AgentRunner, ParsedResponse, PermissionRules, SandboxLevel, StreamEvent,
    json_error_message, json_str,
};
use crate::engine::agents::parser;
use crate::engine::agents::parser::AgentResponse;
use crate::sys::cmd;
use crate::sys::cmd_cache;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use tokio::process::Command;
use tokio::runtime::Handle;
use tokio::runtime::Runtime;
use tokio::time;

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::PoisonError;
use std::thread;
use std::time::Duration;

/// Module-level cache for discovered free opencode models.
/// Shared across all callers (router, runner, failover).
static FREE_MODELS_CACHE: OnceLock<Mutex<(i64, Vec<String>)>> = OnceLock::new();
static FREE_MODELS_REFRESH_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
/// Module-level cache for all discovered opencode models.
/// Shared across all callers (router, runner, failover).
static ALL_MODELS_CACHE: OnceLock<Mutex<(i64, Vec<String>)>> = OnceLock::new();
static ALL_MODELS_REFRESH_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
pub(crate) fn reset_model_caches_for_test() {
    if let Some(cache) = FREE_MODELS_CACHE.get()
        && let Ok(mut guard) = cache.lock()
    {
        *guard = (0, Vec::new());
    }
    if let Some(cache) = ALL_MODELS_CACHE.get()
        && let Ok(mut guard) = cache.lock()
    {
        *guard = (0, Vec::new());
    }
    FREE_MODELS_REFRESH_IN_PROGRESS.store(false, Ordering::Release);
    ALL_MODELS_REFRESH_IN_PROGRESS.store(false, Ordering::Release);
}

/**
 * Model discovery runs the real `opencode` binary, and opencode 2.2.1 starts an
 * `opencode serve --service` child that outlives it, so a suite run left one server
 * behind per test process. It also made the result depend on the machine, and CI has no
 * opencode at all. A test that needs models passes them in, or uses `Router::new_for_test`.
 * `cfg(test)` is unset when the integration tests link the library, so debug builds count
 * too, and the run-time check keeps the live path compiled in every build
 **/
fn skip_live_opencode_discovery() -> bool {
    cfg!(any(test, debug_assertions))
}

fn update_discovered_models_cache(
    cache: &Mutex<(i64, Vec<String>)>,
    discovered: Vec<String>,
    cache_name: &'static str,
    empty_reason: &'static str,
) {
    let now = chrono::Utc::now().timestamp();
    let mut guard = cache.lock().unwrap_or_else(PoisonError::into_inner);

    if discovered.is_empty() && !guard.1.is_empty() {
        tracing::warn!(
            cache = cache_name,
            cached_models = guard.1.len(),
            reason = empty_reason,
            "opencode model discovery returned empty; preserving previous cache contents"
        );
        // Still advance the timestamp so a persistently-failing discovery
        // backs off for the full TTL window instead of being retried on
        // every subsequent call.
        guard.0 = now;
        return;
    }

    tracing::debug!(
        cache = cache_name,
        discovered_models = discovered.len(),
        "opencode model discovery refreshed cache"
    );
    *guard = (now, discovered);
}

/// Runner for OpenCode agent.
#[derive(Debug)]
pub struct OpenCodeRunner;

pub(crate) fn extract_ndjson_text(events: &[serde_json::Value]) -> Option<String> {
    let mut texts = Vec::new();

    for event in events {
        let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");

        // OpenCode emits several shapes across versions. Also scan assistant
        // messages coming from Claude-like wrappers which use `type: "assistant"`
        // with a `message.content` array containing text items.
        // We intentionally restrict extraction to event/part/message types that
        // represent assistant text output (not tool I/O).
        if event_type == "text" || event_type == "message" || event_type == "assistant" {
            if let Some(part) = event.get("part") {
                texts.extend(extract_text_from_part(part));
            }

            // Handle opencode `text` events that put text at top-level
            if let Some(text) = event.get("text").and_then(|v| v.as_str()) {
                texts.push(text.to_string());
            }

            // Handle Claude-like assistant messages: message.content -> [{type:"text", text:...}]
            if let Some(message) = event.get("message")
                && let Some(items) = message.get("content").and_then(|v| v.as_array())
            {
                for item in items {
                    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if item_type == "text"
                        && let Some(text) = item.get("text").and_then(|v| v.as_str())
                    {
                        texts.push(text.to_string());
                    }
                }
            }

            continue;
        }

        // Some builds use a generic event type but keep a text-ish part.
        if let Some(part) = event.get("part") {
            let part_type = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if part_type == "text" || part_type == "output_text" || part_type == "message" {
                texts.extend(extract_text_from_part(part));
            }
        }
    }

    // If we found no text-like events, try to look for a terminal `result` event
    // which some opencode builds emit. Prefer the last result event (newest).
    if texts.is_empty() {
        for event in events.iter().rev() {
            if let Some(event_type) = event.get("type").and_then(|v| v.as_str()) {
                // Ignore system/init envelopes
                if event_type == "system" {
                    continue;
                }
            }

            if let Some(result) = event.get("result") {
                // If result is a string, return it; if object/array return its JSON
                if let Some(s) = result.as_str() {
                    return Some(s.to_string());
                }
                if result.is_object() || result.is_array() {
                    return Some(result.to_string());
                }
            }

            // Some events include direct text payloads outside of `part`
            if let Some(text) = event.get("text").and_then(|v| v.as_str())
                && !text.trim().is_empty()
            {
                return Some(text.to_string());
            }
        }

        return None;
    }

    let full = texts.join("");
    if let Some(json) = extract_json_object(&full) {
        // Prefer a full-json payload that parses as an AgentResponse.
        if parser::parse(&json).is_ok() {
            return Some(json);
        }

        // Also accept JSON that looks like a router decision / agent envelope
        // even when it's not a full AgentResponse (e.g. {"executor":"opencode"}).
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&json)
            && let Some(obj) = val.as_object()
            && (obj.contains_key("executor")
                || obj.contains_key("status")
                || obj.contains_key("complexity")
                || obj.contains_key("decision"))
        {
            return Some(json);
        }
        // Otherwise fall through and try per-event heuristics below
    }

    for text in texts.iter().rev() {
        let trimmed = text.trim();
        if let Some(json) = extract_json_object(trimmed) {
            // Prefer JSON blobs that parse as AgentResponse. This allows
            // accepting payloads that don't include routing keys but are
            // nevertheless valid task responses.
            if parser::parse(&json).is_ok() {
                return Some(json);
            }

            // Also accept routing-style JSON objects that don't match the
            // AgentResponse schema (router decisions, etc.).
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&json)
                && let Some(obj) = val.as_object()
                && (obj.contains_key("executor") || obj.contains_key("status"))
            {
                return Some(json);
            }
        }
        // As a conservative fallback, if the trimmed chunk contains a '{' and
        // looks like it mentions routing keys, return it. We keep this branch
        // minimal to avoid accidental acceptance of unrelated log blobs.
        if trimmed.contains('{') {
            // As a last-ditch conservative heuristic, accept the chunk if it
            // can be interpreted as a valid AgentResponse or if it contains
            // routing keys. This avoids accepting unrelated log blobs while
            // remaining tolerant of router-style payloads.
            if parser::parse(trimmed).is_ok() {
                return Some(trimmed.to_string());
            }

            if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed)
                && let Some(obj) = val.as_object()
                && (obj.contains_key("executor")
                    || obj.contains_key("status")
                    || obj.contains_key("complexity"))
            {
                return Some(trimmed.to_string());
            }
        }
    }

    Some(full)
}

fn extract_text_from_part(part: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();

    if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
        out.push(text.to_string());
    }

    // Some schemas store text content as: {"content":[{"type":"text","text":"..."}, ...]}
    if let Some(items) = part.get("content").and_then(|v| v.as_array()) {
        for item in items {
            let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if item_type == "text"
                && let Some(text) = item.get("text").and_then(|v| v.as_str())
            {
                out.push(text.to_string());
            }
        }
    }

    out
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
    let events = super::parse_ndjson(raw.trim());
    if events.is_empty() {
        return None;
    }
    extract_ndjson_text(&events)
}

// Each worktree gets its own snapshot git repo under opencode's data dir and nothing removes it.
// Orch commits the work itself, so turn snapshots off for orch runs only
const NO_SNAPSHOT_CONFIG: &str = r#"{"snapshot":false}"#;

impl OpenCodeRunner {
    pub fn new() -> Self {
        Self
    }

    /// Split an opencode model identifier into base model and optional variant.
    ///
    /// The identifier shape is: "provider/model[@variant]". Splitting is
    /// performed on the LAST `@` so provider/model strings containing `@`
    /// are handled correctly. Returns (base, Some(variant)) when a trailing
    /// `@` is present (variant may be empty), or (s, None) when no `@` exists.
    pub(crate) fn split_model_variant(s: &str) -> (&str, Option<&str>) {
        if let Some(pos) = s.rfind('@') {
            let base = &s[..pos];
            let variant = &s[pos + 1..];
            (base, Some(variant))
        } else {
            (s, None)
        }
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
    fn extract_text_from_events(events: &[serde_json::Value]) -> Option<String> {
        extract_ndjson_text(events)
    }

    /// Extract token usage from `step_finish` events.
    fn extract_tokens(events: &[serde_json::Value]) -> (Option<u64>, Option<u64>) {
        for event in events.iter().rev() {
            let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");

            if event_type == "step_finish"
                && let Some(part) = event.get("part")
                && let Some(tokens) = part.get("tokens").and_then(|v| v.as_object())
            {
                let input = tokens.get("input").and_then(serde_json::Value::as_u64);
                let output = tokens.get("output").and_then(serde_json::Value::as_u64);
                return (input, output);
            }
        }

        (None, None)
    }

    /// Check for error events in the stream.
    fn detect_error(events: &[serde_json::Value]) -> Option<AgentError> {
        super::patterns::detect_ndjson_error(
            events,
            |event| {
                let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match event_type {
                    "error" => Some(opencode_error_text(event)),
                    "step_finish" => {
                        let part = event.get("part")?;
                        let reason = part.get("reason").and_then(|v| v.as_str()).unwrap_or("");
                        if reason == "error" || reason == "failed" {
                            Some(
                                part.get("error")
                                    .map_or_else(|| "step failed".to_string(), error_value_text),
                            )
                        } else {
                            None
                        }
                    }
                    _ => None,
                }
            },
            classify_opencode_message,
        )
    }
}

/// Render an error value as text: strings as-is, objects by their message/name or raw JSON
fn error_value_text(v: &serde_json::Value) -> String {
    if let Some(s) = v.as_str() {
        return s.to_string();
    }
    v.get("data")
        .and_then(|d| d.get("message").or(Some(d)))
        .and_then(|m| m.as_str())
        .or_else(|| v.get("message").and_then(|m| m.as_str()))
        .or_else(|| v.get("name").and_then(|n| n.as_str()))
        .map_or_else(|| v.to_string(), str::to_string)
}

/// Extract the message from an opencode `error` event, keeping the raw JSON when no shape matches
fn opencode_error_text(event: &serde_json::Value) -> String {
    if let Some(s) = event.get("message").and_then(|v| v.as_str()) {
        return s.to_string();
    }
    if let Some(e) = event.get("error") {
        let text = error_value_text(e);
        if e.is_object() && text == *e {
            tracing::warn!(event = %event, "unrecognized opencode error event shape");
        }
        text
    } else {
        tracing::warn!(event = %event, "unrecognized opencode error event shape");
        event.to_string()
    }
}

impl AgentRunner for OpenCodeRunner {
    #[cfg(test)]
    fn name(&self) -> &'static str {
        "opencode"
    }

    fn stream_events(&self, event: &serde_json::Value) -> Vec<StreamEvent> {
        stream_events(event)
    }

    fn router_transcript(&self) -> super::RouterTranscript {
        super::RouterTranscript::OpenCode
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

        // Propagate terminal errors so the review pipeline records cooldowns.
        if let Some(err) = Self::detect_error(&events) {
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
        // Support optional variant suffix on opencode model identifiers:
        // provider/model[@variant]
        let (model_flag, variant_flag) = if let Some(m) = model {
            let (base, variant) = Self::split_model_variant(m);
            let mf = format!("--model {}", super::shell_single_quote(base));
            let vf = effort
                .or(variant)
                .map(|v| format!("--variant {}", super::shell_single_quote(v)));
            (mf, vf)
        } else {
            (String::new(), None)
        };

        // OpenCode autonomous permission control.
        //
        // `--dangerously-skip-permissions` auto-approves any tool that isn't
        // explicitly denied, which is what orch needs for unattended dispatch.
        // It avoids the user's global ~/.config/opencode/opencode.json defaults
        // (e.g. "edit":"ask", "external_directory":"ask") that would otherwise
        // block agent file edits and git operations on the worktree.
        //
        // The previously-used .orch-opencode/opencode/opencode.json +
        // XDG_CONFIG_HOME override is no longer needed: the only `deny` entries
        // it set were for opencode's interactive-only features (todowrite,
        // question, codesearch, list) which never fire under `opencode run`.
        let skip_perms = if permissions.autonomous {
            "--dangerously-skip-permissions "
        } else {
            ""
        };

        // `opencode run` is a client of a background service that runs every tool outside the
        // sandbox. `--standalone` keeps the server in this process tree, and its pty helper's
        // socket must sit where the sandbox allows one
        let (pty_env, standalone) = if permissions.sandbox == SandboxLevel::Os {
            (
                r#"mkdir -p "$TMPDIR/pty" && OPENCODE_PTY_SOCKET_DIR="$TMPDIR/pty" "#,
                "--standalone ",
            )
        } else {
            ("", "")
        };

        // Append variant flag when present
        let variant_part = variant_flag.unwrap_or_default();
        // Feed both files via process substitution rather than a leading `cat |`
        // pipe stage — a leading pipe would make `cat`, not opencode, the first
        // stage in the runner script's pipeline, so PIPESTATUS[0] there would
        // always report cat's (always-zero) exit code instead of opencode's.
        format!(
            r#"{pty_env}OPENCODE_CONFIG_CONTENT={config} {timeout_cmd} opencode run {standalone}{model_flag} {variant_part} {skip_perms}\
  --format json - < <(cat "{sys_file}" "{msg_file}")"#,
            config = super::shell_single_quote(NO_SNAPSHOT_CONFIG),
            sys_file = sys_file,
            msg_file = msg_file,
            timeout_cmd = timeout_cmd,
            model_flag = model_flag,
            variant_part = variant_part,
            skip_perms = skip_perms,
        )
    }

    fn parse_response(&self, raw: &str) -> Result<ParsedResponse, AgentError> {
        let trimmed = raw.trim();
        // Empty output with exit code 0 is a silent failure — return Unknown with
        // a deterministic message matching what fallback.rs expects so the model
        // gets a cooldown and free-model retry is attempted.
        if trimmed.is_empty() {
            return Err(AgentError::Unknown {
                exit_code: 0,
                message: "empty-output-exit0: opencode returned exit 0 with empty stdout"
                    .to_string(),
            });
        }

        let events = super::parse_ndjson(trimmed);

        if events.is_empty() {
            // Maybe direct JSON
            if let Ok(resp) = parser::parse(trimmed) {
                return Ok(ParsedResponse {
                    response: resp,
                    input_tokens: None,
                    output_tokens: None,
                    cost_usd: None,
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

        // Extract tokens (needed for both the text path and the tool-only fallback)
        let (input_tokens, output_tokens) = Self::extract_tokens(&events);

        // Extract text — when opencode emits only tool_use/step events with no
        // text content, synthesize a human-readable summary from the event
        // stream instead of falling through to InvalidResponse. Otherwise the
        // upstream `synthesize_response_from_text` heuristic gets handed the
        // entire raw NDJSON, matches a "completed" substring inside tool state
        // JSON, and emits 500 bytes of raw NDJSON as the task summary.
        let Some(text) = Self::extract_text_from_events(&events) else {
            let response = synthesize_response_from_events(&events);
            return Ok(ParsedResponse {
                response,
                input_tokens,
                output_tokens,
                cost_usd: extract_cost(&events),
                duration_ms: None,
            });
        };

        // Parse the text through standard parser
        let response = match parser::parse(&text) {
            Ok(r) => r,
            Err(_) => {
                // Structured parse failed — try plain-text synthesis before
                // giving up, same rescue path claude.rs uses (issue #3467).
                super::synthesize_response_from_text(&text)
                    .ok_or_else(|| AgentError::InvalidResponse { raw: text.clone() })?
            }
        };

        Ok(ParsedResponse {
            response,
            input_tokens,
            output_tokens,
            cost_usd: extract_cost(&events),
            duration_ms: None,
        })
    }

    fn classify_error(&self, exit_code: i32, stdout: &str, stderr: &str) -> AgentError {
        // Try parsing NDJSON events from stdout for structured errors
        let events = super::parse_ndjson(stdout);
        if let Some(err) = Self::detect_error(&events) {
            return err;
        }

        let combined = format!("{stdout}\n{stderr}");
        super::patterns::classify_from_text(exit_code, &combined)
    }

    fn free_models(&self) -> Vec<String> {
        discover_free_opencode_models()
    }

    fn available_models(&self) -> Vec<String> {
        // Known default models; could be extended with `opencode models` discovery
        vec![
            "anthropic/claude-sonnet-4-20250514".to_string(),
            "openai/gpt-4.1".to_string(),
        ]
    }

    fn router_command(&self, prompt: &str, model: Option<&str>) -> anyhow::Result<Command> {
        // Routing only needs text output — no tools fire.
        // `--dangerously-skip-permissions` prevents the user's global
        // ~/.config/opencode/opencode.json interactive defaults (e.g.
        // "edit":"ask") from producing PermissionError events that the router
        // would misclassify as model failures.
        let mut cmd = Command::new("opencode");
        cmd.env("OPENCODE_CONFIG_CONTENT", NO_SNAPSHOT_CONFIG)
            .arg("run")
            .arg("--format")
            .arg("json")
            .arg("--dangerously-skip-permissions");
        if let Some(m) = model {
            let (base, variant) = Self::split_model_variant(m);
            cmd.arg("--model").arg(base);
            if let Some(v) = variant {
                // pass variant even if empty (last-token-wins semantics)
                cmd.arg("--variant").arg(v);
            }
        }
        cmd.arg(prompt);
        Ok(cmd)
    }
}

/// Classify an OpenCode error message.
fn classify_opencode_message(message: &str) -> AgentError {
    let lower = message.to_lowercase();

    // Rate limit — delegate to shared helper
    if let Some(e) = super::patterns::detect_rate_limit(message) {
        return e;
    }

    // Context overflow — delegate to shared helper (covers "context overflow", "context length", etc.)
    if let Some(e) = super::patterns::detect_context_overflow(message) {
        return e;
    }

    // Auth errors — delegate to shared helper (includes HTTP 401/403, billing, etc.)
    if let Some(e) = super::patterns::detect_auth_error(message) {
        return e;
    }

    // Permission denied — delegate to shared helper
    if let Some(e) = super::patterns::detect_permission_denied(message) {
        return e;
    }

    // Model not available (opencode-specific extraction pattern)
    //
    // Patterns handled:
    //   "Model not found: anthropic/claude-sonnet-4-6."
    //   "Model not found: github-copilot/gemini-3.1-pro. Did you mean: gemini-3.1-pro-preview?"
    //   "model not supported: X"
    //   "The free model has been deprecated. Transition to qwen/qwen3.6-plus for continued paid access."
    //   "No endpoints found for qwen/qwen3.6-plus:free."
    //   "This model is unavailable for free. The paid version is available now - use this slug instead: X"
    //   "This model is not available in your country."
    let is_model_unavailable = (lower.contains("model")
        && (lower.contains("not found")
            || lower.contains("not supported")
            || lower.contains("deprecated")
            || lower.contains("unavailable for free")
            || lower.contains("not available in your country")
            || lower.contains("not available in your region")))
        || lower.contains("no endpoints found");

    if is_model_unavailable {
        // Try to extract model name from patterns like "Model not found: anthropic/claude-sonnet-4-6."
        // or "No endpoints found for qwen/qwen3.6-plus:free."
        let model = if lower.contains("no endpoints found") {
            // "No endpoints found for X." — extract X after "for "
            message
                .split(" for ")
                .nth(1)
                .map(|s| s.trim_end_matches('.').to_string())
                .unwrap_or_default()
        } else {
            // Try "Model not found: X" or "model not supported: X" patterns
            let from_colon = message
                .split(": ")
                .nth(1)
                .map(|s| {
                    // Strip opencode's "Did you mean: X?" suggestion suffix if present
                    s.split(". Did you mean")
                        .next()
                        .unwrap_or(s)
                        .trim_end_matches('.')
                        .to_string()
                })
                .filter(|s| !s.is_empty());

            // For deprecated messages like "The free model has been deprecated. Transition to X for..."
            // extract the model from "Transition to X" when colon-based extraction yields nothing.
            let from_transition = if lower.contains("deprecated") {
                message
                    .split("Transition to ")
                    .nth(1)
                    .and_then(|s| s.split(" for ").next())
                    .map(|s| s.trim_end_matches('.').to_string())
                    .filter(|s| !s.is_empty())
            } else {
                None
            };

            // For "unavailable for free" messages like "... use this slug instead: X"
            // extract the suggested replacement slug.
            let from_slug_suggestion = if lower.contains("unavailable for free") {
                message
                    .split("use this slug instead: ")
                    .nth(1)
                    .map(|s| s.trim_end_matches('.').trim().to_string())
                    .filter(|s| !s.is_empty())
            } else {
                None
            };

            // Transition/slug extractions are tied to specific message shapes and take
            // priority over the generic colon split, which would otherwise grab the
            // wrong substring (e.g. the "[404] ..." text before the real slug).
            from_transition
                .or(from_slug_suggestion)
                .or(from_colon)
                .unwrap_or_default()
        };
        return AgentError::ModelUnavailable {
            message: message.to_string(),
            model,
        };
    }

    // OpenCode surfaces opaque upstream provider failures as "Provider returned error".
    // Treat this as a usage-limit class signal so fallback/error tracking records a
    // rate-limit event (instead of a generic failure), allowing router health/degraded
    // checks to react faster to unstable provider/model endpoints.
    if lower.contains("provider returned error") {
        return AgentError::RateLimit {
            message: message.to_string(),
        };
    }

    // Transient network/transport errors (e.g. "Upstream idle timeout exceeded")
    if let Some(e) = super::patterns::detect_network_error(message) {
        return e;
    }

    AgentError::AgentFailed {
        message: message.to_string(),
    }
}

/// Discover free opencode models, with 1-hour cache and background refresh.
///
/// This is the single source of truth for free model discovery. Called by
/// the router (pool expansion, model_map), the runner (failover), and
/// startup priming.
///
/// Non-blocking: returns cached data immediately. When the cache is cold
/// or expired, spawns a background thread to refresh via `opencode models`.
pub fn discover_free_opencode_models() -> Vec<String> {
    if skip_live_opencode_discovery() {
        return Vec::new();
    }
    let cache = FREE_MODELS_CACHE.get_or_init(|| Mutex::new((0, Vec::new())));

    let now = chrono::Utc::now().timestamp();
    // Fast path: return cached data if still fresh (1-hour TTL).
    {
        let guard = cache.lock().unwrap_or_else(PoisonError::into_inner);
        let (ts, models) = &*guard;
        if *ts != 0 && now.saturating_sub(*ts) < 3600 {
            return models.clone();
        }
    }

    // Cache is cold or expired. Spawn a background task on the current runtime to refresh.
    // Only one refresh runs at a time.
    if !FREE_MODELS_REFRESH_IN_PROGRESS.swap(true, Ordering::AcqRel) {
        let refresh = async {
            let _guard = scopeguard::guard((), |()| {
                FREE_MODELS_REFRESH_IN_PROGRESS.store(false, Ordering::Release);
            });
            let (models, empty_reason) = run_opencode_models_discovery_async().await;
            let raw_count = models.len();
            let discovered = models
                .into_iter()
                .filter(|m| m.to_lowercase().contains("free"))
                .collect::<Vec<_>>();
            let empty_reason = if discovered.is_empty() && raw_count > 0 {
                "no_free_models_in_catalog"
            } else {
                empty_reason
            };
            let cache = FREE_MODELS_CACHE.get_or_init(|| Mutex::new((0, Vec::new())));
            update_discovered_models_cache(cache, discovered, "free", empty_reason);
        };
        match Handle::try_current() {
            Ok(handle) => {
                // Use the current tokio runtime's handle to spawn the async discovery task,
                // avoiding the overhead of a std::thread + new tokio runtime.
                handle.spawn(refresh);
            }
            _ => {
                // No tokio runtime (e.g. called from a plain unit test). Spawn a thread
                // with a dedicated runtime — this is the fallback path.
                thread::spawn(move || {
                    if let Ok(rt) = Runtime::new() {
                        rt.block_on(refresh);
                    }
                });
            }
        }
    }

    // Return whatever is in cache (may be empty on first call).
    let guard = cache.lock().unwrap_or_else(PoisonError::into_inner);
    guard.1.clone()
}

/// Discover all opencode models, with 1-hour cache and background refresh.
///
/// Non-blocking: returns cached data immediately. When the cache is cold
/// or expired, spawns a background refresh via `opencode models`.
pub fn discover_opencode_models() -> Vec<String> {
    if skip_live_opencode_discovery() {
        return Vec::new();
    }
    let cache = ALL_MODELS_CACHE.get_or_init(|| Mutex::new((0, Vec::new())));
    let now = chrono::Utc::now().timestamp();

    {
        let guard = cache.lock().unwrap_or_else(PoisonError::into_inner);
        let (ts, models) = &*guard;
        if *ts != 0 && now.saturating_sub(*ts) < 3600 {
            return models.clone();
        }
    }

    if !ALL_MODELS_REFRESH_IN_PROGRESS.swap(true, Ordering::AcqRel) {
        let refresh = async {
            let _guard = scopeguard::guard((), |()| {
                ALL_MODELS_REFRESH_IN_PROGRESS.store(false, Ordering::Release);
            });
            let (discovered, empty_reason) = run_opencode_models_discovery_async().await;
            let cache = ALL_MODELS_CACHE.get_or_init(|| Mutex::new((0, Vec::new())));
            update_discovered_models_cache(cache, discovered, "all", empty_reason);
        };
        match Handle::try_current() {
            Ok(handle) => {
                handle.spawn(refresh);
            }
            _ => {
                thread::spawn(move || {
                    if let Ok(rt) = Runtime::new() {
                        rt.block_on(refresh);
                    }
                });
            }
        }
    }

    let guard = cache.lock().unwrap_or_else(PoisonError::into_inner);
    guard.1.clone()
}

/// Prime the free-model cache synchronously at startup.
///
/// Called from `Router::new()` via `spawn_blocking` so the initial
/// blocking subprocess doesn't stall the Tokio runtime.
/// Runs `run_opencode_models_discovery_async()` in a dedicated thread with its
/// own tokio runtime, avoiding `block_on` restrictions when called from within
/// an existing runtime (e.g. `#[tokio::test]` or a nested runtime context).
pub fn prime_free_model_cache() {
    if skip_live_opencode_discovery() {
        return;
    }
    let discovered_all = thread::spawn(|| match Runtime::new() {
        Ok(rt) => rt.block_on(run_opencode_models_discovery_async()).0,
        _ => {
            vec![]
        }
    })
    .join()
    .unwrap_or_default();
    let cache = FREE_MODELS_CACHE.get_or_init(|| Mutex::new((0, Vec::new())));
    let now = chrono::Utc::now().timestamp();
    if let Ok(mut guard) = cache.lock() {
        *guard = (
            now,
            discovered_all
                .iter()
                .filter(|m| m.to_lowercase().contains("free"))
                .cloned()
                .collect(),
        );
    }
    let all_cache = ALL_MODELS_CACHE.get_or_init(|| Mutex::new((0, Vec::new())));
    if let Ok(mut guard) = all_cache.lock() {
        *guard = (now, discovered_all);
    }
}

/// Async version of `run_opencode_models_discovery` that uses `tokio::time::sleep`
/// instead of blocking `std::thread::sleep`.
///
/// Returns the discovered models plus a reason code describing why the list
/// is empty (`"ok"` when discovery actually succeeded with a non-empty
/// result). The reason is threaded into `update_discovered_models_cache` so
/// the "returned empty" warning says which of the failure branches fired,
/// without requiring `RUST_LOG=debug` in production.
async fn run_opencode_models_discovery_async() -> (Vec<String>, &'static str) {
    run_opencode_models_cli(Duration::from_secs(30)).await
}

/// Run `opencode models` and reap any process-group leftovers when it returns
pub(crate) async fn run_opencode_models_cli(timeout: Duration) -> (Vec<String>, &'static str) {
    run_opencode_models_cli_inner("opencode", timeout).await
}

fn opencode_program_available(program: &str) -> bool {
    let path = Path::new(program);
    if path.is_absolute() || program.contains('/') {
        path.is_file()
    } else {
        cmd_cache::command_exists(program)
    }
}

async fn run_opencode_models_cli_inner(
    program: &str,
    timeout: Duration,
) -> (Vec<String>, &'static str) {
    if !opencode_program_available(program) {
        tracing::warn!("opencode not in PATH, skipping model discovery");
        return (vec![], "command_not_found");
    }

    let mut cmd = Command::new(program);
    cmd.args(["models"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to spawn opencode models");
            return (vec![], "spawn_failed");
        }
    };
    let pgid = child.id();
    let wait = time::timeout(timeout, child.wait_with_output()).await;

    #[cfg(unix)]
    if let Some(pid) = pgid {
        cmd::kill_process_group(pid);
    }

    match wait {
        Ok(Ok(output)) if output.status.success() => {
            let models: Vec<String> = String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect();
            let reason = if models.is_empty() {
                "empty_output"
            } else {
                "ok"
            };
            (models, reason)
        }
        Ok(Ok(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            let stderr_display = if stderr.len() > 500 {
                format!("{}...", &stderr[..stderr.floor_char_boundary(500)])
            } else {
                stderr.to_string()
            };
            tracing::warn!(
                status = ?output.status,
                stderr = %stderr_display,
                "opencode models command failed"
            );
            (vec![], "exit_status_failure")
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "failed to wait on opencode models");
            (vec![], "wait_failed")
        }
        Err(_) => {
            tracing::warn!(
                secs = timeout.as_secs(),
                "opencode models timed out, killing process group"
            );
            (vec![], "timeout")
        }
    }
}

/// Build a synthetic `AgentResponse` summarizing an opencode NDJSON event
/// stream when no `text` events are present.
///
/// Opencode runs that produced tool calls but never emitted a text event
/// (model exited mid-tool, stop_reason without a final reply, etc.) would
/// otherwise be passed to `synthesize_response_from_text` as raw NDJSON.
/// That heuristic matches "completed" substrings inside tool state JSON and
/// emits the first 500 bytes of NDJSON as the notification summary — what
/// the user sees as garbage in Telegram/Discord/Slack.
///
/// Status is `needs_review` because there is no text confirming completion;
/// the agent did work but never reported on it.
fn synthesize_response_from_events(events: &[serde_json::Value]) -> AgentResponse {
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    let mut tool_names: Vec<String> = Vec::new();
    let mut step_finish_reason: Option<String> = None;

    for event in events {
        let event_type = event
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        *counts.entry(event_type.to_string()).or_insert(0) += 1;

        if event_type == "step_finish"
            && let Some(reason) = event
                .get("part")
                .and_then(|p| p.get("reason"))
                .and_then(|r| r.as_str())
        {
            step_finish_reason = Some(reason.to_string());
        }

        // Capture tool names from common opencode/claude-style shapes.
        if event_type == "tool_use"
            && let Some(name) = event
                .get("name")
                .and_then(|v| v.as_str())
                .or_else(|| event.get("tool").and_then(|v| v.as_str()))
                .or_else(|| {
                    event
                        .get("part")
                        .and_then(|p| p.get("name"))
                        .and_then(|v| v.as_str())
                })
                .or_else(|| {
                    event
                        .get("tool")
                        .and_then(|t| t.get("name"))
                        .and_then(|v| v.as_str())
                })
        {
            tool_names.push(name.to_string());
        }
    }

    let total_events = events.len();
    let reason = step_finish_reason.as_deref().unwrap_or("unknown");

    let tool_summary = if tool_names.is_empty() {
        String::new()
    } else {
        let mut tool_counts: BTreeMap<String, u32> = BTreeMap::new();
        for n in &tool_names {
            *tool_counts.entry(n.clone()).or_insert(0) += 1;
        }
        let parts: Vec<String> = tool_counts
            .iter()
            .map(|(k, v)| {
                if *v > 1 {
                    format!("{k}×{v}")
                } else {
                    k.clone()
                }
            })
            .collect();
        format!(" (tools: {})", parts.join(", "))
    };

    let summary = format!(
        "opencode emitted {total_events} event(s) with no text output{tool_summary}; \
         step_finish reason: {reason}"
    );

    AgentResponse {
        status: "needs_review".to_string(),
        summary,
        error: Some("agent emitted no text response — only tool/step events".to_string()),
        ..Default::default()
    }
}

/// The cost opencode reports in the last `step_finish` event, if any.
fn extract_cost(events: &[serde_json::Value]) -> Option<f64> {
    events.iter().rev().find_map(|event| {
        if event.get("type").and_then(|v| v.as_str()) == Some("step_finish") {
            event
                .get("part")
                .and_then(|p| p.get("cost"))
                .and_then(serde_json::Value::as_f64)
                .filter(|&c| c > 0.0)
        } else {
            None
        }
    })
}

/// Extract a structured `AgentResult` from OpenCode NDJSON output.
///
/// Concatenates text from `type:text` events (both `part.text` and direct
/// `text` field formats), extracts tokens from the last `step_finish` event,
/// and checks for `error` / `step_finish.reason=error` events.
///
/// Returns `None` only if no parseable NDJSON events are found.
pub fn find_opencode_result(ndjson: &str) -> Option<super::AgentResult> {
    let events = super::parse_ndjson(ndjson.trim());
    if events.is_empty() {
        return None;
    }

    // Check for errors first
    let is_error;
    let error_text;
    if let Some(err) = OpenCodeRunner::detect_error(&events) {
        is_error = true;
        error_text = Some(err.to_string());
    } else {
        is_error = false;
        error_text = None;
    }

    // Extract text content
    let result_text = if is_error {
        error_text.unwrap_or_default()
    } else {
        extract_ndjson_text(&events).unwrap_or_default()
    };

    // Extract tokens from step_finish
    let (input_tokens, output_tokens) = OpenCodeRunner::extract_tokens(&events);

    // The model was cut off for exceeding its output/reasoning token budget,
    // not because it produced malformed prose. Distinguish this from a
    // generic parse failure so callers don't apply the long
    // format-incapable-model cooldown for a token-budget fluke. A reasoning
    // model that spends its entire budget on hidden reasoning emits zero
    // text and a final step_finish of reason="length" (or opencode's
    // "unknown" with all-zero tokens) — same failure mode, must be computed
    // before the empty-text early return below or it never gets checked.
    let last_step_finish_reason = events
        .iter()
        .rev()
        .find(|event| event.get("type").and_then(|v| v.as_str()) == Some("step_finish"))
        .and_then(|event| {
            event
                .get("part")?
                .get("reason")?
                .as_str()
                .map(str::to_string)
        });
    let truncated_by_length = match last_step_finish_reason.as_deref() {
        Some("length") => true,
        Some("unknown") => output_tokens == Some(0),
        _ => false,
    };

    if result_text.is_empty() && !is_error && !truncated_by_length {
        return None;
    }

    let cost_usd = extract_cost(&events);

    Some(super::AgentResult {
        is_error,
        result_text,
        input_tokens,
        output_tokens,
        cost_usd,
        duration_ms: None,
        truncated_by_length,
    })
}

/// OpenCode `--format json`: a `tool_use` event carries both the call and its finished state
fn stream_events(v: &serde_json::Value) -> Vec<StreamEvent> {
    let text = || {
        v.pointer("/part/text")
            .or_else(|| v.get("text"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    match json_str(v, "/type") {
        "text" => vec![StreamEvent::Text(text())],
        "reasoning" => vec![StreamEvent::Thinking(text())],
        "tool_use" => {
            let part = &v["part"];
            let state = &part["state"];
            let id = json_str(part, "/callID").to_string();
            let name = json_str(part, "/tool").to_string();
            let mut out = vec![StreamEvent::ToolCall {
                id: id.clone(),
                name: name.clone(),
                input: state["input"].clone(),
            }];
            let result = match json_str(state, "/status") {
                "completed" => Some((json_str(state, "/output").to_string(), false)),
                "error" => Some((json_error_message(state), true)),
                _ => None,
            };
            if let Some((output, is_error)) = result {
                out.push(StreamEvent::ToolResult {
                    id,
                    name,
                    output,
                    is_error,
                });
            }

            out
        }
        "step_finish" if json_str(v, "/part/reason") == "stop" => {
            vec![StreamEvent::Done(String::new())]
        }
        "error" => vec![StreamEvent::Error(json_error_message(v))],
        _ => vec![],
    }
}

#[cfg(test)]
mod router_text_tests;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod error_text_tests;

#[cfg(test)]
mod stream_tests;
