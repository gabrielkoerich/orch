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

use super::{AgentError, AgentRunner, ParsedResponse, PermissionRules};
use crate::parser;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

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
    if let Some(cache) = FREE_MODELS_CACHE.get() {
        if let Ok(mut guard) = cache.lock() {
            *guard = (0, Vec::new());
        }
    }
    if let Some(cache) = ALL_MODELS_CACHE.get() {
        if let Ok(mut guard) = cache.lock() {
            *guard = (0, Vec::new());
        }
    }
    FREE_MODELS_REFRESH_IN_PROGRESS.store(false, Ordering::Release);
    ALL_MODELS_REFRESH_IN_PROGRESS.store(false, Ordering::Release);
}

fn update_discovered_models_cache(
    cache: &Mutex<(i64, Vec<String>)>,
    discovered: Vec<String>,
    cache_name: &'static str,
    empty_reason: &'static str,
) {
    let now = chrono::Utc::now().timestamp();
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());

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
            if let Some(message) = event.get("message") {
                if let Some(items) = message.get("content").and_then(|v| v.as_array()) {
                    for item in items {
                        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        if item_type == "text" {
                            if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                                texts.push(text.to_string());
                            }
                        }
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
            if let Some(text) = event.get("text").and_then(|v| v.as_str()) {
                if !text.trim().is_empty() {
                    return Some(text.to_string());
                }
            }
        }

        return None;
    }

    let full = texts.join("");
    if let Some(json) = extract_json_object(&full) {
        // Prefer a full-json payload that parses as an AgentResponse.
        if crate::parser::parse(&json).is_ok() {
            return Some(json);
        }

        // Also accept JSON that looks like a router decision / agent envelope
        // even when it's not a full AgentResponse (e.g. {"executor":"opencode"}).
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&json) {
            if let Some(obj) = val.as_object() {
                if obj.contains_key("executor")
                    || obj.contains_key("status")
                    || obj.contains_key("complexity")
                    || obj.contains_key("decision")
                {
                    return Some(json);
                }
            }
        }
        // Otherwise fall through and try per-event heuristics below
    }

    for text in texts.iter().rev() {
        let trimmed = text.trim();
        if let Some(json) = extract_json_object(trimmed) {
            // Prefer JSON blobs that parse as AgentResponse. This allows
            // accepting payloads that don't include routing keys but are
            // nevertheless valid task responses.
            if crate::parser::parse(&json).is_ok() {
                return Some(json);
            }

            // Also accept routing-style JSON objects that don't match the
            // AgentResponse schema (router decisions, etc.).
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&json) {
                if let Some(obj) = val.as_object() {
                    if obj.contains_key("executor") || obj.contains_key("status") {
                        return Some(json);
                    }
                }
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
            if crate::parser::parse(trimmed).is_ok() {
                return Some(trimmed.to_string());
            }

            if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
                if let Some(obj) = val.as_object() {
                    if obj.contains_key("executor")
                        || obj.contains_key("status")
                        || obj.contains_key("complexity")
                    {
                        return Some(trimmed.to_string());
                    }
                }
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
            if item_type == "text" {
                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                    out.push(text.to_string());
                }
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

#[cfg(test)]
mod router_text_tests {
    use super::*;

    #[test]
    fn extract_router_text_supports_message_with_content_array() {
        // Some opencode versions nest assistant text in a message.content array.
        let raw = r#"{"type":"step_start","timestamp":1}
{"type":"message","timestamp":2,"part":{"type":"message","content":[{"type":"text","text":"```json\n{\"executor\":\"opencode\",\"complexity\":\"medium\",\"reason\":\"ok\"}\n```"}]}}
{"type":"step_finish","timestamp":3,"part":{"type":"step-finish","reason":"stop"}}"#;
        let text = extract_router_text(raw).expect("should extract text");
        assert!(
            text.contains("executor"),
            "extracted text should include JSON"
        );
    }

    #[test]
    fn extract_router_text_ignores_leading_system_init() {
        // Early opencode wrappers may emit a system/init envelope before the
        // actual text/result event. Ensure the extractor skips the init envelope
        // and returns the real JSON payload from the subsequent text event.
        let raw = r#"{"type":"system","subtype":"init","cwd":"/"}
{"type":"text","timestamp":2,"text":"{\"executor\":\"opencode\",\"complexity\":\"medium\",\"reason\":\"ndjson\"}"}
{"type":"step_finish","timestamp":3}"#;

        let text = extract_router_text(raw).expect("should extract text even with leading init");
        let parsed: serde_json::Value =
            serde_json::from_str(&text).expect("extracted text must be JSON");
        assert_eq!(parsed["executor"], "opencode");
        assert_eq!(parsed["complexity"], "medium");
    }
}

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
        super::patterns::detect_ndjson_error(
            events,
            |event| {
                let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match event_type {
                    "error" => {
                        // OpenCode error events have multiple shapes:
                        // 1. {"type":"error","message":"..."}
                        // 2. {"type":"error","error":"string message"}
                        // 3. {"type":"error","error":{"name":"...","data":{"message":"..."}}}
                        Some(
                            event
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
                                    event
                                        .get("error")
                                        .and_then(|e| e.get("name"))
                                        .and_then(|n| n.as_str())
                                })
                                .unwrap_or("unknown error")
                                .to_string(),
                        )
                    }
                    "step_finish" => {
                        let part = event.get("part")?;
                        let reason = part.get("reason").and_then(|v| v.as_str()).unwrap_or("");
                        if reason == "error" || reason == "failed" {
                            Some(
                                part.get("error")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("step failed")
                                    .to_string(),
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

        let events = super::parse_ndjson(trimmed);
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
        // Support optional variant suffix on opencode model identifiers:
        // provider/model[@variant]
        let (model_flag, variant_flag) = if let Some(m) = model {
            let (base, variant) = Self::split_model_variant(m);
            let mf = format!("--model {}", super::shell_single_quote(base));
            let vf = variant.map(|v| format!("--variant {}", super::shell_single_quote(v)));
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

        // Append variant flag when present
        let variant_part = variant_flag.unwrap_or_default();
        // Feed both files via process substitution rather than a leading `cat |`
        // pipe stage — a leading pipe would make `cat`, not opencode, the first
        // stage in the runner script's pipeline, so PIPESTATUS[0] there would
        // always report cat's (always-zero) exit code instead of opencode's.
        format!(
            r#"{timeout_cmd} opencode run {model_flag} {variant_part} {skip_perms}\
  --format json - < <(cat "{sys_file}" "{msg_file}")"#,
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

        // Extract tokens (needed for both the text path and the tool-only fallback)
        let (input_tokens, output_tokens) = self.extract_tokens(&events);

        // Extract text — when opencode emits only tool_use/step events with no
        // text content, synthesize a human-readable summary from the event
        // stream instead of falling through to InvalidResponse. Otherwise the
        // upstream `synthesize_response_from_text` heuristic gets handed the
        // entire raw NDJSON, matches a "completed" substring inside tool state
        // JSON, and emits 500 bytes of raw NDJSON as the task summary.
        let text = match self.extract_text_from_events(&events) {
            Some(t) => t,
            None => {
                let response = synthesize_response_from_events(&events);
                return Ok(ParsedResponse {
                    response,
                    input_tokens,
                    output_tokens,
                    duration_ms: None,
                });
            }
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
            duration_ms: None,
        })
    }

    fn classify_error(&self, exit_code: i32, stdout: &str, stderr: &str) -> AgentError {
        // Try parsing NDJSON events from stdout for structured errors
        let events = super::parse_ndjson(stdout);
        if let Some(err) = self.detect_error(&events) {
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

    fn router_command(
        &self,
        prompt: &str,
        model: Option<&str>,
    ) -> anyhow::Result<tokio::process::Command> {
        // Routing only needs text output — no tools fire.
        // `--dangerously-skip-permissions` prevents the user's global
        // ~/.config/opencode/opencode.json interactive defaults (e.g.
        // "edit":"ask") from producing PermissionError events that the router
        // would misclassify as model failures.
        let mut cmd = tokio::process::Command::new("opencode");
        cmd.arg("run")
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
    let is_model_unavailable = (lower.contains("model")
        && (lower.contains("not found")
            || lower.contains("not supported")
            || lower.contains("deprecated")
            || lower.contains("unavailable for free")))
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
    let cache = FREE_MODELS_CACHE.get_or_init(|| Mutex::new((0, Vec::new())));

    let now = chrono::Utc::now().timestamp();
    // Fast path: return cached data if still fresh (1-hour TTL).
    {
        let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        let (ts, models) = &*guard;
        if *ts != 0 && now.saturating_sub(*ts) < 3600 {
            return models.clone();
        }
    }

    // Cache is cold or expired. Spawn a background task on the current runtime to refresh.
    // Only one refresh runs at a time.
    if !FREE_MODELS_REFRESH_IN_PROGRESS.swap(true, Ordering::AcqRel) {
        let refresh = async {
            let _guard = scopeguard::guard((), |_| {
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
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            // Use the current tokio runtime's handle to spawn the async discovery task,
            // avoiding the overhead of a std::thread + new tokio runtime.
            handle.spawn(refresh);
        } else {
            // No tokio runtime (e.g. called from a plain unit test). Spawn a thread
            // with a dedicated runtime — this is the fallback path.
            std::thread::spawn(move || {
                if let Ok(rt) = tokio::runtime::Runtime::new() {
                    rt.block_on(refresh);
                }
            });
        }
    }

    // Return whatever is in cache (may be empty on first call).
    let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    guard.1.clone()
}

/// Discover all opencode models, with 1-hour cache and background refresh.
///
/// Non-blocking: returns cached data immediately. When the cache is cold
/// or expired, spawns a background refresh via `opencode models`.
pub fn discover_opencode_models() -> Vec<String> {
    let cache = ALL_MODELS_CACHE.get_or_init(|| Mutex::new((0, Vec::new())));
    let now = chrono::Utc::now().timestamp();

    {
        let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        let (ts, models) = &*guard;
        if *ts != 0 && now.saturating_sub(*ts) < 3600 {
            return models.clone();
        }
    }

    if !ALL_MODELS_REFRESH_IN_PROGRESS.swap(true, Ordering::AcqRel) {
        let refresh = async {
            let _guard = scopeguard::guard((), |_| {
                ALL_MODELS_REFRESH_IN_PROGRESS.store(false, Ordering::Release);
            });
            let (discovered, empty_reason) = run_opencode_models_discovery_async().await;
            let cache = ALL_MODELS_CACHE.get_or_init(|| Mutex::new((0, Vec::new())));
            update_discovered_models_cache(cache, discovered, "all", empty_reason);
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(refresh);
        } else {
            std::thread::spawn(move || {
                if let Ok(rt) = tokio::runtime::Runtime::new() {
                    rt.block_on(refresh);
                }
            });
        }
    }

    let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
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
    let discovered_all = std::thread::spawn(|| {
        if let Ok(rt) = tokio::runtime::Runtime::new() {
            rt.block_on(run_opencode_models_discovery_async()).0
        } else {
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
    if !crate::cmd_cache::command_exists("opencode") {
        tracing::warn!("opencode not in PATH — skipping model discovery");
        return (vec![], "command_not_found");
    }

    let mut child = match tokio::process::Command::new("opencode")
        .args(["models"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to spawn opencode models");
            return (vec![], "spawn_failed");
        }
    };

    let timeout = std::time::Duration::from_secs(30);
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                // Use tokio's async read to read stdout
                let stdout = match child.stdout.take() {
                    Some(mut s) => {
                        let mut buf = String::new();
                        use tokio::io::AsyncReadExt;
                        let _ = s.read_to_string(&mut buf).await.ok();
                        buf
                    }
                    None => String::new(),
                };
                let models: Vec<String> = stdout
                    .lines()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .collect();
                let reason = if models.is_empty() {
                    "empty_output"
                } else {
                    "ok"
                };
                return (models, reason);
            }
            Ok(Some(status)) => {
                tracing::warn!(?status, "opencode models command failed");
                return (vec![], "exit_status_failure");
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    tracing::warn!("opencode models timed out after 30s, killing process");
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    return (vec![], "timeout");
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to wait on opencode models");
                let _ = child.kill().await;
                return (vec![], "wait_failed");
            }
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
fn synthesize_response_from_events(events: &[serde_json::Value]) -> crate::parser::AgentResponse {
    use std::collections::BTreeMap;

    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    let mut tool_names: Vec<String> = Vec::new();
    let mut step_finish_reason: Option<String> = None;

    for event in events {
        let event_type = event
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        *counts.entry(event_type.to_string()).or_insert(0) += 1;

        if event_type == "step_finish" {
            if let Some(reason) = event
                .get("part")
                .and_then(|p| p.get("reason"))
                .and_then(|r| r.as_str())
            {
                step_finish_reason = Some(reason.to_string());
            }
        }

        // Capture tool names from common opencode/claude-style shapes.
        if event_type == "tool_use" {
            if let Some(name) = event
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

    crate::parser::AgentResponse {
        status: "needs_review".to_string(),
        summary,
        error: Some("agent emitted no text response — only tool/step events".to_string()),
        ..Default::default()
    }
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
    let runner = OpenCodeRunner::new();
    if let Some(err) = runner.detect_error(&events) {
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
    let (input_tokens, output_tokens) = runner.extract_tokens(&events);

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

    // Extract cost from step_finish if available
    let cost_usd = events.iter().rev().find_map(|event| {
        if event.get("type").and_then(|v| v.as_str()) == Some("step_finish") {
            event
                .get("part")
                .and_then(|p| p.get("cost"))
                .and_then(|c| c.as_f64())
                .filter(|&c| c > 0.0)
        } else {
            None
        }
    });

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
    fn parse_opencode_embedded_json_fragment_without_executor_is_ignored() {
        // NDJSON with a text event that contains a JSON-like fragment which does
        // NOT include routing keys (executor/status). This should NOT be
        // interpreted as a routing payload; parse_response should fail with
        // InvalidResponse rather than accidentally accepting the fragment.
        let raw = r#"{"type":"step_start","timestamp":1}
{"type":"text","timestamp":2,"part":{"type":"text","text":"Here is a debug trace: {\"trace\":{\"duration\":123,\"id\":\"abc\"}}"}}
{"type":"step_finish","timestamp":3,"part":{"type":"step-finish","reason":"stop"}}"#;

        let err = runner().parse_response(raw).unwrap_err();
        match err {
            AgentError::InvalidResponse { raw: _ } => {
                // expected
            }
            other => panic!("expected InvalidResponse, got: {other:?}"),
        }
    }

    #[test]
    fn parse_opencode_error_event() {
        let raw = r#"{"type":"error","message":"rate limit exceeded for this model"}"#;

        let err = runner().parse_response(raw).unwrap_err();
        assert!(matches!(err, AgentError::RateLimit { .. }));
    }

    /// Regression: opencode runs that emit only tool_use / step_start / step_finish
    /// events (no `text` events) must synthesize a clean needs_review response from
    /// the event stream instead of returning InvalidResponse with the raw NDJSON
    /// blob, which would otherwise be passed to synthesize_response_from_text and
    /// leak as 500 bytes of raw JSON into the notification summary.
    #[test]
    fn parse_opencode_tool_only_stream_synthesizes_needs_review() {
        let raw = r#"{"type":"step_start","timestamp":1,"part":{"type":"step-start"}}
{"type":"tool_use","timestamp":2,"name":"bash","input":{"cmd":"ls"}}
{"type":"tool_use","timestamp":3,"name":"read","input":{"path":"src/main.rs"}}
{"type":"tool_use","timestamp":4,"name":"bash","input":{"cmd":"cargo check"}}
{"type":"step_finish","timestamp":5,"part":{"type":"step-finish","reason":"stop","tokens":{"input":500,"output":0}}}"#;

        let parsed = runner()
            .parse_response(raw)
            .expect("tool-only NDJSON must synthesize a response, not error");
        assert_eq!(parsed.response.status, "needs_review");
        assert!(
            !parsed.response.summary.contains("\"type\":\"step_start\""),
            "summary must not contain raw NDJSON; got: {}",
            parsed.response.summary
        );
        assert!(
            parsed.response.summary.contains("bash") && parsed.response.summary.contains("read"),
            "summary should mention tools used; got: {}",
            parsed.response.summary
        );
        assert!(
            parsed.response.summary.contains("stop"),
            "summary should mention step_finish reason; got: {}",
            parsed.response.summary
        );
        // Tokens from step_finish must still be propagated.
        assert_eq!(parsed.input_tokens, Some(500));
        assert_eq!(parsed.output_tokens, Some(0));
    }

    /// Regression test for issue #3467: when opencode's `text` event contains
    /// a plain-prose completion summary (not JSON), parser::parse fails but
    /// synthesize_response_from_text must rescue it instead of returning
    /// InvalidResponse, mirroring the fallback claude.rs already has.
    #[test]
    fn parse_opencode_plain_prose_completion_synthesizes_response() {
        let raw = r#"{"type":"step_start","timestamp":1000,"part":{"type":"step-start"}}
{"type":"text","timestamp":1001,"part":{"type":"text","text":"Fixed the typo and corrected the deadline reference in the review."}}
{"type":"step_finish","timestamp":1002,"part":{"type":"step-finish","reason":"stop","tokens":{"input":300,"output":40}}}"#;

        let parsed = runner()
            .parse_response(raw)
            .expect("plain-prose completion text must synthesize successfully");
        assert_eq!(parsed.response.status, "done");
        assert_eq!(parsed.input_tokens, Some(300));
        assert_eq!(parsed.output_tokens, Some(40));
    }

    #[test]
    fn parse_opencode_step_finish_error() {
        let raw = r#"{"type":"step_start","timestamp":1000,"part":{"type":"step-start"}}
{"type":"step_finish","timestamp":1001,"part":{"type":"step-finish","reason":"error","error":"context length exceeded"}}"#;

        let err = runner().parse_response(raw).unwrap_err();
        assert!(matches!(err, AgentError::ContextOverflow { .. }));
    }

    #[test]
    fn parse_opencode_empty_returns_silent_failure() {
        let err = runner().parse_response("").unwrap_err();
        // Empty output with exit 0 is treated as silent failure so fallback.rs
        // can apply model cooldown and retry with free models
        assert!(matches!(err, AgentError::Unknown { exit_code: 0, .. }));
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
    fn classify_opencode_model_not_found_with_suggestion() {
        // Issue #1934: opencode returns "Model not found: X. Did you mean: Y?"
        // The parser must extract "X", not "X. Did you mean"
        let err = classify_opencode_message(
            "Model not found: github-copilot/gemini-3.1-pro. Did you mean: gemini-3.1-pro-preview?",
        );
        match err {
            AgentError::ModelUnavailable { model, message } => {
                assert_eq!(model, "github-copilot/gemini-3.1-pro");
                assert!(message.contains("Did you mean"));
            }
            other => panic!("expected ModelUnavailable, got: {other:?}"),
        }
    }

    #[test]
    fn classify_opencode_model_deprecated() {
        // Issue #2228: "The free model has been deprecated." should be ModelUnavailable
        // with the model extracted from "Transition to X" so the cooldown key is non-empty.
        let err = classify_opencode_message(
            "The free model has been deprecated. Transition to qwen/qwen3.6-plus for continued paid access.",
        );
        match err {
            AgentError::ModelUnavailable { model, .. } => {
                assert_eq!(
                    model, "qwen/qwen3.6-plus",
                    "expected model extracted from 'Transition to X', got: {model:?}"
                );
            }
            other => panic!("expected ModelUnavailable, got: {other:?}"),
        }
    }

    #[test]
    fn classify_opencode_model_unavailable_for_free() {
        // Issue #3475: "This model is unavailable for free ... use this slug instead: X"
        // must classify as ModelUnavailable, not fall through to the transient
        // network-error path via the "Upstream request failed" substring.
        let err = classify_opencode_message(
            "network error: Error from provider (Console): Upstream request failed: [404] This model is unavailable for free. The paid version is available now - use this slug instead: inclusionai/ling-3.0-flash",
        );
        match err {
            AgentError::ModelUnavailable { model, .. } => {
                assert_eq!(
                    model, "inclusionai/ling-3.0-flash",
                    "expected model extracted from 'use this slug instead: X', got: {model:?}"
                );
            }
            other => panic!("expected ModelUnavailable, got: {other:?}"),
        }
    }

    #[test]
    fn classify_opencode_no_endpoints_found() {
        // Issue #2228: "No endpoints found for X." should be ModelUnavailable with model extracted
        let err = classify_opencode_message("No endpoints found for qwen/qwen3.6-plus:free.");
        match err {
            AgentError::ModelUnavailable { model, .. } => {
                assert_eq!(model, "qwen/qwen3.6-plus:free");
            }
            other => panic!("expected ModelUnavailable, got: {other:?}"),
        }
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
        // Autonomous mode passes --dangerously-skip-permissions instead of
        // writing a config file or overriding XDG_CONFIG_HOME.
        assert!(
            cmd.contains("--dangerously-skip-permissions"),
            "expected --dangerously-skip-permissions, got: {cmd}"
        );
        assert!(
            !cmd.contains("opencode.json"),
            "should not write a config file, got: {cmd}"
        );
        assert!(
            !cmd.contains("XDG_CONFIG_HOME"),
            "should not override XDG_CONFIG_HOME, got: {cmd}"
        );
        assert!(
            !cmd.contains(".orch-opencode"),
            "should not reference .orch-opencode, got: {cmd}"
        );
        // Regression: the command must not start with a `cat sys msg |` pipe
        // stage. `build_runner_script` captures the agent's exit status via
        // `PIPESTATUS[0]` of `{agent_cmd} | tee ...` — a leading `cat |` would
        // make PIPESTATUS[0] always report cat's (always-zero) exit code
        // instead of opencode's real exit code. sys/msg files must be fed via
        // input redirection instead.
        assert!(
            !cmd.trim_start().starts_with("cat "),
            "opencode command must not start with a `cat |` pipe stage, got: {cmd}"
        );
        assert!(
            cmd.contains("< <(cat \"/tmp/sys.txt\" \"/tmp/msg.txt\")"),
            "sys/msg files must be fed via process substitution, got: {cmd}"
        );
    }

    #[test]
    fn build_command_opencode_supervised_no_skip_flag() {
        let r = runner();
        let perms = PermissionRules {
            autonomous: false,
            ..PermissionRules::default()
        };
        let cmd = r.build_command(
            Some("anthropic/claude-sonnet-4-20250514"),
            "timeout 1800",
            "/tmp/sys.txt",
            "/tmp/msg.txt",
            &perms,
        );
        assert!(
            !cmd.contains("--dangerously-skip-permissions"),
            "supervised mode must not pass --dangerously-skip-permissions, got: {cmd}"
        );
    }

    #[test]
    fn split_model_variant_no_at() {
        let (base, variant) = OpenCodeRunner::split_model_variant("openai/gpt-5.5");
        assert_eq!(base, "openai/gpt-5.5");
        assert!(variant.is_none());
    }

    #[test]
    fn split_model_variant_with_variant() {
        let (base, variant) = OpenCodeRunner::split_model_variant("openai/gpt-5.5@xhigh");
        assert_eq!(base, "openai/gpt-5.5");
        assert_eq!(variant, Some("xhigh"));
    }

    #[test]
    fn split_model_variant_trailing_at_empty() {
        let (base, variant) = OpenCodeRunner::split_model_variant("openai/gpt-5.5@");
        assert_eq!(base, "openai/gpt-5.5");
        assert_eq!(variant, Some(""));
    }

    #[test]
    fn split_model_variant_multiple_ats_last_wins() {
        let (base, variant) = OpenCodeRunner::split_model_variant("a@b@c@v");
        assert_eq!(base, "a@b@c");
        assert_eq!(variant, Some("v"));
    }

    #[test]
    fn build_command_opencode_with_variant() {
        let r = runner();
        let perms = PermissionRules::default();
        let cmd = r.build_command(
            Some("openai/gpt-5.5@high"),
            "timeout 1800",
            "/tmp/sys.txt",
            "/tmp/msg.txt",
            &perms,
        );
        assert!(cmd.contains("--model 'openai/gpt-5.5'"));
        assert!(cmd.contains("--variant 'high'"));
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
    fn classify_opencode_provider_returned_error() {
        // Issue #2478: "Provider returned error" is an opaque upstream failure from the
        // opencode CLI. Classify as RateLimit so fallback records a rate-limit signal
        // and applies cooldown behavior tuned for transient provider instability.
        let err = classify_opencode_message("Provider returned error");
        assert!(
            matches!(err, AgentError::RateLimit { .. }),
            "expected RateLimit, got: {err:?}"
        );
        let err_with_detail =
            classify_opencode_message("Provider returned error: upstream connection timed out");
        assert!(
            matches!(err_with_detail, AgentError::RateLimit { .. }),
            "expected RateLimit, got: {err_with_detail:?}"
        );
    }

    #[test]
    fn classify_opencode_upstream_idle_timeout() {
        // Issue #3301: "Upstream idle timeout exceeded" must produce NetworkError so
        // fallback.rs applies a model-level cooldown instead of retrying immediately.
        let err = classify_opencode_message("Upstream idle timeout exceeded");
        assert!(
            matches!(err, AgentError::NetworkError { .. }),
            "expected NetworkError, got: {err:?}"
        );
    }

    #[test]
    fn free_models_discovery_returns_vec() {
        // discover_free_opencode_models returns empty when cache is cold
        // and opencode isn't installed (no hardcoded fallbacks)
        let models = discover_free_opencode_models();
        // Just verify it returns without panicking — actual content
        // depends on whether opencode is installed and responsive
        assert!(models.len() < 1000, "sanity check");
    }

    #[test]
    fn empty_free_discovery_preserves_existing_cache_contents() {
        reset_model_caches_for_test();
        let cache = FREE_MODELS_CACHE.get_or_init(|| Mutex::new((0, Vec::new())));
        {
            let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
            *guard = (123, vec!["opencode/model-free".to_string()]);
        }

        update_discovered_models_cache(cache, Vec::new(), "free", "empty_output");

        let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        // Timestamp must advance on a failed refresh so the TTL provides a
        // real backoff window instead of retrying on every subsequent call.
        assert!(guard.0 > 123);
        assert_eq!(guard.1, vec!["opencode/model-free".to_string()]);
    }

    #[test]
    fn empty_all_models_discovery_preserves_existing_cache_contents() {
        reset_model_caches_for_test();
        let cache = ALL_MODELS_CACHE.get_or_init(|| Mutex::new((0, Vec::new())));
        {
            let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
            *guard = (
                456,
                vec![
                    "opencode/model-free".to_string(),
                    "opencode/model-paid".to_string(),
                ],
            );
        }

        update_discovered_models_cache(cache, Vec::new(), "all", "exit_status_failure");

        let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        assert!(guard.0 > 456);
        assert_eq!(
            guard.1,
            vec![
                "opencode/model-free".to_string(),
                "opencode/model-paid".to_string(),
            ]
        );
    }

    #[test]
    fn empty_discovery_populates_empty_cache() {
        reset_model_caches_for_test();
        let cache = FREE_MODELS_CACHE.get_or_init(|| Mutex::new((0, Vec::new())));

        update_discovered_models_cache(cache, Vec::new(), "free", "command_not_found");

        let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        assert!(guard.0 > 0);
        assert!(guard.1.is_empty());
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

    /// Real failure: OpenCode returns "Provider returned error" as an opaque upstream
    /// provider failure. Classified as RateLimit so degraded/rate-limit tracking applies.
    #[test]
    fn fixture_opencode_provider_returned_error() {
        let raw = include_str!("../../../../tests/fixtures/opencode_provider_returned_error.jsonl");
        let err = runner().parse_response(raw).unwrap_err();
        assert!(
            matches!(err, AgentError::RateLimit { .. }),
            "expected RateLimit, got: {err:?}"
        );
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

    // ── find_opencode_result ────────────────────────────────────

    #[test]
    fn find_opencode_result_success_with_tokens() {
        let ndjson = concat!(
            r#"{"type":"step_start","timestamp":1000,"part":{"type":"step-start"}}"#,
            "\n",
            r#"{"type":"text","timestamp":1001,"part":{"type":"text","text":"{\"decision\":\"approve\",\"notes\":\"LGTM\"}"}}"#,
            "\n",
            r#"{"type":"step_finish","timestamp":1002,"part":{"type":"step-finish","reason":"stop","cost":0.05,"tokens":{"input":5000,"output":200}}}"#,
        );
        let result = find_opencode_result(ndjson).expect("should find result");
        assert!(!result.is_error);
        assert!(result.result_text.contains("approve"));
        assert_eq!(result.input_tokens, Some(5000));
        assert_eq!(result.output_tokens, Some(200));
        assert_eq!(result.cost_usd, Some(0.05));
        assert!(!result.truncated_by_length);
    }

    #[test]
    fn find_opencode_result_error_event() {
        let ndjson = r#"{"type":"error","message":"rate limit exceeded: 429"}"#;
        let result = find_opencode_result(ndjson).expect("should find error result");
        assert!(result.is_error);
        assert!(result.result_text.contains("rate limit"));
        assert!(!result.truncated_by_length);
    }

    /// step_finish reason=length signals the model was cut off by its
    /// output/reasoning token budget, not malformed output — the flag must
    /// be set so callers avoid the persistent-format-failure cooldown.
    #[test]
    fn find_opencode_result_length_truncation() {
        let ndjson = concat!(
            r#"{"type":"step_start","timestamp":1000,"part":{"type":"step-start"}}"#,
            "\n",
            r#"{"type":"text","timestamp":1001,"part":{"type":"text","text":"{\"decision\":"}}"#,
            "\n",
            r#"{"type":"step_finish","timestamp":1002,"part":{"type":"step-finish","reason":"length","tokens":{"input":5485,"output":759,"reasoning":31241}}}"#,
        );
        let result = find_opencode_result(ndjson).expect("should find result");
        assert!(result.truncated_by_length);
        assert!(!result.is_error, "length truncation is not a hard error");
    }

    /// A reasoning model can burn its entire budget on hidden reasoning and
    /// emit zero text before step_finish reason=length. The old early return
    /// on empty result_text bailed out with None before truncated_by_length
    /// was ever computed, so this got misclassified as parse_error and hit
    /// the 4h-7d persistent-model cooldown instead of standard backoff.
    #[test]
    fn find_opencode_result_zero_output_length_truncation() {
        let ndjson = concat!(
            r#"{"type":"step_start","timestamp":1000,"part":{"type":"step-start"}}"#,
            "\n",
            r#"{"type":"step_finish","timestamp":1002,"part":{"type":"step-finish","reason":"length","tokens":{"total":71482,"input":39482,"output":0,"reasoning":40000}}}"#,
        );
        let result = find_opencode_result(ndjson).expect("should find result, not None");
        assert!(result.truncated_by_length);
        assert!(!result.is_error);
        assert!(result.result_text.is_empty());
    }

    /// opencode also emits reason="unknown" with all-zero tokens for the same
    /// zero-output failure mode — must be treated as an equivalent signal.
    #[test]
    fn find_opencode_result_zero_output_unknown_reason_truncation() {
        let ndjson = concat!(
            r#"{"type":"step_start","timestamp":1000,"part":{"type":"step-start"}}"#,
            "\n",
            r#"{"type":"step_finish","timestamp":1002,"part":{"type":"step-finish","reason":"unknown","tokens":{"input":0,"output":0,"reasoning":0}}}"#,
        );
        let result = find_opencode_result(ndjson).expect("should find result, not None");
        assert!(result.truncated_by_length);
        assert!(!result.is_error);
        assert!(result.result_text.is_empty());
    }

    /// reason="unknown" with nonzero output tokens means the model did
    /// produce something and the step just wasn't cleanly terminated — not
    /// the same "wrote nothing" signal, so it must not be flagged.
    #[test]
    fn find_opencode_result_unknown_reason_with_output_not_truncated() {
        let ndjson = concat!(
            r#"{"type":"step_start","timestamp":1000,"part":{"type":"step-start"}}"#,
            "\n",
            r#"{"type":"text","timestamp":1001,"part":{"type":"text","text":"partial answer"}}"#,
            "\n",
            r#"{"type":"step_finish","timestamp":1002,"part":{"type":"step-finish","reason":"unknown","tokens":{"input":100,"output":50,"reasoning":0}}}"#,
        );
        let result = find_opencode_result(ndjson).expect("should find result");
        assert!(!result.truncated_by_length);
        assert!(!result.is_error);
    }

    #[test]
    fn find_opencode_result_empty_returns_none() {
        assert!(find_opencode_result("").is_none());
        assert!(find_opencode_result("   ").is_none());
    }

    #[test]
    fn find_opencode_result_direct_text_format() {
        let ndjson = concat!(
            r#"{"type":"step_start","timestamp":1000}"#,
            "\n",
            r#"{"type":"text","timestamp":1001,"text":"Review complete. All tests passed."}"#,
            "\n",
            r#"{"type":"step_finish","timestamp":1002,"part":{"type":"step-finish","reason":"stop","tokens":{"input":100,"output":10}}}"#,
        );
        let result = find_opencode_result(ndjson).expect("should find result");
        assert!(!result.is_error);
        assert!(result.result_text.contains("All tests passed"));
    }

    #[test]
    fn find_opencode_result_plain_text_returns_none() {
        assert!(find_opencode_result("just some plain text").is_none());
    }
}
