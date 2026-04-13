use crate::cli::init_store;
use crate::cli::ndjson;
use crate::home;
use crate::store::{TaskStatus, TaskStore};
use anyhow::Context;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Arc;

pub async fn export(
    task_id: &str,
    attempt: Option<u32>,
    format: ExportFormat,
) -> anyhow::Result<()> {
    let store = Arc::new(init_store().await?);

    let (internal_id, repo) = parse_task_id(task_id, &store).await?;
    let task = store.get(internal_id).await.context("task not found")?;

    let attempt = match attempt {
        Some(n) => n,
        None => {
            let runs = store.get_runs(internal_id).await?;
            if runs.is_empty() {
                anyhow::bail!("no attempts found for task");
            }
            let max_attempt = runs.iter().map(|r| r.attempt).max().unwrap_or(1);
            max_attempt as u32
        }
    };

    let attempt_dir = home::task_attempt_dir(&repo, task_id, attempt)?;
    let output_path = attempt_dir.join("output.json");

    if !output_path.exists() {
        anyhow::bail!("attempt {} not found (no output file)", attempt);
    }

    let content = std::fs::read_to_string(&output_path)?;

    match format {
        ExportFormat::Markdown => export_markdown(&task, attempt, &content, &store, &repo).await,
        ExportFormat::Json => export_json(&task, attempt, &content),
        ExportFormat::Raw => {
            println!("{}", content);
            Ok(())
        }
    }
}

/// Structured summary extracted from raw NDJSON output.
struct ParsedOutput {
    /// Files written or edited (deduplicated, sorted).
    files_changed: Vec<String>,
    /// Files read during the session (deduplicated, sorted).
    files_read: Vec<String>,
    /// Tool call counts keyed by tool name.
    tool_counts: BTreeMap<String, usize>,
    /// Git-related bash commands executed.
    git_commands: Vec<String>,
    /// Representative assistant text lines (for "key decisions").
    key_messages: Vec<String>,
}

impl ParsedOutput {
    fn is_empty(&self) -> bool {
        self.files_changed.is_empty()
            && self.files_read.is_empty()
            && self.tool_counts.is_empty()
            && self.git_commands.is_empty()
            && self.key_messages.is_empty()
    }
}

/// Parse NDJSON content into a structured [`ParsedOutput`].
///
/// Handles all three agent formats (Claude, Codex, OpenCode) detected by
/// `ndjson::format_line`.  Only write/edit tool calls contribute to
/// `files_changed`; read calls go to `files_read`.  Bash calls that start
/// with `git` are captured as `git_commands`.  Assistant text messages
/// longer than 40 chars are kept as `key_messages` (capped at 10).
fn parse_output(content: &str) -> ParsedOutput {
    let mut files_changed: std::collections::BTreeSet<String> = Default::default();
    let mut files_read: std::collections::BTreeSet<String> = Default::default();
    let mut tool_counts: BTreeMap<String, usize> = Default::default();
    let mut git_commands: Vec<String> = Vec::new();
    let mut key_messages: Vec<String> = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let event_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");

        match event_type {
            // ── Claude ────────────────────────────────────────────────────
            "assistant" => {
                if let Some(text) = extract_claude_text(&v) {
                    if text.len() > 40 && key_messages.len() < 10 {
                        key_messages.push(first_sentence(&text, 160));
                    }
                }
            }

            "tool_use" if v.get("tool").is_some() => {
                // Claude tool_use: {"type":"tool_use","tool":{"name":"...","input":{...}}}
                if let Some(tool) = v.get("tool") {
                    let name = tool.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                    *tool_counts.entry(name.to_string()).or_default() += 1;
                    let input = tool.get("input");
                    record_file_or_command(
                        name,
                        input,
                        &mut files_changed,
                        &mut files_read,
                        &mut git_commands,
                    );
                }
            }

            // ── OpenCode ──────────────────────────────────────────────────
            "text" => {
                let text = v
                    .get("part")
                    .and_then(|p| p.get("text"))
                    .and_then(|t| t.as_str())
                    .or_else(|| v.get("text").and_then(|t| t.as_str()))
                    .unwrap_or("");
                if text.len() > 40 && key_messages.len() < 10 {
                    key_messages.push(first_sentence(text, 160));
                }
            }

            "tool_use" if v.get("part").is_some() => {
                // OpenCode tool_use: {"type":"tool_use","part":{"tool":"...","state":{"input":{...}}}}
                if let Some(part) = v.get("part") {
                    let name = part.get("tool").and_then(|t| t.as_str()).unwrap_or("?");
                    *tool_counts.entry(name.to_string()).or_default() += 1;
                    let input = part.get("state").and_then(|s| s.get("input"));
                    record_file_or_command(
                        name,
                        input,
                        &mut files_changed,
                        &mut files_read,
                        &mut git_commands,
                    );
                }
            }

            // ── Codex ─────────────────────────────────────────────────────
            "item.completed" => {
                if let Some(item) = v.get("item") {
                    let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    match item_type {
                        "agent_message" => {
                            let text = item.get("text").and_then(|t| t.as_str()).unwrap_or("");
                            if text.len() > 40 && key_messages.len() < 10 {
                                key_messages.push(first_sentence(text, 160));
                            }
                        }
                        "command_execution" => {
                            let cmd = item.get("command").and_then(|c| c.as_str()).unwrap_or("");
                            *tool_counts.entry("bash".to_string()).or_default() += 1;
                            let first_line = cmd.lines().next().unwrap_or("").trim();
                            if (first_line.starts_with("git ") || first_line == "git")
                                && git_commands.len() < 20
                            {
                                git_commands.push(truncate_str(first_line, 120));
                            }
                        }
                        _ => {}
                    }
                }
            }

            _ => {}
        }
    }

    ParsedOutput {
        files_changed: files_changed.into_iter().collect(),
        files_read: files_read.into_iter().collect(),
        tool_counts,
        git_commands,
        key_messages,
    }
}

