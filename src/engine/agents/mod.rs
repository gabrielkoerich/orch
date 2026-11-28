//! Per-agent runner trait, error types, and agent registry.
//!
//! Each agent (Claude, Codex, OpenCode, Grok) has a different CLI invocation,
//! output format, and error pattern. This module defines a common trait
//! so the runner can delegate parsing and error classification to the
//! correct implementation.

#[macro_use]
mod delegate;

#[cfg(test)]
pub(crate) mod fixture;
/// Shared error pattern detection utilities used by multiple agent runners.
pub(crate) mod patterns;

pub mod claude;
pub mod codex;
pub mod direct;
pub mod grok;
pub mod kimi;
pub mod minimax;
pub mod opencode;
pub mod parser;
pub mod status_heuristics;

use crate::config;
use crate::engine::agents::parser::AgentResponse;
use std::collections::BTreeSet;
use std::error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;

/// Unified permission rules that each agent translates into its native flags.
///
/// Instead of hardcoding `--permission-mode bypassPermissions` for Claude and
/// `--sandbox workspace-write` for Codex, we define a single set of rules and
/// let each `AgentRunner` format them for its CLI.
#[derive(Debug, Clone)]
pub struct PermissionRules {
    /// Whether the agent runs fully autonomous (no user prompts).
    pub autonomous: bool,
    /// Sandbox level for filesystem access.
    pub sandbox: SandboxLevel,
    /// Tool patterns to disallow (e.g., `["Bash(rm *)", "Bash(rm -*)"]`).
    pub disallowed_tools: Vec<String>,
    /// Tools to allow (whitelist). Overrides `disallowed_tools` when non-empty.
    ///
    /// Items are either agent-native tool names (e.g., "Edit", "Write", "Bash")
    /// or CLI command names (e.g., "git", "npm") which get translated to
    /// agent-specific patterns (e.g., `Bash(git*)` for Claude).
    pub allowed_tools: Vec<String>,
    /// Paths where the agent is allowed to edit files (worktree path).
    /// When set, Edit/Write tools are scoped to these paths only.
    /// Set dynamically per invocation (not from config).
    pub allowed_edit_paths: Vec<PathBuf>,
    /// Restrict to orchestration-only operations — denies Read, Glob, Grep,
    /// and Bash commands for browsing/reading files (ls, cat, find, etc.).
    /// Agents translate this into their native permission model.
    pub deny_read_only: bool,
    /// Additional directories that must be writable (e.g. the git common dir
    /// when running inside a worktree whose `.git` metadata lives outside the
    /// sandbox root). Codex translates these into `--add-dir` flags; other
    /// agents ignore the field.
    pub extra_writable_dirs: Vec<PathBuf>,
}

/// Sandbox level — how much filesystem access the agent gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxLevel {
    /// Agent can only write within the workspace/worktree.
    WorkspaceWrite,
    /// Agent has full filesystem access (dangerous).
    FullAccess,
    /// No sandboxing (orch manages isolation externally).
    None,
    /// The OS sandbox runs the agent, so the agent's own sandbox is off
    Os,
    /// Reads only, and runs `allowed_tools` and nothing else, refused without a prompt. For a chat
    /// whose context holds text anyone can write
    ReadOnly,
}

impl Default for PermissionRules {
    fn default() -> Self {
        Self {
            autonomous: true,
            sandbox: SandboxLevel::WorkspaceWrite,
            disallowed_tools: vec![
                "Bash(rm *)".to_string(),
                "Bash(rm -*)".to_string(),
                "Bash(git push*)".to_string(),
            ],
            allowed_tools: vec![],
            allowed_edit_paths: vec![],
            deny_read_only: false,
            extra_writable_dirs: vec![],
        }
    }
}

impl PermissionRules {
    /// Load permission rules from config, falling back to defaults.
    pub fn from_config() -> Self {
        let mut rules = Self::default();

        if let Ok(mode) = config::get("workflow.permissions.mode") {
            rules.autonomous = mode != "supervised";
        }

        if let Ok(sandbox) = config::get("workflow.permissions.sandbox") {
            rules.sandbox = match sandbox.as_str() {
                "full-access" | "danger-full-access" => SandboxLevel::FullAccess,
                "none" => SandboxLevel::None,
                _ => SandboxLevel::WorkspaceWrite,
            };
        }

        if let Ok(tools) = config::get("workflow.disallowed_tools") {
            let parsed: Vec<String> = tools
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !parsed.is_empty() {
                rules.disallowed_tools = parsed;
            }
        }

        if let Ok(tools) = config::get_list("workflow.allowed_tools") {
            rules.allowed_tools = tools;
        }

        rules
    }

