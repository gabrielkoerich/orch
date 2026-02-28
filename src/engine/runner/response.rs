//! Response collection + error classification.
//!
//! After the agent finishes (tmux session ends), this module:
//! 1. Reads the output file
//! 2. Parses the response JSON
//! 3. Classifies errors (timeout, usage limit, auth, tooling)
//! 4. Determines next action (success, reroute, needs_review)

use crate::engine::runner::agents::patterns;
use crate::parser::{self, AgentResponse};
use crate::sidecar;
use fs2::FileExt; // for try_lock_exclusive / unlock
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

/// Classification of agent execution result (legacy, used in tests).
///
/// This enum is kept for backwards compatibility with older call sites and
/// unit tests that inspect detailed execution outcomes. Newer code prefers
/// `WeightSignal` and the parsing helpers above, but removing this enum would
/// require updating multiple modules and tests, so it remains intentionally
/// available.
#[allow(dead_code)]
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

/// Outcome signal for the engine to update router weights.
///
/// Returned by the task runner so the engine can feed rate limit and
/// success signals back to the router's weighted round-robin implementation.
#[derive(Debug, Clone)]
pub enum WeightSignal {
    /// Agent completed a task successfully.
    Success { agent: String },
    /// Agent hit a rate limit / usage limit.
    RateLimited { agent: String },
    /// No weight-relevant signal (timeout, auth error, etc.)
    None,
}

/// Collect and classify the agent's response (legacy, kept for backward compat).
#[allow(dead_code)]
pub fn collect_response(task_id: &str, exit_code: i32, output_file: &Path) -> RunResult {
    // Read stderr (check new state dir, fall back to legacy)
    let stderr_path = sidecar::state_file(&format!("stderr-{task_id}.txt"))
        .unwrap_or_else(|_| PathBuf::from(format!("/tmp/stderr-{task_id}.txt")));
    let stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();

    // Read response from output file (legacy path — no repo context)
    let response_content = read_output_file(task_id, output_file, "");

    let combined = format!("{response_content}{stderr}");

    // Check exit code first
    if exit_code != 0 {
        // Check for missing tooling
        if let Some(crate::engine::runner::agents::AgentError::MissingTool { tool }) =
            patterns::detect_missing_tool(&combined)
        {
            return RunResult::MissingTooling(format!("missing tool: {tool}"));
        }

        // Timeout (exit 124)
        if exit_code == 124 {
            return RunResult::Timeout;
        }

        // Usage/rate limit
        if patterns::detect_rate_limit(&combined).is_some() {
            return RunResult::UsageLimit(snippet(&combined));
        }

        // Auth/billing error
        if patterns::detect_auth_error(&combined).is_some() {
            return RunResult::AuthError(snippet(&combined));
        }

        // General failure
        return RunResult::Failed(format!("agent exited with code {exit_code}"));
    }

    // Exit 0 — try to parse response
    if response_content.is_empty() {
        // Empty response might be usage limit
        if patterns::detect_rate_limit(&stderr).is_some() {
            return RunResult::UsageLimit(snippet(&stderr));
        }
        if patterns::detect_auth_error(&stderr).is_some() {
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
                    if patterns::detect_rate_limit(reason).is_some() {
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
            if let Some(crate::engine::runner::agents::AgentError::MissingTool { tool }) =
                patterns::detect_missing_tool(&check_text)
            {
                return RunResult::MissingTooling(format!("missing tool: {tool}"));
            }

            RunResult::Success(resp)
        }
        Err(_) => {
            // Failed to parse — check for known error patterns
            if patterns::detect_rate_limit(&combined).is_some() {
                return RunResult::UsageLimit(snippet(&combined));
            }
            if patterns::detect_auth_error(&combined).is_some() {
                return RunResult::AuthError(snippet(&combined));
            }
            RunResult::Failed("invalid agent response JSON".to_string())
        }
    }
}

/// Read the agent's output file, trying multiple locations.
///
/// Checks per-task attempt directory first, then legacy flat paths.
pub fn read_output_file(task_id: &str, primary_path: &Path, repo: &str) -> String {
    // Primary: explicit output file (already points to attempt dir)
    if let Ok(content) = std::fs::read_to_string(primary_path) {
        if !content.is_empty() {
            return content;
        }
    }

    // Fallback: check all attempt dirs for this task (newest first)
    if let Ok(task_dir) = crate::home::task_dir(repo, task_id) {
        let attempts_dir = task_dir.join("attempts");
        if attempts_dir.is_dir() {
            let mut attempt_nums: Vec<u32> = std::fs::read_dir(&attempts_dir)
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse().ok()))
                .collect();
            attempt_nums.sort_unstable_by(|a, b| b.cmp(a)); // newest first
            for n in attempt_nums {
                let p = attempts_dir.join(n.to_string()).join("output.json");
                if let Ok(content) = std::fs::read_to_string(&p) {
                    if !content.is_empty() {
                        tracing::info!(task_id, path = %p.display(), "read output from attempt dir");
                        return content;
                    }
                }
            }
        }
    }

    // Legacy fallback locations
    let state_dir = sidecar::state_dir().unwrap_or_else(|_| PathBuf::from("/tmp"));

    let mut fallbacks = vec![
        PathBuf::from(format!("/tmp/output-{task_id}.json")),
        state_dir.join(format!("output-{task_id}.json")),
    ];

    if let Ok(legacy_path) = sidecar::state_file(&format!("output-{task_id}.json")) {
        if !fallbacks.contains(&legacy_path) {
            fallbacks.push(legacy_path);
        }
    }

    for path in &fallbacks {
        if let Ok(content) = std::fs::read_to_string(path) {
            if !content.is_empty() {
                tracing::info!(task_id, path = %path.display(), "read output from legacy fallback");
                return content;
            }
        }
    }

    String::new()
}

