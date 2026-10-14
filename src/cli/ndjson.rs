//! Renders agent NDJSON as a Claude Code style transcript for `orch stream`, `orch task watch`
//! and session export. Each agent's runner turns its own lines into [`StreamEvent`]s

use crate::engine::runner::agents::{
    get_runner, json_error_message, json_str, AgentRunner, StreamEvent,
};
use regex::Regex;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::sync::LazyLock;

/// Prefix of a tool result line, printed right under its tool call
pub const RESULT_PREFIX: &str = "  ⎿  ";

static ANSI: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07]*\x07|\r").expect("ansi regex is valid")
});
static WORKTREE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\S*/\.orch/worktrees/[^/\s]+/[^/\s]+/").expect("worktree regex is valid")
});

const BOLD: &str = "1";
const DIM: &str = "2";
const THINK: &str = "2;3";
const RED: &str = "31";
const GREEN: &str = "32";

/// Turns one agent's NDJSON lines into transcript entries, picking the runner from the first line
#[derive(Default)]
pub struct Formatter {
    color: bool,
    runner: Option<Box<dyn AgentRunner>>,
    tools: HashMap<String, String>,
    changed: BTreeSet<String>,
}

impl Formatter {
    pub fn new(color: bool) -> Self {
        Self {
            color,
            ..Self::default()
        }
    }

    /// Entries for one output line. A step starts with a marker, a result with [`RESULT_PREFIX`]
    pub fn format(&mut self, line: &str) -> Vec<String> {
        let line = ANSI.replace_all(line, "");
        let line = line.trim();
        if line.is_empty() {
            return vec![];
        }

        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return vec![self.paint(DIM, line)];
        };

        if self.runner.is_none() {
            self.runner = detect_agent(&v).map(get_runner);
        }

        let events = match &self.runner {
            Some(runner) => runner.stream_events(&v),
            None if json_str(&v, "/type") == "error" => {
                vec![StreamEvent::Error(json_error_message(&v))]
            }
            None => vec![],
        };
        events.into_iter().flat_map(|e| self.render(e)).collect()
    }

    /// Files the agent wrote or edited so far, as the agent named them
    pub fn changed_files(&self) -> Vec<String> {
        self.changed.iter().cloned().collect()
    }

    fn render(&mut self, event: StreamEvent) -> Vec<String> {
        match event {
            StreamEvent::Started { model } => {
                vec![self.paint(DIM, &format!("⏺ Session started · {model}"))]
            }
            StreamEvent::Text(text) => {
                let text = text.trim();
                if text.is_empty() {
                    return vec![];
                }

                vec![format!("⏺ {}", text.replace('\n', "\n  "))]
            }
            StreamEvent::Thinking(text) => {
                let Some(first) = text.lines().map(str::trim).find(|l| !l.is_empty()) else {
                    return vec![];
                };
                let first = truncate(&first.replace("**", ""), 120);
                vec![self.paint(THINK, &format!("✻ {first}"))]
            }
            StreamEvent::ToolCall { id, name, input } => {
                self.record_change(&name, &input);
                let call = self.tool_call(&name, &input);
                self.tools.insert(id, name);
                vec![call]
            }
            StreamEvent::ToolResult {
                id,
                name,
                output,
                is_error,
            } => {
                let called = self.tools.remove(&id).unwrap_or_default();
                let name = if name.is_empty() { called } else { name };
                vec![self.result(&name, &output, is_error)]
            }
            StreamEvent::Done(detail) => self.done(&detail),
            StreamEvent::Error(text) => {
                let text = text.trim();
                let text = if text.is_empty() {
                    "unknown error"
                } else {
                    text
                };
                vec![self.paint(RED, &format!("✗ {}", truncate(text, 300)))]
            }
        }
    }

    fn record_change(&mut self, name: &str, input: &Value) {
        let edits = matches!(
            name.to_ascii_lowercase().as_str(),
            "write" | "edit" | "multiedit" | "notebookedit"
        );
        if !edits {
            return;
        }

        let path = ["file_path", "filePath", "notebook_path", "path"]
            .iter()
            .find_map(|k| input[k].as_str().filter(|s| !s.is_empty()));
        if let Some(path) = path {
            self.changed.insert(path.to_string());
        }
    }

    fn tool_call(&self, name: &str, input: &Value) -> String {
        let name = self.paint(BOLD, &capitalize(name));
        let summary = summarize_input(input);
        let call = if summary.is_empty() {
            name
        } else {
            format!("{name}({summary})")
        };
        format!("{} {call}", self.paint(GREEN, "⏺"))
    }

    fn result(&self, tool: &str, output: &str, is_error: bool) -> String {
        let lines: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
        let first = lines
            .first()
            .map(|l| truncate(&shorten(l.trim()), 100))
            .unwrap_or_default();
        if is_error {
            let first = if first.is_empty() {
                "Error".to_string()
            } else {
                first
            };
            return format!("{RESULT_PREFIX}{}", self.paint(RED, &first));
        }

        let body = if lines.is_empty() {
            "(no output)".to_string()
        } else if tool.eq_ignore_ascii_case("read") {
            format!("Read {} lines", lines.len())
        } else if lines.len() > 1 {
            format!("{first} … +{} lines", lines.len() - 1)
        } else {
            first
        };
        format!("{RESULT_PREFIX}{}", self.paint(DIM, &body))
    }

    fn done(&self, detail: &str) -> Vec<String> {
        let text = if detail.is_empty() {
            "✓ Done".to_string()
        } else {
            format!("✓ Done · {detail}")
        };
        let mut out = vec![self.paint(GREEN, &text)];
        if !self.changed.is_empty() {
            let files: Vec<String> = self.changed.iter().map(|f| shorten(f)).collect();
            let noun = if files.len() == 1 { "file" } else { "files" };
            let body = format!("Changed {} {noun}: {}", files.len(), files.join(", "));
            out.push(format!("{RESULT_PREFIX}{}", self.paint(DIM, &body)));
        }
        out
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
}