    /// Restriction preset for control sessions: denies Read, Glob, Grep, and
    /// Bash commands that read files or list directories (ls, cat, find, etc.).
    ///
    /// The agent remains autonomous but is prevented from browsing or reading
    /// arbitrary files on the host machine. Translation into agent-native flags
    /// is handled by each `AgentRunner::build_command()` implementation.
    /// A chat that may read and run `allowed`, and is refused everything else
    pub fn read_only(allowed: Vec<String>) -> Self {
        Self {
            autonomous: false,
            sandbox: SandboxLevel::ReadOnly,
            disallowed_tools: vec![],
            allowed_tools: allowed,
            allowed_edit_paths: vec![],
            deny_read_only: false,
            extra_writable_dirs: vec![],
        }
    }

    pub fn deny_read_only() -> Self {
        Self {
            autonomous: true,
            sandbox: SandboxLevel::WorkspaceWrite,
            disallowed_tools: vec![
                "Bash(rm *)".to_string(),
                "Bash(rm -*)".to_string(),
                "Bash(git push*)".to_string(),
                "Bash(ls *)".to_string(),
                "Bash(ls)".to_string(),
                "Bash(find *)".to_string(),
                "Bash(cat *)".to_string(),
                "Bash(head *)".to_string(),
                "Bash(tail *)".to_string(),
                "Bash(fzf *)".to_string(),
                "Bash(less *)".to_string(),
                "Bash(more *)".to_string(),
                "Read".to_string(),
                "Glob".to_string(),
                "Grep".to_string(),
            ],
            allowed_tools: vec![],
            allowed_edit_paths: vec![],
            deny_read_only: true,
            extra_writable_dirs: vec![],
        }
    }
}

/// Structured result from per-agent NDJSON extraction.
///
/// Each agent emits a different NDJSON format. The per-agent `find_result`
/// functions parse agent-specific envelopes and return a unified `AgentResult`
/// so callers (review pipeline, response handler) don't need format knowledge.
#[derive(Debug, Clone)]
pub struct AgentResult {
    /// Whether the agent reported an error (e.g. `is_error: true` in Claude).
    pub is_error: bool,
    /// The extracted result text (inner content, stripped of envelope).
    pub result_text: String,
    /// Input tokens consumed (if reported by the agent).
    pub input_tokens: Option<u64>,
    /// Output tokens consumed (if reported by the agent).
    pub output_tokens: Option<u64>,
    /// Total cost in USD (if reported by the agent).
    pub cost_usd: Option<f64>,
    /// Wall-clock duration in milliseconds (if reported by the agent).
    pub duration_ms: Option<u64>,
    /// True when the provider cut the response short by exceeding its
    /// output/reasoning token budget (e.g. opencode's `step_finish` event
    /// reporting `reason: "length"`), as opposed to producing malformed
    /// output. Callers use this to distinguish a token-budget fluke from a
    /// genuinely format-incapable model.
    pub truncated_by_length: bool,
}

/// Concatenate text from all assistant-turn messages in the agent's NDJSON
/// output stream.  Used as an intermediate fallback when the `type:result`
/// envelope text is not valid AgentResponse JSON but an earlier assistant
/// message in the conversation might contain it.
pub fn collect_assistant_messages_text(_agent: &str, ndjson: &str) -> String {
    claude::collect_assistant_messages_text(ndjson)
}

/// Dispatch to the appropriate per-agent result extractor.
///
/// Returns `None` if no structured result could be found in the output
/// (e.g. plain text, empty output, or unrecognized format).
pub fn find_agent_result(agent: &str, ndjson: &str) -> Option<AgentResult> {
    match agent {
        "grok" => grok::find_grok_result(ndjson),
        "opencode" => opencode::find_opencode_result(ndjson),
        "codex" => codex::find_codex_result(ndjson),
        // claude, kimi, minimax and unknown agents all emit the claude format
        _ => claude::find_claude_result(ndjson),
    }
}

/// Parsed response from an agent, including metadata extracted from the
/// agent-specific output envelope.
#[derive(Debug, Clone)]
pub struct ParsedResponse {
    /// Normalized task response (status, summary, accomplished, etc.)
    pub response: AgentResponse,
    /// Input tokens consumed (if reported by the agent).
    pub input_tokens: Option<u64>,
    /// Output tokens consumed (if reported by the agent).
    pub output_tokens: Option<u64>,
    /// Cost in USD of this run, as the agent reported it.
    pub cost_usd: Option<f64>,
    /// Wall-clock duration in milliseconds (if reported by the agent).
    pub duration_ms: Option<u64>,
}