/// Cooldown duration for failed agents (30 minutes).
const AGENT_COOLDOWN_SECS: i64 = 30 * 60;

/// Cooldown duration for model-specific failures (1 hour).
/// When a specific agent+model combo fails (e.g., model not available,
/// model-specific rate limit), we ban that combo for longer.
const MODEL_COOLDOWN_SECS: i64 = 60 * 60;

/// Path to the agent cooldowns file.
fn cooldowns_path() -> std::path::PathBuf {
    crate::sidecar::state_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp"))
        .join("agent_cooldowns.json")
}

// Expose lockable file operations to tests or other modules if needed

/// Record that an agent has failed and should be temporarily avoided.
pub fn record_agent_failure(agent_name: &str) {
    record_failure_with_reason(agent_name, "agent_error");
}

/// Record that a specific agent+model combo has failed.
///
/// The cooldown key is `"agent:model"` so we can track model-specific
/// failures separately (e.g., codex with o3-mini fails but gpt-4o works).
pub fn record_model_failure(agent_name: &str, model: &str) {
    let key = format!("{agent_name}:{model}");
    record_failure_with_reason(&key, "model_error");
}

/// Check if a specific agent+model combo is in cooldown.
pub fn is_model_in_cooldown(agent_name: &str, model: &str) -> bool {
    let key = format!("{agent_name}:{model}");
    is_key_in_cooldown(&key, MODEL_COOLDOWN_SECS)
}

/// Check if an agent is currently in cooldown period.
pub fn is_agent_in_cooldown(agent_name: &str) -> bool {
    is_key_in_cooldown(agent_name, AGENT_COOLDOWN_SECS)
}

/// Shared helper: check if a given key is in cooldown within `max_age_secs`.
fn is_key_in_cooldown(key: &str, max_age_secs: i64) -> bool {
    let cooldowns = read_cooldowns_file();
    if let Some(entry) = cooldowns.get(key) {
        if let Some(failed_at) = entry.get("failed_at").and_then(|v| v.as_i64()) {
            let now = chrono::Utc::now().timestamp();
            return (now - failed_at) < max_age_secs;
        }
    }
    false
}

/// Read and parse the cooldowns JSON file. Returns empty map on any error.
fn read_cooldowns_file() -> serde_json::Map<String, serde_json::Value> {
    let path = cooldowns_path();
    if !path.exists() {
        return serde_json::Map::new();
    }
    // Read without locking; callers use locking where needed
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    serde_json::from_str(&content).unwrap_or_default()
}