/// Record a file path or git command from a tool call.
fn record_file_or_command(
    name: &str,
    input: Option<&serde_json::Value>,
    files_changed: &mut std::collections::BTreeSet<String>,
    files_read: &mut std::collections::BTreeSet<String>,
    git_commands: &mut Vec<String>,
) {
    let Some(input) = input else { return };
    match name {
        "Write" | "write" | "NotebookEdit" => {
            if let Some(p) = str_field(input, "file_path") {
                files_changed.insert(p);
            }
        }
        "Edit" | "edit" | "MultiEdit" => {
            if let Some(p) = str_field(input, "file_path") {
                files_changed.insert(p);
            }
        }
        "Read" | "read" => {
            if let Some(p) = str_field(input, "file_path") {
                files_read.insert(p);
            }
        }
        "Bash" | "bash" => {
            if let Some(cmd) = str_field(input, "command") {
                let first_line = cmd.lines().next().unwrap_or("").trim();
                if (first_line.starts_with("git ") || first_line == "git")
                    && git_commands.len() < 20
                {
                    git_commands.push(truncate_str(first_line, 120));
                }
            }
        }
        _ => {}
    }
}

async fn export_markdown(
    task: &crate::store::Task,
    attempt: u32,
    content: &str,
    store: &Arc<TaskStore>,
    _repo: &str,
) -> anyhow::Result<()> {
    let runs = store.get_runs(task.id).await?;
    let attempt_run = runs.iter().find(|r| r.attempt == attempt as i32);

    let agent = task.agent.as_deref().unwrap_or("unknown");
    let model = task.model.as_deref().unwrap_or("-");

    let duration = attempt_run.map(|r| r.duration_secs).unwrap_or(0.0);
    let duration_str = format_duration(duration);

    let input_tokens = task.input_tokens;
    let output_tokens = task.output_tokens;

    let cost = task.total_cost_usd;

    println!(
        "# Session: {} (Attempt {})",
        task_id_display(&task.id, &task.external_id),
        attempt
    );
    println!();
    println!("**Agent**: {} ({})", agent, model);
    println!("**Duration**: {}", duration_str);
    println!(
        "**Tokens**: {} in / {} out",
        format_num(input_tokens),
        format_num(output_tokens)
    );
    if cost > 0.0 {
        println!("**Cost**: ${:.4}", cost);
    }
    println!("---");

    if content.trim().is_empty() {
        println!("(no output captured)");
        return Ok(());
    }

    // ── Structured summary ────────────────────────────────────────────────
    let parsed = parse_output(content);
    if !parsed.is_empty() {
        println!();
        println!("## Summary");

        if !parsed.files_changed.is_empty() {
            println!();
            println!("### Files Changed");
            for f in &parsed.files_changed {
                println!("- `{}`", f);
            }
        }

        if !parsed.tool_counts.is_empty() {
            println!();
            println!("### Tool Usage");
            // Sort by count descending, then name ascending
            let mut counts: Vec<(&String, &usize)> = parsed.tool_counts.iter().collect();
            counts.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            for (tool, count) in &counts {
                println!("- **{}**: {}", tool, count);
            }
        }

        if !parsed.git_commands.is_empty() {
            println!();
            println!("### Git Context");
            // Deduplicate while preserving order
            let mut seen = std::collections::HashSet::new();
            for cmd in &parsed.git_commands {
                if seen.insert(cmd.as_str()) {
                    println!("- `{}`", cmd);
                }
            }
        }

        if !parsed.key_messages.is_empty() {
            println!();
            println!("### Key Decisions");
            for msg in &parsed.key_messages {
                println!("- {}", msg);
            }
        }
    }

    // ── Full output ───────────────────────────────────────────────────────
    println!();
    println!("## Output");
    println!();

    let lines: Vec<&str> = content.lines().collect();

    for line in &lines {
        if let Some(formatted) = ndjson::format_line(line) {
            for fline in formatted.lines() {
                println!("{fline}");
            }
        }
    }

    // ── Result ────────────────────────────────────────────────────────────
    let summary = &task.summary;
    if !summary.is_empty() {
        println!();
        println!("---");
        println!();
        println!("## Result");
        println!();

        let status = if task.status == TaskStatus::Done {
            "success"
        } else if task.status == TaskStatus::Blocked {
            "blocked"
        } else {
            "incomplete"
        };

        println!("**Status**: {}", status);
        println!("**Summary**: {}", summary);
    }

    let error = &task.last_error;
    if !error.is_empty() {
        println!();
        println!("**Error**: {}", error);
    }

    Ok(())
}