/// Agent-specific error with enough detail for autonomous recovery.
#[derive(Debug, Clone)]
pub enum AgentError {
    /// Rate/usage limit — reroute to different agent, cooldown current.
    RateLimit { message: String },
    /// Auth/billing/API key error — switch agent entirely.
    Auth { message: String },
    /// Requested model not available — try different model, then switch agent.
    ModelUnavailable { message: String, model: String },
    /// Context window exceeded — truncate and retry, then switch agent.
    ContextOverflow { message: String },
    /// Agent hit the task time limit. Retry without cooling the model
    Timeout { elapsed: Duration },
    /// Process exited with no result envelope, usually a kill rather than a model fault
    Interrupted { message: String },
    /// Required tool/binary missing from the environment.
    MissingTool { tool: String },
    /// Sandbox or filesystem permission denied.
    PermissionDenied { message: String },
    /// Agent is waiting for interactive input (e.g., 1Password, SSH).
    WaitingForInput { message: String },
    /// The agent's session ID is no longer valid (session expired or was cleared).
    ///
    /// Returned when Claude reports "No conversation found with session ID: <uuid>".
    /// The caller should reset the stored UUID and retry with a fresh `--session-id`.
    StaleSession { session_id: String },
    /// Agent returned unparseable output.
    InvalidResponse { raw: String },
    /// Agent self-reported a failure in its response.
    AgentFailed { message: String },
    /// Transient network connectivity error — retry same agent, no failover.
    NetworkError { message: String },
    /// Thinking blocks cannot be modified (Claude extended-thinking + tool-use 400).
    ///
    /// Transient API incompatibility inside the Claude CLI's multi-turn tool-use
    /// loop — the Anthropic API rejects the request when thinking blocks are altered
    /// between turns. Not a model capacity problem: does NOT increment failure counts
    /// or apply cooldowns. The task is rerouted to a different agent.
    ThinkingBlockConflict { message: String },
    /// Unclassified error.
    Unknown { exit_code: i32, message: String },
}

impl Display for AgentError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::RateLimit { message, .. } => write!(f, "rate limit: {message}"),
            Self::Auth { message } => write!(f, "auth error: {message}"),
            Self::ModelUnavailable { message, model } => {
                write!(f, "model unavailable ({model}): {message}")
            }
            Self::ContextOverflow { message, .. } => write!(f, "context overflow: {message}"),
            Self::Timeout { elapsed } => write!(f, "timeout after {}s", elapsed.as_secs()),
            Self::Interrupted { message } => write!(f, "interrupted: {message}"),
            Self::MissingTool { tool } => write!(f, "missing tool: {tool}"),
            Self::PermissionDenied { message } => write!(f, "permission denied: {message}"),
            Self::WaitingForInput { message } => write!(f, "waiting for input: {message}"),
            Self::StaleSession { session_id } => {
                write!(f, "stale session (session expired): {session_id}")
            }
            Self::InvalidResponse { raw } => {
                let end = truncate_at_char_boundary(raw, 200);
                write!(f, "invalid response: {}", &raw[..end])
            }
            Self::AgentFailed { message } => write!(f, "agent failed: {message}"),
            Self::NetworkError { message } => write!(f, "network error: {message}"),
            Self::ThinkingBlockConflict { message } => {
                write!(f, "thinking block conflict: {message}")
            }
            Self::Unknown { exit_code, message } => {
                write!(f, "unknown error (exit {exit_code}): {message}")
            }
        }
    }
}

impl error::Error for AgentError {}

/// Return the variant name of an `AgentError` as a static string,
/// for structured logging and `result.json` output.
pub fn error_class_name(err: &AgentError) -> &'static str {
    match err {
        AgentError::RateLimit { .. } => "RateLimit",
        AgentError::Auth { .. } => "Auth",
        AgentError::ModelUnavailable { .. } => "ModelUnavailable",
        AgentError::ContextOverflow { .. } => "ContextOverflow",
        AgentError::Timeout { .. } => "Timeout",
        AgentError::Interrupted { .. } => "Interrupted",
        AgentError::MissingTool { .. } => "MissingTool",
        AgentError::PermissionDenied { .. } => "PermissionDenied",
        AgentError::WaitingForInput { .. } => "WaitingForInput",
        AgentError::StaleSession { .. } => "StaleSession",
        AgentError::InvalidResponse { .. } => "InvalidResponse",
        AgentError::AgentFailed { .. } => "AgentFailed",
        AgentError::NetworkError { .. } => "NetworkError",
        AgentError::ThinkingBlockConflict { .. } => "ThinkingBlockConflict",
        AgentError::Unknown { .. } => "Unknown",
    }
}

