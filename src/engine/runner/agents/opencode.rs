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

use std::sync::{Arc, Mutex};

type FreeModelsCache = Arc<Mutex<Option<(Vec<String>, std::time::Instant)>>>;

/// Runner for OpenCode agent.
pub struct OpenCodeRunner {
    /// Cached free models (model list + timestamp).
    free_models_cache: FreeModelsCache,
    /// True while a background thread is refreshing the free-models cache.
    /// Guards against spawning duplicate refresh threads.
    free_models_refresh_in_progress: Arc<std::sync::atomic::AtomicBool>,
}

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
        Self {
            free_models_cache: Arc::new(Mutex::new(None)),
            free_models_refresh_in_progress: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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

    /// Return the cached free-model list, refreshing in the background when stale.
    ///
    /// This method is intentionally non-blocking: it never calls the subprocess
    /// directly. When the cache is cold or expired it spawns a `std::thread` to
    /// run `discover_free_models()` in the background and returns the known
    /// defaults immediately. This avoids stalling a Tokio worker thread with
    /// blocking subprocess I/O when called from an async context.
    ///
    /// See: <https://github.com/gabrielkoerich/orch/issues/1473>
    fn discover_free_models_cached(&self) -> Vec<String> {
        // Fast path: return cached data if still fresh.
        {
            let cache = self
                .free_models_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some((ref models, ref ts)) = *cache {
                if ts.elapsed() < std::time::Duration::from_secs(3600) {
                    return models.clone();
                }
            }
        }

        // Cache is cold. Spawn a background thread to refresh it without
        // blocking the calling (async) thread. Only one refresh runs at a time.
        if !self
            .free_models_refresh_in_progress
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            let cache_arc = Arc::clone(&self.free_models_cache);
            let flag_arc = Arc::clone(&self.free_models_refresh_in_progress);
            std::thread::spawn(move || {
                let _guard = scopeguard::guard((), |_| {
                    flag_arc.store(false, std::sync::atomic::Ordering::Release);
                });
                let models = discover_free_models();
                if let Ok(mut cache) = cache_arc.lock() {
                    *cache = Some((models, std::time::Instant::now()));
                }
            });
        }

        // Return known defaults while the background refresh is in progress.
        known_free_models()
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
            let sq_permission_json = super::shell_single_quote(&permission_json);
            format!(
                r#"mkdir -p .orch-opencode/opencode || {{ printf '%s\n' 'failed to create opencode config directory: .orch-opencode/opencode' >&2; exit 1; }}
printf '%s\n' {sq_permission_json} > .orch-opencode/opencode/opencode.json || {{ printf '%s\n' 'failed to write opencode config: .orch-opencode/opencode/opencode.json' >&2; exit 1; }}
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
        let events = super::parse_ndjson(stdout);
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
        // Isolate opencode from the user's global config so that interactive
        // permission defaults (e.g. "edit":"ask") don't trigger PermissionError
        // events that the router misclassifies as model failures and applies
        // unnecessary cooldowns to.  We write a deny-all permission config to a
        // stable temp directory (reused across routing calls) and point
        // XDG_CONFIG_HOME at it.
        let config_dir = ensure_router_opencode_config()?;

        // When XDG_CONFIG_HOME is overridden, gh CLI would also look for its
        // config under the new home and fail to authenticate.  GH_CONFIG_DIR pins
        // gh to its actual config regardless of XDG_CONFIG_HOME.
        let gh_config_dir = std::env::var("GH_CONFIG_DIR")
            .ok()
            .or_else(|| {
                dirs::home_dir().map(|h| h.join(".config").join("gh").display().to_string())
            })
            .unwrap_or_else(|| String::from("~/.config/gh"));

        let mut cmd = tokio::process::Command::new("opencode");
        cmd.arg("run").arg("--format").arg("json");
        if let Some(m) = model {
            cmd.arg("--model").arg(m);
        }
        cmd.arg(prompt);
        cmd.env("XDG_CONFIG_HOME", &config_dir);
        cmd.env("GH_CONFIG_DIR", &gh_config_dir);
        Ok(cmd)
    }
}

