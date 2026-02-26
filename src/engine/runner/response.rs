//! Response collection + error classification.
//!
//! After the agent finishes (tmux session ends), this module:
//! 1. Reads the output file
//! 2. Parses the response JSON
//! 3. Classifies errors (timeout, usage limit, auth, tooling)
//! 4. Determines next action (success, reroute, needs_review)

use crate::parser::{self, AgentResponse};
use crate::sidecar;
use std::path::{Path, PathBuf};

/// Classification of agent execution result.
pub enum RunResult {
    /// Agent completed successfully with a parsed response.
    Success(AgentResponse),
    /// Agent timed out (exit 124).
    Timeout,
    /// Usage/rate limit hit — should reroute to different agent.
    UsageLimit(String),
    /// Auth/billing error — should try fallback agent.
    AuthError(String),
    /// Missing tooling — mark needs_review.
    MissingTooling(String),
    /// General failure — mark needs_review.
    Failed(String),
}

/// Collect and classify the agent's response.
pub fn collect_response(task_id: &str, exit_code: i32, output_file: &Path) -> RunResult {
    let state_dir = crate::home::state_dir().unwrap_or_default();

    // Read stderr
    let stderr_path = state_dir.join(format!("stderr-{task_id}.txt"));
    let stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();

    // Read response from output file
    let response_content = read_output_file(task_id, output_file);

    let combined = format!("{response_content}{stderr}");

    // Check exit code first
    if exit_code != 0 {
        // Check for missing tooling
        if let Some(tool) = detect_missing_tooling(&combined) {
            return RunResult::MissingTooling(format!("missing tool: {tool}"));
        }

        // Timeout (exit 124)
        if exit_code == 124 {
            return RunResult::Timeout;
        }

        // Usage/rate limit
        if is_usage_limit_error(&combined) {
            return RunResult::UsageLimit(snippet(&combined));
        }

        // Auth/billing error
        if is_auth_error(&combined) {
            return RunResult::AuthError(snippet(&combined));
        }

        // General failure
        return RunResult::Failed(format!("agent exited with code {exit_code}"));
    }

    // Exit 0 — try to parse response
    if response_content.is_empty() {
        // Empty response might be usage limit
        if is_usage_limit_error(&stderr) {
            return RunResult::UsageLimit(snippet(&stderr));
        }
        if is_auth_error(&stderr) {
            return RunResult::AuthError(snippet(&stderr));
        }
        return RunResult::Failed("empty agent response".to_string());
    }

    // Parse the response
    match parser::parse(&response_content) {
        Ok(resp) => {
            // Check if agent self-reported usage limit
            if matches!(resp.status.as_str(), "needs_review" | "blocked") {
                if let Some(ref reason) = resp.error {
                    if is_usage_limit_error(reason) {
                        return RunResult::UsageLimit(snippet(reason));
                    }
                }
            }

            // Check for missing tooling in response
            let check_text = format!(
                "{}{}{}",
                resp.error.as_deref().unwrap_or(""),
                resp.summary,
                resp.remaining.join(" "),
            );
            if let Some(tool) = detect_missing_tooling(&check_text) {
                return RunResult::MissingTooling(format!("missing tool: {tool}"));
            }

            RunResult::Success(resp)
        }
        Err(_) => {
            // Failed to parse — check for known error patterns
            if is_usage_limit_error(&combined) {
                return RunResult::UsageLimit(snippet(&combined));
            }
            if is_auth_error(&combined) {
                return RunResult::AuthError(snippet(&combined));
            }
            RunResult::Failed("invalid agent response JSON".to_string())
        }
    }
}

/// Read the agent's output file, trying multiple locations.
fn read_output_file(task_id: &str, primary_path: &Path) -> String {
    // Primary: explicit output file
    if let Ok(content) = std::fs::read_to_string(primary_path) {
        if !content.is_empty() {
            return content;
        }
    }

    // Fallback locations
    let state_dir = crate::home::state_dir().unwrap_or_default();

    let fallbacks = [
        PathBuf::from(format!("/tmp/output-{task_id}.json")),
        state_dir.join(format!("output-{task_id}.json")),
    ];

    for path in &fallbacks {
        if let Ok(content) = std::fs::read_to_string(path) {
            if !content.is_empty() {
                tracing::info!(task_id, path = %path.display(), "read output from fallback");
                return content;
            }
        }
    }

    String::new()
}

/// Check if output indicates a usage/rate limit error.
fn is_usage_limit_error(text: &str) -> bool {
    let lower = text.to_lowercase();
    let patterns = [
        "rate limit",
        "rate_limit",
        "ratelimit",
        "too many requests",
        "429",
        "usage limit",
        "quota exceeded",
        "overloaded",
        "capacity",
        "throttled",
        "credit balance too low",
        "insufficient_quota",
        "tokens_exceeded",
        "context_length_exceeded",
    ];
    patterns.iter().any(|p| lower.contains(p))
}