/// Extract file paths from free-form text.
///
/// Matches common source file patterns like `src/foo/bar.rs`, `tests/x.py`,
/// `path/to/file.ts`, etc. Deduplicates and returns them sorted.
fn extract_file_paths(text: &str) -> Vec<String> {
    // Match path-like tokens: optional leading slash or no slash, at least one
    // directory segment or a plain filename, with a recognised extension.
    let extensions = [
        "rs", "py", "ts", "tsx", "js", "jsx", "go", "java", "c", "cpp", "h", "hpp", "rb", "sh",
        "md", "toml", "yaml", "yml", "json", "sql", "html", "css", "swift", "kt",
    ];
    let mut found: BTreeSet<String> = BTreeSet::new();
    for token in text.split_whitespace() {
        // Strip surrounding punctuation (backticks, quotes, parens, commas).
        // We intentionally do NOT strip '.' here because it is part of file extensions.
        let token = token.trim_matches(|c: char| {
            matches!(
                c,
                '`' | '\'' | '"' | '(' | ')' | '[' | ']' | ',' | ':' | ';'
            )
        });
        // Strip a lone trailing dot that ends a sentence (e.g. "src/foo.rs.").
        // Only strip if the result still has a recognised extension — this avoids
        // accidentally stripping the final dot of "src/foo.rs" when there is none.
        let token = if let Some(without) = token.strip_suffix('.') {
            let still_has_ext = extensions
                .iter()
                .any(|ext| without.ends_with(&format!(".{ext}")));
            if still_has_ext { without } else { token }
        } else {
            token
        };
        // Must contain at least one slash (directory separator) or look like a
        // root-relative path, and end with a known extension.
        let has_ext = extensions
            .iter()
            .any(|ext| token.ends_with(&format!(".{ext}")));
        if !has_ext {
            continue;
        }
        // Accept tokens with at least one path separator, or starting with a
        // known top-level directory name common in Rust/JS/Python projects.
        let path_like = token.contains('/')
            || token.starts_with("src")
            || token.starts_with("tests")
            || token.starts_with("lib")
            || token.starts_with("bin")
            || token.starts_with("prompts")
            || token.starts_with("migrations");
        if path_like && token.len() >= 4 && !token.contains("://") {
            found.insert(token.to_string());
        }
    }
    found.into_iter().collect()
}

/// Split free-form text into accomplished and remaining bullet lists.
///
/// Lines under "remaining", "todo", "next steps", "still needed" headings are
/// classified as `remaining`; all other bullet lines go into `accomplished`.
fn extract_bullet_sections(text: &str) -> (Vec<String>, Vec<String>) {
    let remaining_headers = [
        "remaining",
        "todo",
        "to do",
        "next steps",
        "still needed",
        "left to do",
        "not done",
        "outstanding",
    ];

    let mut accomplished: Vec<String> = vec![];
    let mut remaining: Vec<String> = vec![];
    let mut in_remaining_section = false;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_lowercase();

        // Detect section headings: only lines with explicit heading markers.
        // Require trailing ':' or '#'-prefix to avoid misclassifying continuation
        // lines (e.g. explanatory text after a bullet) as headings, which would
        // incorrectly reset `in_remaining_section`.
        let is_heading = trimmed.ends_with(':') || trimmed.starts_with('#');

        if is_heading {
            in_remaining_section = remaining_headers.iter().any(|h| lower.contains(h));
            continue;
        }

        // Bullet lines: -, *, or numbered (1., 2.)
        let is_bullet = trimmed.starts_with('-')
            || trimmed.starts_with('*')
            || trimmed.chars().next().is_some_and(|c| c.is_ascii_digit()) && trimmed.contains('.');

        if !is_bullet {
            continue;
        }

        // Strip leading bullet character(s).
        let content = trimmed
            .trim_start_matches(|c: char| c == '-' || c == '*' || c.is_ascii_digit() || c == '.')
            .trim()
            .to_string();

        if content.is_empty() {
            continue;
        }

        if in_remaining_section {
            remaining.push(content);
        } else {
            accomplished.push(content);
        }
    }

    (accomplished, remaining)
}

/// Detect if text is a CLI/arg-parser diagnostic rather than an agent response.
/// CLI diagnostics indicate process startup failure, not an agent answer.
fn is_cli_parser_diagnostic(text: &str) -> bool {
    let lower = text.to_lowercase();

    // Clap-style error messages
    if lower.contains("error: unexpected argument") {
        return true;
    }

    // Usage/help text patterns
    if text.lines().any(|line| {
        let trimmed = line.trim();
        trimmed.starts_with("Usage:") || trimmed.starts_with("usage:")
    }) {
        return true;
    }

    // Clap help/info hints
    if lower.contains("for more information, try '--help'") {
        return true;
    }

    // Clap "tip" messages about passing values
    if lower.contains("tip: to pass '") {
        return true;
    }

    false
}

/// Detect whether `text` looks like an NDJSON event stream emitted by an
/// agent CLI (opencode/claude/codex). Returns true when the first non-empty
/// line parses as a JSON object containing a `type` field with one of the
/// known event-type strings.
///
/// This guard prevents raw NDJSON from being interpreted as agent prose and
/// leaking into notification summaries.
fn looks_like_ndjson_event_stream(text: &str) -> bool {
    let Some(first_line) = text.lines().map(str::trim).find(|l| !l.is_empty()) else {
        return false;
    };

    if !first_line.starts_with('{') {
        return false;
    }

    let Ok(value) = serde_json::from_str::<serde_json::Value>(first_line) else {
        return false;
    };

    let Some(event_type) = value.get("type").and_then(|v| v.as_str()) else {
        return false;
    };

    matches!(
        event_type,
        "step_start"
            | "step_finish"
            | "tool_use"
            | "tool_result"
            | "assistant"
            | "system"
            | "message"
            | "text"
    )
}

