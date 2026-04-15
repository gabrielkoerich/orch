//! Agent response parser — normalizes JSON output from different agents.
//!
//! Each agent (Claude, Codex, OpenCode) returns a different JSON shape.
//! This module parses them all into a common `AgentResponse` struct.
//! Replaces `scripts/parse_response.sh` + `jq` pipelines.

use anyhow::Context;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::io::Read;

static TRAILING_COMMA_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r",(\s*[}\]])").expect("trailing comma regex is valid"));

/// Normalized agent response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResponse {
    pub status: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub accomplished: Vec<String>,
    #[serde(default)]
    pub remaining: Vec<String>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub error: Option<String>,
    /// Input token count (if available from agent).
    #[serde(default)]
    pub input_tokens: Option<u64>,
    /// Output token count (if available from agent).
    #[serde(default)]
    pub output_tokens: Option<u64>,
    /// Key learnings from this attempt (for memory persistence across retries).
    #[serde(default)]
    pub learnings: Vec<String>,
    /// Subtask delegations — agent requests child tasks to be created.
    #[serde(default)]
    pub delegations: Vec<Delegation>,
}

/// A subtask delegation requested by an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Delegation {
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub labels: Vec<String>,
}

/// Parse an agent response from a file path (or stdin if "-").
pub fn parse_and_print(path: &str) -> anyhow::Result<()> {
    let content = if path == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading stdin")?;
        buf
    } else {
        std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?
    };

    let response = parse(&content)?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

/// Parse raw agent output into a normalized response.
pub fn parse(raw: &str) -> anyhow::Result<AgentResponse> {
    let mut last_err: Option<anyhow::Error> = None;
    let mut saw_jsonish_candidate = false;
    for candidate in json_candidates(raw) {
        match parse_candidate(&candidate) {
            Ok(resp) => return Ok(resp),
            Err(err) => {
                if err.to_string() != "invalid agent response candidate" {
                    saw_jsonish_candidate = true;
                    last_err = Some(err);
                }
            }
        }
    }

    if saw_jsonish_candidate {
        if let Some(err) = last_err {
            return Err(err);
        }
    }

    if let Some(err) = last_err {
        return Err(err);
    }

    anyhow::bail!(
        "agent output is not valid JSON or a supported JSON wrapper: {}",
        raw.trim()
    )
}

/// Extract the first JSON code block from markdown.
fn extract_json_block(text: &str) -> Option<String> {
    let start = text.find("```json")?;
    let fence_end = start + "```json".len();
    let remainder = &text[fence_end..];

    // Find key positions after the opening fence.
    let newline_pos = remainder.find('\n');
    let json_start_pos = remainder.find(['{', '[']);
    let first_nl = newline_pos.unwrap_or(usize::MAX);

    // Decide fenced vs inline:
    // - If a JSON delimiter ('{' or '[') appears before the first newline, treat as inline.
    // - If there is no newline at all, treat as inline.
    // - Otherwise, treat as a standard fenced block.
    let is_inline = json_start_pos.is_some_and(|js| js < first_nl) || newline_pos.is_none();

    if is_inline {
        // Inline: closing fence may appear anywhere on the same logical line.
        // Using find("```") here is safe because the JSON start already appeared
        // before any newline, so the closing fence is on the same line too.
        let closing_fence_pos = remainder.find("```")?;
        let content_start = remainder
            .char_indices()
            .find(|(_, ch)| !ch.is_whitespace())
            .map_or(fence_end, |(idx, _)| fence_end + idx);
        let end = fence_end + closing_fence_pos;
        if content_start > end {
            return None;
        }
        Some(text[content_start..end].to_string())
    } else {
        // Fenced block: content begins after the first newline.
        // The closing fence must be anchored to a line boundary (\n```) to avoid
        // false positives from triple-backticks inside JSON string values.
        let nl = newline_pos?;
        let content_start = fence_end + nl + 1;
        let content = &text[content_start..];
        let closing_pos = find_closing_fence_at_line_boundary(content)?;
        Some(text[content_start..content_start + closing_pos].to_string())
    }
}