/// A deny-all opencode permission config used for routing invocations.
///
/// The router only needs text output — it should never exercise any tools.
/// Denying all tools prevents interactive permission prompts (e.g. "edit":"ask"
/// from the user's global config) from producing PermissionError events that
/// the router misidentifies as model failures.
///
/// `external_directory` is kept "allow" for consistency with task-agent configs;
/// the router doesn't use it, but leaving it "deny" would be harmless too.
const ROUTER_DENY_ALL_CONFIG: &str = r#"{"permission":{"read":"deny","edit":"deny","glob":"deny","grep":"deny","bash":"deny","task":"deny","skill":"deny","webfetch":"deny","websearch":"deny","list":"deny","todowrite":"deny","todoread":"deny","question":"deny","codesearch":"deny","external_directory":"allow"}}"#;

/// Return the path to the router opencode config directory, creating it and
/// writing the deny-all config if needed.
///
/// Uses a stable path under the system temp directory so it persists across
/// routing calls without needing a `TempDir` lifetime.  The directory and file
/// are created idempotently — concurrent calls are safe because `write` is
/// atomic on every supported platform for small files.
fn ensure_router_opencode_config() -> anyhow::Result<std::path::PathBuf> {
    let dir = std::env::temp_dir()
        .join("orch-router-opencode")
        .join("opencode");
    std::fs::create_dir_all(&dir).map_err(|e| {
        anyhow::anyhow!(
            "failed to create router opencode config dir {}: {e}",
            dir.display()
        )
    })?;

    let config_file = dir.join("opencode.json");
    std::fs::write(&config_file, ROUTER_DENY_ALL_CONFIG).map_err(|e| {
        anyhow::anyhow!(
            "failed to write router opencode config {}: {e}",
            config_file.display()
        )
    })?;

    // Return the parent of the "opencode" subdirectory — that is the value to
    // set as XDG_CONFIG_HOME so opencode finds opencode/opencode.json inside it.
    Ok(dir.parent().expect("dir always has a parent").to_path_buf())
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
    if lower.contains("model") && (lower.contains("not found") || lower.contains("not supported")) {
        // Try to extract model name from patterns like "Model not found: anthropic/claude-sonnet-4-6."
        // or "Model not found: github-copilot/gemini-3.1-pro. Did you mean: gemini-3.1-pro-preview?"
        let model = message
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

/// Statically-known free models used as a fallback when dynamic discovery
/// is unavailable or has not completed yet.
fn known_free_models() -> Vec<String> {
    vec![
        "opencode/minimax-m2.5-free".to_string(),
        "opencode/trinity-large-preview-free".to_string(),
    ]
}

/// Discover free models by running `opencode models` and filtering.
///
/// This function is synchronous and may block while the subprocess runs.
/// Do not call it directly from async code; use `discover_free_models_cached()`
/// instead, which wraps discovery in a background thread.
fn discover_free_models() -> Vec<String> {
    // Known free models as fallback
    let known = known_free_models();

    // Spawn with a 30s timeout to prevent orphaned processes.
    // `opencode models` can hang indefinitely on network requests.
    let mut child = match std::process::Command::new("opencode")
        .args(["models"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return known,
    };

    let timeout = std::time::Duration::from_secs(30);
    let start = std::time::Instant::now();
    let stdout = loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                break child
                    .stdout
                    .take()
                    .map(|mut s| {
                        let mut buf = String::new();
                        std::io::Read::read_to_string(&mut s, &mut buf).ok();
                        buf
                    })
                    .unwrap_or_default();
            }
            Ok(Some(_)) => return known,
            Ok(None) => {
                if start.elapsed() > timeout {
                    tracing::warn!("opencode models timed out after 30s, killing process");
                    let _ = child.kill();
                    let _ = child.wait();
                    return known;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Err(_) => {
                let _ = child.kill();
                return known;
            }
        }
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

    if result_text.is_empty() && !is_error {
        return None;
    }

    // Extract tokens from step_finish
    let (input_tokens, output_tokens) = runner.extract_tokens(&events);

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
    }

    #[test]
    fn find_opencode_result_error_event() {
        let ndjson = r#"{"type":"error","message":"rate limit exceeded: 429"}"#;
        let result = find_opencode_result(ndjson).expect("should find error result");
        assert!(result.is_error);
        assert!(result.result_text.contains("rate limit"));
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
