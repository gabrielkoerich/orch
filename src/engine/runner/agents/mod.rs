//! Per-agent runner trait, error types, and agent registry.
//!
//! Each agent (Claude, Codex, OpenCode) has a different CLI invocation,
//! output format, and error pattern. This module defines a common trait
//! so the runner can delegate parsing and error classification to the
//! correct implementation.

pub mod claude;
pub mod codex;
pub mod kimi;
pub mod minimax;
pub mod opencode;

use crate::parser::AgentResponse;
use std::path::PathBuf;
use std::time::Duration;

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
    #[allow(dead_code)]
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

        if let Ok(mode) = crate::config::get("workflow.permissions.mode") {
            rules.autonomous = mode != "supervised";
        }

        if let Ok(sandbox) = crate::config::get("workflow.permissions.sandbox") {
            rules.sandbox = match sandbox.as_str() {
                "full-access" | "danger-full-access" => SandboxLevel::FullAccess,
                "none" => SandboxLevel::None,
                _ => SandboxLevel::WorkspaceWrite,
            };
        }

        if let Ok(tools) = crate::config::get("workflow.disallowed_tools") {
            let parsed: Vec<String> = tools
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !parsed.is_empty() {
                rules.disallowed_tools = parsed;
            }
        }

        if let Ok(tools) = crate::config::get_list("workflow.allowed_tools") {
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
#[allow(dead_code)] // fields read by tests and future phases
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
        "claude" | "kimi" | "minimax" => claude::find_claude_result(ndjson),
        "opencode" => opencode::find_opencode_result(ndjson),
        "codex" => codex::find_codex_result(ndjson),
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
    /// Agent timed out — retry once, then switch agent.
    Timeout { elapsed: Duration },
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

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RateLimit { message, .. } => write!(f, "rate limit: {message}"),
            Self::Auth { message } => write!(f, "auth error: {message}"),
            Self::ModelUnavailable { message, model } => {
                write!(f, "model unavailable ({model}): {message}")
            }
            Self::ContextOverflow { message, .. } => write!(f, "context overflow: {message}"),
            Self::Timeout { elapsed } => write!(f, "timeout after {}s", elapsed.as_secs()),
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

impl std::error::Error for AgentError {}

/// Return the variant name of an `AgentError` as a static string,
/// for structured logging and `result.json` output.
pub fn error_class_name(err: &AgentError) -> &'static str {
    match err {
        AgentError::RateLimit { .. } => "RateLimit",
        AgentError::Auth { .. } => "Auth",
        AgentError::ModelUnavailable { .. } => "ModelUnavailable",
        AgentError::ContextOverflow { .. } => "ContextOverflow",
        AgentError::Timeout { .. } => "Timeout",
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
    let mut found: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
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
            if still_has_ext {
                without
            } else {
                token
            }
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

/// Build a synthetic agent response from plain text when structured parsing fails.
///
/// Returns `None` for empty/whitespace-only text so callers can preserve the
/// original `InvalidResponse` behavior.
pub fn synthesize_response_from_text(text: &str) -> Option<AgentResponse> {
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

    let lower = trimmed.to_lowercase();
    // Also accept plain-language confirmations of performed actions (issue/PR
    // creation, comments, etc.) as a successful "done" signal. This prevents
    // tasks from being retried when an agent reports success in free-form
    // text instead of returning structured JSON.
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
        .map(|s| s.trim())
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
    #[allow(dead_code)] // used by per-agent unit tests; production uses find_agent_result
    fn extract_text(&self, raw: &str) -> Result<String, AgentError> {
        Ok(raw.to_string())
    }

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
        if let AgentError::Timeout { elapsed } = &mut err {
            if let Some(actual_elapsed) = elapsed_secs {
                *elapsed = Duration::from_secs(actual_elapsed);
            }
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

    /// Build a minimal CLI command for LLM-based routing.
    ///
    /// The command should run the agent with `prompt` as the sole task and
    /// return structured (JSON or NDJSON) output on stdout. Used by the router
    /// to classify tasks without launching a full agent session.
    fn router_command(
        &self,
        prompt: &str,
        model: Option<&str>,
    ) -> anyhow::Result<tokio::process::Command>;
}

/// Get the appropriate AgentRunner implementation for an agent name.
pub fn get_runner(agent_name: &str) -> Box<dyn AgentRunner> {
    // Agents that are known to be claude-compatible (same NDJSON output format)
    const CLAUDE_COMPATIBLE_AGENTS: &[&str] = &["claude", "kimi", "minimax", "olm", "glm"];

    match agent_name {
        "claude" => Box::new(claude::ClaudeRunner::new(agent_name)),
        "kimi" => Box::new(kimi::KimiClaudeRunner::new()),
        "minimax" => Box::new(minimax::MiniMaxClaudeRunner::new(agent_name)),
        "codex" => Box::new(codex::CodexRunner),
        "opencode" => Box::new(opencode::OpenCodeRunner::new()),
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

/// Generate a complete, delegating `AgentRunner` impl for a wrapper struct.
///
/// Use this for runner types that delegate every `AgentRunner` method to an
/// inner field (e.g., `KimiClaudeRunner → MiniMaxClaudeRunner`).
///
/// ```ignore
/// delegate_agent_runner!(KimiClaudeRunner, inner);
/// ```
macro_rules! delegate_agent_runner {
    ($runner:ty, $inner:ident) => {
        impl AgentRunner for $runner {
            #[cfg(test)]
            fn name(&self) -> &str {
                self.$inner.name()
            }

            fn build_command(
                &self,
                model: Option<&str>,
                timeout_cmd: &str,
                sys_file: &str,
                msg_file: &str,
                permissions: &PermissionRules,
            ) -> String {
                self.$inner
                    .build_command(model, timeout_cmd, sys_file, msg_file, permissions)
            }

            fn parse_response(&self, raw: &str) -> Result<ParsedResponse, AgentError> {
                self.$inner.parse_response(raw)
            }

            fn extract_text(&self, raw: &str) -> Result<String, AgentError> {
                self.$inner.extract_text(raw)
            }

            fn classify_error(&self, exit_code: i32, stdout: &str, stderr: &str) -> AgentError {
                self.$inner.classify_error(exit_code, stdout, stderr)
            }

            fn router_command(
                &self,
                prompt: &str,
                model: Option<&str>,
            ) -> anyhow::Result<tokio::process::Command> {
                self.$inner.router_command(prompt, model)
            }
        }
    };
}

pub(crate) use delegate_agent_runner;

/// Shared error pattern detection utilities used by multiple agent runners.
pub(crate) mod patterns {
    use super::AgentError;
    use std::time::Duration;

    /// Check for rate limit / usage limit patterns in text.
    pub fn detect_rate_limit(text: &str) -> Option<AgentError> {
        let lower = text.to_lowercase();
        let patterns = [
            "rate limit",
            "rate_limit",
            "ratelimit",
            "rate increased too quickly", // Alibaba / Qwen via OpenCode
            "too many requests",
            "usage limit",
            "quota exceeded",
            "overloaded",
            "capacity",
            "throttled",
            "insufficient_quota",
            "tokens_exceeded",
            "you've hit your usage limit",
        ];
        // Find the earliest match position so we can extract context around the
        // actual error message rather than the tail (which may be unrelated JSON).
        // Map the byte offset from `lower` back to `text` via char-count to avoid
        // using a lowercased byte index on the original string (Unicode case-folding
        // can change byte lengths, e.g. İ→i̇).
        let match_pos = patterns
            .iter()
            .filter_map(|p| lower.find(p))
            .min()
            .map(|lower_pos| {
                let char_idx = lower[..lower_pos].chars().count();
                text.char_indices()
                    .nth(char_idx)
                    .map_or(text.len(), |(i, _)| i)
            });
        let has_429 = lower.contains("429");
        // HTTP 529 is used by Cloudflare for rate limiting. Only match in HTTP status
        // contexts (e.g. "HTTP 529", "status: 529") to avoid false positives from bare
        // numbers like line numbers, port numbers, or file sizes.
        let has_529 = lower.contains("http 529")
            || lower.contains("529 service")
            || (lower.find(": 529").is_some_and(|pos| {
                !lower[pos + 5..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_digit())
            }));
        if match_pos.is_some() || has_429 || has_529 {
            let message = if let Some(pos) = match_pos {
                extract_context_around(text, pos, 300)
            } else {
                safe_tail(text, 300).to_string()
            };
            return Some(AgentError::RateLimit { message });
        }
        None
    }

    /// Extract up to `window` bytes of context centred around `byte_pos`.
    ///
    /// `byte_pos` must be a valid byte offset in `text` (not derived from a
    /// transformed copy).  Both ends are snapped to char boundaries to avoid
    /// splitting multi-byte codepoints.
    fn extract_context_around(text: &str, byte_pos: usize, window: usize) -> String {
        let half = window / 2;
        let start = byte_pos.saturating_sub(half);
        let end = (byte_pos + window).min(text.len());
        // Align both ends to char boundaries.
        let mut s = start;
        while s < text.len() && !text.is_char_boundary(s) {
            s += 1;
        }
        let mut e = end;
        while e < text.len() && !text.is_char_boundary(e) {
            e += 1;
        }
        text[s..e].to_string()
    }

    /// Returns true if `lower` contains the HTTP status `code` (e.g. "401") as a
    /// standalone number — not as part of a larger digit sequence like `4010292`.
    ///
    /// Matches:
    ///   - `"http 401"` / `"http/1.1 401"`
    ///   - `"401 unauthorized"` / `"401\n"` / `"401"` at end-of-string
    ///   - `": 401"` when not immediately followed by another digit
    pub fn contains_http_status(lower: &str, code: &str) -> bool {
        // Fast prefix checks that are inherently unambiguous.
        if lower.contains(&format!("http {code}")) || lower.contains(&format!("{code} ")) {
            return true;
        }
        // ": NNN" — only accept when the next character is not a digit.
        let needle = format!(": {code}");
        let mut start = 0;
        while let Some(rel) = lower[start..].find(needle.as_str()) {
            let after = start + rel + needle.len();
            match lower[after..].chars().next() {
                Some(c) if c.is_ascii_digit() => {
                    // Part of a longer number (e.g. ": 4010292") — skip.
                    start = after;
                }
                _ => return true,
            }
        }
        false
    }

    /// Check for auth / billing error patterns in text.
    ///
    /// Returns the line containing the match (not the tail) so the stored
    /// error message is the actual error string rather than trailing output
    /// (e.g. NDJSON session metadata that agents emit at exit).
    pub fn detect_auth_error(text: &str) -> Option<AgentError> {
        let lower = text.to_lowercase();
        let patterns = [
            "unauthorized",
            "authentication required",
            "invalid api",
            "invalid key",
            "invalid token",
            "auth fail",
            "no api key",
            "no token",
            "expired key",
            "expired token",
            "expired plan",
            // Use specific billing-related phrases instead of the bare word
            // "billing" which produced false positives when agents mentioned
            // billing in non-error contexts (e.g. reviews discussing billing).
            "billing plan",
            "billing error",
            "billing suspended",
            "billing expired",
            "billing limit",
            "billing failed",
            "billing cycle exhausted",
            "insufficient credit",
            "credit balance too low",
            "payment required",
        ];
        // Use standalone check for HTTP status codes (same rationale as
        // detect_rate_limit: avoid matching port numbers, line numbers, etc.).
        let http_401 = contains_http_status(&lower, "401");
        let http_403 = contains_http_status(&lower, "403");
        let http_407 = contains_http_status(&lower, "407");
        let proxy_auth_required = lower.contains("proxy authentication required");
        if !patterns.iter().any(|p| lower.contains(p))
            && !http_401
            && !http_403
            && !http_407
            && !proxy_auth_required
        {
            return None;
        }
        // Find the first auth error match position to extract its line.
        let first_match_pos =
            patterns
                .iter()
                .filter_map(|p| lower.find(p))
                .min()
                .map(|lower_pos| {
                    let char_idx = lower[..lower_pos].chars().count();
                    text.char_indices()
                        .nth(char_idx)
                        .map_or(text.len(), |(i, _)| i)
                });
        // Also check HTTP status codes for standalone matches (not caught by
        // pattern strings like "401" is too short to add as a pattern).
        let http_match_pos = if http_401 {
            lower.find("401 ").or_else(|| lower.find(": 401"))
        } else {
            None
        };
        let http_pos = http_match_pos.and_then(|rel| {
            let char_idx = lower[..rel].chars().count();
            text.char_indices().nth(char_idx).map(|(i, _)| i)
        });
        let match_pos = [first_match_pos, http_pos].into_iter().flatten().min();
        let message = if let Some(pos) = match_pos {
            find_line_containing(text, pos)
        } else {
            // Pattern matched but couldn't find position (e.g. 401 standalone).
            // Fall back to scanning last 50 lines for the error line.
            find_auth_line_scan(text).unwrap_or_else(|| safe_tail(text, 300).to_string())
        };
        Some(AgentError::Auth { message })
    }

    /// Returns the full line containing `byte_pos` in `text`.
    fn find_line_containing(text: &str, byte_pos: usize) -> String {
        let start = text[..byte_pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let end = text[byte_pos..]
            .find('\n')
            .map(|i| byte_pos + i)
            .unwrap_or(text.len());
        text[start..end].trim().to_string()
    }

    /// Scans the last 50 lines of `text` for a line containing an auth error
    /// pattern (401, unauthorized, invalid api, etc.). Used as fallback when
    /// match position is unavailable.
    fn find_auth_line_scan(text: &str) -> Option<String> {
        for line in text.lines().rev().take(50) {
            let line_lower = line.to_lowercase();
            if line_lower.contains("401")
                || line_lower.contains("403")
                || line_lower.contains("unauthorized")
                || line_lower.contains("authentication required")
                || line_lower.contains("invalid api")
                || line_lower.contains("invalid key")
                || line_lower.contains("invalid token")
                || line_lower.contains("auth fail")
                || line_lower.contains("no api key")
                || line_lower.contains("no token")
                || line_lower.contains("expired key")
                || line_lower.contains("expired token")
                || line_lower.contains("expired plan")
                || line_lower.contains("billing plan")
                || line_lower.contains("billing error")
                || line_lower.contains("billing suspended")
                || line_lower.contains("billing expired")
                || line_lower.contains("billing limit")
                || line_lower.contains("billing failed")
                || line_lower.contains("billing cycle exhausted")
                || line_lower.contains("insufficient credit")
                || line_lower.contains("credit balance too low")
                || line_lower.contains("payment required")
                || line_lower.contains("proxy authentication required")
            {
                return Some(line.trim().to_string());
            }
        }
        None
    }

    /// Check for transient network connectivity errors.
    pub fn detect_network_error(text: &str) -> Option<AgentError> {
        let lower = text.to_lowercase();
        let patterns = [
            "connectionrefused",
            "connection refused",
            "unable to connect",
            "econnrefused",
            "network unreachable",
            // Socket / HTTP transport errors (Node fetch / undici / http module)
            "socket connection was closed",
            "socket hang up",
            "econnreset",
            "connection reset",
            "etimedout",
            "connect etimedout",
            "aborterror",
            "the operation was aborted",
            "network request failed",
            "fetch failed",
        ];
        // Map the byte offset from `lower` back to `text` via char-count (same
        // rationale as detect_rate_limit: Unicode case-folding can change byte lengths).
        let match_pos = patterns
            .iter()
            .filter_map(|p| lower.find(p))
            .min()
            .map(|lower_pos| {
                let char_idx = lower[..lower_pos].chars().count();
                text.char_indices()
                    .nth(char_idx)
                    .map_or(text.len(), |(i, _)| i)
            });
        if let Some(pos) = match_pos {
            return Some(AgentError::NetworkError {
                message: extract_context_around(text, pos, 300),
            });
        }
        None
    }

    /// Check for missing worktree / working directory setup failures.
    pub fn detect_worktree_missing(text: &str) -> Option<AgentError> {
        let lower = text.to_lowercase();
        let patterns = [
            "worktree directory does not exist",
            "failed to create opencode config directory",
            "failed to write opencode config",
        ];
        if patterns.iter().any(|p| lower.contains(p)) {
            return Some(AgentError::AgentFailed {
                message: safe_tail(text, 300).to_string(),
            });
        }
        None
    }

    /// Check for context overflow patterns.
    pub fn detect_context_overflow(text: &str) -> Option<AgentError> {
        let lower = text.to_lowercase();
        let patterns = [
            "context_length_exceeded",
            "context length exceeded",
            "maximum context length",
            "too many tokens",
            "token limit",
            "context overflow",
        ];
        if patterns.iter().any(|p| lower.contains(p)) {
            return Some(AgentError::ContextOverflow {
                message: safe_tail(text, 300).to_string(),
            });
        }
        None
    }

    /// Check for missing tooling patterns. Returns the tool name.
    pub fn detect_missing_tool(text: &str) -> Option<AgentError> {
        let known_tools = [
            "bun",
            "node",
            "npm",
            "pnpm",
            "yarn",
            "deno",
            "tsc",
            "eslint",
            "prettier",
            "jest",
            "vitest",
            "cargo",
            "rustc",
            "go",
            "python",
            "python3",
            "pip",
            "pip3",
            "uv",
            "poetry",
            "pytest",
            "ruff",
            "black",
            "mypy",
            "make",
            "cmake",
            "ninja",
            "just",
            "bats",
            "docker",
            "docker-compose",
            "podman",
            "kubectl",
            "helm",
            "terraform",
            "anchor",
            "avm",
            "solana",
            "solana-test-validator",
        ];

        let lower = text.to_lowercase();
        for tool in &known_tools {
            if lower.contains(&format!("{tool}: command not found"))
                || lower.contains(&format!("command not found: {tool}"))
                || lower.contains(&format!("{tool}: not found"))
                || lower.contains(&format!("env: {tool}: no such file"))
                || lower.contains(&format!("spawn {tool} enoent"))
            {
                return Some(AgentError::MissingTool {
                    tool: tool.to_string(),
                });
            }
        }
        None
    }

    /// Check for permission/sandbox denied patterns.
    pub fn detect_permission_denied(text: &str) -> Option<AgentError> {
        let lower = text.to_lowercase();
        let patterns = [
            "permission denied",
            "rejected permission",
            "permissionerror",
            "operation not permitted",
            "sandbox violation",
            "access denied",
            "eacces",
            "eperm",
            "read-only file system",
            "not writable",
            "disallowed tool",
        ];
        if patterns.iter().any(|p| lower.contains(p)) {
            return Some(AgentError::PermissionDenied {
                message: safe_tail(text, 300).to_string(),
            });
        }
        None
    }

    /// Check for interactive input prompts (1Password, SSH passphrase, etc.).
    pub fn detect_waiting_for_input(text: &str) -> Option<AgentError> {
        let lower = text.to_lowercase();
        let patterns = [
            "enter passphrase",
            "1password",
            "op signin",
            "ssh passphrase",
            "password:",
        ];
        let auth_required_with_interactive_context = lower.contains("authentication required")
            && (lower.contains("ssh")
                || lower.contains("passphrase")
                || lower.contains("1password")
                || lower.contains("op signin"));
        if patterns.iter().any(|p| lower.contains(p)) || auth_required_with_interactive_context {
            return Some(AgentError::WaitingForInput {
                message: safe_tail(text, 300).to_string(),
            });
        }
        None
    }

    /// Check for the Claude 400 "thinking blocks cannot be modified" API error.
    ///
    /// Occurs when Claude CLI reconstructs a multi-turn tool-use request and the
    /// Anthropic API rejects it because a thinking block was altered between turns.
    /// Classified separately to avoid penalising the model with a failure count.
    pub fn detect_thinking_block_conflict(text: &str) -> Option<AgentError> {
        // Match the exact phrasing the Anthropic API emits (backtick-quoted names,
        // case-sensitive — the API always uses this exact string).
        if text.contains(
            "thinking` or `redacted_thinking` blocks in the latest assistant message cannot be modified",
        ) {
            let message = text
                .lines()
                .find(|l| l.contains("cannot be modified"))
                .map(|l| l.trim().to_string())
                .unwrap_or_else(|| safe_tail(text, 300).to_string());
            return Some(AgentError::ThinkingBlockConflict { message });
        }
        None
    }

    /// Check for a stale/expired Claude session ID.
    ///
    /// Claude returns `"No conversation found with session ID: <uuid>"` when
    /// the session UUID stored in orch's KV has expired server-side. The caller
    /// should reset the stored UUID and retry with a fresh `--session-id`.
    pub fn detect_stale_session(text: &str) -> Option<AgentError> {
        // Case-insensitive match; extract the UUID if present.
        // We search the lowercased copy for the pattern, then find the
        // corresponding byte offset in the *original* text via char_indices so
        // we never use a byte index from `lower` (whose byte length may differ
        // from `text` after Unicode case-folding, e.g. ß→ss, İ→i̇).
        let needle = "no conversation found with session id";
        let lower = text.to_lowercase();
        if let Some(lower_pos) = lower.find(needle) {
            // Map the byte offset in `lower` back to a char index, then find
            // the same char index in `text`.  Because to_lowercase() maps each
            // char to one or more chars, the char count up to the match may
            // differ between `lower` and `text`, so we count chars in `lower`.
            let char_idx = lower[..lower_pos].chars().count();
            let pos = text
                .char_indices()
                .nth(char_idx)
                .map(|(i, _)| i)
                .unwrap_or(text.len());
            // Try to extract the UUID that follows the colon.
            let after = &text[pos..];
            let session_id = after
                .split_once(':')
                .map(|(_, rest)| rest.trim())
                // Take only the UUID portion (up to whitespace or end)
                .map(|s| s.split_whitespace().next().unwrap_or("").to_string())
                .unwrap_or_default();
            return Some(AgentError::StaleSession { session_id });
        }
        None
    }

    /// Exit code returned by `timeout(1)` when the child is killed.
    const TIMEOUT_EXIT_CODE: i32 = 124;

    /// Default assumed timeout duration when we only have the exit code.
    const DEFAULT_TIMEOUT_SECS: u64 = 1800;

    /// How many bytes from the end of combined stdout+stderr we scan for
    /// CLI-style error patterns that should come from the process tail.
    ///
    /// Rationale: real CLI error messages (rate limits, auth failures, network
    /// failures) almost always appear at the end of the process output.
    /// Scanning only the tail avoids false-positives from agent work product
    /// (code, diffs, commit messages) which may mention "quota",
    /// "unauthorized", HTTP status codes, etc. 3000 bytes is a conservative
    /// window large enough to capture multi-line error dumps but small enough
    /// to exclude most long-form agent outputs.
    const RATE_LIMIT_SCAN_TAIL_BYTES: usize = 3000;

    /// Run all pattern detectors against combined stdout+stderr.
    /// Returns the first matching AgentError, or a generic Unknown.
    pub fn classify_from_text(exit_code: i32, text: &str) -> AgentError {
        if exit_code == TIMEOUT_EXIT_CODE {
            return AgentError::Timeout {
                elapsed: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            };
        }

        // Exit 0 with empty output is a silent failure (common with GitHub Copilot
        // models in opencode). Return Unknown with a deterministic message so the
        // caller (fallback.rs) detects it and applies model cooldown + free-model
        // retry instead of treating it as a generic success.
        if exit_code == 0 && text.trim().is_empty() {
            return AgentError::Unknown {
                exit_code,
                message: "empty-output-exit0".to_string(),
            };
        }

        if let Some(e) = detect_missing_tool(text) {
            return e;
        }
        if let Some(e) = detect_worktree_missing(text) {
            return e;
        }
        if let Some(e) = detect_thinking_block_conflict(text) {
            return e;
        }
        // Only scan the tail of the combined output for CLI-style error
        // patterns. The full output may contain agent work product (code,
        // diffs, commit messages) that incidentally mentions rate limiting,
        // authorization, HTTP status codes, or context/auth errors. Real
        // process errors appear at the end of the output. Scan a bounded tail
        // (`RATE_LIMIT_SCAN_TAIL_BYTES`) to reduce false positives.
        let scan_tail = safe_tail(text, RATE_LIMIT_SCAN_TAIL_BYTES);
        if let Some(e) = detect_context_overflow(scan_tail) {
            return e;
        }
        if let Some(e) = detect_rate_limit(scan_tail) {
            return e;
        }
        if let Some(e) = detect_network_error(scan_tail) {
            return e;
        }
        if let Some(e) = detect_auth_error(scan_tail) {
            return e;
        }
        if let Some(e) = detect_waiting_for_input(scan_tail) {
            return e;
        }
        if let Some(e) = detect_permission_denied(scan_tail) {
            return e;
        }
        if let Some(e) = detect_stale_session(text) {
            return e;
        }

        AgentError::Unknown {
            exit_code,
            message: safe_tail(text, 300).to_string(),
        }
    }

    /// Safely extract the last `max_bytes` of a string, respecting UTF-8 boundaries.
    pub fn safe_tail(text: &str, max_bytes: usize) -> &str {
        if text.len() <= max_bytes {
            return text;
        }
        let start = text.len() - max_bytes;
        // Walk forward to find a char boundary
        let mut idx = start;
        while idx < text.len() && !text.is_char_boundary(idx) {
            idx += 1;
        }
        &text[idx..]
    }

    /// Scan NDJSON events for errors using agent-specific extraction and classification.
    ///
    /// `extract_message` is called for each event to extract an error message string.
    /// If it returns `Some`, the message is classified via `classify`. The most
    /// specific error is kept — any typed error (RateLimit, Auth, etc.) takes
    /// precedence over a generic `AgentFailed`.
    pub fn detect_ndjson_error(
        events: &[serde_json::Value],
        extract_message: impl Fn(&serde_json::Value) -> Option<String>,
        classify: impl Fn(&str) -> AgentError,
    ) -> Option<AgentError> {
        let mut best: Option<AgentError> = None;

        for event in events {
            if let Some(message) = extract_message(event) {
                let err = classify(&message);
                // Priority (highest wins): RateLimit / Auth / other > NetworkError > AgentFailed
                // A transient reconnect mid-stream must not obscure a subsequent billing/rate error.
                let upgrade = match &best {
                    None => true,
                    Some(AgentError::AgentFailed { .. }) => true,
                    Some(AgentError::NetworkError { .. }) => !matches!(
                        err,
                        AgentError::NetworkError { .. } | AgentError::AgentFailed { .. }
                    ),
                    Some(_) => false,
                };
                if upgrade {
                    best = Some(err);
                }
            }
        }

        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_runner_returns_correct_types() {
        assert_eq!(get_runner("claude").name(), "claude");
        assert_eq!(get_runner("kimi").name(), "kimi");
        assert_eq!(get_runner("minimax").name(), "minimax");
        assert_eq!(get_runner("codex").name(), "codex");
        assert_eq!(get_runner("opencode").name(), "opencode");
        // olm and glm are claude-compatible and should return correct name
        assert_eq!(get_runner("olm").name(), "olm");
        assert_eq!(get_runner("glm").name(), "glm");
        // Unknown falls back to claude-compatible
        assert_eq!(get_runner("unknown-agent").name(), "unknown-agent");
    }

    /// Regression test: known claude-compatible agents (including glm) must not
    /// emit "unknown agent" warnings when get_runner() is called.
    #[test]
    fn get_runner_no_warning_for_claude_compatible_agents() {
        use std::sync::{Arc, Mutex};
        use tracing::Level;
        use tracing_subscriber::util::SubscriberInitExt;
        use tracing_subscriber::EnvFilter;

        let output: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let output_clone = output.clone();

        struct CaptureWriter(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for CaptureWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::writer::MakeWriter<'a> for CaptureWriter {
            type Writer = CaptureWriter;
            fn make_writer(&'a self) -> Self::Writer {
                CaptureWriter(self.0.clone())
            }
        }

        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_default_env().add_directive(Level::WARN.into()))
            .with_writer(CaptureWriter(output_clone))
            .with_ansi(false)
            .finish()
            .init();

        for agent in &["claude", "kimi", "minimax", "olm", "glm"] {
            get_runner(agent);
        }

        let captured = {
            let guard = output.lock().unwrap();
            String::from_utf8_lossy(&guard).to_string()
        };
        assert!(
            !captured.contains("unknown agent"),
            "known claude-compatible agents must not emit 'unknown agent' warnings; got: {}",
            captured
        );
    }

    #[test]
    fn agent_error_display() {
        let e = AgentError::RateLimit {
            message: "429 Too Many Requests".to_string(),
        };
        assert!(e.to_string().contains("rate limit"));

        let e = AgentError::Timeout {
            elapsed: Duration::from_secs(1800),
        };
        assert!(e.to_string().contains("1800"));
    }

    #[test]
    fn pattern_detect_rate_limit() {
        assert!(patterns::detect_rate_limit("Error: rate limit exceeded").is_some());
        assert!(patterns::detect_rate_limit("HTTP 429 Too Many Requests").is_some());
        assert!(patterns::detect_rate_limit("You've hit your usage limit").is_some());
        assert!(patterns::detect_rate_limit(
            "Upstream error from Alibaba: Request rate increased too quickly. \
             To ensure system stability, please adjust your client logic to scale requests more smoothly over time."
        ).is_some());
        assert!(patterns::detect_rate_limit("all good").is_none());
    }

    #[test]
    fn detect_rate_limit_message_contains_context_not_tail() {
        // Simulate a long claude NDJSON output where the rate limit message is in
        // the middle and usage stats JSON is at the tail.
        let padding = "x".repeat(5000);
        let text = format!(
            "{padding}you've reached your usage limit for this billing cycle{padding}\
             {{\"web_fetch_requests\":0,\"service_tier\":\"standard\"}}"
        );
        let err = patterns::detect_rate_limit(&text).expect("should detect");
        let msg = err.to_string();
        assert!(
            msg.contains("usage limit"),
            "message should contain the actual error, got: {msg}"
        );
        assert!(
            !msg.contains("web_fetch_requests"),
            "message should not be the tail JSON, got: {msg}"
        );
    }

    #[test]
    fn pattern_detect_rate_limit_529_not_bare() {
        // Bare "529" in various contexts should NOT trigger rate limit detection.
        // These are false positive cases we want to avoid.
        assert!(
            patterns::detect_rate_limit("error on line 529").is_none(),
            "bare line number 529 should not match"
        );
        assert!(
            patterns::detect_rate_limit("file size 529 bytes").is_none(),
            "bare file size 529 should not match"
        );
        assert!(
            patterns::detect_rate_limit("port :5290 in config").is_none(),
            "port number containing 529 should not match"
        );
        assert!(
            patterns::detect_rate_limit("timeout after 5290ms").is_none(),
            "duration containing 529 should not match"
        );
        // But HTTP 529 status codes SHOULD match.
        assert!(
            patterns::detect_rate_limit("HTTP 529 Service Unavailable").is_some(),
            "HTTP 529 status should match"
        );
        assert!(
            patterns::detect_rate_limit("http 529").is_some(),
            "http 529 should match"
        );
        assert!(
            patterns::detect_rate_limit("529 service unavailable").is_some(),
            "529 service unavailable should match"
        );
        assert!(
            patterns::detect_rate_limit("status: 529").is_some(),
            "status: 529 should match"
        );
    }

    #[test]
    fn pattern_detect_auth() {
        assert!(patterns::detect_auth_error("401 Unauthorized").is_some());
        assert!(patterns::detect_auth_error("HTTP 401").is_some());
        assert!(patterns::detect_auth_error("error: 401").is_some());
        assert!(patterns::detect_auth_error("403 Forbidden").is_some());
        assert!(patterns::detect_auth_error("HTTP 403").is_some());
        assert!(patterns::detect_auth_error("HTTP 407 Authentication Required").is_some());
        assert!(patterns::detect_auth_error("407 Proxy Authentication Required").is_some());
        assert!(patterns::detect_auth_error("invalid api key").is_some());
        assert!(patterns::detect_auth_error("billing expired").is_some());
        assert!(patterns::detect_auth_error("task done").is_none());
        // Bare numbers in JSON must NOT trigger auth classification.
        assert!(patterns::detect_auth_error("duration_api_ms 4010292").is_none());
        assert!(patterns::detect_auth_error(
            "API Error: Unable to connect to API (ConnectionRefused) duration_api_ms 4010292"
        )
        .is_none());
        // JSON field with colon: "duration_api_ms": 4010292 — contains ": 401" as substring.
        assert!(patterns::detect_auth_error(r#""duration_api_ms": 4010292"#).is_none());
        assert!(patterns::detect_auth_error(
            r#"{"error":"ConnectionRefused","duration_api_ms": 4010292}"#
        )
        .is_none());
        // ": 401" at end-of-string or followed by non-digit must still match.
        assert!(patterns::detect_auth_error("status: 401").is_some());
        assert!(patterns::detect_auth_error("error: 403").is_some());
    }

    #[test]
    fn auth_error_message_contains_match_not_tail() {
        // Simulate real agent output: auth error appears early, NDJSON session
        // metadata at the end. Stored message must be the error line, not the
        // trailing JSON that agents emit at exit.
        let text = "some output\nunauthorized: your api key has expired\nx".repeat(400)
            + r#","outputTokens":744,"cacheReadInputTokens":344785,"uuid":"86d13b1f-..."#;
        let err = patterns::detect_auth_error(&text).expect("should detect auth error");
        let AgentError::Auth { message } = err else {
            panic!("expected Auth, got {err:?}");
        };
        assert!(
            message.to_lowercase().contains("unauthorized"),
            "stored message should contain the matched pattern, got: {message}"
        );
        assert!(
            !message.contains("outputTokens"),
            "stored message must not be the trailing NDJSON, got: {message}"
        );
        // The message should be the line containing the auth error, not the tail.
        assert!(
            message.lines().count() <= 2,
            "message should be the auth line, not the full tail: {}",
            message.len()
        );
    }

    #[test]
    fn pattern_detect_network_error() {
        assert!(patterns::detect_network_error("connection refused").is_some());
        assert!(patterns::detect_network_error("ConnectionRefused").is_some());
        assert!(patterns::detect_network_error("Unable to connect to API").is_some());
        assert!(patterns::detect_network_error("ECONNREFUSED").is_some());
        assert!(patterns::detect_network_error("network unreachable").is_some());
        assert!(patterns::detect_network_error("all systems operational").is_none());
    }

    #[test]
    fn network_error_message_contains_match_not_tail() {
        // Simulate real agent output: error appears early, unrelated stats JSON at the end.
        let prefix = "connection refused connecting to api.anthropic.com:443 ";
        let suffix = "x".repeat(2000)
            + r#"":0,"web_fetch_requests":0},"service_tier":"standard","cache_creation":{"ephemeral_1h_input_tokens":0}"#;
        let text = format!("{prefix}{suffix}");

        let err = patterns::detect_network_error(&text).expect("should detect network error");
        let AgentError::NetworkError { message } = err else {
            panic!("expected NetworkError, got {err:?}");
        };
        assert!(
            message.to_lowercase().contains("connection refused"),
            "stored message should contain the matched pattern, got: {message}"
        );
        assert!(
            !message.contains("web_fetch_requests"),
            "stored message must not be the unrelated stats tail, got: {message}"
        );
    }

    #[test]
    fn pattern_detect_context_overflow() {
        assert!(patterns::detect_context_overflow("context_length_exceeded").is_some());
        assert!(patterns::detect_context_overflow("too many tokens in prompt").is_some());
        assert!(patterns::detect_context_overflow("success").is_none());
    }

    #[test]
    fn pattern_detect_worktree_missing() {
        assert!(
            patterns::detect_worktree_missing("worktree directory does not exist: /tmp/wt")
                .is_some()
        );
        assert!(patterns::detect_worktree_missing(
            "failed to create opencode config directory: .orch-opencode/opencode"
        )
        .is_some());
        assert!(patterns::detect_worktree_missing("all good").is_none());
    }

    #[test]
    fn pattern_detect_missing_tool() {
        assert!(patterns::detect_missing_tool("bun: command not found").is_some());
        assert!(patterns::detect_missing_tool("env: anchor: no such file").is_some());
        assert!(patterns::detect_missing_tool("everything works").is_none());
    }

    #[test]
    fn pattern_detect_permission_denied() {
        assert!(patterns::detect_permission_denied("permission denied: /etc/hosts").is_some());
        assert!(patterns::detect_permission_denied("sandbox violation detected").is_some());
        assert!(patterns::detect_permission_denied("disallowed tool: Bash(rm *)").is_some());
        assert!(patterns::detect_permission_denied("task completed").is_none());
    }

    #[test]
    fn pattern_detect_waiting_for_input() {
        assert!(patterns::detect_waiting_for_input("Enter passphrase for key").is_some());
        assert!(patterns::detect_waiting_for_input("1Password CLI required").is_some());
        assert!(
            patterns::detect_waiting_for_input("SSH authentication required for deploy key")
                .is_some()
        );
        assert!(
            patterns::detect_waiting_for_input("HTTP 407 Proxy Authentication Required").is_none()
        );
        assert!(patterns::detect_waiting_for_input("done").is_none());
    }

    #[test]
    fn pattern_detect_stale_session() {
        // Exact error message from Claude
        let err = patterns::detect_stale_session(
            "No conversation found with session ID: 2be572c4-57bb-4f86-bc03-b593c329177c",
        );
        assert!(err.is_some(), "should detect stale session");
        if let Some(AgentError::StaleSession { session_id }) = err {
            assert_eq!(session_id, "2be572c4-57bb-4f86-bc03-b593c329177c");
        }

        // Case-insensitive
        let err = patterns::detect_stale_session("no conversation found with session id: abc-123");
        assert!(err.is_some(), "should detect lowercase variant");

        // Non-matching text
        assert!(patterns::detect_stale_session("all good").is_none());
        assert!(patterns::detect_stale_session("conversation started").is_none());

        // Multi-byte UTF-8 characters before the match must not panic.
        // German ß lowercases to "ss" (1 byte → 2 bytes), so using a byte
        // offset from `lower` directly into `text` would be wrong.
        let multibyte = "Straße: No conversation found with session ID: dead-beef";
        let err = patterns::detect_stale_session(multibyte);
        assert!(
            err.is_some(),
            "should detect stale session after multi-byte chars"
        );
        if let Some(AgentError::StaleSession { session_id }) = err {
            assert_eq!(session_id, "dead-beef");
        }
    }

    #[test]
    fn stale_session_display() {
        let e = AgentError::StaleSession {
            session_id: "abc-123".to_string(),
        };
        assert!(e.to_string().contains("stale session"));
        assert!(e.to_string().contains("abc-123"));
    }

    #[test]
    fn classify_from_text_timeout() {
        let err = patterns::classify_from_text(124, "");
        assert!(matches!(err, AgentError::Timeout { .. }));
    }

    #[test]
    fn classify_from_text_missing_tool_before_rate_limit() {
        // Missing tool takes priority over rate limit patterns
        let err = patterns::classify_from_text(1, "bun: command not found rate limit");
        assert!(matches!(err, AgentError::MissingTool { .. }));
    }

    #[test]
    fn classify_from_text_worktree_missing() {
        let err = patterns::classify_from_text(1, "worktree directory does not exist: /tmp/wt");
        assert!(matches!(err, AgentError::AgentFailed { .. }));
    }

    #[test]
    fn classify_from_text_no_false_positive_from_work_product() {
        // Regression test for issue #1292: agent output containing rate-limit
        // keywords in code/diffs must not be misclassified as a rate limit error.
        let work_product = "Added `is_copilot_model(model) -> bool` helper\n\
             - Added copilot quota patterns to `parse_retry_at`\n\
             fn detect_rate_limit(text: &str) -> Option<AgentError> {\n\
             quota exceeded handling for billing cycle\n"
            .repeat(50); // ~200 lines of work product — well over 3000 chars
        let actual_error = "\nerror: command not found: cargo\n";
        let text = format!("{work_product}{actual_error}");
        let err = patterns::classify_from_text(1, &text);
        assert!(
            !matches!(err, AgentError::RateLimit { .. }),
            "work product keywords must not trigger rate limit, got: {err:?}"
        );
    }

    #[test]
    fn classify_from_text_still_detects_real_rate_limit_at_tail() {
        // A real rate-limit error printed at the end of the output must still
        // be detected, even if work product precedes it.
        let padding = "normal agent work output ".repeat(100);
        let text = format!("{padding}\nError: rate limit exceeded (429 Too Many Requests)\n");
        let err = patterns::classify_from_text(1, &text);
        assert!(
            matches!(err, AgentError::RateLimit { .. }),
            "real rate limit at tail must be detected, got: {err:?}"
        );
    }

    #[test]
    fn classify_from_text_no_false_positive_auth_from_work_product() {
        // Regression test for issue #2126: auth keywords in agent work product
        // must not trigger auth classification when the real failure is
        // unrelated and appears at the end of the output.
        let work_product = "if response.status == 401 { return Err(\"unauthorized\") }\n\
             // handle 403 forbidden errors\n\
             // billing checks happen elsewhere\n"
            .repeat(80);
        let actual_error = "\nerror: command not found: cargo\n";
        let text = format!("{work_product}{actual_error}");
        let err = patterns::classify_from_text(1, &text);
        assert!(
            !matches!(err, AgentError::Auth { .. }),
            "work product auth keywords must not trigger auth classification, got: {err:?}"
        );
        assert!(
            matches!(err, AgentError::MissingTool { .. }),
            "the real tail error should still win, got: {err:?}"
        );
    }

    #[test]
    fn classify_from_text_still_detects_real_auth_at_tail() {
        let padding = "normal agent work output ".repeat(100);
        let text = format!("{padding}\nHTTP 401 Unauthorized\n");
        let err = patterns::classify_from_text(1, &text);
        assert!(
            matches!(err, AgentError::Auth { .. }),
            "real auth error at tail must be detected, got: {err:?}"
        );
    }

    #[test]
    fn classify_from_text_no_false_positive_context_overflow_from_work_product() {
        // Regression test for issue #2205: context overflow keywords in agent
        // work product must not trigger ContextOverflow when the real failure is
        // unrelated and appears at the end of the output.
        let work_product = "const MAX_TOKEN_LIMIT = 10000;\n\
             // Ensure we don't exceed the token limit per request\n\
             fn check_token_limit(tokens: usize) { ... }\n\
             if total_tokens > MAX_TOKEN_LIMIT { ... }\n\
             // Note: too many tokens in the prompt causes context_length_exceeded\n"
            .repeat(80);
        let actual_error = "\nerror: command not found: cargo\n";
        let text = format!("{work_product}{actual_error}");
        let err = patterns::classify_from_text(1, &text);
        assert!(
            !matches!(err, AgentError::ContextOverflow { .. }),
            "work product context keywords must not trigger ContextOverflow, got: {err:?}"
        );
        assert!(
            matches!(err, AgentError::MissingTool { .. }),
            "the real tail error should still win, got: {err:?}"
        );
    }

    #[test]
    fn classify_from_text_still_detects_real_context_overflow_at_tail() {
        let padding = "normal agent work output ".repeat(100);
        let text = format!("{padding}\nError: context_length_exceeded\n");
        let err = patterns::classify_from_text(1, &text);
        assert!(
            matches!(err, AgentError::ContextOverflow { .. }),
            "real context overflow at tail must be detected, got: {err:?}"
        );
    }

    #[test]
    fn classify_from_text_no_false_positive_permission_denied_from_work_product() {
        // Regression test for issue #2205: permission-related keywords in agent
        // work product must not trigger PermissionDenied when the real failure
        // is unrelated and appears at the end of the output.
        let work_product = "match fs::metadata(path) {\n\
             Err(ref e) if e.kind() == ErrorKind::PermissionDenied => {\n\
                 bail!(\"access denied: {path}\");\n\
             }\n\
             // Check if directory is writable\n\
             if !path.is_writable() { ... }\n\
             }\n"
        .repeat(80);
        let actual_error = "\nerror: command not found: cargo\n";
        let text = format!("{work_product}{actual_error}");
        let err = patterns::classify_from_text(1, &text);
        assert!(
            !matches!(err, AgentError::PermissionDenied { .. }),
            "work product permission keywords must not trigger PermissionDenied, got: {err:?}"
        );
        assert!(
            matches!(err, AgentError::MissingTool { .. }),
            "the real tail error should still win, got: {err:?}"
        );
    }

    #[test]
    fn classify_from_text_still_detects_real_permission_denied_at_tail() {
        let padding = "normal agent work output ".repeat(100);
        let text = format!("{padding}\npermission denied: /etc/hosts\n");
        let err = patterns::classify_from_text(1, &text);
        assert!(
            matches!(err, AgentError::PermissionDenied { .. }),
            "real permission denied at tail must be detected, got: {err:?}"
        );
    }

    #[test]
    fn classify_from_text_no_false_positive_waiting_for_input_from_work_product() {
        // Regression test for issue #2205: input-request keywords in agent work
        // product must not trigger WaitingForInput when the real failure is
        // unrelated and appears at the end of the output.
        let work_product = "// Handle password prompts gracefully\n\
             if prompt.contains(\"password:\") {\n\
                 log::warn!(\"Password prompt detected — skipping interactive step\");\n\
             }\n\
             // SSH agent will ask for the deploy key passphrase\n\
             fn handle_ssh_passphrase() { ... }\n\
             )\n"
        .repeat(80);
        let actual_error = "\nerror: command not found: cargo\n";
        let text = format!("{work_product}{actual_error}");
        let err = patterns::classify_from_text(1, &text);
        assert!(
            !matches!(err, AgentError::WaitingForInput { .. }),
            "work product input keywords must not trigger WaitingForInput, got: {err:?}"
        );
        assert!(
            matches!(err, AgentError::MissingTool { .. }),
            "the real tail error should still win, got: {err:?}"
        );
    }

    #[test]
    fn classify_from_text_still_detects_real_waiting_for_input_at_tail() {
        let padding = "normal agent work output ".repeat(100);
        let text = format!("{padding}\nEnter passphrase for key:\n");
        let err = patterns::classify_from_text(1, &text);
        assert!(
            matches!(err, AgentError::WaitingForInput { .. }),
            "real waiting for input at tail must be detected, got: {err:?}"
        );
    }

    #[test]
    fn classify_from_text_prefers_auth_over_permission_denied() {
        let err = patterns::classify_from_text(1, "HTTP 403 access denied: invalid token");
        assert!(
            matches!(err, AgentError::Auth { .. }),
            "auth errors must outrank permission denied, got: {err:?}"
        );
    }

    #[test]
    fn classify_from_text_prefers_auth_over_waiting_for_input() {
        let err = patterns::classify_from_text(1, "HTTP 407 Proxy Authentication Required");
        assert!(
            matches!(err, AgentError::Auth { .. }),
            "HTTP auth failures must not be treated as waiting for input, got: {err:?}"
        );
    }

    #[test]
    fn classify_from_text_detects_stale_session() {
        // Regression test for issue #1800: classify_from_text must call
        // detect_stale_session to catch "No conversation found with session ID"
        // errors. Without this, the error would be misclassified as Unknown and
        // the control.rs recovery code would not trigger.
        let text = "No conversation found with session ID: 2be572c4-57bb-4f86-bc03-b593c329177c";
        let err = patterns::classify_from_text(1, text);
        assert!(
            matches!(err, AgentError::StaleSession { .. }),
            "stale session must be detected via classify_from_text, got: {err:?}"
        );
        // Verify the Display format matches what control.rs expects
        assert!(
            err.to_string().contains("stale session"),
            "error display must contain 'stale session' for recovery code to trigger"
        );
    }

    #[test]
    fn classify_from_text_detects_socket_closed() {
        // Regression test for issue #3045: "socket connection was closed" must be
        // classified as NetworkError, not Unknown.
        let text = "API Error: The socket connection was closed unexpectedly. \
             For more information, pass `verbose: true` in the second argument to fetch()";
        let err = patterns::classify_from_text(1, text);
        assert!(
            matches!(err, AgentError::NetworkError { .. }),
            "socket closed must be NetworkError, got: {err:?}"
        );
    }

    #[test]
    fn classify_from_text_detects_socket_hang_up() {
        let err = patterns::classify_from_text(1, "Error: socket hang up");
        assert!(
            matches!(err, AgentError::NetworkError { .. }),
            "socket hang up must be NetworkError, got: {err:?}"
        );
    }

    #[test]
    fn classify_from_text_detects_fetch_failed() {
        let err = patterns::classify_from_text(1, "TypeError: fetch failed");
        assert!(
            matches!(err, AgentError::NetworkError { .. }),
            "fetch failed must be NetworkError, got: {err:?}"
        );
    }

    #[test]
    fn classify_from_text_detects_econnreset() {
        let err = patterns::classify_from_text(1, "read ECONNRESET");
        assert!(
            matches!(err, AgentError::NetworkError { .. }),
            "ECONNRESET must be NetworkError, got: {err:?}"
        );
    }

    #[test]
    fn synthesize_response_marks_done_for_plain_text_no_op() {
        let response = synthesize_response_from_text(
            "No open positions, no trade executions, no conditions change. Full cash, holding.",
        )
        .unwrap();

        assert_eq!(response.status, "done");
        assert_eq!(
            response.summary,
            "No open positions, no trade executions, no conditions change. Full cash, holding."
        );
        assert!(response.error.is_none());
    }

    #[test]
    fn synthesize_response_marks_needs_review_for_plain_text_error() {
        let response =
            synthesize_response_from_text("Failed to update branch: permission denied").unwrap();

        assert_eq!(response.status, "needs_review");
        assert_eq!(
            response.error.as_deref(),
            Some("agent returned plain text instead of JSON")
        );
    }

    #[test]
    fn synthesize_response_rejects_empty_text() {
        assert!(synthesize_response_from_text("   \n\t  ").is_none());
    }

    // Regression: opencode/nemotron-3-super-free and similar models sometimes exit
    // with code 0 after emitting only exploratory or startup text (e.g. "Let's go!",
    // planning prose) without completing any work.  The synthesis must return None for
    // such ambiguous text so the runner treats the run as an invalid response and
    // reroutes the task instead of falsely advancing it to needs_review / in_review.
    // (Issues #2404, #2653, #2666)
    #[test]
    fn synthesize_response_returns_none_for_ambiguous_startup_text() {
        // Short greeting — not an error, not done, not an explicit negation.
        assert!(
            synthesize_response_from_text("Let's go!").is_none(),
            "startup greeting must not synthesize as needs_review"
        );
        // Exploratory planning prose — agent is thinking out loud, not reporting completion.
        assert!(
            synthesize_response_from_text(
                "I need to find where the stream command is implemented. \
                 Looking at the CLI structure, it seems like the stream functionality \
                 might be in the task module or another file. Let me check the CLI \
                 command structure. First, let me look at the task module since stream \
                 is related to tasks."
            )
            .is_none(),
            "exploratory prose must not synthesize as needs_review"
        );
    }

    #[test]
    fn synthesize_response_does_not_mark_done_for_incomplete() {
        let response = synthesize_response_from_text("The task is incomplete").unwrap();
        assert_eq!(
            response.status, "needs_review",
            "\"incomplete\" should not match \"complete\""
        );
    }

    #[test]
    fn synthesize_response_does_not_mark_done_for_not_done() {
        let response = synthesize_response_from_text("Changes are not done yet").unwrap();
        assert_eq!(
            response.status, "needs_review",
            "\"not done\" should not match bare \"done\""
        );
    }

    #[test]
    fn synthesize_response_does_not_mark_done_for_not_complete() {
        let response = synthesize_response_from_text("Still in progress, not complete").unwrap();
        assert_eq!(
            response.status, "needs_review",
            "\"not complete\" should not match \"complete\""
        );
    }

    #[test]
    fn synthesize_response_does_not_mark_done_for_not_yet_completed() {
        let response = synthesize_response_from_text("The task is not yet completed").unwrap();
        assert_eq!(
            response.status, "needs_review",
            "\"not yet completed\" should not match \"completed\""
        );
    }

    #[test]
    fn synthesize_response_marks_done_for_completed() {
        let response =
            synthesize_response_from_text("Task has been completed successfully").unwrap();
        assert_eq!(response.status, "done");
    }

    #[test]
    fn synthesize_response_marks_done_for_word_boundary_done() {
        let response = synthesize_response_from_text("All changes are done.").unwrap();
        assert_eq!(response.status, "done");
    }

    #[test]
    fn synthesize_response_marks_done_for_issue_creation_text() {
        let response =
            synthesize_response_from_text("Filed 3 high-value GitHub issues: #1037, #1038, #1039")
                .unwrap();

        assert_eq!(response.status, "done");
        assert!(response.error.is_none());
        assert!(response
            .summary
            .contains("Filed 3 high-value GitHub issues"));
    }

    #[test]
    fn synthesize_response_marks_done_for_committed() {
        let response =
            synthesize_response_from_text("I've committed the changes to the repository").unwrap();
        assert_eq!(response.status, "done");
    }

    #[test]
    fn synthesize_response_rejects_exploratory_prose() {
        // Regression test: ensure exploratory analysis that mentions "let me check"
        // or planning language is NOT synthesized into a "done" completion.
        // With the ambiguous-text fix, this text correctly returns None (treated as
        // an invalid response / reroute signal) rather than synthesizing needs_review.
        // Crucially, it must NOT be marked "done".
        let exploratory = "I explored the codebase and found several .expect()/.unwrap() uses.\n\nLet me check each occurrence directly and run the tests to be sure. I may commit fixes afterwards.";
        match synthesize_response_from_text(exploratory) {
            None => {
                // Ambiguous exploratory prose correctly returns None — not marked done.
            }
            Some(resp) => {
                assert_ne!(
                    resp.status, "done",
                    "Exploratory prose must not be auto-marked done"
                );
            }
        }
    }

    #[test]
    fn synthesize_response_marks_done_for_commit_created() {
        let response = synthesize_response_from_text(
            "Commit created locally. The push is being blocked by permissions.",
        )
        .unwrap();
        assert_eq!(response.status, "done");
    }

    #[test]
    fn synthesize_response_does_not_mark_done_for_not_committed() {
        let response = synthesize_response_from_text("The changes are not committed yet").unwrap();
        assert_eq!(
            response.status, "needs_review",
            "\"not committed\" should not match \"committed\""
        );
    }

    // ── Regression: #1362/#1363 — agent text misclassified as needs_review ──

    #[test]
    fn synthesize_response_marks_done_for_fix_is_complete() {
        // Exact agent output from task 30203 (issue #1363).
        // "The fix is complete" was misclassified as needs_review because
        // the pattern list had "completed" but not "complete".
        let response = synthesize_response_from_text(
            "All 73 cooldown tests pass. The fix is complete.\n\n\
             **Summary of changes:**\n\n\
             1. **`src/store/kv.rs`** — Added `kv_increment()` method that uses a single \
             `INSERT … ON CONFLICT … DO UPDATE … RETURNING` SQL statement.",
        )
        .unwrap();
        assert_eq!(
            response.status, "done",
            "\"The fix is complete\" must be classified as done, not needs_review"
        );
    }

    #[test]
    fn synthesize_response_marks_done_for_tests_pass_fix_working() {
        // Exact agent output from task 30204 (issue #1362).
        // "All tests pass. The fix is clean and working." was misclassified
        // as needs_review because none of the looks_done patterns matched.
        let response = synthesize_response_from_text(
            "All tests pass. The fix is clean and working.\n\n\
             **Summary of changes** to `src/engine/runner/response.rs:37`:\n\n\
             - **Before:** `capped + jitter` → range `[capped, 1.6*capped]`\n\
             - **After:** `capped.saturating_sub(jitter_range) + jitter` → range `[0.7*capped, 1.3*capped]`",
        )
        .unwrap();
        assert_eq!(
            response.status, "done",
            "\"All tests pass. The fix is clean and working.\" must be classified as done"
        );
    }

    // ── parse_ndjson edge cases ───────────────────────────────────

    #[test]
    fn parse_ndjson_handles_standard_lines() {
        let raw = r#"{"type":"text","text":"hello"}
{"type":"text","text":"world"}"#;
        let events = parse_ndjson(raw);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].get("type").unwrap().as_str(), Some("text"));
        assert_eq!(events[1].get("type").unwrap().as_str(), Some("text"));
    }

    #[test]
    fn parse_ndjson_skips_empty_lines() {
        let raw = r#"{"type":"text"}

{"type":"text"}
"#;
        let events = parse_ndjson(raw);
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn parse_ndjson_skips_malformed_lines() {
        let raw = r#"{"type":"valid"}
not json at all
{"type":"also_valid"}"#;
        let events = parse_ndjson(raw);
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn parse_ndjson_fallback_extracts_embedded_json() {
        // Edge case: JSON with closing fence inside string field
        let raw =
            r#"text: {"text": "here is the result: ```json\n{\"status\":\"done\"}\n```"} more"#;
        let events = parse_ndjson(raw);
        // Should have extracted the embedded JSON object
        assert!(
            !events.is_empty(),
            "should extract JSON objects via fallback"
        );
    }

    #[test]
    fn parse_ndjson_fallback_handles_fragmented_objects() {
        // Edge case: JSON object spread across multiple lines (fragmented NDJSON)
        let raw = r#"{"type": "text
", "content": "hello
world"}"#;
        let events = parse_ndjson(raw);
        // Should still extract the JSON via fallback
        assert!(!events.is_empty(), "should handle fragmented JSON");
    }

    // ── PermissionRules defaults ────────────────────────────────

    #[test]
    fn permission_rules_default_is_autonomous() {
        let rules = PermissionRules::default();
        assert!(rules.autonomous);
    }

    #[test]
    fn permission_rules_default_sandbox_is_workspace_write() {
        let rules = PermissionRules::default();
        assert_eq!(rules.sandbox, SandboxLevel::WorkspaceWrite);
    }

    #[test]
    fn permission_rules_default_disallows_rm() {
        let rules = PermissionRules::default();
        assert!(rules.disallowed_tools.contains(&"Bash(rm *)".to_string()));
        assert!(rules.disallowed_tools.contains(&"Bash(rm -*)".to_string()));
    }

    #[test]
    fn permission_rules_default_no_allowed_edit_paths() {
        let rules = PermissionRules::default();
        assert!(rules.allowed_edit_paths.is_empty());
    }

    // ── Permission translation across all agents ────────────────

    /// Test that all agents handle the default permission rules consistently.
    #[test]
    fn all_agents_handle_default_permissions() {
        let perms = PermissionRules::default();
        let sys = "/tmp/sys.md";
        let msg = "/tmp/msg.md";

        // Claude: should have bypassPermissions and rm disallowed
        let claude = get_runner("claude");
        let cmd = claude.build_command(None, "", sys, msg, &perms);
        assert!(
            cmd.contains("--permission-mode bypassPermissions"),
            "claude default: expected bypassPermissions, got: {cmd}"
        );
        assert!(
            cmd.contains("Bash(rm *)"),
            "claude default: expected rm disallowed, got: {cmd}"
        );

        // Codex: autonomous + workspace-write → --sandbox workspace-write -c 'approval_policy="never"'
        let codex = get_runner("codex");
        let cmd = codex.build_command(None, "", sys, msg, &perms);
        assert!(
            cmd.contains("--sandbox workspace-write"),
            "codex default: expected --sandbox workspace-write, got: {cmd}"
        );
        assert!(
            cmd.contains(r#"-c 'approval_policy="never"'"#),
            "codex default: expected -c approval_policy=never, got: {cmd}"
        );
        assert!(
            !cmd.contains("--ask-for-approval"),
            "codex default: --ask-for-approval was removed in 0.133.0 and must not appear, got: {cmd}"
        );
        assert!(
            !cmd.contains("--full-auto"),
            "codex default: --full-auto is deprecated and must not appear, got: {cmd}"
        );

        // OpenCode: should write permission config and override XDG_CONFIG_HOME
        let opencode = get_runner("opencode");
        let cmd = opencode.build_command(None, "", sys, msg, &perms);
        assert!(
            cmd.contains("opencode.json"),
            "opencode default: should write permission config, got: {cmd}"
        );
        assert!(
            cmd.contains("XDG_CONFIG_HOME=.orch-opencode"),
            "opencode default: should set XDG_CONFIG_HOME, got: {cmd}"
        );
    }

    /// Test supervised mode translates correctly for each agent.
    #[test]
    fn all_agents_handle_supervised_mode() {
        let perms = PermissionRules {
            autonomous: false,
            sandbox: SandboxLevel::WorkspaceWrite,
            disallowed_tools: vec![],
            allowed_tools: vec![],
            allowed_edit_paths: vec![],
            deny_read_only: false,
            extra_writable_dirs: vec![],
        };
        let sys = "/tmp/sys.md";
        let msg = "/tmp/msg.md";

        // Claude: supervised → acceptEdits
        let claude = get_runner("claude");
        let cmd = claude.build_command(None, "", sys, msg, &perms);
        assert!(
            cmd.contains("--permission-mode acceptEdits"),
            "claude supervised: expected acceptEdits, got: {cmd}"
        );

        // Codex: supervised → on-request
        let codex = get_runner("codex");
        let cmd = codex.build_command(None, "", sys, msg, &perms);
        assert!(
            cmd.contains(r#"-c 'approval_policy="on-request"'"#),
            "codex supervised: expected -c approval_policy=on-request, got: {cmd}"
        );
        assert!(
            !cmd.contains("--ask-for-approval"),
            "codex supervised: --ask-for-approval was removed in 0.133.0 and must not appear, got: {cmd}"
        );
    }

    /// Test full access sandbox translates correctly for each agent.
    #[test]
    fn all_agents_handle_full_access_sandbox() {
        let perms = PermissionRules {
            autonomous: true,
            sandbox: SandboxLevel::FullAccess,
            disallowed_tools: vec![],
            allowed_tools: vec![],
            allowed_edit_paths: vec![],
            deny_read_only: false,
            extra_writable_dirs: vec![],
        };
        let sys = "/tmp/sys.md";
        let msg = "/tmp/msg.md";

        // Claude: ignores sandbox level (no --sandbox flag)
        let claude = get_runner("claude");
        let cmd = claude.build_command(None, "", sys, msg, &perms);
        assert!(
            !cmd.contains("--sandbox"),
            "claude full_access: should have no sandbox flag, got: {cmd}"
        );

        // Codex: full access → --dangerously-bypass-approvals-and-sandbox
        let codex = get_runner("codex");
        let cmd = codex.build_command(None, "", sys, msg, &perms);
        assert!(
            cmd.contains("--dangerously-bypass-approvals-and-sandbox"),
            "codex full_access: expected dangerously-bypass, got: {cmd}"
        );
    }

    /// SandboxLevel::None with autonomous uses --sandbox workspace-write (same as WorkspaceWrite).
    #[test]
    fn codex_sandbox_none_uses_workspace_write() {
        let perms = PermissionRules {
            autonomous: true,
            sandbox: SandboxLevel::None,
            disallowed_tools: vec![],
            allowed_tools: vec![],
            allowed_edit_paths: vec![],
            deny_read_only: false,
            extra_writable_dirs: vec![],
        };
        let codex = get_runner("codex");
        let cmd = codex.build_command(None, "", "/tmp/sys.md", "/tmp/msg.md", &perms);
        assert!(
            cmd.contains("--sandbox workspace-write"),
            "codex sandbox::none + autonomous: should use --sandbox workspace-write, got: {cmd}"
        );
        assert!(
            cmd.contains(r#"-c 'approval_policy="never"'"#),
            "codex sandbox::none + autonomous: should use -c approval_policy=never, got: {cmd}"
        );
        assert!(
            !cmd.contains("--ask-for-approval"),
            "codex sandbox::none: --ask-for-approval was removed in 0.133.0, got: {cmd}"
        );
        assert!(
            !cmd.contains("--full-auto"),
            "codex sandbox::none: --full-auto is deprecated and must not appear, got: {cmd}"
        );
    }

    /// Test that allowed_edit_paths scopes Edit/Write when used with allowed_tools.
    #[test]
    fn claude_allowed_edit_paths_scope_edit_write() {
        let perms = PermissionRules {
            autonomous: true,
            sandbox: SandboxLevel::WorkspaceWrite,
            disallowed_tools: vec![],
            allowed_tools: vec![
                "Edit".to_string(),
                "Write".to_string(),
                "Read".to_string(),
                "Bash".to_string(),
            ],
            allowed_edit_paths: vec![PathBuf::from("/home/user/worktree")],
            deny_read_only: false,
            extra_writable_dirs: vec![],
        };
        let claude = get_runner("claude");
        let cmd = claude.build_command(None, "", "/tmp/sys.md", "/tmp/msg.md", &perms);

        // Edit/Write should be scoped to worktree path
        assert!(
            cmd.contains("Edit(/home/user/worktree/*)"),
            "missing scoped Edit, got: {cmd}"
        );
        assert!(
            cmd.contains("Write(/home/user/worktree/*)"),
            "missing scoped Write, got: {cmd}"
        );
        // Read should be unrestricted
        assert!(cmd.contains("Read"), "missing Read");
    }

    /// Test that blocked paths don't affect Codex (it uses sandbox isolation).
    #[test]
    fn codex_ignores_allowed_edit_paths() {
        let perms = PermissionRules {
            autonomous: true,
            sandbox: SandboxLevel::WorkspaceWrite,
            disallowed_tools: vec![],
            allowed_tools: vec![],
            allowed_edit_paths: vec![PathBuf::from("/home/user/project")],
            deny_read_only: false,
            extra_writable_dirs: vec![],
        };
        let codex = get_runner("codex");
        let cmd = codex.build_command(None, "", "/tmp/sys.md", "/tmp/msg.md", &perms);
        assert!(
            !cmd.contains("/home/user/project"),
            "codex: should not reference blocked paths, got: {cmd}"
        );
    }

    /// Test that blocked paths don't affect OpenCode.
    #[test]
    fn opencode_ignores_allowed_edit_paths() {
        let perms = PermissionRules {
            autonomous: true,
            sandbox: SandboxLevel::WorkspaceWrite,
            disallowed_tools: vec![],
            allowed_tools: vec![],
            allowed_edit_paths: vec![PathBuf::from("/home/user/project")],
            deny_read_only: false,
            extra_writable_dirs: vec![],
        };
        let opencode = get_runner("opencode");
        let cmd = opencode.build_command(None, "", "/tmp/sys.md", "/tmp/msg.md", &perms);
        assert!(
            !cmd.contains("/home/user/project"),
            "opencode: should not reference blocked paths, got: {cmd}"
        );
    }

    /// Test that disallowed tools merge correctly for Claude (defaults + custom).
    #[test]
    fn claude_merges_disallowed_tools() {
        let perms = PermissionRules {
            autonomous: true,
            sandbox: SandboxLevel::WorkspaceWrite,
            disallowed_tools: vec![
                "Bash(rm *)".to_string(),
                "Bash(sudo *)".to_string(),
                "WebFetch".to_string(),
            ],
            allowed_tools: vec![],
            allowed_edit_paths: vec![],
            deny_read_only: false,
            extra_writable_dirs: vec![],
        };
        let claude = get_runner("claude");
        let cmd = claude.build_command(None, "", "/tmp/sys.md", "/tmp/msg.md", &perms);
        assert!(cmd.contains("Bash(rm *)"), "missing rm disallow");
        assert!(cmd.contains("Bash(sudo *)"), "missing sudo disallow");
        assert!(cmd.contains("WebFetch"), "missing WebFetch disallow");
    }

    /// Test that Codex ignores disallowed tools (has no such flag).
    #[test]
    fn codex_ignores_disallowed_tools() {
        let perms = PermissionRules {
            autonomous: true,
            sandbox: SandboxLevel::WorkspaceWrite,
            disallowed_tools: vec!["Bash(rm *)".to_string(), "WebFetch".to_string()],
            allowed_tools: vec![],
            allowed_edit_paths: vec![],
            deny_read_only: false,
            extra_writable_dirs: vec![],
        };
        let codex = get_runner("codex");
        let cmd = codex.build_command(None, "", "/tmp/sys.md", "/tmp/msg.md", &perms);
        assert!(
            !cmd.contains("disallow"),
            "codex: should not have disallow flags, got: {cmd}"
        );
    }

    /// Test Claude with empty disallowed tools produces no --disallowedTools flag.
    #[test]
    fn claude_no_disallowed_flag_when_empty() {
        let perms = PermissionRules {
            autonomous: true,
            sandbox: SandboxLevel::WorkspaceWrite,
            disallowed_tools: vec![],
            allowed_tools: vec![],
            allowed_edit_paths: vec![],
            deny_read_only: false,
            extra_writable_dirs: vec![],
        };
        let claude = get_runner("claude");
        let cmd = claude.build_command(None, "", "/tmp/sys.md", "/tmp/msg.md", &perms);
        assert!(
            !cmd.contains("--disallowedTools"),
            "claude: should have no disallowed flag when empty, got: {cmd}"
        );
    }

    /// Test Kimi/MiniMax aliases inherit Claude permission translation.
    #[test]
    fn kimi_minimax_use_claude_permissions() {
        let perms = PermissionRules {
            autonomous: false,
            sandbox: SandboxLevel::WorkspaceWrite,
            disallowed_tools: vec!["Bash(rm *)".to_string()],
            allowed_tools: vec![],
            allowed_edit_paths: vec![],
            deny_read_only: false,
            extra_writable_dirs: vec![],
        };
        let sys = "/tmp/sys.md";
        let msg = "/tmp/msg.md";

        for agent in &["kimi", "minimax"] {
            let runner = get_runner(agent);
            let cmd = runner.build_command(None, "", sys, msg, &perms);
            assert!(
                cmd.contains("--permission-mode acceptEdits"),
                "{agent}: expected acceptEdits"
            );
            assert!(
                cmd.contains("Bash(rm *)"),
                "{agent}: expected rm disallowed"
            );
        }
    }

    /// Test kimi/minimax get --allowedTools when allowed_tools is set.
    #[test]
    fn kimi_minimax_use_allowed_tools() {
        let perms = PermissionRules {
            autonomous: true,
            sandbox: SandboxLevel::WorkspaceWrite,
            disallowed_tools: vec![],
            allowed_tools: vec!["Edit".to_string(), "Bash".to_string(), "git".to_string()],
            allowed_edit_paths: vec![],
            deny_read_only: false,
            extra_writable_dirs: vec![],
        };

        for agent in &["kimi", "minimax"] {
            let runner = get_runner(agent);
            let cmd = runner.build_command(None, "", "/tmp/sys.md", "/tmp/msg.md", &perms);
            assert!(
                cmd.contains("--allowedTools"),
                "{agent}: expected --allowedTools, got: {cmd}"
            );
            assert!(
                cmd.contains("Bash(git *)"),
                "{agent}: expected Bash(git *) pattern, got: {cmd}"
            );
        }
    }

    // ── Allowed tools across agents ─────────────────────────────

    /// Test that allowed_tools generates --allowedTools for Claude.
    #[test]
    fn claude_allowed_tools_generates_allowedtools_flag() {
        let perms = PermissionRules {
            autonomous: true,
            sandbox: SandboxLevel::WorkspaceWrite,
            disallowed_tools: vec!["Bash(rm *)".to_string()],
            allowed_tools: vec![
                "Edit".to_string(),
                "Write".to_string(),
                "Read".to_string(),
                "Bash".to_string(),
                "git".to_string(),
            ],
            allowed_edit_paths: vec![],
            deny_read_only: false,
            extra_writable_dirs: vec![],
        };

        let claude = get_runner("claude");
        let cmd = claude.build_command(None, "", "/tmp/sys.md", "/tmp/msg.md", &perms);
        assert!(
            cmd.contains("--allowedTools"),
            "expected --allowedTools, got: {cmd}"
        );
        assert!(
            !cmd.contains("--disallowedTools"),
            "should not have --disallowedTools when allowed_tools is set"
        );
    }

    /// Test that codex/opencode ignore allowed_tools (no equivalent flag).
    #[test]
    fn codex_opencode_ignore_allowed_tools() {
        let perms = PermissionRules {
            autonomous: true,
            sandbox: SandboxLevel::WorkspaceWrite,
            disallowed_tools: vec![],
            allowed_tools: vec!["Edit".to_string(), "git".to_string()],
            allowed_edit_paths: vec![],
            deny_read_only: false,
            extra_writable_dirs: vec![],
        };

        let codex = get_runner("codex");
        let cmd = codex.build_command(None, "", "/tmp/sys.md", "/tmp/msg.md", &perms);
        assert!(
            !cmd.contains("--allowedTools"),
            "codex should not have --allowedTools, got: {cmd}"
        );

        let opencode = get_runner("opencode");
        let cmd = opencode.build_command(None, "", "/tmp/sys.md", "/tmp/msg.md", &perms);
        assert!(
            !cmd.contains("--allowedTools"),
            "opencode should not have --allowedTools, got: {cmd}"
        );
    }

    // ── Integration: from_config() → agent translation ──────────

    /// Integration test: verify from_config() loads real config and agents
    /// translate it correctly. Requires ~/.orch/config.yml to exist.
    #[test]
    #[ignore]
    fn integration_from_config_to_agent_commands() {
        let perms = PermissionRules::from_config();

        // Verify defaults are reasonable
        assert!(
            perms.autonomous,
            "from_config: expected autonomous=true by default"
        );

        // Verify each agent can build a command from the config-loaded rules
        for agent_name in &["claude", "codex", "opencode", "kimi", "minimax"] {
            let runner = get_runner(agent_name);
            let cmd =
                runner.build_command(None, "timeout 1800", "/tmp/sys.md", "/tmp/msg.md", &perms);
            assert!(
                !cmd.is_empty(),
                "{agent_name}: build_command returned empty string"
            );
            assert!(
                cmd.contains(agent_name) || *agent_name == "kimi" || *agent_name == "minimax",
                "{agent_name}: command should reference agent binary, got: {cmd}"
            );
        }
    }

    /// Integration test: verify from_config() handles supervised mode.
    /// Requires config with workflow.permissions.mode = "supervised".
    #[test]
    #[ignore]
    fn integration_supervised_config_translates_correctly() {
        // This test requires config to have:
        // workflow:
        //   permissions:
        //     mode: supervised
        let perms = PermissionRules::from_config();

        let claude = get_runner("claude");
        let cmd = claude.build_command(None, "", "/tmp/sys.md", "/tmp/msg.md", &perms);

        if perms.autonomous {
            assert!(
                cmd.contains("bypassPermissions"),
                "autonomous config → bypassPermissions"
            );
        } else {
            assert!(
                cmd.contains("acceptEdits"),
                "supervised config → acceptEdits"
            );
        }

        let codex = get_runner("codex");
        let cmd = codex.build_command(None, "", "/tmp/sys.md", "/tmp/msg.md", &perms);

        if perms.autonomous {
            // autonomous codex uses --sandbox workspace-write -c 'approval_policy="never"'
            assert!(
                cmd.contains("--sandbox workspace-write"),
                "autonomous config → --sandbox workspace-write, got: {cmd}"
            );
            assert!(
                cmd.contains(r#"-c 'approval_policy="never"'"#),
                "autonomous config → -c approval_policy=never, got: {cmd}"
            );
        } else {
            assert!(
                cmd.contains(r#"-c 'approval_policy="on-request"'"#),
                "supervised config → -c approval_policy=on-request, got: {cmd}"
            );
        }
        assert!(
            !cmd.contains("--ask-for-approval"),
            "--ask-for-approval was removed in codex 0.133.0 and must not appear, got: {cmd}"
        );
    }

    // ── deny_read_only preset ───────────────────────────────────

    /// Test that deny_read_only() sets the right disallowed tools for control sessions.
    #[test]
    fn deny_read_only_blocks_reading_tools() {
        let perms = PermissionRules::deny_read_only();

        let read_tools = [
            "Bash(ls *)",
            "Bash(ls)",
            "Bash(find *)",
            "Bash(cat *)",
            "Bash(head *)",
            "Bash(tail *)",
            "Bash(fzf *)",
            "Bash(less *)",
            "Bash(more *)",
            "Read",
            "Glob",
            "Grep",
        ];

        for tool in read_tools {
            assert!(
                perms.disallowed_tools.contains(&tool.to_string()),
                "deny_read_only should block {tool}, got: {:?}",
                perms.disallowed_tools
            );
        }
    }

    /// Test that deny_read_only() produces correct Claude command for control session.
    #[test]
    fn deny_read_only_claude_command() {
        let perms = PermissionRules::deny_read_only();
        let claude = get_runner("claude");
        let cmd = claude.build_command(None, "", "/tmp/sys.md", "/tmp/msg.md", &perms);

        // Should contain Read, Glob, Grep disallow patterns
        assert!(
            cmd.contains("Read"),
            "should block Read for control session"
        );
        assert!(
            cmd.contains("Glob"),
            "should block Glob for control session"
        );
        assert!(
            cmd.contains("Grep"),
            "should block Grep for control session"
        );
        // Should block ls
        assert!(
            cmd.contains("Bash(ls") || cmd.contains("Bash(ls *)"),
            "should block ls"
        );
        // Should still have bypassPermissions (autonomous)
        assert!(
            cmd.contains("--permission-mode bypassPermissions"),
            "should be autonomous (bypassPermissions)"
        );
    }

    // --- synthesize_response_from_text structured extraction ---

    #[test]
    fn synthesize_extracts_file_paths() {
        let text = "Fixed the issue by modifying src/engine/runner/agents/mod.rs and tests/integration.rs.";
        let resp = synthesize_response_from_text(text).unwrap();
        assert!(
            resp.files
                .contains(&"src/engine/runner/agents/mod.rs".to_string()),
            "expected src/engine/runner/agents/mod.rs in files, got: {:?}",
            resp.files
        );
        assert!(
            resp.files.contains(&"tests/integration.rs".to_string()),
            "expected tests/integration.rs in files, got: {:?}",
            resp.files
        );
    }

    #[test]
    fn synthesize_extracts_accomplished_bullets() {
        let text = "All tests pass.\n- Added logging to src/foo.rs\n- Updated migration file";
        let resp = synthesize_response_from_text(text).unwrap();
        assert_eq!(resp.status, "done");
        assert!(
            resp.accomplished
                .iter()
                .any(|a| a.contains("Added logging")),
            "expected accomplished bullet, got: {:?}",
            resp.accomplished
        );
        assert!(
            resp.accomplished
                .iter()
                .any(|a| a.contains("Updated migration")),
            "expected accomplished bullet, got: {:?}",
            resp.accomplished
        );
    }

    #[test]
    fn synthesize_extracts_remaining_section() {
        let text = "Completed the main fix.\n- Patched src/lib.rs\n\nRemaining:\n- Write unit tests\n- Update docs";
        let resp = synthesize_response_from_text(text).unwrap();
        assert!(
            resp.remaining
                .iter()
                .any(|r| r.contains("Write unit tests")),
            "expected remaining item, got: {:?}",
            resp.remaining
        );
        assert!(
            resp.remaining.iter().any(|r| r.contains("Update docs")),
            "expected remaining item, got: {:?}",
            resp.remaining
        );
        assert!(
            resp.accomplished.iter().any(|a| a.contains("Patched")),
            "expected accomplished item, got: {:?}",
            resp.accomplished
        );
    }

    #[test]
    fn synthesize_fixed_phrase_is_done() {
        let text = "Fixed. The bug no longer reproduces.";
        let resp = synthesize_response_from_text(text).unwrap();
        assert_eq!(resp.status, "done", "expected done for 'Fixed.' text");
    }

    // ── detect_rate_limit false-positive guard ────────────────────────────
    // Regression: NDJSON telemetry blobs can contain "429" as a token count
    // (e.g. "outputTokens":429). `detect_rate_limit` should still flag those
    // because it cannot distinguish context without the terminal_reason guard.
    // The guard lives in review.rs, but here we verify the raw detector
    // behaviour so callers know they must apply the guard themselves.
    #[test]
    fn detect_rate_limit_fires_on_bare_429_in_token_count() {
        // This SHOULD return Some — the generic detector has no context.
        // The caller (review.rs) is responsible for suppressing it when
        // terminal_reason:completed is present.
        let payload = r#"{"type":"result","terminal_reason":"completed","outputTokens":429,"is_error":false}"#;
        assert!(
            patterns::detect_rate_limit(payload).is_some(),
            "detect_rate_limit must still fire on bare 429 so callers apply the guard"
        );
    }

    // Verify the guard logic that review.rs uses: when raw NDJSON has
    // terminal_reason:completed without is_error:true, the result of
    // detect_rate_limit should be suppressed.
    #[test]
    fn terminal_reason_completed_guard_suppresses_false_rate_limit() {
        let raw_output = r#"{"type":"result","terminal_reason":"completed","outputTokens":429,"is_error":false}"#;
        let ndjson_completed = raw_output.contains("\"terminal_reason\":\"completed\"")
            && !raw_output.contains("\"is_error\":true");
        // Apply the same guard that review.rs now uses.
        let detected = (!ndjson_completed)
            .then(|| patterns::detect_rate_limit(raw_output))
            .flatten();
        assert!(
            detected.is_none(),
            "guard must suppress rate-limit false positive from token-count 429 when terminal_reason:completed"
        );
    }

    // Verify the guard does NOT suppress a real rate-limit error.
    #[test]
    fn terminal_reason_completed_guard_passes_real_rate_limit() {
        // Real rate-limit output has no terminal_reason:completed.
        let raw_output = "Error: 429 Too Many Requests — you have exceeded your quota";
        let ndjson_completed = raw_output.contains("\"terminal_reason\":\"completed\"")
            && !raw_output.contains("\"is_error\":true");
        let detected = (!ndjson_completed)
            .then(|| patterns::detect_rate_limit(raw_output))
            .flatten();
        assert!(
            detected.is_some(),
            "guard must NOT suppress a real rate-limit error"
        );
    }

    #[test]
    fn synthesize_no_false_positive_file_paths() {
        // URLs and version strings should not appear in files list.
        // The text is ambiguous (no error, no done signal, no explicit negation) so
        // synthesize_response_from_text returns None — no file extraction happens.
        let text = "See https://example.com/foo.rs for details. Version 1.2.3 is fine.";
        match synthesize_response_from_text(text) {
            None => {} // Ambiguous text correctly returns None — no files extracted.
            Some(resp) => {
                assert!(
                    resp.files.is_empty(),
                    "expected no file paths extracted from URL text, got: {:?}",
                    resp.files
                );
            }
        }
    }

    // Regression: issue #3197 — CLI arg-parser diagnostics were being synthesized as
    // agent responses. When a process (e.g. codex) fails with an invalid CLI flag, it
    // exits non-zero with a clap/help diagnostic on stderr. This should be rejected
    // (return None) rather than synthesized as a "needs_review" response.
    #[test]
    fn synthesize_rejects_cli_parser_diagnostics_unexpected_argument() {
        // Real clap error from the bug report (codex exit 141)
        let text = "error: unexpected argument '--ask-for-approval' found\n  \
                    tip: to pass '--ask-for-approval' as a value, use '-- --ask-for-approval'\n\
                    Usage: codex exec [OPTIONS] [PROMPT]";
        assert!(
            synthesize_response_from_text(text).is_none(),
            "CLI parser diagnostic must not synthesize as agent response"
        );
    }

    #[test]
    fn synthesize_rejects_cli_parser_diagnostics_usage_line() {
        let text = "Usage: mycommand [OPTIONS] <arg>";
        assert!(
            synthesize_response_from_text(text).is_none(),
            "Usage line must not synthesize as agent response"
        );
    }

    #[test]
    fn synthesize_rejects_cli_parser_diagnostics_help_hint() {
        let text = "Error: unknown flag --foo\nFor more information, try '--help'";
        assert!(
            synthesize_response_from_text(text).is_none(),
            "Help hint must not synthesize as agent response"
        );
    }

    #[test]
    fn synthesize_rejects_cli_parser_diagnostics_tip() {
        let text = "error: bad value\ntip: to pass '--flag' as a value, use '-- --flag'";
        assert!(
            synthesize_response_from_text(text).is_none(),
            "Clap tip must not synthesize as agent response"
        );
    }

    #[test]
    fn synthesize_accepts_agent_error_messages_that_mention_error() {
        // Agent returning plain text that happens to contain "error:" but is an actual
        // agent message (e.g. summarizing what went wrong) should still synthesize.
        let text = "I tried to run the command but got an error: permission denied on that file.";
        let response =
            synthesize_response_from_text(text).expect("agent error message should synthesize");
        assert_eq!(response.status, "needs_review");
        assert!(response.error.is_some());
    }

    // Regression: Issue #3197 — exact clap error from bug report.
    // When codex fails with an invalid CLI flag (e.g., --ask-for-approval with old codex versions),
    // it should return None, not synthesize as "needs_review". Without this fix, the run
    // would be marked as success and advance to in_review, wasting resources.
    #[test]
    fn synthesize_rejects_exact_codex_cli_error() {
        let stderr_from_codex = "error: unexpected argument '--ask-for-approval' found\n  \
                                 tip: to pass '--ask-for-approval' as a value, use '-- --ask-for-approval'\n\
                                 Usage: codex exec [OPTIONS] [PROMPT]";
        assert!(
            synthesize_response_from_text(stderr_from_codex).is_none(),
            "exact codex CLI error from #3197 must not synthesize as agent response"
        );
    }
}