fn export_json(task: &crate::store::Task, attempt: u32, content: &str) -> anyhow::Result<()> {
    let output_lines: Vec<serde_json::Value> = content
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            serde_json::from_str(trimmed).ok()
        })
        .collect();

    let parsed = parse_output(content);

    #[derive(Serialize)]
    struct SessionExport {
        task_id: i64,
        external_id: Option<String>,
        attempt: u32,
        agent: Option<String>,
        model: Option<String>,
        input_tokens: i64,
        output_tokens: i64,
        total_cost_usd: f64,
        summary: String,
        last_error: String,
        status: String,
        files_changed: Vec<String>,
        files_read: Vec<String>,
        tool_counts: BTreeMap<String, usize>,
        git_commands: Vec<String>,
        key_messages: Vec<String>,
        output: Vec<serde_json::Value>,
    }

    let export = SessionExport {
        task_id: task.id,
        external_id: task.external_id.clone(),
        attempt,
        agent: task.agent.clone(),
        model: task.model.clone(),
        input_tokens: task.input_tokens,
        output_tokens: task.output_tokens,
        total_cost_usd: task.total_cost_usd,
        summary: task.summary.clone(),
        last_error: task.last_error.clone(),
        status: task.status.as_str().to_string(),
        files_changed: parsed.files_changed,
        files_read: parsed.files_read,
        tool_counts: parsed.tool_counts,
        git_commands: parsed.git_commands,
        key_messages: parsed.key_messages,
        output: output_lines,
    };

    println!("{}", serde_json::to_string_pretty(&export)?);

    Ok(())
}

async fn parse_task_id(task_id: &str, store: &Arc<TaskStore>) -> anyhow::Result<(i64, String)> {
    if task_id.starts_with("internal:") {
        let id_part = task_id
            .strip_prefix("internal:")
            .unwrap_or(task_id)
            .parse::<i64>()
            .context("invalid internal task ID")?;
        let repo = store
            .get(id_part)
            .await
            .context("internal task not found")?
            .repo;
        Ok((id_part, repo))
    } else {
        let id: i64 = task_id.parse().context("invalid task ID")?;
        let repo = crate::config::get_current_repo()
            .context("no project configured — run `orch init` first")?;
        Ok((id, repo))
    }
}

fn task_id_display(internal_id: &i64, external_id: &Option<String>) -> String {
    match external_id {
        Some(ext) => format!("{}:{}", ext, internal_id),
        None => format!("internal:{}", internal_id),
    }
}

fn format_num(n: i64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

fn format_duration(secs: f64) -> String {
    if secs < 60.0 {
        format!("{:.0}s", secs)
    } else if secs < 3600.0 {
        let mins = secs / 60.0;
        if mins < 10.0 {
            format!("{:.1}m", mins)
        } else {
            format!("{:.0}m", mins)
        }
    } else {
        let hours = secs / 3600.0;
        format!("{:.1}h", hours)
    }
}

/// Extract text content from a Claude `assistant` event.
fn extract_claude_text(v: &serde_json::Value) -> Option<String> {
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

/// Get a string field from a JSON object.
fn str_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|s| s.as_str()).map(str::to_string)
}

/// Return the first sentence (or up to `max` chars) of `s`.
fn first_sentence(s: &str, max: usize) -> String {
    // Find the end of the first sentence.
    let sentence_end = s
        .char_indices()
        .find(|(_, c)| matches!(c, '.' | '!' | '?'))
        .map(|(i, _)| i + 1)
        .unwrap_or(s.len());
    let candidate = &s[..sentence_end.min(s.len())];
    truncate_str(candidate.trim(), max)
}

