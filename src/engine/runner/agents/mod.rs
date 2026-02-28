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
    /// Paths the agent must not access (e.g., main project dir).
    pub blocked_paths: Vec<PathBuf>,
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
            disallowed_tools: vec!["Bash(rm *)".to_string(), "Bash(rm -*)".to_string()],
            blocked_paths: vec![],
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
    /// Permission denials encountered during execution.
    pub permission_denials: Vec<String>,
}

/// Agent-specific error with enough detail for autonomous recovery.
#[derive(Debug, Clone)]
pub enum AgentError {
    /// Rate/usage limit — reroute to different agent, cooldown current.
    RateLimit {
        message: String,
        retry_after: Option<Duration>,
    },
    /// Auth/billing/API key error — switch agent entirely.
    Auth { message: String },
    /// Requested model not available — try different model, then switch agent.
    ModelUnavailable { message: String, model: String },
    /// Context window exceeded — truncate and retry, then switch agent.
    ContextOverflow {
        message: String,
        max_tokens: Option<u64>,
    },
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
        AgentError::Unknown { .. } => "Unknown",
    }
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
    /// Agent name (e.g., "claude", "codex", "opencode").
    fn name(&self) -> &str;

    /// Check if this agent's binary is available on the system.
    fn is_available(&self) -> bool;

    /// Build the CLI command string for the runner shell script.
    ///
    /// The returned string is substituted into the bash runner script
    /// as the `RESPONSE=$(...)` command. The `permissions` struct carries
    /// unified rules that each agent translates to its own CLI flags.
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
        if patterns.iter().any(|p| lower.contains(p)) || lower.contains("429") {
            return Some(AgentError::RateLimit {
                message: safe_tail(text, 300),
                retry_after: None,
            });
        }
        None
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
        if patterns.iter().any(|p| lower.contains(p))
            || lower.contains("401")
            || lower.contains("403")
        {
            return Some(AgentError::Auth {
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
                max_tokens: None,
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
        if let Some(e) = detect_context_overflow(text) {
            return e;
        }
        if let Some(e) = detect_rate_limit(text) {
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
            retry_after: None,
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
    fn pattern_detect_auth() {
        assert!(patterns::detect_auth_error("401 Unauthorized").is_some());
        assert!(patterns::detect_auth_error("invalid api key").is_some());
        assert!(patterns::detect_auth_error("billing expired").is_some());
        assert!(patterns::detect_auth_error("task done").is_none());
    }

    #[test]
    fn pattern_detect_context_overflow() {
        assert!(patterns::detect_context_overflow("context_length_exceeded").is_some());
        assert!(patterns::detect_context_overflow("too many tokens in prompt").is_some());
        assert!(patterns::detect_context_overflow("success").is_none());
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
    fn permission_rules_default_no_blocked_paths() {
        let rules = PermissionRules::default();
        assert!(rules.blocked_paths.is_empty());
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

        // Codex: should have approval=never and sandbox=workspace-write
        let codex = get_runner("codex");
        let cmd = codex.build_command(None, "", sys, msg, &perms);
        assert!(
            cmd.contains("--ask-for-approval never"),
            "codex default: expected approval never, got: {cmd}"
        );
        assert!(
            cmd.contains("--sandbox workspace-write"),
            "codex default: expected workspace-write sandbox, got: {cmd}"
        );

        // OpenCode: should have no permission flags (relies on worktree isolation)
        let opencode = get_runner("opencode");
        let cmd = opencode.build_command(None, "", sys, msg, &perms);
        assert!(
            !cmd.contains("--permission"),
            "opencode default: should have no permission flags, got: {cmd}"
        );
        assert!(
            !cmd.contains("--sandbox"),
            "opencode default: should have no sandbox flags, got: {cmd}"
        );
    }

    /// Test supervised mode translates correctly for each agent.
    #[test]
    fn all_agents_handle_supervised_mode() {
        let perms = PermissionRules {
            autonomous: false,
            sandbox: SandboxLevel::WorkspaceWrite,
            disallowed_tools: vec![],
            blocked_paths: vec![],
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
            blocked_paths: vec![],
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

        // Codex: full access → danger-full-access
        let codex = get_runner("codex");
        let cmd = codex.build_command(None, "", sys, msg, &perms);
        assert!(
            cmd.contains("--sandbox danger-full-access"),
            "codex full_access: expected danger-full-access, got: {cmd}"
        );
    }

    /// Test SandboxLevel::None falls back to safe default for Codex.
    #[test]
    fn codex_sandbox_none_defaults_to_workspace_write() {
        let perms = PermissionRules {
            autonomous: true,
            sandbox: SandboxLevel::None,
            disallowed_tools: vec![],
            blocked_paths: vec![],
        };
        let codex = get_runner("codex");
        let cmd = codex.build_command(None, "", "/tmp/sys.md", "/tmp/msg.md", &perms);
        assert!(
            cmd.contains("--sandbox workspace-write"),
            "codex sandbox::none: should default to workspace-write, got: {cmd}"
        );
    }

    /// Test blocked paths are translated to Claude disallowed tools.
    #[test]
    fn claude_blocked_paths_become_disallowed_tools() {
        let perms = PermissionRules {
            autonomous: true,
            sandbox: SandboxLevel::WorkspaceWrite,
            disallowed_tools: vec![],
            blocked_paths: vec![
                PathBuf::from("/home/user/project"),
                PathBuf::from("/opt/other"),
            ],
        };
        let claude = get_runner("claude");
        let cmd = claude.build_command(None, "", "/tmp/sys.md", "/tmp/msg.md", &perms);

        // Each blocked path should generate 4 disallowed tool patterns
        assert!(
            cmd.contains("Bash(cd /home/user/project*)"),
            "missing Bash(cd) for first path"
        );
        assert!(
            cmd.contains("Read(/home/user/project/*)"),
            "missing Read for first path"
        );
        assert!(
            cmd.contains("Write(/home/user/project/*)"),
            "missing Write for first path"
        );
        assert!(
            cmd.contains("Edit(/home/user/project/*)"),
            "missing Edit for first path"
        );
        assert!(
            cmd.contains("Bash(cd /opt/other*)"),
            "missing Bash(cd) for second path"
        );
        assert!(
            cmd.contains("Read(/opt/other/*)"),
            "missing Read for second path"
        );
    }

    /// Test that blocked paths don't affect Codex (it uses sandbox isolation).
    #[test]
    fn codex_ignores_blocked_paths() {
        let perms = PermissionRules {
            autonomous: true,
            sandbox: SandboxLevel::WorkspaceWrite,
            disallowed_tools: vec![],
            blocked_paths: vec![PathBuf::from("/home/user/project")],
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
    fn opencode_ignores_blocked_paths() {
        let perms = PermissionRules {
            autonomous: true,
            sandbox: SandboxLevel::WorkspaceWrite,
            disallowed_tools: vec![],
            blocked_paths: vec![PathBuf::from("/home/user/project")],
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
            blocked_paths: vec![],
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
            blocked_paths: vec![],
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
            blocked_paths: vec![],
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
            blocked_paths: vec![PathBuf::from("/blocked")],
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
            assert!(
                cmd.contains("Read(/blocked/*)"),
                "{agent}: expected blocked path Read"
            );
        }
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
            assert!(cmd.contains("--ask-for-approval never"));
        } else {
            assert!(cmd.contains("--ask-for-approval suggest"));
        }
    }
}