fn record_failure_with_reason(key: &str, reason: &str) {
    let path = cooldowns_path();
    // Ensure the directory exists
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // Use an advisory file lock to guard read-modify-write across processes.
    // fs2 provides a simple cross-platform lock via File::try_lock_exclusive().
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
    {
        // Use blocking lock to avoid race conditions (try_lock + fallback is unsafe)
        if f.lock_exclusive().is_ok() {
            let mut cooldowns = serde_json::Map::new();
            // Read existing content
            let mut content = String::new();
            if f.read_to_string(&mut content).is_ok() && !content.is_empty() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                    if let Some(map) = v.as_object() {
                        cooldowns = map.clone();
                    }
                }
            }

            // Record failure with current timestamp
            let timestamp = chrono::Utc::now().timestamp();
            cooldowns.insert(
                key.to_string(),
                serde_json::json!({ "failed_at": timestamp, "reason": reason }),
            );

            if let Ok(content) = serde_json::to_string_pretty(&serde_json::Value::Object(cooldowns))
            {
                // Truncate then write
                let _ = f.set_len(0);
                let _ = f.rewind();
                let _ = f.write_all(content.as_bytes());
            }

            let _ = f.unlock();
        } else {
            tracing::warn!("could not acquire lock on cooldowns file, skipping write");
        }
    } else {
        // Can't open file — write directly
        let mut cooldowns = read_cooldowns_file();
        let timestamp = chrono::Utc::now().timestamp();
        cooldowns.insert(
            key.to_string(),
            serde_json::json!({ "failed_at": timestamp, "reason": reason }),
        );
        if let Ok(content) = serde_json::to_string_pretty(&serde_json::Value::Object(cooldowns)) {
            std::fs::write(&path, content).ok();
        }
    }
}

/// Clear expired cooldowns from the file.
pub fn clear_expired_cooldowns() {
    let path = cooldowns_path();
    if !path.exists() {
        return;
    }

    // Use same file-locking strategy as record_failure_with_reason
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
    {
        // Use blocking lock to avoid race conditions
        if f.lock_exclusive().is_ok() {
            let mut content = String::new();
            if f.read_to_string(&mut content).is_ok() {
                let mut cooldowns = if !content.is_empty() {
                    serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&content)
                        .unwrap_or_default()
                } else {
                    serde_json::Map::new()
                };

                let now = chrono::Utc::now().timestamp();
                let mut to_remove = Vec::new();
                for (agent, entry) in &cooldowns {
                    if let Some(failed_at) = entry.get("failed_at").and_then(|v| v.as_i64()) {
                        if (now - failed_at) >= AGENT_COOLDOWN_SECS {
                            to_remove.push(agent.clone());
                        }
                    }
                }
                for agent in to_remove {
                    cooldowns.remove(&agent);
                }

                if let Ok(content) =
                    serde_json::to_string_pretty(&serde_json::Value::Object(cooldowns))
                {
                    let _ = f.set_len(0);
                    let _ = f.rewind();
                    let _ = f.write_all(content.as_bytes());
                }
            }

            let _ = f.unlock();
        } else {
            tracing::warn!("could not acquire lock on cooldowns file, skipping cleanup");
        }
    }
}

/// Pick a fallback agent, avoiding agents already in the reroute chain and agents in cooldown.
pub fn pick_fallback_agent(
    current_agent: &str,
    chain: &str,
    available_agents: &[String],
) -> Option<String> {
    // Clear expired cooldowns first
    clear_expired_cooldowns();

    let chain_set: std::collections::HashSet<&str> = if chain.is_empty() {
        std::collections::HashSet::new()
    } else {
        chain.split(',').collect()
    };

    for agent in available_agents {
        if agent != current_agent
            && !chain_set.contains(agent.as_str())
            && !is_agent_in_cooldown(agent)
        {
            return Some(agent.clone());
        }
    }

    None
}

/// Retryable error types that should trigger agent failover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryableError {
    /// Agent timed out.
    Timeout,
    /// Usage/rate limit hit.
    UsageLimit,
    /// Auth/billing error.
    AuthError,
    /// General failure (non-retryable but we still try fallback agents).
    Failed,
    /// Missing tooling - fallback might help if another agent has the tool.
    MissingTooling,
}