/// Check if output indicates an auth/billing error.
fn is_auth_error(text: &str) -> bool {
    let lower = text.to_lowercase();
    let patterns = [
        "unauthorized",
        "invalid api",
        "invalid key",
        "invalid token",
        "auth fail",
        "401",
        "403",
        "no api key",
        "no token",
        "expired key",
        "expired token",
        "expired plan",
        "billing",
        "insufficient credit",
        "payment required",
    ];
    patterns.iter().any(|p| lower.contains(p))
}

/// Detect missing tooling from agent output.
fn detect_missing_tooling(text: &str) -> Option<String> {
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
        // Check "command not found" patterns
        if lower.contains(&format!("{tool}: command not found"))
            || lower.contains(&format!("command not found: {tool}"))
            || lower.contains(&format!("{tool}: not found"))
            || lower.contains(&format!("env: {tool}: no such file"))
            || lower.contains(&format!("spawn {tool} enoent"))
        {
            return Some(tool.to_string());
        }
    }

    None
}

/// Pick a fallback agent, avoiding agents already in the reroute chain.
pub fn pick_fallback_agent(
    current_agent: &str,
    chain: &str,
    available_agents: &[String],
) -> Option<String> {
    let chain_set: std::collections::HashSet<&str> = if chain.is_empty() {
        std::collections::HashSet::new()
    } else {
        chain.split(',').collect()
    };

    for agent in available_agents {
        if agent != current_agent && !chain_set.contains(agent.as_str()) {
            return Some(agent.clone());
        }
    }

    None
}

/// Get a truncated snippet for error messages (last 300 chars).
fn snippet(text: &str) -> String {
    if text.len() > 300 {
        text[text.len() - 300..].to_string()
    } else {
        text.to_string()
    }
}

/// Get the reroute chain from sidecar.
pub fn get_reroute_chain(task_id: &str) -> String {
    sidecar::get(task_id, "limit_reroute_chain")
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Update the reroute chain in sidecar.
#[allow(dead_code)]
pub fn update_reroute_chain(task_id: &str, current_agent: &str, existing_chain: &str) -> String {
    let mut chain = existing_chain.to_string();
    if chain.is_empty() {
        chain = current_agent.to_string();
    } else if !chain.split(',').any(|a| a == current_agent) {
        chain = format!("{chain},{current_agent}");
    }

    sidecar::set(task_id, &[format!("limit_reroute_chain={chain}")]).ok();
    chain
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_usage_limit_detects_rate_limit() {
        assert!(is_usage_limit_error("Error: rate limit exceeded"));
        assert!(is_usage_limit_error("HTTP 429 Too Many Requests"));
        assert!(is_usage_limit_error("quota exceeded for model"));
        assert!(is_usage_limit_error("context_length_exceeded"));
    }

    #[test]
    fn is_usage_limit_rejects_normal_text() {
        assert!(!is_usage_limit_error("task completed successfully"));
        assert!(!is_usage_limit_error(""));
    }

    #[test]
    fn is_auth_error_detects_common_patterns() {
        assert!(is_auth_error("401 Unauthorized"));
        assert!(is_auth_error("invalid api key provided"));
        assert!(is_auth_error("Your billing plan has expired"));
        assert!(is_auth_error("Error: 403 Forbidden"));
    }

    #[test]
    fn is_auth_error_rejects_normal_text() {
        assert!(!is_auth_error("task completed successfully"));
        assert!(!is_auth_error(""));
    }

    #[test]
    fn detect_missing_tooling_finds_known_tools() {
        assert_eq!(
            detect_missing_tooling("bun: command not found"),
            Some("bun".to_string())
        );
        assert_eq!(
            detect_missing_tooling("env: anchor: no such file"),
            Some("anchor".to_string())
        );
        assert_eq!(
            detect_missing_tooling("spawn docker enoent"),
            Some("docker".to_string())
        );
    }

    #[test]
    fn detect_missing_tooling_returns_none_for_normal() {
        assert!(detect_missing_tooling("everything works fine").is_none());
        assert!(detect_missing_tooling("").is_none());
    }

    #[test]
    fn pick_fallback_skips_current_agent() {
        let available = vec!["claude".to_string(), "codex".to_string()];
        let result = pick_fallback_agent("claude", "", &available);
        assert_eq!(result, Some("codex".to_string()));
    }

    #[test]
    fn pick_fallback_skips_chain_agents() {
        let available = vec![
            "claude".to_string(),
            "codex".to_string(),
            "opencode".to_string(),
        ];
        let result = pick_fallback_agent("claude", "claude,codex", &available);
        assert_eq!(result, Some("opencode".to_string()));
    }

    #[test]
    fn pick_fallback_returns_none_when_exhausted() {
        let available = vec!["claude".to_string(), "codex".to_string()];
        let result = pick_fallback_agent("claude", "claude,codex", &available);
        assert!(result.is_none());
    }

    #[test]
    fn snippet_truncates_long_text() {
        let long = "x".repeat(500);
        let s = snippet(&long);
        assert_eq!(s.len(), 300);
    }

    #[test]
    fn snippet_preserves_short_text() {
        let short = "hello";
        assert_eq!(snippet(short), "hello");
    }
}