/// Build a synthetic agent response from plain text when structured parsing fails.
///
/// Returns `None` for empty/whitespace-only text so callers can preserve the
/// original `InvalidResponse` behavior.
pub fn synthesize_response_from_text(text: &str) -> Option<AgentResponse> {
    // Returns true when `word` appears as a whole word in `text`
    // (not preceded or followed by an ASCII alphanumeric character).
    fn contains_word(text: &str, word: &str) -> bool {
        let mut start = 0;
        while let Some(pos) = text[start..].find(word) {
            let abs = start + pos;
            let before_ok = abs == 0 || !text.as_bytes()[abs - 1].is_ascii_alphanumeric();
            let after_ok = abs + word.len() >= text.len()
                || !text.as_bytes()[abs + word.len()].is_ascii_alphanumeric();
            if before_ok && after_ok {
                return true;
            }
            start = abs + 1;
            if start >= text.len() {
                break;
            }
        }
        false
    }

    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Detect CLI/arg-parser diagnostics and reject them. These indicate the agent
    // process failed to start properly (e.g. invalid flag), not that the agent
    // produced a free-form answer. Clap-based CLIs emit patterns like:
    //   "error: unexpected argument '--flag' found"
    //   "Usage: command [OPTIONS]"
    //   "For more information, try '--help'"
    //   "tip: to pass '...' as a value, use '-- ...'"
    if is_cli_parser_diagnostic(trimmed) {
        return None;
    }

    // Detect NDJSON event streams (opencode/claude/codex stdout) and reject
    // them. Raw NDJSON masquerading as prose would otherwise leak into
    // notification summaries — the "completed" substring inside tool state
    // JSON triggers a false "done" classification and the first 500 bytes of
    // NDJSON become the user-visible summary.
    if looks_like_ndjson_event_stream(trimmed) {
        return None;
    }

    let lower = trimmed.to_lowercase();
    // Also accept plain-language confirmations of performed actions (issue/PR
    // creation, comments, etc.) as a successful "done" signal. This prevents
    // tasks from being retried when an agent reports success in free-form
    // text instead of returning structured JSON.

    // Negation phrases that override a "done/completed" match.
    let looks_negative = [
        "not done",
        "not yet done",
        "not complete",
        "not yet complete",
        "not completed",
        "not yet completed",
        "not committed",
        "incomplete",
        "undone",
    ]
    .iter()
    .any(|needle| lower.contains(needle));

    // Strong explicit completion phrases. These are high-confidence indicators
    // that the agent performed an action (filed issues, created commits, etc.).
    let explicit_done = [
        "nothing to",
        "nothing to do",
        "nothing to execute",
        "no positions",
        "no open positions",
        "no trades",
        "no trade",
        "no action needed",
        "no changes to commit",
        "completed",
        // Action/issue completion phrases
        "filed",
        "filed issue",
        "filed issues",
        "created issue",
        "created issues",
        "opened issue",
        "opened issues",
        "issue created",
        "issues created",
        "issues filed",
        "task created",
        "posted comment",
        "comment posted",
        "changes committed",
        "commit created",
        "fixed.",
        "has been fixed",
        "the fix has been applied",
        "the fix has already been applied",
        // Agent completion phrases (regression: #1362/#1363)
        "the fix is complete",
        "fix is working",
        "all tests pass",
        "tests pass.",
    ]
    .iter()
    .any(|needle| lower.contains(needle));

    // Conservative sentence-level heuristics for single-word signals like "done".
    // Accept "done" only when it appears in a short, self-contained sentence
    // without hedging language ("let me", "checking", "will", etc.). This
    // reduces false-positives from exploratory prose that mentions "done"
    // as part of a planning or investigatory sentence.
    let mut done_confident = false;
    for sentence in trimmed
        .split(['.', '!', '?', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let s_lower = sentence.to_lowercase();
        // short sentence heuristic
        let word_count = sentence.split_whitespace().count();
        let hedging = [
            "let",
            "let me",
            "check",
            "checking",
            "will",
            "i'll",
            "i will",
            "trying",
            "try",
            "investigat",
            "maybe",
            "could",
            "should",
            "might",
            "if ",
            "plan",
            "planning",
        ];
        let contains_hedge = hedging.iter().any(|h| s_lower.contains(h));

        if contains_word(&s_lower, "done") && word_count <= 8 && !contains_hedge {
            done_confident = true;
            break;
        }

        // Commit/concrete action heuristics: require explicit commit language
        // rather than opportunistic mentions. Examples: "I've committed the changes",
        // "changes committed", or "commit created".
        if s_lower.contains("committed") || s_lower.contains("commit created") {
            // Accept only if sentence also suggests the changes were actually written
            if s_lower.contains("changes")
                || s_lower.contains("i've")
                || s_lower.contains("i have")
                || s_lower.contains("the commit")
            {
                done_confident = true;
                break;
            }
        }

        // Past-tense action verb at sentence start indicates a completed action
        // (e.g. "Added detect_rate_limit patterns", "Fixed the nil-pointer bug").
        // Accept only for short sentences without hedging language to avoid matching
        // prose like "I added a note that this might fail later".
        let past_tense_actions = [
            "added ",
            "fixed ",
            "updated ",
            "implemented ",
            "created ",
            "removed ",
            "changed ",
            "deleted ",
            "applied ",
            "resolved ",
            "patched ",
            "refactored ",
            "merged ",
            "pushed ",
            "deployed ",
        ];
        if !contains_hedge
            && word_count <= 12
            && past_tense_actions.iter().any(|v| s_lower.starts_with(v))
        {
            done_confident = true;
            break;
        }
    }

    let looks_done = !looks_negative && (explicit_done || done_confident);

    let looks_error = [
        "error:",
        "failed to",
        "cannot",
        "permission denied",
        "unable to",
    ]
    .iter()
    .any(|needle| lower.contains(needle));

    let status = if looks_error {
        "needs_review"
    } else if looks_done {
        "done"
    } else if looks_negative {
        // Agent explicitly stated the task is not complete — surface for human review.
        "needs_review"
    } else {
        // Text is ambiguous: no error signal, no completion signal, no explicit negation.
        // This happens when agents emit exploratory or startup text (e.g. "Let's go!",
        // "I need to find where...") and exit without producing structured JSON.
        // Returning None causes callers to treat this as an invalid response and reroute
        // the task rather than falsely advancing it to needs_review / in_review.
        return None;
    };

    let summary_end = truncate_at_char_boundary(trimmed, 500);
    let summary = trimmed[..summary_end].to_string();
    let error =
        (status == "needs_review").then(|| "agent returned plain text instead of JSON".to_string());

    // Extract structured fields from free-form text.
    let files = extract_file_paths(trimmed);
    let (accomplished, remaining) = extract_bullet_sections(trimmed);

    Some(AgentResponse {
        status: status.to_string(),
        summary,
        accomplished,
        remaining,
        files,
        error,
        input_tokens: None,
        output_tokens: None,
        learnings: vec![],
        delegations: vec![],
        screenshots: vec![],
        screenshot_note: None,
    })
}

/// Quote a string for safe insertion into a POSIX shell command.
pub(crate) fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Find the largest byte index <= `max_bytes` that lies on a UTF-8 char
/// boundary.  Used for safe string truncation in error messages.
pub fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> usize {
    if max_bytes >= s.len() {
        return s.len();
    }
    // Walk backwards from max_bytes until we hit a char boundary
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// One step of an agent's work, as `orch stream` shows it
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    Started {
        model: String,
    },
    Text(String),
    Thinking(String),
    /// `id` links the call to its result, empty when the agent has none
    ToolCall {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// `name` is empty when only `id` names the call
    ToolResult {
        id: String,
        name: String,
        output: String,
        is_error: bool,
    },
    Done(String),
    Error(String),
}

/// String at a JSON pointer, empty when missing
pub(crate) fn json_str<'a>(v: &'a serde_json::Value, pointer: &str) -> &'a str {
    v.pointer(pointer)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
}

