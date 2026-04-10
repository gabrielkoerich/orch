//! NDJSON stream formatter for `orch stream`.
//!
//! Parses agent NDJSON output lines and converts them to human-readable text.
//! Handles the three agent output formats: Claude, Codex, and OpenCode.
//!
//! ## Claude (`--output-format stream-json`)
//! ```jsonl
//! {"type":"system","subtype":"init",...}
//! {"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"..."}]}}
//! {"type":"tool_use","tool":{"name":"Read","input":{"file_path":"..."}}}
//! {"type":"tool_result","result":{"content":"..."}}
//! {"type":"result","subtype":"success","result":"...","usage":{"input_tokens":1234,"output_tokens":567}}
//! ```
//!
//! ## Codex (`--json`)
//! ```jsonl
//! {"type":"thread.started","thread_id":"..."}
//! {"type":"item.completed","item":{"type":"agent_message","text":"..."}}
//! {"type":"item.completed","item":{"type":"command_execution","command":"..."}}
//! {"type":"item.completed","item":{"type":"reasoning","text":"..."}}
//! ```
//!
//! ## OpenCode (`--format json`)
//! ```jsonl
//! {"type":"text","part":{"type":"text","text":"..."}}
//! {"type":"text","text":"..."}
//! {"type":"tool_use","part":{"tool":"bash","state":{"input":{"command":"..."}}}}
//! {"type":"step_finish","part":{"reason":"stop","cost":0.015,"tokens":{"input":1234,"output":567}}}
//! ```

/// Format a single NDJSON line into human-readable text.
///
/// Returns `None` for lines that should be suppressed (tool results, system
/// init, reasoning items, empty lines, etc.). Returns the original `line`
/// unchanged if it is not valid JSON.
pub fn format_line(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    let v: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        // Not JSON — pass through as-is (e.g. git output, shell messages)
        Err(_) => return Some(line.to_string()),
    };

    let event_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");

    match event_type {
        // ── Claude format ──────────────────────────────────────────────────
        "assistant" => {
            // Extract text content from message.content array
            let text = extract_claude_assistant_text(&v)?;
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        }

        "tool_use" if v.get("tool").is_some() => {
            // Claude: {"type":"tool_use","tool":{"name":"Read","input":{...}}}
            let tool = v.get("tool")?;
            let name = tool.get("name").and_then(|n| n.as_str()).unwrap_or("?");
            let input = tool.get("input");
            let summary = summarize_tool_input(name, input);
            if summary.is_empty() {
                Some(format!("→ {name}"))
            } else {
                Some(format!("→ {name} {summary}"))
            }
        }

        "tool_result" => None, // suppress tool results (too verbose)

        "result" => {
            // Claude final result event
            let is_error = v.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
            let subtype = v.get("subtype").and_then(|s| s.as_str()).unwrap_or("");
            if is_error || subtype.contains("error") {
                let msg = v
                    .get("result")
                    .and_then(|r| r.as_str())
                    .unwrap_or("unknown error");
                Some(format!("✗ Error: {}", truncate(msg, 200)))
            } else {
                let usage = v.get("usage");
                let input_tokens = usage
                    .and_then(|u| u.get("input_tokens"))
                    .and_then(|t| t.as_u64())
                    .unwrap_or(0);
                let output_tokens = usage
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(|t| t.as_u64())
                    .unwrap_or(0);
                Some(format!(
                    "✓ Done ({input_tokens} in / {output_tokens} out tokens)"
                ))
            }
        }

        "system" => None, // suppress system init events

        // ── Codex format ───────────────────────────────────────────────────
        "item.completed" => {
            let item = v.get("item")?;
            let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match item_type {
                "agent_message" => {
                    let text = item.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    if text.is_empty() {
                        None
                    } else {
                        Some(text.to_string())
                    }
                }
                "command_execution" => {
                    let cmd = item.get("command").and_then(|c| c.as_str()).unwrap_or("?");
                    Some(format!("$ {}", truncate(cmd, 120)))
                }
                // Skip reasoning (internal thinking) and other item types
                _ => None,
            }
        }

        "thread.started" | "turn.started" | "turn.completed" => None,

        "turn.failed" => {
            let error = v
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("unknown error");
            Some(format!("✗ {}", truncate(error, 200)))
        }

        // ── OpenCode format ────────────────────────────────────────────────
        "text" => {
            // Format 1: {"type":"text","part":{"type":"text","text":"..."}}
            // Format 2: {"type":"text","text":"..."}
            let text = v
                .get("part")
                .and_then(|p| p.get("text"))
                .and_then(|t| t.as_str())
                .or_else(|| v.get("text").and_then(|t| t.as_str()))
                .unwrap_or("");
            if text.is_empty() {
                None
            } else {
                Some(text.to_string())
            }
        }

        "tool_use" if v.get("part").is_some() => {
            // OpenCode: {"type":"tool_use","part":{"tool":"bash","state":{"input":{...}}}}
            let part = v.get("part")?;
            let tool_name = part.get("tool").and_then(|t| t.as_str()).unwrap_or("?");
            let state_input = part.get("state").and_then(|s| s.get("input"));
            let summary = summarize_tool_input(tool_name, state_input);
            if tool_name == "bash" || tool_name == "Bash" {
                if summary.is_empty() {
                    Some("$ (bash)".to_string())
                } else {
                    Some(format!("$ {summary}"))
                }
            } else if summary.is_empty() {
                Some(format!("→ {tool_name}"))
            } else {
                Some(format!("→ {tool_name} {summary}"))
            }
        }

        "step_finish" => {
            let part = v.get("part");
            let cost = part.and_then(|p| p.get("cost")).and_then(|c| c.as_f64());
            let tokens = part.and_then(|p| p.get("tokens"));
            let input_tokens = tokens
                .and_then(|t| t.get("input"))
                .and_then(|t| t.as_u64())
                .unwrap_or(0);
            let output_tokens = tokens
                .and_then(|t| t.get("output"))
                .and_then(|t| t.as_u64())
                .unwrap_or(0);
            let cost_str = cost.map(|c| format!(", ${c:.3}")).unwrap_or_default();
            Some(format!(
                "✓ Done ({input_tokens} in / {output_tokens} out tokens{cost_str})"
            ))
        }

        "step_start" => None,

        // Unknown event types — suppress to keep output clean
        _ => None,
    }
}