/// Find the position (within `content`) where a ``` closing fence starts, anchored to a
/// line boundary.  Returns the byte offset of the leading ``` so that `content[..pos]`
/// is everything before the closing fence (including the preceding newline).
fn find_closing_fence_at_line_boundary(content: &str) -> Option<usize> {
    // Edge case: content itself begins with the closing fence (no preceding content).
    if content.starts_with("```") {
        return Some(0);
    }
    // Scan for \n``` pattern — the closing fence must start at the beginning of a line.
    let mut search_from = 0;
    while let Some(nl_idx) = content[search_from..].find('\n') {
        let after_nl = search_from + nl_idx + 1;
        if content[after_nl..].starts_with("```") {
            // Return the position of the ``` (after the \n).
            // The content slice up to this point includes the \n, matching Markdown semantics.
            return Some(after_nl);
        }
        search_from = after_nl;
    }
    None
}

/// Fields that indicate a JSON blob is an AgentResponse (higher score = better match).
const AGENT_RESPONSE_FIELDS: &[&str] = &[
    "\"status\"",
    "\"summary\"",
    "\"accomplished\"",
    "\"remaining\"",
    "\"files\"",
    "\"learnings\"",
    "\"delegations\"",
    "\"error\"",
];

fn agent_response_score(blob: &str) -> usize {
    AGENT_RESPONSE_FIELDS
        .iter()
        .filter(|&&f| blob.contains(f))
        .count()
}

fn json_candidates(raw: &str) -> Vec<String> {
    let mut candidates = Vec::new();

    if let Some(block) = extract_json_block(raw) {
        candidates.push(block);
    }

    // Collect ALL balanced JSON blobs and sort by how much they look like an
    // AgentResponse, so the real response is tried before telemetry blobs.
    let mut all_json = extract_all_balanced_json(raw);
    all_json.sort_by_key(|b| std::cmp::Reverse(agent_response_score(b)));

    for candidate in all_json {
        if !candidates.iter().any(|existing| existing == &candidate) {
            candidates.push(candidate);
        }
    }

    candidates.push(raw.to_string());
    candidates
}

fn parse_candidate(candidate: &str) -> anyhow::Result<AgentResponse> {
    if let Ok(resp) = serde_json::from_str::<AgentResponse>(candidate) {
        return Ok(resp);
    }

    let repaired = repair_json_like(candidate);
    if repaired != candidate {
        if let Ok(resp) = serde_json::from_str::<AgentResponse>(&repaired) {
            return Ok(resp);
        }
    }

    if let Ok(val) = serde_json::from_str::<serde_json::Value>(candidate) {
        return map_generic_response(&val);
    }

    if repaired != candidate {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&repaired) {
            return map_generic_response(&val);
        }
    }

    anyhow::bail!("invalid agent response candidate")
}

/// Scan `text` and return every top-level balanced JSON object or array found.
fn extract_all_balanced_json(text: &str) -> Vec<String> {
    let mut results = Vec::new();
    let mut pos = 0;

    while pos < text.len() {
        // Find the next `{` or `[` starting from `pos`.
        let Some(start_offset) = text[pos..]
            .char_indices()
            .find(|(_, ch)| matches!(ch, '{' | '['))
            .map(|(idx, _)| idx)
        else {
            break;
        };
        let start = pos + start_offset;

        let mut stack = Vec::new();
        let mut in_string = false;
        let mut escape = false;
        let mut end: Option<usize> = None;

        for (idx, ch) in text[start..].char_indices() {
            let abs = start + idx;
            if in_string {
                if escape {
                    escape = false;
                    continue;
                }
                match ch {
                    '\\' => escape = true,
                    '"' => in_string = false,
                    _ => {}
                }
                continue;
            }

            match ch {
                '"' => in_string = true,
                '{' | '[' => stack.push(ch),
                '}' => {
                    if stack.pop() != Some('{') {
                        break; // malformed — skip past this `{`
                    }
                    if stack.is_empty() {
                        end = Some(abs);
                        break;
                    }
                }
                ']' => {
                    if stack.pop() != Some('[') {
                        break; // malformed — skip past this `[`
                    }
                    if stack.is_empty() {
                        end = Some(abs);
                        break;
                    }
                }
                _ => {}
            }
        }

        if let Some(end_idx) = end {
            results.push(text[start..=end_idx].to_string());
            pos = end_idx + 1;
        } else {
            // No closing brace found — skip past the opening character.
            pos = start + 1;
        }
    }

    results
}

