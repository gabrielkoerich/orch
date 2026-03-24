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

static TRAILING_COMMA_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r",(\s*[}\]])").unwrap());

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
    let content_start = text[start..].find('\n')? + start + 1;
    let end = text[content_start..].find("```")? + content_start;
    Some(text[content_start..end].to_string())
}

fn json_candidates(raw: &str) -> Vec<String> {
    let mut candidates = Vec::new();

    if let Some(block) = extract_json_block(raw) {
        candidates.push(block);
    }

    if let Some(candidate) = extract_balanced_json(raw) {
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

fn extract_balanced_json(text: &str) -> Option<String> {
    let start = text
        .char_indices()
        .find(|(_, ch)| matches!(ch, '{' | '['))
        .map(|(idx, _)| idx)?;

    let mut stack = Vec::new();
    let mut in_string = false;
    let mut escape = false;

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
                    return None;
                }
                if stack.is_empty() {
                    return Some(text[start..=abs].to_string());
                }
            }
            ']' => {
                if stack.pop() != Some('[') {
                    return None;
                }
                if stack.is_empty() {
                    return Some(text[start..=abs].to_string());
                }
            }
            _ => {}
        }
    }

    None
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

    // Extract delegations
    let delegations = obj
        .get("delegations")
        .and_then(|v| serde_json::from_value::<Vec<Delegation>>(v.clone()).ok())
        .unwrap_or_default();

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
}