/// Error text from any of the shapes agents use: `message`, `error`, `error.message`, `error.data.message`
pub(crate) fn json_error_message(v: &serde_json::Value) -> String {
    [
        "/message",
        "/error",
        "/error/message",
        "/error/data/message",
    ]
    .iter()
    .find_map(|p| v.pointer(p).and_then(serde_json::Value::as_str))
    .unwrap_or("unknown error")
    .to_string()
}

/// Router stdout shape. Each runner reports its own
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouterTranscript {
    /// Claude Messages NDJSON (`type:result`)
    Messages,
    /// OpenCode NDJSON (`type:text`, `step_*`)
    OpenCode,
    /// Stdout is used as-is
    Raw,
}

/// Per-agent runner trait.
///
/// Each agent implements this to handle its specific CLI invocation,
/// output parsing, and error classification.
pub trait AgentRunner: Send + Sync {
    /// Agent name (e.g., "claude", "codex", "opencode"). Used in tests.
    #[cfg(test)]
    fn name(&self) -> &str;

    /// Build the CLI command string for the legacy tmux runner.
    ///
    /// The returned string is embedded into a shell command that captures
    /// output in the tmux session. The `permissions` struct carries unified
    /// rules that each agent translates to its own CLI flags.
    fn build_command(
        &self,
        model: Option<&str>,
        timeout_cmd: &str,
        sys_file: &str,
        msg_file: &str,
        permissions: &PermissionRules,
    ) -> String;