/// Extract concatenated text from a Claude `assistant` event.
///
/// The content array may have multiple items; we join all `type:"text"` ones.
fn extract_claude_assistant_text(v: &serde_json::Value) -> Option<String> {
    let content = v.get("message")?.get("content")?.as_array()?;

    let text: String = content
        .iter()
        .filter_map(|item| {
            if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                item.get("text")
                    .and_then(|t| t.as_str())
                    .map(str::to_string)
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("");

    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Generate a short human-readable summary of a tool's input arguments.
///
/// Recognises common tool names and extracts the most relevant field.
/// Falls back to the first string value found, or an empty string.
fn summarize_tool_input(tool: &str, input: Option<&serde_json::Value>) -> String {
    let Some(input) = input else {
        return String::new();
    };

    match tool {
        "Read" | "read" => field_str(input, "file_path"),
        "Write" | "write" => field_str(input, "file_path"),
        "Edit" | "edit" => {
            let path = field_str(input, "file_path");
            let line = input
                .get("old_string")
                .and_then(|s| s.as_str())
                .map(|s| truncate(s.lines().next().unwrap_or(""), 60))
                .unwrap_or_default();
            if line.is_empty() {
                path
            } else {
                format!("{path} \"{line}\"")
            }
        }
        "Bash" | "bash" => field_str(input, "command")
            .lines()
            .next()
            .map(|l| truncate(l, 100))
            .unwrap_or_default(),
        "Grep" | "grep" => {
            let pattern = field_str(input, "pattern");
            let path = field_str(input, "path");
            if path.is_empty() {
                pattern
            } else {
                format!("{pattern} in {path}")
            }
        }
        "Glob" | "glob" => field_str(input, "pattern"),
        _ => {
            // Generic fallback: try common key names, then first string value
            for key in &["file_path", "command", "path", "pattern", "query"] {
                let s = field_str(input, key);
                if !s.is_empty() {
                    return truncate(&s, 100);
                }
            }
            // Last resort: first string value in the object
            if let Some(obj) = input.as_object() {
                for val in obj.values() {
                    if let Some(s) = val.as_str() {
                        if !s.is_empty() {
                            return truncate(s, 100);
                        }
                    }
                }
            }
            String::new()
        }
    }
}

/// Get a field as a String, returning empty string if missing/not a string.
fn field_str(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(|s| s.as_str())
        .map(str::to_string)
        .unwrap_or_default()
}

/// Truncate a string to at most `max` chars, appending `…` if cut.
fn truncate(s: &str, max: usize) -> String {
    for (char_count, (byte_idx, _ch)) in s.char_indices().enumerate() {
        if char_count >= max {
            return format!("{}…", &s[..byte_idx]);
        }
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Claude format ──────────────────────────────────────────────────────

    #[test]
    fn claude_assistant_text() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Let me read the file."}]}}"#;
        let out = format_line(line).unwrap();
        assert_eq!(out, "Let me read the file.");
    }

    #[test]
    fn claude_tool_use_read() {
        let line =
            r#"{"type":"tool_use","tool":{"name":"Read","input":{"file_path":"src/main.rs"}}}"#;
        let out = format_line(line).unwrap();
        assert_eq!(out, "→ Read src/main.rs");
    }

    #[test]
    fn claude_tool_use_bash() {
        let line =
            r#"{"type":"tool_use","tool":{"name":"Bash","input":{"command":"cargo test --lib"}}}"#;
        let out = format_line(line).unwrap();
        assert_eq!(out, "→ Bash cargo test --lib");
    }

    #[test]
    fn claude_tool_result_suppressed() {
        let line = r#"{"type":"tool_result","result":{"content":"file contents here"}}"#;
        assert!(format_line(line).is_none());
    }

    #[test]
    fn claude_result_success() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"done","usage":{"input_tokens":1234,"output_tokens":567}}"#;
        let out = format_line(line).unwrap();
        assert_eq!(out, "✓ Done (1234 in / 567 out tokens)");
    }

    #[test]
    fn claude_result_error() {
        let line = r#"{"type":"result","subtype":"error","is_error":true,"result":"something went wrong","usage":{"input_tokens":10,"output_tokens":5}}"#;
        let out = format_line(line).unwrap();
        assert!(out.starts_with("✗ Error:"));
        assert!(out.contains("something went wrong"));
    }

    #[test]
    fn claude_system_suppressed() {
        let line = r#"{"type":"system","subtype":"init","session_id":"abc"}"#;
        assert!(format_line(line).is_none());
    }

    // ── Codex format ───────────────────────────────────────────────────────

    #[test]
    fn codex_agent_message() {
        let line = r#"{"type":"item.completed","item":{"type":"agent_message","text":"I will fix the bug."}}"#;
        let out = format_line(line).unwrap();
        assert_eq!(out, "I will fix the bug.");
    }

    #[test]
    fn codex_command_execution() {
        let line = r#"{"type":"item.completed","item":{"type":"command_execution","command":"cargo fmt"}}"#;
        let out = format_line(line).unwrap();
        assert_eq!(out, "$ cargo fmt");
    }

    #[test]
    fn codex_reasoning_suppressed() {
        let line =
            r#"{"type":"item.completed","item":{"type":"reasoning","text":"internal thoughts"}}"#;
        assert!(format_line(line).is_none());
    }

    #[test]
    fn codex_turn_events_suppressed() {
        assert!(format_line(r#"{"type":"thread.started","thread_id":"t1"}"#).is_none());
        assert!(format_line(r#"{"type":"turn.started"}"#).is_none());
        assert!(format_line(r#"{"type":"turn.completed"}"#).is_none());
    }

    #[test]
    fn codex_turn_failed() {
        let line = r#"{"type":"turn.failed","error":"Provider quota exceeded"}"#;
        let out = format_line(line).unwrap();
        assert!(out.starts_with("✗ "));
        assert!(out.contains("Provider quota exceeded"));
    }

    #[test]
    fn codex_turn_failed_unknown() {
        let line = r#"{"type":"turn.failed"}"#;
        let out = format_line(line).unwrap();
        assert_eq!(out, "✗ unknown error");
    }

    // ── OpenCode format ────────────────────────────────────────────────────

    #[test]
    fn opencode_text_part_format() {
        let line = r#"{"type":"text","part":{"type":"text","text":"Analyzing the code..."}}"#;
        let out = format_line(line).unwrap();
        assert_eq!(out, "Analyzing the code...");
    }

    #[test]
    fn opencode_text_direct_format() {
        let line = r#"{"type":"text","text":"Working on it."}"#;
        let out = format_line(line).unwrap();
        assert_eq!(out, "Working on it.");
    }

    #[test]
    fn opencode_tool_use_bash() {
        let line = r#"{"type":"tool_use","part":{"tool":"bash","state":{"input":{"command":"git status"}}}}"#;
        let out = format_line(line).unwrap();
        assert_eq!(out, "$ git status");
    }

    #[test]
    fn opencode_tool_use_non_bash() {
        let line = r#"{"type":"tool_use","part":{"tool":"read","state":{"input":{"file_path":"src/lib.rs"}}}}"#;
        let out = format_line(line).unwrap();
        assert_eq!(out, "→ read src/lib.rs");
    }

    #[test]
    fn opencode_step_finish_with_cost() {
        let line = r#"{"type":"step_finish","part":{"reason":"stop","cost":0.015,"tokens":{"input":800,"output":200}}}"#;
        let out = format_line(line).unwrap();
        assert_eq!(out, "✓ Done (800 in / 200 out tokens, $0.015)");
    }

    #[test]
    fn opencode_step_finish_no_cost() {
        let line = r#"{"type":"step_finish","part":{"reason":"stop"}}"#;
        let out = format_line(line).unwrap();
        assert_eq!(out, "✓ Done (0 in / 0 out tokens)");
    }

    #[test]
    fn opencode_step_start_suppressed() {
        let line = r#"{"type":"step_start","timestamp":1000}"#;
        assert!(format_line(line).is_none());
    }

    // ── Generic behaviour ──────────────────────────────────────────────────

    #[test]
    fn non_json_passed_through() {
        let line = "fatal: not a git repository";
        let out = format_line(line).unwrap();
        assert_eq!(out, line);
    }

    #[test]
    fn empty_line_suppressed() {
        assert!(format_line("").is_none());
        assert!(format_line("   ").is_none());
    }

    #[test]
    fn unknown_event_type_suppressed() {
        let line = r#"{"type":"some_unknown_event","data":"x"}"#;
        assert!(format_line(line).is_none());
    }

    #[test]
    fn truncate_short_unchanged() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_adds_ellipsis() {
        let s = "abcdefghij";
        let out = truncate(s, 5);
        assert!(out.ends_with('…'));
        assert!(out.len() < s.len());
    }
}