/// Handle failover for any retryable error type.
/// Returns true if the task was rerouted, false if it should be marked needs_review.
///
/// Note: DB recording of rate limit events is handled by the caller (mod.rs)
/// which has async context. This function only handles sidecar state + cooldowns.
pub fn handle_failover(
    task_id: &str,
    agent_name: &str,
    error_type: RetryableError,
    error_message: &str,
) -> bool {
    // Get the reroute chain
    let chain = get_reroute_chain(task_id);
    let chain = update_reroute_chain(task_id, agent_name, &chain);

    // Get all available agents
    let available: Vec<String> = ["claude", "codex", "opencode", "kimi", "minimax"]
        .iter()
        .filter(|a| crate::cmd_cache::command_exists(a))
        .map(|s| s.to_string())
        .collect();

    // Check if we've exhausted all agents (build chain_set once)
    let chain_set: std::collections::HashSet<&str> = if chain.is_empty() {
        std::collections::HashSet::new()
    } else {
        chain.split(',').collect()
    };
    let is_exhausted = available.iter().all(|a| chain_set.contains(a.as_str()));

    if is_exhausted {
        tracing::warn!(
            task_id,
            agent = agent_name,
            "all agents exhausted, marking needs_review"
        );
        if let Err(e) = sidecar::set(
            task_id,
            &[
                "status=needs_review".to_string(),
                format!("last_error={error_message} (all agents exhausted)"),
            ],
        ) {
            tracing::error!(task_id, ?e, "failed to update task status during failover");
        }
        return false;
    }

    // Pick a fallback agent
    if let Some(next) = pick_fallback_agent(agent_name, &chain, &available) {
        tracing::info!(
            task_id,
            from = agent_name,
            to = next,
            error_type = ?error_type,
            "failover: switching to fallback agent"
        );

        // Record agent failure for cooldown tracking
        // Skip cooldown for MissingTooling — it's permanent, not transient
        if !matches!(error_type, RetryableError::MissingTooling) {
            record_agent_failure(agent_name);
        }

        if let Err(e) = sidecar::set(
            task_id,
            &[
                format!("agent={next}"),
                "model=".to_string(),
                "status=new".to_string(),
                format!("last_error={error_message}, rerouted to {next}"),
            ],
        ) {
            tracing::error!(task_id, ?e, "failed to update task status during failover");
        }
        return true;
    }

    // No fallback available
    tracing::warn!(task_id, agent = agent_name, "no fallback agents available");
    if let Err(e) = sidecar::set(
        task_id,
        &[
            "status=needs_review".to_string(),
            format!("last_error={error_message}, no fallback agents"),
        ],
    ) {
        tracing::error!(task_id, ?e, "failed to update task status during failover");
    }
    false
}

/// Get a truncated snippet for error messages (last 300 chars, UTF-8 safe).
#[allow(dead_code)]
fn snippet(text: &str) -> String {
    if text.len() <= 300 {
        return text.to_string();
    }
    let start = text.len() - 300;
    // Walk forward to find a char boundary
    let mut idx = start;
    while idx < text.len() && !text.is_char_boundary(idx) {
        idx += 1;
    }
    text[idx..].to_string()
}

/// Get the reroute chain from sidecar.
pub fn get_reroute_chain(task_id: &str) -> String {
    sidecar::get(task_id, "limit_reroute_chain")
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Update the reroute chain in sidecar.
pub fn update_reroute_chain(task_id: &str, current_agent: &str, existing_chain: &str) -> String {
    let mut chain = existing_chain.to_string();
    if chain.is_empty() {
        chain = current_agent.to_string();
    } else if !chain.split(',').any(|a| a == current_agent) {
        chain = format!("{chain},{current_agent}");
    }

    if let Err(e) = sidecar::set(task_id, &[format!("limit_reroute_chain={chain}")]) {
        tracing::warn!(task_id, error = ?e, "failed to update reroute chain");
    }
    chain
}

/// Review response from the review agent.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ReviewResponse {
    /// Decision: "approve" | "request_changes"
    pub decision: String,
    /// Detailed review feedback.
    pub notes: String,
    /// Test results: "pass" | "fail" | "skipped"
    pub test_results: Option<String>,
    /// List of issues found.
    pub issues: Vec<ReviewIssue>,
}

/// A single issue found during review.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct ReviewIssue {
    /// File path.
    pub file: String,
    /// Line number (optional).
    pub line: Option<u32>,
    /// Severity: "error" | "warning"
    pub severity: String,
    /// Description of the issue.
    pub description: String,
}

/// Parse a review response from JSON.
pub fn parse_review_response(raw: &str) -> anyhow::Result<ReviewResponse> {
    // Extract result text from agent envelope (works for all agents)
    let text = unwrap_agent_result(raw);

    // Try direct JSON parse
    if let Ok(resp) = serde_json::from_str::<ReviewResponse>(&text) {
        return Ok(resp);
    }

    // Try extracting JSON from markdown code blocks
    if let Some(json_str) = extract_json_block(&text) {
        if let Ok(resp) = serde_json::from_str::<ReviewResponse>(&json_str) {
            return Ok(resp);
        }
    }

    anyhow::bail!("failed to parse review response")
}

