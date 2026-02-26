//! Agent response parser — normalizes JSON output from different agents.
//!
//! Each agent (Claude, Codex, OpenCode) returns a different JSON shape.
//! This module parses them all into a common `AgentResponse` struct.
//! Replaces `scripts/parse_response.sh` + `jq` pipelines.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::io::Read;

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
    // Try direct JSON parse first (Claude structured output)
    if let Ok(resp) = serde_json::from_str::<AgentResponse>(raw) {
        return Ok(resp);
    }

    // Try extracting JSON from markdown code blocks
    if let Some(json_str) = extract_json_block(raw) {
        if let Ok(resp) = serde_json::from_str::<AgentResponse>(&json_str) {
            return Ok(resp);
        }
    }

    // Try parsing as a generic JSON value and mapping fields
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(raw) {
        return map_generic_response(&val);
    }

    // Fallback: treat entire content as summary
    Ok(AgentResponse {
        status: "done".to_string(),
        summary: raw.trim().to_string(),
        accomplished: vec![],
        remaining: vec![],
        files: vec![],
        error: None,
        input_tokens: None,
        output_tokens: None,
    })
}

/// Extract the first JSON code block from markdown.
fn extract_json_block(text: &str) -> Option<String> {
    let start = text.find("```json")?;
    let content_start = text[start..].find('\n')? + start + 1;
    let end = text[content_start..].find("```")? + content_start;
    Some(text[content_start..end].to_string())
}

/// Map a generic JSON object to AgentResponse.
fn map_generic_response(val: &serde_json::Value) -> anyhow::Result<AgentResponse> {
    let obj = val.as_object().context("expected JSON object")?;

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
    let files = extract_string_array(obj.get("files"));
    let error = obj.get("error").and_then(|v| v.as_str()).map(String::from);

    // Extract token counts if available
    let input_tokens = extract_u64(obj.get("input_tokens"))
        .or_else(|| extract_u64(obj.get("tokens_input")))
        .or_else(|| extract_usage_tokens(obj.get("usage"), true));
    let output_tokens = extract_u64(obj.get("output_tokens"))
        .or_else(|| extract_u64(obj.get("tokens_output")))
        .or_else(|| extract_usage_tokens(obj.get("usage"), false));

    Ok(AgentResponse {
        status,
        summary,
        accomplished,
        remaining,
        files,
        error,
        input_tokens,
        output_tokens,
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
        let resp = parse(input).unwrap();
        assert_eq!(resp.status, "done");
        assert_eq!(resp.summary, input);
        assert!(resp.accomplished.is_empty());
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
        let resp = parse(input).unwrap();
        assert_eq!(resp.status, "done");
        assert_eq!(resp.summary, "");
    }
}