    /// Build a routed invocation with an optional native reasoning effort.
    fn build_command_with_effort(
        &self,
        model: Option<&str>,
        _effort: Option<&str>,
        timeout_cmd: &str,
        sys_file: &str,
        msg_file: &str,
        permissions: &PermissionRules,
    ) -> String {
        self.build_command(model, timeout_cmd, sys_file, msg_file, permissions)
    }

    /// Parse raw stdout into a ParsedResponse.
    ///
    /// Returns `Ok(ParsedResponse)` on success, `Err(AgentError)` if the
    /// output indicates an error or cannot be parsed.
    fn parse_response(&self, raw: &str) -> Result<ParsedResponse, AgentError>;

    /// Extract the raw inner text from an agent output envelope, without
    /// attempting to parse it as an `AgentResponse`.
    ///
    /// Use this in the review pipeline so that a `ReviewResponse` JSON can be
    /// recovered even when the text doesn't match the task `AgentResponse` schema.
    ///
    /// - For Claude/Kimi/MiniMax: strips the `--output-format stream-json` NDJSON
    ///   wrapper and returns the `"result"` field of the final `"type":"result"` line.
    /// - For Codex: returns the text of the last `agent_message` item.
    /// - For OpenCode: concatenates all `text` events.
    /// - Default: returns `raw` unchanged (safe fallback for unknown agents).
    ///
    /// Returns `Err(AgentError)` only for terminal errors (rate limit, auth, etc.)
    /// that should abort the review pipeline entirely.
    fn extract_text(&self, raw: &str) -> Result<String, AgentError> {
        Ok(raw.to_string())
    }

    /// Transcript steps for one parsed line of this agent's NDJSON output
    fn stream_events(&self, event: &serde_json::Value) -> Vec<StreamEvent>;

    /// Classify an error from exit code + stdout + stderr into an AgentError.
    ///
    /// Called when the agent process exits with a non-zero code, or when
    /// parse_response fails to find a valid result.
    fn classify_error(&self, exit_code: i32, stdout: &str, stderr: &str) -> AgentError;

    /// Classify error with elapsed session time (for accurate timeout reporting).
    fn classify_error_with_elapsed(
        &self,
        exit_code: i32,
        stdout: &str,
        stderr: &str,
        elapsed_secs: Option<u64>,
    ) -> AgentError {
        let mut err = self.classify_error(exit_code, stdout, stderr);
        if let AgentError::Timeout { elapsed } = &mut err
            && let Some(actual_elapsed) = elapsed_secs
        {
            *elapsed = Duration::from_secs(actual_elapsed);
        }
        err
    }

    /// Models available for this agent (for intra-agent failover).
    fn available_models(&self) -> Vec<String> {
        vec![]
    }

    /// Free/fallback models for last-resort failover.
    fn free_models(&self) -> Vec<String> {
        vec![]
    }

    /// Router stdout shape. The router matches on this instead of agent names
    fn router_transcript(&self) -> RouterTranscript {
        RouterTranscript::Raw
    }

    /// Build a minimal CLI command for LLM-based routing.
    ///
    /// The command should run the agent with `prompt` as the sole task and
    /// return structured (JSON or NDJSON) output on stdout. Used by the router
    /// to classify tasks without launching a full agent session.
    fn router_command(&self, prompt: &str, model: Option<&str>) -> anyhow::Result<Command>;
}

/// Agents whose CLI takes `--session-id` for a new session and `--resume` to continue it
pub fn supports_session(agent: &str) -> bool {
    matches!(agent, "claude" | "kimi" | "minimax" | "grok")
}

/// Get the appropriate AgentRunner implementation for an agent name.
/// Agents that are known to be claude-compatible (same NDJSON output format and flags)
const CLAUDE_COMPATIBLE_AGENTS: &[&str] = &["claude", "kimi", "minimax", "olm", "glm"];

/// Whether the agent runs Claude Code's CLI, and so honours a tool allowlist
pub fn is_claude_compatible(agent_name: &str) -> bool {
    CLAUDE_COMPATIBLE_AGENTS.contains(&agent_name)
}

pub fn get_runner(agent_name: &str) -> Box<dyn AgentRunner> {
    match agent_name {
        "claude" => Box::new(claude::ClaudeRunner::new(agent_name)),
        "kimi" => Box::new(kimi::KimiClaudeRunner::new()),
        "minimax" => Box::new(minimax::MiniMaxClaudeRunner::new(agent_name)),
        "codex" => Box::new(codex::CodexRunner),
        "opencode" => Box::new(opencode::OpenCodeRunner::new()),
        "grok" => Box::new(grok::GrokRunner),
        // Unknown agents fall back to Claude-compatible parsing
        other => {
            // Only warn if the agent is not known to be claude-compatible
            if !CLAUDE_COMPATIBLE_AGENTS.contains(&other) {
                tracing::warn!(
                    agent = other,
                    "unknown agent, using claude-compatible runner"
                );
            }
            Box::new(claude::ClaudeRunner::new(other))
        }
    }
}