/// Extract the result text from an agent output envelope.
///
/// All agents (claude, codex, opencode) wrap output in a JSON envelope
/// with a "result" field. This extracts that text. If the input isn't
/// an envelope, returns it as-is.
pub fn unwrap_agent_result(raw: &str) -> String {
    // Try single JSON object first (claude)
    if let Ok(envelope) = serde_json::from_str::<serde_json::Value>(raw) {
        match envelope.get("result") {
            Some(serde_json::Value::String(s)) => return s.clone(),
            Some(serde_json::Value::Object(obj)) => {
                return serde_json::to_string(obj).unwrap_or_else(|_| raw.to_string());
            }
            _ => {}
        }
    }
    // Try NDJSON: last line with a "result" field (codex, opencode)
    for line in raw.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) {
            match obj.get("result") {
                Some(serde_json::Value::String(s)) => return s.clone(),
                Some(serde_json::Value::Object(o)) => {
                    return serde_json::to_string(o).unwrap_or_else(|_| line.to_string());
                }
                _ => {}
            }
        }
    }
    raw.to_string()
}

/// Extract the first valid JSON object from markdown code blocks.
///
/// Searches for ```json blocks and returns the first one whose content
/// starts with `{`, skipping non-JSON code blocks.
fn extract_json_block(text: &str) -> Option<String> {
    let mut search_from = 0;
    while let Some(start) = text[search_from..].find("```json") {
        let abs_start = search_from + start;
        let after_tag = abs_start + "```json".len();
        let newline_pos = text[after_tag..].find('\n')?;
        let content_start = after_tag + newline_pos + 1;
        let end = text[content_start..]
            .find("```")
            .map(|e| content_start + e)?;
        let content = text[content_start..end].trim();
        if content.starts_with('{') {
            return Some(content.to_string());
        }
        search_from = end + 3;
    }
    None
}

/// Extract learnings and store as memory for future attempts.
pub fn store_learnings_from_response(
    task_id: &str,
    attempt: u32,
    agent: &str,
    model: Option<&str>,
    response: &crate::parser::AgentResponse,
    error: Option<&str>,
) {
    // Build the memory entry
    let entry = crate::sidecar::MemoryEntry {
        attempt,
        agent: agent.to_string(),
        model: model.map(String::from),
        learnings: response.learnings.clone(),
        error: error.map(String::from),
        files_modified: response.files.clone(),
        approach: response.summary.clone(),
        timestamp: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    };

    if let Err(e) = crate::sidecar::store_memory(task_id, &entry) {
        tracing::warn!(task_id, error = ?e, "failed to store memory");
    } else {
        tracing::debug!(task_id, attempt, "stored memory for attempt");
    }
}