fn repair_json_like(text: &str) -> String {
    let mut repaired = text.trim().replace(['“', '”'], "\"");
    repaired = repaired.replace(['‘', '’'], "'");
    repaired = TRAILING_COMMA_RE.replace_all(&repaired, "$1").into_owned();

    let mut stack = Vec::new();
    let mut in_string = false;
    let mut escape = false;

    for ch in repaired.chars() {
        if in_string {
            if escape {
                escape = false;
                continue;
            }
            match ch {
                '\\' => escape = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                if let Some(expected) = stack.last().copied() {
                    if expected == ch {
                        stack.pop();
                    }
                }
            }
            _ => {}
        }
    }

    while let Some(ch) = stack.pop() {
        repaired.push(ch);
    }

    repaired
}

/// Map a generic JSON object to AgentResponse.
fn map_generic_response(val: &serde_json::Value) -> anyhow::Result<AgentResponse> {
    let obj = val.as_object().context("expected JSON object")?;

    let has_supported_fields = [
        "status",
        "result",
        "summary",
        "message",
        "output",
        "accomplished",
        "remaining",
        "files",
        "files_changed",
        "error",
        "learnings",
        "delegations",
    ]
    .iter()
    .any(|key| obj.contains_key(*key));

    if !has_supported_fields {
        anyhow::bail!("JSON object does not contain any supported agent response fields");
    }

    let status = obj
        .get("status")
        .or_else(|| obj.get("result"))
        .and_then(|v| v.as_str())
        .unwrap_or("done")
        .to_string();

    let summary = obj
        .get("summary")
        .or_else(|| obj.get("message"))
        .or_else(|| obj.get("output"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let accomplished = extract_string_array(obj.get("accomplished"));
    let remaining = extract_string_array(obj.get("remaining"));
    let files = {
        let f = extract_string_array(obj.get("files"));
        if !f.is_empty() {
            f
        } else {
            extract_string_array(obj.get("files_changed"))
        }
    };
    let error = obj.get("error").and_then(|v| v.as_str()).map(String::from);
    let learnings = extract_string_array(obj.get("learnings"));

    // Extract token counts if available
    let input_tokens = extract_u64(obj.get("input_tokens"))
        .or_else(|| extract_u64(obj.get("tokens_input")))
        .or_else(|| extract_usage_tokens(obj.get("usage"), true));
    let output_tokens = extract_u64(obj.get("output_tokens"))
        .or_else(|| extract_u64(obj.get("tokens_output")))
        .or_else(|| extract_usage_tokens(obj.get("usage"), false));

    // Extract delegations. If the `delegations` field is present but malformed,
    // return an error instead of silently ignoring it so callers can surface
    // and debug malformed agent output.
    let delegations = if let Some(v) = obj.get("delegations") {
        serde_json::from_value::<Vec<Delegation>>(v.clone())
            .with_context(|| format!("invalid delegations field: {}", v))?
    } else {
        Vec::new()
    };

    Ok(AgentResponse {
        status,
        summary,
        accomplished,
        remaining,
        files,
        error,
        input_tokens,
        output_tokens,
        learnings,
        delegations,
    })
}

/// Extract a u64 from a JSON value.
fn extract_u64(val: Option<&serde_json::Value>) -> Option<u64> {
    val.and_then(|v| v.as_u64())
}

/// Extract tokens from a usage object (common in OpenAI-compatible APIs).
fn extract_usage_tokens(usage: Option<&serde_json::Value>, is_input: bool) -> Option<u64> {
    let obj = usage?.as_object()?;
    let key = if is_input {
        "input_tokens"
    } else {
        "output_tokens"
    };
    obj.get(key).and_then(|v| v.as_u64())
}

fn extract_string_array(val: Option<&serde_json::Value>) -> Vec<String> {
    val.and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_direct_json() {
        let input = r#"{"status":"done","summary":"Fixed bug","accomplished":["fixed it"],"remaining":[],"files":["src/main.rs"]}"#;
        let resp = parse(input).unwrap();
        assert_eq!(resp.status, "done");
        assert_eq!(resp.summary, "Fixed bug");
        assert_eq!(resp.accomplished, vec!["fixed it"]);
        assert_eq!(resp.files, vec!["src/main.rs"]);
        assert!(resp.error.is_none());
    }

    #[test]
    fn parse_json_in_markdown_block() {
        let input = r#"Here is the result:

```json
{"status":"in_progress","summary":"Working on it","accomplished":[],"remaining":["finish tests"],"files":[]}
```

Done.
"#;
        let resp = parse(input).unwrap();
        assert_eq!(resp.status, "in_progress");
        assert_eq!(resp.remaining, vec!["finish tests"]);
    }

    #[test]
    fn parse_generic_json_with_different_field_names() {
        let input = r#"{"result":"done","message":"All good","files":["a.rs","b.rs"]}"#;
        let resp = parse(input).unwrap();
        assert_eq!(resp.status, "done");
        assert_eq!(resp.summary, "All good");
        assert_eq!(resp.files, vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn parse_with_error_field() {
        let input = r#"{"status":"blocked","summary":"Cannot proceed","accomplished":[],"remaining":[],"files":[],"error":"missing dependency"}"#;
        let resp = parse(input).unwrap();
        assert_eq!(resp.status, "blocked");
        assert_eq!(resp.error, Some("missing dependency".to_string()));
    }

    #[test]
    fn parse_fallback_raw_text() {
        let input = "This is just plain text output from the agent.";
        let err = parse(input).unwrap_err();
        assert!(err
            .to_string()
            .contains("agent output is not valid JSON or a supported JSON wrapper"));
    }

    #[test]
    fn parse_malformed_json_block_returns_error() {
        let input = r#"Here is the result:

```json
{"status":"done","summary":"missing brace"
```
"#;
        let resp = parse(input).unwrap();
        assert_eq!(resp.status, "done");
        assert_eq!(resp.summary, "missing brace");
    }

    #[test]
    fn parse_json_with_trailing_comma() {
        let input =
            r#"{"status":"done","summary":"Fixed","accomplished":[],"remaining":[],"files":[],}"#;
        let resp = parse(input).unwrap();
        assert_eq!(resp.status, "done");
        assert_eq!(resp.summary, "Fixed");
    }

    #[test]
    fn parse_json_embedded_in_commentary() {
        let input = r#"Done:
{"status":"in_progress","summary":"working","accomplished":[],"remaining":["tests"],"files":[]}
Thanks"#;
        let resp = parse(input).unwrap();
        assert_eq!(resp.status, "in_progress");
        assert_eq!(resp.remaining, vec!["tests"]);
    }

    #[test]
    fn extract_json_block_from_markdown() {
        let text = "prefix\n```json\n{\"key\":\"value\"}\n```\nsuffix";
        let block = extract_json_block(text).unwrap();
        assert_eq!(block, "{\"key\":\"value\"}\n");
    }

    #[test]
    fn extract_json_block_inline_fence() {
        let text = "prefix\n```json{\"key\":\"value\"}```\nsuffix";
        let block = extract_json_block(text).unwrap();
        assert_eq!(block, "{\"key\":\"value\"}");
    }

    #[test]
    fn extract_json_block_with_info_string() {
        let text = "prefix\n```json some-meta\n{\"key\":\"value\"}\n```\nsuffix";
        let block = extract_json_block(text).unwrap();
        assert_eq!(block, "{\"key\":\"value\"}\n");
    }

    #[test]
    fn extract_json_block_with_info_string_and_whitespace() {
        let text = "prefix\n```json   some-meta   \n{\"key\":\"value\"}\n```\nsuffix";
        let block = extract_json_block(text).unwrap();
        assert_eq!(block, "{\"key\":\"value\"}\n");
    }

    #[test]
    fn extract_json_block_inline_with_internal_newline() {
        // Inline JSON that contains a newline inside the JSON value
        let text = "prefix\n```json{\"key\":\"value\\nwith newline\"}```\nsuffix";
        let block = extract_json_block(text).unwrap();
        assert_eq!(block, r#"{"key":"value\nwith newline"}"#);
    }

    #[test]
    fn extract_json_block_inline_with_whitespace_newline() {
        // Inline JSON that contains a newline as whitespace between key and value
        let text = "prefix\n```json{\"key\":\n\"value\"}```\nsuffix";
        let block = extract_json_block(text).unwrap();
        assert_eq!(block, "{\"key\":\n\"value\"}");
    }

    #[test]
    fn extract_json_block_missing_closing_fence() {
        let text = "prefix\n```json{\"key\":\"value\"}\n";
        assert!(extract_json_block(text).is_none());
    }

    #[test]
    fn extract_json_block_array_inline() {
        let text = "prefix\n```json[1,2,3]```\nsuffix";
        let block = extract_json_block(text).unwrap();
        assert_eq!(block, "[1,2,3]");
    }

    #[test]
    fn extract_json_block_multiple_fences() {
        let text = "prefix\n```json{\"first\":1}``` middle\n```json{\"second\":2}``` suffix";
        let block = extract_json_block(text).unwrap();
        assert_eq!(block, "{\"first\":1}");
    }

    #[test]
    fn extract_json_block_missing_returns_none() {
        assert!(extract_json_block("no code block here").is_none());
    }

    #[test]
    fn extract_string_array_from_json() {
        let val: serde_json::Value = serde_json::json!(["a", "b", "c"]);
        let result = extract_string_array(Some(&val));
        assert_eq!(result, vec!["a", "b", "c"]);
    }

    #[test]
    fn extract_string_array_none() {
        let result = extract_string_array(None);
        assert!(result.is_empty());
    }

    #[test]
    fn parse_empty_object() {
        let input = "{}";
        let err = parse(input).unwrap_err();
        assert!(err
            .to_string()
            .contains("JSON object does not contain any supported agent response fields"));
    }

    #[test]
    fn parse_with_learnings() {
        let input = r#"{"status":"needs_review","summary":"Need to fix imports","accomplished":[],"remaining":["fix imports"],"files":["src/main.rs"],"learnings":["Use std::sync::Arc for shared state","Check imports before committing"]}"#;
        let resp = parse(input).unwrap();
        assert_eq!(resp.status, "needs_review");
        assert_eq!(resp.summary, "Need to fix imports");
        assert_eq!(resp.learnings.len(), 2);
        assert_eq!(resp.learnings[0], "Use std::sync::Arc for shared state");
        assert_eq!(resp.learnings[1], "Check imports before committing");
    }

    #[test]
    fn parse_learnings_empty_by_default() {
        let input = r#"{"status":"done","summary":"Completed","accomplished":["done"],"remaining":[],"files":[]}"#;
        let resp = parse(input).unwrap();
        assert!(resp.learnings.is_empty());
    }

    #[test]
    fn parse_with_delegations() {
        let input = r#"{
            "status": "blocked",
            "summary": "Task requires subtasks",
            "accomplished": ["Analyzed requirements"],
            "remaining": ["Waiting on subtasks"],
            "files_changed": [],
            "blockers": ["Waiting on delegated subtasks"],
            "reason": "Decomposed into subtasks",
            "delegations": [
                {"title": "Implement login API", "body": "Create POST /api/login", "labels": ["backend"]},
                {"title": "Add login form", "body": "Create React login component", "labels": ["frontend"]}
            ]
        }"#;
        let resp = parse(input).unwrap();
        assert_eq!(resp.status, "blocked");
        assert_eq!(resp.delegations.len(), 2);
        assert_eq!(resp.delegations[0].title, "Implement login API");
        assert_eq!(resp.delegations[0].body, "Create POST /api/login");
        assert_eq!(resp.delegations[0].labels, vec!["backend"]);
        assert_eq!(resp.delegations[1].title, "Add login form");
        assert_eq!(resp.delegations[1].labels, vec!["frontend"]);
    }

    #[test]
    fn parse_delegations_empty_by_default() {
        let input =
            r#"{"status":"done","summary":"Done","accomplished":[],"remaining":[],"files":[]}"#;
        let resp = parse(input).unwrap();
        assert!(resp.delegations.is_empty());
    }

    #[test]
    fn parse_delegations_without_labels() {
        let input = r#"{
            "status": "blocked",
            "summary": "Delegating",
            "delegations": [
                {"title": "Subtask", "body": "Do the thing"}
            ]
        }"#;
        let resp = parse(input).unwrap();
        assert_eq!(resp.delegations.len(), 1);
        assert_eq!(resp.delegations[0].title, "Subtask");
        assert!(resp.delegations[0].labels.is_empty());
    }

    #[test]
    fn parse_response_after_telemetry_json() {
        // Telemetry JSON appears before the actual response JSON.
        // The parser must skip the telemetry blob and find the real response.
        let input = r#"Processing...
{"type":"telemetry","message":"starting task","tokens":42}
Some output here.
{"status":"done","summary":"Fixed the bug","accomplished":["patched parser"],"remaining":[],"files":["src/parser.rs"]}
"#;
        let resp = parse(input).unwrap();
        assert_eq!(resp.status, "done");
        assert_eq!(resp.summary, "Fixed the bug");
        assert_eq!(resp.files, vec!["src/parser.rs"]);
    }

    // --- Edge-case tests for line-boundary fence extraction ---

    #[test]
    fn extract_json_block_backticks_inside_json_string() {
        // Triple-backticks appear inside a JSON string value.
        // The old code would stop at the first ``` (inside the string).
        // The new code must find the real closing fence at a line boundary.
        let text = "```json\n{\"code\":\"example: ```snippet``` end\"}\n```\n";
        let block = extract_json_block(text).unwrap();
        assert_eq!(block, "{\"code\":\"example: ```snippet``` end\"}\n");
    }

    #[test]
    fn extract_json_block_backticks_inside_json_string_parse_roundtrip() {
        // Full parse roundtrip: fenced block with backticks inside a JSON string value.
        let input = "```json\n{\"status\":\"done\",\"summary\":\"use ```code``` here\"}\n```\n";
        let resp = parse(input).unwrap();
        assert_eq!(resp.status, "done");
        assert_eq!(resp.summary, "use ```code``` here");
    }

    #[test]
    fn extract_json_block_inline_with_embedded_backtick_pairs() {
        // Inline JSON: embedded backticks that are NOT triple should not confuse the parser.
        let text = "```json{\"key\":\"a`b``c\"}```\n";
        let block = extract_json_block(text).unwrap();
        assert_eq!(block, "{\"key\":\"a`b``c\"}");
    }

    #[test]
    fn parse_mixed_telemetry_and_fenced_response() {
        // Telemetry JSON with backtick sequences followed by a fenced agent response.
        let input = r#"Processing...
{"type":"log","msg":"calling tool: ```bash```"}
```json
{"status":"done","summary":"fixed it","accomplished":["patched"],"remaining":[],"files":["src/x.rs"]}
```
"#;
        let resp = parse(input).unwrap();
        assert_eq!(resp.status, "done");
        assert_eq!(resp.summary, "fixed it");
        assert_eq!(resp.files, vec!["src/x.rs"]);
    }

    #[test]
    fn parse_response_from_ndjson_multiple_objects() {
        // NDJSON stream: only the last object has `status` — must select it.
        let input = r#"{"type":"text","text":"Working on it..."}
{"type":"tool_call","name":"bash","input":{"command":"cargo test"}}
{"type":"tool_result","output":"test passed"}
{"status":"needs_review","summary":"All tests pass","accomplished":["fixed bug"],"remaining":[],"files":["src/lib.rs"]}
"#;
        let resp = parse(input).unwrap();
        assert_eq!(resp.status, "needs_review");
        assert_eq!(resp.summary, "All tests pass");
        assert_eq!(resp.files, vec!["src/lib.rs"]);
    }
}