/// Parse an NDJSON stream into a list of JSON values.
///
/// Shared by all agents that emit NDJSON output (Codex, OpenCode, etc.).
/// Lines that are empty or unparseable are silently skipped with a debug log.
///
/// For fragmented streams where complete JSON objects span multiple lines,
/// we also attempt to concatenate non-empty lines and parse them as a stream
/// of JSON objects to handle edge cases where models emit partial NDJSON.
pub(crate) fn parse_ndjson(raw: &str) -> Vec<serde_json::Value> {
    // Fast path: try parsing each line individually (handles most cases)
    let events: Vec<serde_json::Value> = raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| match serde_json::from_str(line) {
            Ok(val) => Some(val),
            Err(e) => {
                tracing::debug!(line, error = %e, "skipping unparseable NDJSON line");
                None
            }
        })
        .collect();

    // If we got events, return them. Otherwise, try the accumulation fallback
    // for edge cases where models emit fragmented JSON (e.g., partial objects
    // that get split across lines, or streams with embedded newlines in strings).
    if !events.is_empty() {
        return events;
    }

    // Fallback: accumulate all non-empty, non-comment lines and try to extract
    // JSON objects by scanning for balanced braces. This handles edge cases like:
    // - Closing fence inside JSON strings: {"text": "```json\n{\"status\": \"done\"}\n```"}
    // - Fragmented NDJSON where incomplete objects span multiple lines
    // - Model outputs with embedded newlines in text fields
    let acc: String = raw
        .lines()
        .filter(|line| {
            let t = line.trim();
            !t.is_empty() && !t.starts_with("//") && !t.starts_with('#')
        })
        .collect::<Vec<_>>()
        .join(" ");

    if acc.is_empty() {
        return Vec::new();
    }

    // Fallback: try to parse the accumulated text directly as a JSON value.
    // This handles multi-line JSON objects that don't fit on single lines.
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(acc.trim()) {
        // Check if stripping ALL whitespace makes it a single JSON object with
        // the same content. If so, the input was plain multi-line JSON (not
        // fragmented NDJSON), and we should return empty to preserve original
        // behavior — callers then treat it as plain text.
        let stripped: String = acc.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        if let Ok(stripped_val) = serde_json::from_str::<serde_json::Value>(&stripped) {
            // Only return empty if the stripped version parses identically,
            // meaning whitespace was insignificant. If they differ, whitespace
            // matters and this is likely fragmented input.
            if stripped_val == val {
                return Vec::new();
            }
        }
        return vec![val];
    }

    // Direct parse failed — try to extract JSON objects by scanning for balanced
    // braces. This handles cases like embedded JSON within text:
    // `{"text": "```json\n{\"status\": \"done\"}\n```"}`
    extract_json_objects(&acc)
}

/// Extract complete JSON objects from a string by scanning for balanced braces.
///
/// This is a fallback for fragmented NDJSON streams where complete objects
/// don't appear on single lines. Returns all parseable objects found.
fn extract_json_objects(text: &str) -> Vec<serde_json::Value> {
    let mut results = Vec::new();
    let mut start: Option<usize> = None;
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape = false;

    for (idx, ch) in text.char_indices() {
        if let Some(start_idx) = start {
            if in_string {
                if escape {
                    escape = false;
                } else if ch == '\\' {
                    escape = true;
                } else if ch == '"' {
                    in_string = false;
                }
                continue;
            }

            match ch {
                '"' => in_string = true,
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        let candidate = &text[start_idx..=idx];
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(candidate) {
                            results.push(val);
                        }
                        start = None;
                    }
                }
                _ => {}
            }
            continue;
        }

        if ch == '{' {
            start = Some(idx);
            depth = 1;
            in_string = false;
            escape = false;
        }
    }

    results
}

/// Env vars that hold an agent's login, so a set one counts as signed in
pub(crate) fn login_env_vars(agent: &str) -> &'static [&'static str] {
    match agent {
        "claude" => &["ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN"],
        "codex" => &["OPENAI_API_KEY", "CODEX_API_KEY", "CODEX_ACCESS_TOKEN"],
        "grok" => &["XAI_API_KEY"],
        "kimi" => &["KIMI_API_KEY"],
        "minimax" => &["MINIMAX_API_KEY"],
        "glm" => &["GLM_API_KEY"],
        _ => &[],
    }
}

/// Tool patterns no agent may run: a worktree shares `.git/config` with the operator's checkout,
/// so a rewritten remote or URL rule would change the main repository
pub(crate) fn remote_change_tools() -> Vec<String> {
    [
        "Bash(git remote set-url*)",
        "Bash(git remote add*)",
        "Bash(git remote remove*)",
        "Bash(git remote rm*)",
        "Bash(git remote rename*)",
        "Bash(git config*remote.*)",
        "Bash(git config*url.*)",
    ]
    .map(String::from)
    .to_vec()
}

#[cfg(test)]
mod tests;