/// Runner name for the output format a line belongs to. kimi and minimax emit Claude's format
fn detect_agent(v: &Value) -> Option<&'static str> {
    let kind = json_str(v, "/type");
    if matches!(kind, "system" | "assistant" | "user" | "result") {
        Some("claude")
    } else if ["thread.", "turn.", "item."]
        .iter()
        .any(|p| kind.starts_with(p))
    {
        Some("codex")
    } else if v.get("part").is_some() || v.get("sessionID").is_some() {
        Some("opencode")
    } else {
        None
    }
}

/// Print entries with a blank line before each step, keeping results right under their call
pub fn print(entries: &[String]) {
    for entry in entries {
        if entry.starts_with(RESULT_PREFIX) {
            println!("{entry}");
        } else {
            println!("\n{entry}");
        }
    }
}

/// Files written or edited in raw NDJSON output, the fallback when the agent reports none
pub fn extract_files_changed(content: &str) -> Vec<String> {
    let mut formatter = Formatter::new(false);
    for line in content.lines() {
        formatter.format(line);
    }
    formatter.changed_files()
}

/// One line for a tool call's input: the command, path, pattern or query it acts on
fn summarize_input(input: &Value) -> String {
    const KEYS: [&str; 9] = [
        "command",
        "file_path",
        "filePath",
        "pattern",
        "path",
        "query",
        "url",
        "description",
        "prompt",
    ];
    let value = KEYS
        .iter()
        .find_map(|k| input[k].as_str().filter(|s| !s.is_empty()))
        .or_else(|| {
            input
                .as_object()?
                .values()
                .find_map(|v| v.as_str().filter(|s| !s.is_empty()))
        })
        .unwrap_or("");
    let mut lines = value.lines();
    let first = shorten(lines.next().unwrap_or(""));
    let more = if lines.next().is_some() { " …" } else { "" };
    truncate(&format!("{first}{more}"), 100)
}

/// Drop the worktree root from paths and abbreviate the home dir
fn shorten(text: &str) -> String {
    let text = WORKTREE.replace_all(text, "");
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => text.replace(&home, "~"),
        _ => text.into_owned(),
    }
}

fn capitalize(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => "Tool".to_string(),
    }
}