/// Store a memory entry for a failed attempt (without a full AgentResponse).
pub fn store_failure_memory(
    task_id: &str,
    attempt: u32,
    agent: &str,
    model: Option<&str>,
    error: &str,
) {
    let entry = crate::sidecar::MemoryEntry {
        attempt,
        agent: agent.to_string(),
        model: model.map(String::from),
        learnings: vec![],
        error: Some(error.to_string()),
        files_modified: vec![],
        approach: String::new(),
        timestamp: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    };

    if let Err(e) = crate::sidecar::store_memory(task_id, &entry) {
        tracing::warn!(task_id, error = ?e, "failed to store failure memory");
    } else {
        tracing::debug!(task_id, attempt, "stored failure memory");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::runner::agents::AgentError;

    #[test]
    fn is_usage_limit_detects_rate_limit() {
        assert!(patterns::detect_rate_limit("Error: rate limit exceeded").is_some());
        assert!(patterns::detect_rate_limit("HTTP 429 Too Many Requests").is_some());
        assert!(patterns::detect_rate_limit("quota exceeded for model").is_some());
        // context_length_exceeded is handled by detect_context_overflow, not detect_rate_limit
        assert!(patterns::detect_context_overflow("context_length_exceeded").is_some());
    }

    #[test]
    fn is_usage_limit_rejects_normal_text() {
        assert!(patterns::detect_rate_limit("task completed successfully").is_none());
        assert!(patterns::detect_rate_limit("").is_none());
    }

    #[test]
    fn is_auth_error_detects_common_patterns() {
        assert!(patterns::detect_auth_error("401 Unauthorized").is_some());
        assert!(patterns::detect_auth_error("invalid api key provided").is_some());
        assert!(patterns::detect_auth_error("Your billing plan has expired").is_some());
        assert!(patterns::detect_auth_error("Error: 403 Forbidden").is_some());
    }

    #[test]
    fn is_auth_error_rejects_normal_text() {
        assert!(patterns::detect_auth_error("task completed successfully").is_none());
        assert!(patterns::detect_auth_error("").is_none());
    }

    #[test]
    fn detect_missing_tooling_finds_known_tools() {
        let result = patterns::detect_missing_tool("bun: command not found");
        assert!(result.is_some());
        if let AgentError::MissingTool { tool } = result.unwrap() {
            assert_eq!(tool, "bun");
        } else {
            panic!("expected MissingTool error");
        }

        let result = patterns::detect_missing_tool("env: anchor: no such file");
        assert!(result.is_some());
        if let AgentError::MissingTool { tool } = result.unwrap() {
            assert_eq!(tool, "anchor");
        } else {
            panic!("expected MissingTool error");
        }

        let result = patterns::detect_missing_tool("spawn docker enoent");
        assert!(result.is_some());
        if let AgentError::MissingTool { tool } = result.unwrap() {
            assert_eq!(tool, "docker");
        } else {
            panic!("expected MissingTool error");
        }
    }

    #[test]
    fn detect_missing_tooling_returns_none_for_normal() {
        assert!(patterns::detect_missing_tool("everything works fine").is_none());
        assert!(patterns::detect_missing_tool("").is_none());
    }

    // Use fake agent names so real cooldowns on disk don't affect tests.
    #[test]
    fn pick_fallback_skips_current_agent() {
        let available = vec!["test_agent_a".to_string(), "test_agent_b".to_string()];
        let result = pick_fallback_agent("test_agent_a", "", &available);
        assert_eq!(result, Some("test_agent_b".to_string()));
    }

    #[test]
    fn pick_fallback_skips_chain_agents() {
        let available = vec![
            "test_agent_a".to_string(),
            "test_agent_b".to_string(),
            "test_agent_c".to_string(),
        ];
        let result = pick_fallback_agent("test_agent_a", "test_agent_a,test_agent_b", &available);
        assert_eq!(result, Some("test_agent_c".to_string()));
    }

    #[test]
    fn pick_fallback_returns_none_when_exhausted() {
        let available = vec!["test_agent_a".to_string(), "test_agent_b".to_string()];
        let result = pick_fallback_agent("test_agent_a", "test_agent_a,test_agent_b", &available);
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

    #[test]
    fn weight_signal_variants() {
        let success = WeightSignal::Success {
            agent: "claude".to_string(),
        };
        let limited = WeightSignal::RateLimited {
            agent: "codex".to_string(),
        };
        let none = WeightSignal::None;

        // Verify Debug trait
        assert!(format!("{success:?}").contains("claude"));
        assert!(format!("{limited:?}").contains("codex"));
        assert!(format!("{none:?}").contains("None"));
    }

    #[test]
    fn parse_review_response_direct_json() {
        let json = r#"{"decision":"approve","notes":"LGTM","test_results":"pass","issues":[]}"#;
        let resp = parse_review_response(json).unwrap();
        assert_eq!(resp.decision, "approve");
        assert_eq!(resp.notes, "LGTM");
        assert!(resp.issues.is_empty());
    }

    #[test]
    fn parse_review_response_from_markdown() {
        let md = r#"Here is my review:

```json
{"decision":"request_changes","notes":"Fix the bug","issues":[{"file":"src/main.rs","line":10,"severity":"error","description":"null deref"}]}
```

That's all."#;
        let resp = parse_review_response(md).unwrap();
        assert_eq!(resp.decision, "request_changes");
        assert_eq!(resp.issues.len(), 1);
        assert_eq!(resp.issues[0].file, "src/main.rs");
    }

    #[test]
    fn parse_review_response_invalid() {
        let result = parse_review_response("not json at all");
        assert!(result.is_err());
    }

    #[test]
    fn extract_json_block_basic() {
        let md = "text\n```json\n{\"key\": \"value\"}\n```\nmore";
        let result = extract_json_block(md);
        assert!(result.is_some());
        assert!(result.unwrap().contains("\"key\""));
    }

    #[test]
    fn extract_json_block_skips_non_json_blocks() {
        let md = "```json\nnot-json-array\n```\n\n```json\n{\"real\": true}\n```";
        let result = extract_json_block(md);
        assert!(result.is_some());
        assert!(result.unwrap().contains("\"real\""));
    }

    #[test]
    fn extract_json_block_none_when_missing() {
        assert!(extract_json_block("no code blocks here").is_none());
    }
}