/// Truncate a string to at most `max` chars, appending `…` if cut.
fn truncate_str(s: &str, max: usize) -> String {
    for (char_count, (byte_idx, _ch)) in s.char_indices().enumerate() {
        if char_count >= max {
            return format!("{}…", &s[..byte_idx]);
        }
    }
    s.to_string()
}

#[derive(Debug, Clone, Copy, Default)]
pub enum ExportFormat {
    #[default]
    Markdown,
    Json,
    Raw,
}

impl std::str::FromStr for ExportFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "markdown" | "md" => Ok(Self::Markdown),
            "json" | "js" => Ok(Self::Json),
            "raw" | "ndjson" => Ok(Self::Raw),
            _ => Err(format!("unknown format: {}", s)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_claude_write_edit() {
        let content = r#"
{"type":"tool_use","tool":{"name":"Write","input":{"file_path":"src/lib.rs","content":"..."}}}
{"type":"tool_use","tool":{"name":"Edit","input":{"file_path":"src/main.rs","old_string":"foo","new_string":"bar"}}}
{"type":"tool_use","tool":{"name":"Read","input":{"file_path":"Cargo.toml"}}}
"#;
        let p = parse_output(content);
        assert_eq!(p.files_changed, vec!["src/lib.rs", "src/main.rs"]);
        assert_eq!(p.files_read, vec!["Cargo.toml"]);
        assert_eq!(p.tool_counts["Write"], 1);
        assert_eq!(p.tool_counts["Edit"], 1);
        assert_eq!(p.tool_counts["Read"], 1);
    }

    #[test]
    fn parse_claude_git_bash() {
        let content = r#"
{"type":"tool_use","tool":{"name":"Bash","input":{"command":"git status"}}}
{"type":"tool_use","tool":{"name":"Bash","input":{"command":"cargo build"}}}
{"type":"tool_use","tool":{"name":"Bash","input":{"command":"git commit -m \"fix\""}}}
"#;
        let p = parse_output(content);
        assert_eq!(p.git_commands.len(), 2);
        assert!(p.git_commands.contains(&"git status".to_string()));
        assert!(p
            .git_commands
            .contains(&"git commit -m \"fix\"".to_string()));
        assert_eq!(p.tool_counts["Bash"], 3);
    }

    #[test]
    fn parse_opencode_tool_use() {
        let content = r#"
{"type":"tool_use","part":{"tool":"write","state":{"input":{"file_path":"README.md"}}}}
{"type":"tool_use","part":{"tool":"bash","state":{"input":{"command":"git diff"}}}}
"#;
        let p = parse_output(content);
        assert_eq!(p.files_changed, vec!["README.md"]);
        assert_eq!(p.git_commands, vec!["git diff"]);
    }

    #[test]
    fn parse_codex_command_execution() {
        let content = r#"
{"type":"item.completed","item":{"type":"command_execution","command":"git log --oneline -5"}}
{"type":"item.completed","item":{"type":"command_execution","command":"cargo test"}}
"#;
        let p = parse_output(content);
        assert_eq!(p.git_commands, vec!["git log --oneline -5"]);
        assert_eq!(p.tool_counts["bash"], 2);
    }

    #[test]
    fn parse_key_messages_from_assistant() {
        let content = r#"
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"This is a short msg."}]}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I have analyzed the codebase and found that the main issue is in the authentication module."}]}}
"#;
        let p = parse_output(content);
        // Short message (<= 40 chars) should be filtered out
        assert_eq!(p.key_messages.len(), 1);
        assert!(p.key_messages[0].contains("authentication"));
    }

    #[test]
    fn parse_empty_content() {
        let p = parse_output("");
        assert!(p.is_empty());
    }

    #[test]
    fn first_sentence_with_period() {
        let s = "I will fix the bug. Then run tests.";
        assert_eq!(first_sentence(s, 200), "I will fix the bug.");
    }

    #[test]
    fn first_sentence_truncated() {
        let long = "abcdefghijklmnopqrstuvwxyz".repeat(10);
        let result = first_sentence(&long, 20);
        assert!(result.ends_with('…'));
    }

    #[test]
    fn format_num_thousands() {
        assert_eq!(format_num(1234567), "1,234,567");
        assert_eq!(format_num(999), "999");
    }

    #[test]
    fn format_duration_variants() {
        assert_eq!(format_duration(45.0), "45s");
        assert_eq!(format_duration(90.0), "1.5m");
        assert_eq!(format_duration(600.0), "10m");
        assert_eq!(format_duration(7200.0), "2.0h");
    }
}