/// Truncate a string to at most `max` chars, appending `…` if cut
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

    fn fmt(lines: &[&str]) -> Vec<String> {
        let mut f = Formatter::new(false);
        lines.iter().flat_map(|l| f.format(l)).collect()
    }

    #[test]
    fn claude_transcript() {
        let lines = [
            r#"{"type":"system","subtype":"init","model":"claude-sonnet-5"}"#,
            r#"{"type":"system","subtype":"thinking_tokens","tokens":12}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"","signature":"x"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Let me read it.\nThen fix."},{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"/x/.orch/worktrees/orch/br/src/main.rs"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"1\tfn a() {}\n2\tfn b() {}\n"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t2","name":"Edit","input":{"file_path":"/x/.orch/worktrees/orch/br/src/main.rs","old_string":"a","new_string":"b"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t2","content":"Exit code 1\nboom","is_error":true}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"num_turns":3,"duration_ms":42000}"#,
        ];
        assert_eq!(
            fmt(&lines),
            vec![
                "⏺ Session started · claude-sonnet-5",
                "⏺ Let me read it.\n  Then fix.",
                "⏺ Read(src/main.rs)",
                "  ⎿  Read 2 lines",
                "⏺ Edit(src/main.rs)",
                "  ⎿  Exit code 1",
                "✓ Done · 3 turns · 42s",
                "  ⎿  Changed 1 file: src/main.rs",
            ]
        );
    }

    #[test]
    fn codex_transcript() {
        let lines = [
            r#"{"type":"thread.started","thread_id":"t"}"#,
            r#"{"type":"error","message":"Reconnecting... 1/5"}"#,
            r#"{"type":"item.completed","item":{"id":"i0","type":"reasoning","text":"**Checking tests**\n\nbody"}}"#,
            r#"{"type":"item.completed","item":{"id":"i1","type":"agent_message","text":"I will fix the bug."}}"#,
            r#"{"type":"item.started","item":{"id":"i2","type":"command_execution","command":"ls"}}"#,
            r#"{"type":"item.completed","item":{"id":"i2","type":"command_execution","command":"/bin/zsh -lc 'cargo test'","aggregated_output":"running 3 tests\nok\nok\n","exit_code":0}}"#,
            r#"{"type":"item.completed","item":{"id":"i3","type":"file_change","changes":[{"path":"src/x.rs","kind":"update"}]}}"#,
            r#"{"type":"turn.completed","usage":{}}"#,
        ];
        assert_eq!(
            fmt(&lines),
            vec![
                "✗ Reconnecting... 1/5",
                "✻ Checking tests",
                "⏺ I will fix the bug.",
                "⏺ Bash(cargo test)",
                "  ⎿  running 3 tests … +2 lines",
                "⏺ Edit(src/x.rs)",
                "✓ Done",
                "  ⎿  Changed 1 file: src/x.rs",
            ]
        );
    }

    #[test]
    fn opencode_transcript() {
        let lines = [
            r#"{"type":"step_start","sessionID":"s","part":{"type":"step-start"}}"#,
            r#"{"type":"text","sessionID":"s","part":{"type":"text","text":"Reviewing PR."}}"#,
            r#"{"type":"tool_use","sessionID":"s","part":{"tool":"read","callID":"c1","state":{"status":"completed","input":{"filePath":"src/lib.rs"},"output":"a\nb\nc"}}}"#,
            r#"{"type":"tool_use","sessionID":"s","part":{"tool":"bash","callID":"c2","state":{"status":"completed","input":{"command":"git status"},"output":""}}}"#,
            r#"{"type":"step_finish","sessionID":"s","part":{"reason":"tool-calls"}}"#,
            r#"{"type":"step_finish","sessionID":"s","part":{"reason":"stop"}}"#,
        ];
        assert_eq!(
            fmt(&lines),
            vec![
                "⏺ Reviewing PR.",
                "⏺ Read(src/lib.rs)",
                "  ⎿  Read 3 lines",
                "⏺ Bash(git status)",
                "  ⎿  (no output)",
                "✓ Done",
            ]
        );
    }

    #[test]
    fn detects_each_format() {
        let detect = |line: &str| detect_agent(&serde_json::from_str(line).unwrap());
        assert_eq!(
            detect(r#"{"type":"assistant","message":{}}"#),
            Some("claude")
        );
        assert_eq!(detect(r#"{"type":"thread.started"}"#), Some("codex"));
        assert_eq!(detect(r#"{"type":"text","part":{}}"#), Some("opencode"));
        assert_eq!(detect(r#"{"type":"error","message":"x"}"#), None);
    }

    #[test]
    fn error_before_detection_is_shown() {
        assert_eq!(
            fmt(&[r#"{"type":"error","message":"usage limit"}"#]),
            vec!["✗ usage limit"]
        );
    }

    #[test]
    fn long_command_is_cut_to_one_line() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t","name":"Bash","input":{"command":"cargo test\necho ok"}}]}}"#;
        assert_eq!(fmt(&[line]), vec!["⏺ Bash(cargo test …)"]);
    }

    #[test]
    fn non_json_passed_through_without_escapes() {
        assert_eq!(
            fmt(&["\x1b[?25hfatal: not a git repository\r"]),
            vec!["fatal: not a git repository"]
        );
        assert!(fmt(&["\x1b[?25h"]).is_empty());
        assert!(fmt(&["   "]).is_empty());
    }

    #[test]
    fn color_wraps_markers() {
        let mut f = Formatter::new(true);
        f.format(r#"{"type":"thread.started"}"#);
        assert_eq!(
            f.format(r#"{"type":"turn.completed"}"#),
            vec!["\x1b[32m✓ Done\x1b[0m"]
        );
    }

    #[test]
    fn truncate_adds_ellipsis() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("abcdefghij", 5), "abcde…");
    }

    #[test]
    fn extract_files_changed_from_claude_and_opencode() {
        let claude = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"a","name":"Write","input":{"file_path":"src/b.rs"}}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"b","name":"Edit","input":{"file_path":"src/a.rs"}}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"c","name":"Read","input":{"file_path":"src/c.rs"}}]}}"#,
        );
        assert_eq!(extract_files_changed(claude), vec!["src/a.rs", "src/b.rs"]);
        let opencode = r#"{"type":"tool_use","part":{"tool":"edit","callID":"c","state":{"status":"completed","input":{"filePath":"src/bar.rs"}}}}"#;
        assert_eq!(extract_files_changed(opencode), vec!["src/bar.rs"]);
        let none = r#"{"type":"result","subtype":"success","result":"done"}"#;
        assert!(extract_files_changed(none).is_empty());
    }
}
