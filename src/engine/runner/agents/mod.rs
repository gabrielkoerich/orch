//! Per-agent runner trait, error types, and agent registry.
//!
//! Each agent (Claude, Codex, OpenCode) has a different CLI invocation,
//! output format, and error pattern. This module defines a common trait
//! so the runner can delegate parsing and error classification to the
//! correct implementation.

pub mod claude;
pub mod codex;
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
}

/// Sandbox level — how much filesystem access the agent gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxLevel {
    /// Agent can only write within the workspace/worktree.
    WorkspaceWrite,
    /// Agent has full filesystem access (dangerous).
    FullAccess,
    /// No sandboxing (orchestrator manages isolation externally).
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
    /// Agent returned unparseable output.
    InvalidResponse { raw: String },
    /// Agent self-reported a failure in its response.
    AgentFailed { message: String },
    /// Transient network connectivity error — retry same agent, no failover.
    NetworkError { message: String },
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
            Self::InvalidResponse { raw } => {
                let end = truncate_at_char_boundary(raw, 200);
                write!(f, "invalid response: {}", &raw[..end])
            }
            Self::AgentFailed { message } => write!(f, "agent failed: {message}"),
            Self::NetworkError { message } => write!(f, "network error: {message}"),
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
        AgentError::InvalidResponse { .. } => "InvalidResponse",
        AgentError::AgentFailed { .. } => "AgentFailed",
        AgentError::NetworkError { .. } => "NetworkError",
        AgentError::Unknown { .. } => "Unknown",
    }
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

    let looks_done = !looks_negative
        && ([
            "no changes",
            "nothing to",
            "nothing to do",
            "nothing to execute",
            "no positions",
            "no open positions",
            "no trades",
            "no trade",
            "no action needed",
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
            "the commit",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
            || contains_word(&lower, "done")
            || contains_word(&lower, "committed"));

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
    } else {
        "needs_review"
    };

    let summary_end = truncate_at_char_boundary(trimmed, 500);
    let summary = trimmed[..summary_end].to_string();
    let error =
        (status == "needs_review").then(|| "agent returned plain text instead of JSON".to_string());

    Some(AgentResponse {
        status: status.to_string(),
        summary,
        accomplished: vec![],
        remaining: vec![],
        files: vec![],
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
fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> usize {
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
    fn extract_text(&self, raw: &str) -> Result<String, AgentError> {
        Ok(raw.to_string())
    }

    /// Classify an error from exit code + stdout + stderr into an AgentError.
    ///
    /// Called when the agent process exits with a non-zero code, or when
    /// parse_response fails to find a valid result.
    fn classify_error(&self, exit_code: i32, stdout: &str, stderr: &str) -> AgentError;

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
    match agent_name {
        "claude" | "kimi" | "minimax" => Box::new(claude::ClaudeRunner::new(agent_name)),
        "codex" => Box::new(codex::CodexRunner),
        "opencode" => Box::new(opencode::OpenCodeRunner::new()),
        // Unknown agents fall back to Claude-compatible parsing
        other => {
            tracing::warn!(
                agent = other,
                "unknown agent, using claude-compatible runner"
            );
            Box::new(claude::ClaudeRunner::new(other))
        }
    }
}

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
            "too many requests",
            "usage limit",
            "quota exceeded",
            "overloaded",
            "capacity",
            "throttled",
            "insufficient_quota",
            "tokens_exceeded",
            "you've hit your usage limit",
            "529",
        ];
        // Find the earliest match position so we can extract context around the
        // actual error message rather than the tail (which may be unrelated JSON).
        let match_pos = patterns.iter().filter_map(|p| lower.find(p)).min();
        let has_429 = lower.contains("429");
        if match_pos.is_some() || has_429 {
            let message = if let Some(pos) = match_pos {
                extract_context_around(text, pos, 300)
            } else {
                safe_tail(text, 300)
            };
            return Some(AgentError::RateLimit { message });
        }
        None
    }

    /// Extract up to `window` bytes of context centred around `byte_pos`.
    ///
    /// Because `byte_pos` is derived from a lowercased copy, it is exact for
    /// ASCII patterns (the vast majority of agent output).  For non-ASCII edge
    /// cases the window may shift slightly but remains far more useful than
    /// `safe_tail`.
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
    pub fn detect_auth_error(text: &str) -> Option<AgentError> {
        let lower = text.to_lowercase();
        let patterns = [
            "unauthorized",
            "invalid api",
            "invalid key",
            "invalid token",
            "auth fail",
            "no api key",
            "no token",
            "expired key",
            "expired token",
            "expired plan",
            "billing",
            "insufficient credit",
            "credit balance too low",
            "payment required",
        ];
        let http_401 = contains_http_status(&lower, "401");
        let http_403 = contains_http_status(&lower, "403");
        if patterns.iter().any(|p| lower.contains(p)) || http_401 || http_403 {
            return Some(AgentError::Auth {
                message: safe_tail(text, 300),
            });
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
        ];
        if patterns.iter().any(|p| lower.contains(p)) {
            return Some(AgentError::NetworkError {
                message: safe_tail(text, 300),
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
                message: safe_tail(text, 300),
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
        ];
        if patterns.iter().any(|p| lower.contains(p)) {
            return Some(AgentError::ContextOverflow {
                message: text.to_string(),
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
                message: text.to_string(),
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
            "authentication required",
        ];
        if patterns.iter().any(|p| lower.contains(p)) {
            return Some(AgentError::WaitingForInput {
                message: text.to_string(),
            });
        }
        None
    }

    /// Exit code returned by `timeout(1)` when the child is killed.
    const TIMEOUT_EXIT_CODE: i32 = 124;

    /// Default assumed timeout duration when we only have the exit code.
    const DEFAULT_TIMEOUT_SECS: u64 = 1800;

    /// Run all pattern detectors against combined stdout+stderr.
    /// Returns the first matching AgentError, or a generic Unknown.
    pub fn classify_from_text(exit_code: i32, text: &str) -> AgentError {
        if exit_code == TIMEOUT_EXIT_CODE {
            return AgentError::Timeout {
                elapsed: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            };
        }

        if let Some(e) = detect_missing_tool(text) {
            return e;
        }
        if let Some(e) = detect_waiting_for_input(text) {
            return e;
        }
        if let Some(e) = detect_permission_denied(text) {
            return e;
        }
        if let Some(e) = detect_worktree_missing(text) {
            return e;
        }
        if let Some(e) = detect_context_overflow(text) {
            return e;
        }
        if let Some(e) = detect_rate_limit(text) {
            return e;
        }
        if let Some(e) = detect_network_error(text) {
            return e;
        }
        if let Some(e) = detect_auth_error(text) {
            return e;
        }

        AgentError::Unknown {
            exit_code,
            message: safe_tail(text, 300),
        }
    }

    /// Safely extract the last `max_bytes` of a string, respecting UTF-8 boundaries.
    fn safe_tail(text: &str, max_bytes: usize) -> String {
        if text.len() <= max_bytes {
            return text.to_string();
        }
        let start = text.len() - max_bytes;
        // Walk forward to find a char boundary
        let mut idx = start;
        while idx < text.len() && !text.is_char_boundary(idx) {
            idx += 1;
        }
        text[idx..].to_string()
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
        // Unknown falls back to claude-compatible
        assert_eq!(get_runner("unknown-agent").name(), "unknown-agent");
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
    fn pattern_detect_auth() {
        assert!(patterns::detect_auth_error("401 Unauthorized").is_some());
        assert!(patterns::detect_auth_error("HTTP 401").is_some());
        assert!(patterns::detect_auth_error("error: 401").is_some());
        assert!(patterns::detect_auth_error("403 Forbidden").is_some());
        assert!(patterns::detect_auth_error("HTTP 403").is_some());
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
    fn pattern_detect_network_error() {
        assert!(patterns::detect_network_error("connection refused").is_some());
        assert!(patterns::detect_network_error("ConnectionRefused").is_some());
        assert!(patterns::detect_network_error("Unable to connect to API").is_some());
        assert!(patterns::detect_network_error("ECONNREFUSED").is_some());
        assert!(patterns::detect_network_error("network unreachable").is_some());
        assert!(patterns::detect_network_error("all systems operational").is_none());
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
        assert!(patterns::detect_waiting_for_input("done").is_none());
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
    fn synthesize_response_marks_done_for_commit_created() {
        let response = synthesize_response_from_text(
            "The commit is created locally. The push is being blocked by permissions.",
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

        // Codex: should have --full-auto (autonomous + workspace-write)
        let codex = get_runner("codex");
        let cmd = codex.build_command(None, "", sys, msg, &perms);
        assert!(
            cmd.contains("--full-auto"),
            "codex default: expected --full-auto, got: {cmd}"
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

        // Codex: supervised → suggest
        let codex = get_runner("codex");
        let cmd = codex.build_command(None, "", sys, msg, &perms);
        assert!(
            cmd.contains("--ask-for-approval suggest"),
            "codex supervised: expected suggest, got: {cmd}"
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

    /// Test SandboxLevel::None with autonomous falls back to --full-auto for Codex.
    #[test]
    fn codex_sandbox_none_defaults_to_full_auto() {
        let perms = PermissionRules {
            autonomous: true,
            sandbox: SandboxLevel::None,
            disallowed_tools: vec![],
            allowed_tools: vec![],
            allowed_edit_paths: vec![],
        };
        let codex = get_runner("codex");
        let cmd = codex.build_command(None, "", "/tmp/sys.md", "/tmp/msg.md", &perms);
        assert!(
            cmd.contains("--full-auto"),
            "codex sandbox::none + autonomous: should use --full-auto, got: {cmd}"
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
            // autonomous codex uses --full-auto (sandbox::WorkspaceWrite default)
            assert!(
                cmd.contains("--full-auto"),
                "autonomous config → --full-auto, got: {cmd}"
            );
        } else {
            assert!(
                cmd.contains("--ask-for-approval suggest"),
                "supervised config → --ask-for-approval suggest, got: {cmd}"
            );
        }
    }
}
