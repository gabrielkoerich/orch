//! Response collection + error classification.
//!
//! After the agent finishes (tmux session ends), this module:
//! 1. Reads the output file
//! 2. Parses the response JSON
//! 3. Classifies errors (timeout, usage limit, auth, tooling)
//! 4. Determines next action (success, reroute, needs_review)

use crate::store;
use crate::store::TaskStore;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const JITTER_FACTOR: f64 = 0.3;

/// Calculate exponential backoff with jitter for fallback retries.
/// Uses retry count from the reroute chain to determine the delay.
///
/// Delay = min(base * 2^retry_count, max) * (1 + jitter)
pub(crate) fn calculate_backoff_delay(retry_count: usize) -> u64 {
    let base_delay = crate::engine::router::config::retry_base_delay_ms();
    let max_delay = crate::engine::router::config::retry_max_delay_ms();

    // Exponential: base * 2^retry_count
    let exponential = base_delay * (2_u64.saturating_pow(retry_count as u32));
    let capped = exponential.min(max_delay);

    // Add jitter: ±30% using a simple hash-based approach
    let jitter_range = (capped as f64 * JITTER_FACTOR) as u64;
    // Use current time micros for simple randomization
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64;
    let jitter = now % (jitter_range * 2 + 1);

    capped.saturating_sub(jitter_range) + jitter
}

/// Wait with exponential backoff before retrying with a fallback agent.
/// Returns the delay applied in milliseconds.
pub async fn wait_for_fallback_backoff(
    task_id: &str,
    store: &Option<Arc<TaskStore>>,
    repo: &str,
) -> u64 {
    // Get retry count from reroute chain
    let chain = get_reroute_chain(task_id, store, repo).await;
    let retry_count = if chain.is_empty() {
        0
    } else {
        chain.split(',').count()
    };

    let delay = calculate_backoff_delay(retry_count);

    if delay > 0 {
        tracing::debug!(
            task_id,
            retry_count = retry_count,
            delay_ms = delay,
            "fallback backoff: waiting before retry"
        );
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    }

    delay
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
    /// Task is blocked on child tasks (delegations). Do not trigger review agent.
    Blocked,
    /// No weight-relevant signal (timeout, auth error, etc.)
    None,
}

/// Read the agent's output file, trying multiple locations.
///
/// Checks per-task attempt directory first, then legacy flat paths.
pub async fn read_output_file(task_id: &str, primary_path: &Path, repo: &str) -> String {
    // Primary: explicit output file (already points to attempt dir)
    if let Some(content) = tokio::task::spawn_blocking({
        let p = primary_path.to_path_buf();
        move || std::fs::read_to_string(p)
    })
    .await
    .ok()
    .and_then(|r| r.ok())
    {
        if !content.is_empty() {
            return content;
        }
    }

    // Fallback: check all attempt dirs for this task (newest first)
    if let Ok(task_dir) = crate::home::task_dir(repo, task_id) {
        let attempts_dir = task_dir.join("attempts");
        if attempts_dir.is_dir() {
            let mut attempt_nums: Vec<u32> = tokio::task::spawn_blocking({
                let a = attempts_dir.clone();
                move || {
                    std::fs::read_dir(&a)
                        .into_iter()
                        .flatten()
                        .filter_map(|e| e.ok())
                        .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse().ok()))
                        .collect::<Vec<u32>>()
                }
            })
            .await
            .unwrap_or_default();

            attempt_nums.sort_unstable_by(|a, b| b.cmp(a)); // newest first
            for n in attempt_nums {
                let p = attempts_dir.join(n.to_string()).join("output.json");
                if let Some(content) = tokio::task::spawn_blocking({
                    let p = p.clone();
                    move || std::fs::read_to_string(p)
                })
                .await
                .ok()
                .and_then(|r| r.ok())
                {
                    if !content.is_empty() {
                        tracing::info!(task_id, path = %p.display(), "read output from attempt dir");
                        return content;
                    }
                }
            }
        }
    }

    // Legacy fallback locations
    let state_dir = crate::home::state_dir().unwrap_or_else(|_| PathBuf::from("/tmp"));

    let mut fallbacks = vec![
        PathBuf::from(format!("/tmp/output-{task_id}.json")),
        state_dir.join(format!("output-{task_id}.json")),
    ];

    if let Ok(legacy_path) = crate::home::state_file(&format!("output-{task_id}.json")) {
        if !fallbacks.contains(&legacy_path) {
            fallbacks.push(legacy_path);
        }
    }

    for path in &fallbacks {
        if let Some(content) = tokio::task::spawn_blocking({
            let p = path.clone();
            move || std::fs::read_to_string(p)
        })
        .await
        .ok()
        .and_then(|r| r.ok())
        {
            if !content.is_empty() {
                tracing::info!(task_id, path = %path.display(), "read output from legacy fallback");
                return content;
            }
        }
    }

    String::new()
}

// Cooldown tracking is implemented in `crate::engine::cooldown` (shared with the router).
pub use crate::engine::cooldown::{
    clear_expired_cooldowns, is_agent_in_cooldown, is_model_in_cooldown,
    record_agent_failure_with_message, record_model_failure, set_agent_cooldown,
};

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

    // Prefer agents that are not in cooldown and are not heavily degraded
    // from recent rate-limits. This prevents immediate failover to agents
    // that have been heavily penalized and likely to fail as well.
    for agent in available_agents {
        if agent == current_agent || chain_set.contains(agent.as_str()) {
            continue;
        }
        if is_agent_in_cooldown(agent) {
            continue;
        }
        // Weight-based filtering was removed from this module because it used
        // a default-constructed AgentWeights (dead code that always returned
        // true). Keep failover behavior simple here: prefer the first agent
        // that is not in cooldown and not in the reroute chain. If we later
        // expose the router's AgentWeights snapshot to this module we can add
        // weight-based avoidance back in a non-dead way.
        return Some(agent.clone());
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

impl RetryableError {
    /// Return a short classified string for this error type.
    /// Used in DB rate-limit metrics and test assertions.
    #[allow(dead_code)]
    pub fn type_str(self) -> &'static str {
        match self {
            RetryableError::Timeout => "timeout",
            RetryableError::UsageLimit => "rate_limit",
            RetryableError::AuthError => "auth_error",
            RetryableError::Failed | RetryableError::MissingTooling => "failed",
        }
    }
}

/// Handle failover for any retryable error type.
/// Returns the resulting status string: "routed" if rerouted, "needs_review" otherwise.
///
/// Note: DB recording of rate limit events is handled by the caller (mod.rs)
/// which has async context. This function only handles store state + cooldowns.
pub async fn handle_failover(
    task_id: &str,
    agent_name: &str,
    error_type: RetryableError,
    error_message: &str,
    store: &Option<Arc<TaskStore>>,
    repo: &str,
) -> String {
    // Get the reroute chain
    let chain = get_reroute_chain(task_id, store, repo).await;
    let chain = update_reroute_chain(task_id, agent_name, &chain, store, repo).await;

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
        let msg = format!("{error_message} (all agents exhausted)");
        store::store_set(
            store,
            repo,
            task_id,
            &[("last_error", serde_json::json!(msg))],
        )
        .await;
        // If this was a timeout, ensure the agent is placed into cooldown
        // so the router's round-robin avoids it for a while.
        if matches!(error_type, RetryableError::Timeout) {
            record_agent_failure_with_message(agent_name, error_message).await;
        }
        return "needs_review".to_string();
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
            record_agent_failure_with_message(agent_name, error_message).await;
        }

        // Apply brief 120s cooldown on the failed agent so the router skips it
        // on the next routing attempt. This prevents the router from immediately
        // re-selecting the same agent after clearing both agent and model (#1492).
        set_agent_cooldown(agent_name, 120);

        let msg = format!("{error_message}, clearing agent/model for re-route");
        store::store_set(
            store,
            repo,
            task_id,
            &[
                ("agent", serde_json::json!("")),
                ("model", serde_json::json!("")),
                ("last_error", serde_json::json!(msg)),
            ],
        )
        .await;

        // Apply exponential backoff with jitter before retry.
        // Return "routed" so the router re-selects a compatible agent+model
        // from scratch via model_for_complexity() + model_map in config.
        wait_for_fallback_backoff(task_id, store, repo).await;

        return "routed".to_string();
    }

    // No fallback available
    tracing::warn!(task_id, agent = agent_name, "no fallback agents available");

    // Record agent failure for cooldown tracking
    // Skip cooldown for MissingTooling — it's permanent, not transient
    if !matches!(error_type, RetryableError::MissingTooling) {
        record_agent_failure_with_message(agent_name, error_message).await;
    }

    let msg = format!("{error_message}, no fallback agents");
    store::store_set(
        store,
        repo,
        task_id,
        &[("last_error", serde_json::json!(msg))],
    )
    .await;
    "needs_review".to_string()
}

/// Get the reroute chain from store.
pub async fn get_reroute_chain(
    task_id: &str,
    store: &Option<Arc<TaskStore>>,
    repo: &str,
) -> String {
    store::opt_store_get_task(store, repo, task_id)
        .await
        .map(|t| t.limit_reroute_chain)
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Update the reroute chain in store.
pub async fn update_reroute_chain(
    task_id: &str,
    current_agent: &str,
    existing_chain: &str,
    store: &Option<Arc<TaskStore>>,
    repo: &str,
) -> String {
    let mut chain = existing_chain.to_string();
    if chain.is_empty() {
        chain = current_agent.to_string();
    } else if !chain.split(',').any(|a| a == current_agent) {
        chain = format!("{chain},{current_agent}");
    }

    store::store_set(
        store,
        repo,
        task_id,
        &[("limit_reroute_chain", serde_json::json!(chain))],
    )
    .await;
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

/// Parse a review response from raw agent output, handling NDJSON streams.
///
/// This is the primary entry point for review response parsing. It handles:
/// 1. Direct JSON (`ReviewResponse` object)
/// 2. Markdown with ` ```json ` code blocks containing a `ReviewResponse`
/// 3. NDJSON streams (opencode `run --format json` output) — extracts text
///    from `type:"text"` events and then applies steps 1 & 2
///
/// Use this in the review pipeline instead of `parse_review_response` so that
/// raw opencode NDJSON output is handled even when the normal agent-envelope
/// parsing chain fails (e.g. format change, empty summary).
#[allow(dead_code)] // used by integration tests; production code uses parse_review_from_agent_output
pub fn parse_review_from_output(output: &str) -> anyhow::Result<ReviewResponse> {
    // Step 1: direct JSON / markdown parse
    if let Ok(r) = parse_review_response(output) {
        return Ok(r);
    }

    // Step 2: NDJSON — extract text events and parse the concatenated text
    let extracted = ndjson_extract_text(output);
    if !extracted.is_empty() {
        return parse_review_response(&extracted)
            .map_err(|e| anyhow::anyhow!("parse failed after NDJSON extraction: {e}"));
    }

    // Step 3: heuristic fallback for plain-text decisions
    if let Some(resp) = infer_review_response_from_text(output) {
        tracing::warn!(
            output_len = output.len(),
            "review response parsed via keyword fallback"
        );
        return Ok(resp);
    }

    anyhow::bail!("failed to parse review response from output")
}

/// Parse a review response using per-agent NDJSON extraction.
///
/// Unlike `parse_review_from_output`, this uses the agent-specific `find_result`
/// extractors instead of the generic `ndjson_extract_text`. This eliminates false
/// positives from broad substring matching when agent output contains JSON-like
/// fragments or rate-limit keywords in normal text.
///
/// Parsing chain:
/// 1. Direct JSON / markdown parse of the raw output
/// 2. Per-agent NDJSON extraction → parse the extracted result text
/// 3. Heuristic keyword fallback for plain-text decisions
pub fn parse_review_from_agent_output(agent: &str, output: &str) -> anyhow::Result<ReviewResponse> {
    // Step 1: direct JSON / markdown parse
    if let Ok(r) = parse_review_response(output) {
        return Ok(r);
    }

    // Step 2: per-agent NDJSON extraction
    if let Some(result) = crate::engine::runner::agents::find_agent_result(agent, output) {
        if !result.result_text.is_empty() {
            if let Ok(r) = parse_review_response(&result.result_text) {
                return Ok(r);
            }
            // The extracted text might itself need keyword inference
            if let Some(r) = infer_review_response_from_text(&result.result_text) {
                tracing::warn!(
                    agent,
                    output_len = output.len(),
                    "review response parsed via keyword fallback on agent-extracted text"
                );
                return Ok(r);
            }
        }
    }

    // Step 3: heuristic fallback on the full output
    if let Some(resp) = infer_review_response_from_text(output) {
        tracing::warn!(
            agent,
            output_len = output.len(),
            "review response parsed via keyword fallback"
        );
        return Ok(resp);
    }

    anyhow::bail!("failed to parse review response from {agent} output")
}

/// Extract the concatenated text content from an NDJSON event stream.
///
/// **Deprecated**: Prefer `parse_review_from_agent_output` which uses per-agent
/// extractors. This generic version is retained for backward compatibility
/// when the agent name is unknown.
///
/// Handles text event formats from all agents:
/// - opencode Format 1: `{"type":"text","part":{"type":"text","text":"..."}}`
/// - opencode Format 2: `{"type":"text","text":"..."}`
/// - codex Format:      `{"type":"item.completed","item":{"type":"agent_message","text":"..."}}`
/// - claude stream-json: `{"type":"assistant","message":{"content":[{"type":"text","text":"..."}]}}`
/// - claude result:      `{"type":"result","result":"..."}`
#[allow(dead_code)] // used by parse_review_from_output (integration tests)
fn ndjson_extract_text(ndjson: &str) -> String {
    let texts: Vec<String> = ndjson
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|e| {
            let event_type = e.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match event_type {
                // opencode: text event
                "text" => {
                    // Format 1: text nested under "part"
                    e.get("part")
                        .and_then(|p| p.get("text"))
                        .and_then(|t| t.as_str())
                        .map(str::to_string)
                        // Format 2: text directly in event
                        .or_else(|| e.get("text").and_then(|t| t.as_str()).map(str::to_string))
                }
                // codex: item.completed with agent_message item
                "item.completed" => e
                    .get("item")
                    .filter(|item| {
                        item.get("type").and_then(|v| v.as_str()) == Some("agent_message")
                    })
                    .and_then(|item| item.get("text"))
                    .and_then(|t| t.as_str())
                    .map(str::to_string),
                // claude stream-json: assistant message with content array
                "assistant" => e
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array())
                    .map(|arr| {
                        arr.iter()
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
                            .join("")
                    })
                    .filter(|s| !s.is_empty()),
                // claude stream-json: final result event
                "result" => e.get("result").and_then(|r| r.as_str()).map(str::to_string),
                _ => None,
            }
        })
        .collect();

    let joined = texts.join("");
    if parse_review_response(&joined).is_ok() {
        return joined;
    }

    for text in texts.iter().rev() {
        let trimmed = text.trim();
        // Only accept a chunk that actually parses as JSON containing a
        // "decision" key — substring matching on "decision" can false-positive
        // on prose that merely mentions the word.
        if trimmed.contains('{') {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
                if val.as_object().is_some_and(|o| o.contains_key("decision")) {
                    return text.clone();
                }
            }
        }
    }

    joined
}

/// Parse a review response from already-unwrapped text.
///
/// Expects the raw result text (already extracted from the agent envelope
/// by the agent-specific parser). Tries direct JSON parse, then markdown
/// code block extraction.
pub fn parse_review_response(text: &str) -> anyhow::Result<ReviewResponse> {
    // Try direct JSON parse
    if let Ok(resp) = serde_json::from_str::<ReviewResponse>(text) {
        return Ok(resp);
    }

    // Try extracting JSON from markdown code blocks
    if let Some(json_str) = extract_json_block(text) {
        if let Ok(resp) = serde_json::from_str::<ReviewResponse>(&json_str) {
            return Ok(resp);
        }
    }

    if let Some(resp) = extract_review_response_object(text) {
        return Ok(resp);
    }

    anyhow::bail!("failed to parse review response")
}

fn infer_review_response_from_text(text: &str) -> Option<ReviewResponse> {
    let lower = text.to_ascii_lowercase();
    let has_changes_requested =
        lower.contains("changes requested") || lower.contains("request_changes");
    if has_changes_requested {
        return Some(ReviewResponse {
            decision: "request_changes".to_string(),
            notes: "Inferred decision from plain-text review output.".to_string(),
            test_results: None,
            issues: Vec::new(),
        });
    }

    let positive_approval = lower.contains("lgtm")
        || lower.contains("looks good")
        || lower.contains("all checks passed")
        || lower.contains("all tests passed")
        || lower.contains("no issues found")
        // "decision is `approve`" or "decision: `approve`" — LLMs often echo the JSON field value
        || lower.contains("decision is `approve`")
        || lower.contains("decision: `approve`")
        || lower.contains("decision is approve")
        || lower.contains("decision: approve")
        || (lower.contains("approved")
            && !lower.contains("not approved")
            && !lower.contains("unapproved")
            && !lower.contains("not approving")
            && !lower.contains("cannot approve")
            && !lower.contains("can't approve"));
    if positive_approval {
        return Some(ReviewResponse {
            decision: "approve".to_string(),
            notes: "Inferred decision from plain-text review output.".to_string(),
            test_results: None,
            issues: Vec::new(),
        });
    }

    None
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
        let Some(rest) = text[after_tag..].strip_prefix('\n') else {
            search_from = after_tag;
            continue;
        };
        let content_start = text.len() - rest.len();
        let Some(end_offset) = rest.find("```") else {
            search_from = content_start;
            continue;
        };
        let end = content_start + end_offset;
        let content = text[content_start..end].trim();
        if content.starts_with('{') {
            return Some(content.to_string());
        }
        search_from = end + 3;
    }
    None
}

/// Extract a ReviewResponse JSON object from arbitrary text.
///
/// Scans for balanced JSON objects and tries to parse them as ReviewResponse.
fn extract_review_response_object(text: &str) -> Option<ReviewResponse> {
    let mut start: Option<usize> = None;
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape = false;

    for (idx, ch) in text.char_indices() {
        if start.is_some() {
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
                        let start_idx = start.unwrap();
                        let candidate = &text[start_idx..=idx];
                        if let Ok(resp) = serde_json::from_str::<ReviewResponse>(candidate) {
                            return Some(resp);
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

    None
}

/// Extract learnings and store as memory for future attempts.
#[allow(clippy::too_many_arguments)]
pub async fn store_learnings_from_response(
    task_id: &str,
    attempt: u32,
    agent: &str,
    model: Option<&str>,
    response: &crate::parser::AgentResponse,
    error: Option<&str>,
    store: &Option<Arc<TaskStore>>,
    repo: &str,
) {
    // Build the memory entry
    let entry = crate::store::MemoryEntry {
        attempt,
        agent: agent.to_string(),
        model: model.map(String::from),
        learnings: response.learnings.clone(),
        error: error.map(String::from),
        files_modified: response.files.clone(),
        approach: response.summary.clone(),
        timestamp: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    };

    if let Some(ref st) = store {
        if let Ok(Some(store_id)) = st.resolve_task_id(repo, task_id).await {
            if let Err(e) = st.append_memory(store_id, &entry).await {
                tracing::warn!(task_id, error = ?e, "failed to store memory");
            } else {
                tracing::debug!(task_id, attempt, "stored memory for attempt");
            }
        }
    }
}

/// Store a memory entry for a failed attempt (without a full AgentResponse).
pub async fn store_failure_memory(
    task_id: &str,
    attempt: u32,
    agent: &str,
    model: Option<&str>,
    error: &str,
    store: &Option<Arc<TaskStore>>,
    repo: &str,
) {
    let entry = crate::store::MemoryEntry {
        attempt,
        agent: agent.to_string(),
        model: model.map(String::from),
        learnings: vec![],
        error: Some(error.to_string()),
        files_modified: vec![],
        approach: String::new(),
        timestamp: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    };

    if let Some(ref st) = store {
        if let Ok(Some(store_id)) = st.resolve_task_id(repo, task_id).await {
            if let Err(e) = st.append_memory(store_id, &entry).await {
                tracing::warn!(task_id, error = ?e, "failed to store failure memory");
            } else {
                tracing::debug!(task_id, attempt, "stored failure memory");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::runner::agents::{patterns, AgentError};

    #[test]
    fn calculate_backoff_delay_jitter_is_centered() {
        // Run many samples and verify the range is [0.7*capped, 1.3*capped].
        // We can't control `now` directly, but calling the function many times
        // exercises different microsecond values and lets us check bounds.
        let base = crate::engine::router::config::retry_base_delay_ms();
        let max = crate::engine::router::config::retry_max_delay_ms();

        // retry_count=0 → exponential = base, capped = base (assuming base < max)
        let capped = base.min(max);
        let jitter_range = (capped as f64 * JITTER_FACTOR) as u64;
        let lo = capped.saturating_sub(jitter_range);
        let hi = capped + jitter_range;

        for _ in 0..200 {
            let delay = calculate_backoff_delay(0);
            assert!(
                delay >= lo && delay <= hi,
                "delay {delay} out of expected range [{lo}, {hi}]"
            );
        }
    }

    #[test]
    fn retryable_type_str_returns_correct_labels() {
        assert_eq!(RetryableError::Timeout.type_str(), "timeout");
        assert_eq!(RetryableError::UsageLimit.type_str(), "rate_limit");
        assert_eq!(RetryableError::AuthError.type_str(), "auth_error");
        assert_eq!(RetryableError::Failed.type_str(), "failed");
        assert_eq!(RetryableError::MissingTooling.type_str(), "failed");
    }

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

    // ── Cooldown tests ────────────────────────────────────────────

    #[tokio::test]
    async fn record_and_check_agent_cooldown() {
        // Use unique names to avoid interference from other tests
        let agent = "test_cooldown_agent_1";
        assert!(!is_agent_in_cooldown(agent));
        record_agent_failure_with_message(agent, "").await;
        assert!(is_agent_in_cooldown(agent));
    }

    #[tokio::test]
    async fn record_and_check_model_cooldown() {
        let agent = "test_cooldown_agent_2";
        let model = "test_model_x";
        assert!(!is_model_in_cooldown(agent, model));
        record_model_failure(agent, model).await;
        assert!(is_model_in_cooldown(agent, model));
        // Different model should not be in cooldown
        assert!(!is_model_in_cooldown(agent, "other_model"));
    }

    #[tokio::test]
    async fn clear_expired_does_not_remove_fresh_entries() {
        let agent = "test_cooldown_agent_3";
        record_agent_failure_with_message(agent, "").await;
        clear_expired_cooldowns();
        // Should still be in cooldown (just recorded)
        assert!(is_agent_in_cooldown(agent));
    }

    // Use fake agent names so other tests don't interfere.
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
    fn parse_review_response_embedded_json_object() {
        let text = "Review complete.\n\n{\"decision\":\"approve\",\"notes\":\"LGTM\",\"test_results\":\"pass\",\"issues\":[]}\nThanks!";
        let resp = parse_review_response(text).unwrap();
        assert_eq!(resp.decision, "approve");
        assert_eq!(resp.notes, "LGTM");
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

    #[test]
    fn extract_json_block_skips_malformed_intermediate_blocks() {
        let md = "prefix ```json{\"broken\": true}\n\n```json\n{\"real\": true}\n```";
        let result = extract_json_block(md);
        assert_eq!(result.as_deref(), Some("{\"real\": true}"));
    }

    #[test]
    fn extract_json_block_skips_multiple_malformed_intermediate_blocks() {
        let md = "prefix ```json{\"broken\": true}\nmid ```json[1, 2, 3]\n\n```json\n{\"real\": true}\n```";
        let result = extract_json_block(md);
        assert_eq!(result.as_deref(), Some("{\"real\": true}"));
    }

    // ── parse_review_from_output ─────────────────────────────────

    #[test]
    fn parse_review_from_output_direct_json() {
        let json = r#"{"decision":"approve","notes":"LGTM","test_results":"pass","issues":[]}"#;
        let resp = parse_review_from_output(json).unwrap();
        assert_eq!(resp.decision, "approve");
    }

    #[test]
    fn parse_review_from_output_markdown() {
        let md = "Review complete.\n\n```json\n{\"decision\":\"request_changes\",\"notes\":\"Fix it\",\"issues\":[]}\n```\n";
        let resp = parse_review_from_output(md).unwrap();
        assert_eq!(resp.decision, "request_changes");
    }

    #[test]
    fn parse_review_from_output_plain_text_fallback() {
        let text = "The review is done — all tests passed and the PR was approved.";
        let resp = parse_review_from_output(text).unwrap();
        assert_eq!(resp.decision, "approve");
        assert!(resp.issues.is_empty());
    }

    /// opencode NDJSON stream — text events use the `part.text` format.
    #[test]
    fn parse_review_from_output_ndjson_part_format() {
        let ndjson = concat!(
            r#"{"type":"step_start","timestamp":1000,"sessionID":"ses_abc"}"#,
            "\n",
            r#"{"type":"text","timestamp":1001,"part":{"type":"text","text":"Reviewing the PR...\n\n```json\n{\"decision\":\"approve\",\"notes\":\"All checks pass\",\"test_results\":\"pass\",\"issues\":[]}\n```\n"}}"#,
            "\n",
            r#"{"type":"step_finish","timestamp":1002,"part":{"type":"step-finish","reason":"stop"}}"#,
        );
        let resp = parse_review_from_output(ndjson).unwrap();
        assert_eq!(resp.decision, "approve");
        assert_eq!(resp.notes, "All checks pass");
    }

    /// opencode NDJSON stream — text events use the newer `text` directly-in-event format.
    #[test]
    fn parse_review_from_output_ndjson_direct_text_format() {
        let ndjson = concat!(
            r#"{"type":"step_start","timestamp":1000,"sessionID":"ses_abc"}"#,
            "\n",
            r#"{"type":"text","timestamp":1001,"sessionID":"ses_abc","text":"Here is my review:\n\n```json\n{\"decision\":\"request_changes\",\"notes\":\"Fix the bug\",\"test_results\":\"fail\",\"issues\":[]}\n```\n"}"#,
            "\n",
            r#"{"type":"step_finish","timestamp":1002,"sessionID":"ses_abc"}"#,
        );
        let resp = parse_review_from_output(ndjson).unwrap();
        assert_eq!(resp.decision, "request_changes");
        assert_eq!(resp.notes, "Fix the bug");
    }

    /// opencode NDJSON with ReviewResponse JSON directly in a text event (no markdown wrapper).
    #[test]
    fn parse_review_from_output_ndjson_direct_json_in_text_event() {
        let ndjson = concat!(
            r#"{"type":"step_start","timestamp":1000}"#,
            "\n",
            r#"{"type":"text","timestamp":1001,"part":{"type":"text","text":"{\"decision\":\"approve\",\"notes\":\"LGTM\",\"test_results\":\"pass\",\"issues\":[]}"}}"#,
            "\n",
            r#"{"type":"step_finish","timestamp":1002}"#,
        );
        let resp = parse_review_from_output(ndjson).unwrap();
        assert_eq!(resp.decision, "approve");
    }

    #[test]
    fn parse_review_from_output_ndjson_prefers_final_json_text_event() {
        let ndjson = concat!(
            r#"{"type":"text","timestamp":1001,"part":{"type":"text","text":"Working on it..."}}"#,
            "\n",
            r#"{"type":"text","timestamp":1002,"part":{"type":"text","text":"{\"decision\":\"approve\",\"notes\":\"LGTM\",\"test_results\":\"pass\",\"issues\":[]}"}}"#,
        );
        let resp = parse_review_from_output(ndjson).unwrap();
        assert_eq!(resp.decision, "approve");
        assert_eq!(resp.notes, "LGTM");
    }

    /// Concatenated text events produce a valid ReviewResponse.
    #[test]
    fn parse_review_from_output_ndjson_concatenated_text() {
        let ndjson = concat!(
            r#"{"type":"text","timestamp":1001,"part":{"type":"text","text":"Here is my\n"}}"#,
            "\n",
            r#"{"type":"text","timestamp":1002,"part":{"type":"text","text":"review:\n\n```json\n{\"decision\":\"approve\",\"notes\":\"OK\",\"test_results\":\"pass\",\"issues\":[]}\n```\n"}}"#,
        );
        let resp = parse_review_from_output(ndjson).unwrap();
        assert_eq!(resp.decision, "approve");
    }

    /// codex NDJSON stream — agent_message item carries the ReviewResponse JSON.
    ///
    /// Real format from codex `exec --json` output (observed 2026-03-06):
    /// thread.started / turn.started / item.completed(reasoning) / item.completed(agent_message) / turn.completed
    #[test]
    fn parse_review_from_output_codex_agent_message() {
        let ndjson = concat!(
            r#"{"type":"thread.started","thread_id":"t1"}"#,
            "\n",
            r#"{"type":"turn.started"}"#,
            "\n",
            r#"{"type":"item.completed","item":{"id":"item_0","type":"reasoning","text":"Planning git commands..."}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"git log --oneline -5","output":"abc123 fix: something"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"{\"decision\":\"approve\",\"notes\":\"All checks pass\",\"test_results\":\"pass\",\"issues\":[]}"}}"#,
            "\n",
            r#"{"type":"turn.completed"}"#,
        );
        let resp = parse_review_from_output(ndjson).unwrap();
        assert_eq!(resp.decision, "approve");
        assert_eq!(resp.notes, "All checks pass");
    }

    /// codex NDJSON with ReviewResponse in a markdown code block inside agent_message.
    #[test]
    fn parse_review_from_output_codex_agent_message_markdown() {
        let ndjson = concat!(
            r#"{"type":"thread.started","thread_id":"t1"}"#,
            "\n",
            r#"{"type":"turn.started"}"#,
            "\n",
            r#"{"type":"item.completed","item":{"id":"item_0","type":"reasoning","text":"Reviewing..."}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"Review complete.\n\n```json\n{\"decision\":\"request_changes\",\"notes\":\"Fix the tests\",\"issues\":[]}\n```\n"}}"#,
            "\n",
            r#"{"type":"turn.completed"}"#,
        );
        let resp = parse_review_from_output(ndjson).unwrap();
        assert_eq!(resp.decision, "request_changes");
        assert_eq!(resp.notes, "Fix the tests");
    }

    /// reasoning items must NOT be extracted as review text.
    #[test]
    fn ndjson_extract_text_codex_skips_reasoning() {
        let ndjson = concat!(
            r#"{"type":"item.completed","item":{"type":"reasoning","text":"internal thoughts..."}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"actual output"}}"#,
        );
        let extracted = ndjson_extract_text(ndjson);
        assert_eq!(extracted, "actual output");
        assert!(!extracted.contains("internal thoughts"));
    }

    /// Negation patterns must NOT trigger a false approval.
    #[test]
    fn infer_review_response_negation_not_approved() {
        assert!(
            parse_review_from_output("The changes were not approved.").is_err(),
            "\"not approved\" should not infer approval"
        );
    }

    #[test]
    fn infer_review_response_negation_unapproved() {
        assert!(
            parse_review_from_output("Unapproved changes found in the diff.").is_err(),
            "\"unapproved\" should not infer approval"
        );
    }

    #[test]
    fn infer_review_response_negation_not_approving() {
        assert!(
            parse_review_from_output("I am not approving this PR.").is_err(),
            "\"not approving\" should not infer approval"
        );
    }

    /// Positive plain-text approval signals must still work.
    #[test]
    fn infer_review_response_lgtm() {
        let resp = parse_review_from_output("LGTM, everything looks fine.").unwrap();
        assert_eq!(resp.decision, "approve");
    }

    #[test]
    fn infer_review_response_looks_good() {
        let resp = parse_review_from_output("Looks good to me!").unwrap();
        assert_eq!(resp.decision, "approve");
    }

    /// "All checks passed" should infer approval (real failure observed in task 16393).
    #[test]
    fn infer_review_response_all_checks_passed() {
        let resp = parse_review_from_output(
            "The background task completed but I already got the CI results directly. All checks passed — no action needed.",
        )
        .unwrap();
        assert_eq!(resp.decision, "approve");
    }

    #[test]
    fn infer_review_response_checks_passed() {
        let resp =
            parse_review_from_output("All checks passed (fmt ✓, clippy ✓, tests ✓).").unwrap();
        assert_eq!(resp.decision, "approve");
    }

    #[test]
    fn infer_review_response_all_tests_passed() {
        let resp =
            parse_review_from_output("All tests passed and the implementation looks correct.")
                .unwrap();
        assert_eq!(resp.decision, "approve");
    }

    #[test]
    fn infer_review_response_no_issues_found() {
        let resp =
            parse_review_from_output("Review complete. No issues found in the diff.").unwrap();
        assert_eq!(resp.decision, "approve");
    }

    /// Partial "tests passed" mid-sentence must NOT infer approval without "all" prefix.
    #[test]
    fn infer_review_response_partial_tests_passed_no_approve() {
        let text = "The existing tests passed before this change but the new payment logic has a null-pointer risk on line 42.";
        assert!(parse_review_from_output(text).is_err());
    }

    /// Partial "checks passed" mid-sentence must NOT infer approval without "all" prefix.
    #[test]
    fn infer_review_response_partial_checks_passed_no_approve() {
        let text = "CI checks passed for the base branch but this PR adds a broken path.";
        assert!(parse_review_from_output(text).is_err());
    }

    /// LLMs sometimes echo the JSON decision field value literally: "Decision is `approve`."
    #[test]
    fn infer_review_response_decision_is_approve_backtick() {
        let text = "Already retrieved the output and completed the review above. The CI test check passed (3m11s) and all other checks are green. Decision is `approve`.";
        let resp = parse_review_from_output(text).unwrap();
        assert_eq!(resp.decision, "approve");
    }

    /// "Decision: approve" without backticks should also match.
    #[test]
    fn infer_review_response_decision_colon_approve() {
        let text = "Reviewed the PR. No blocking issues found. Decision: approve";
        let resp = parse_review_from_output(text).unwrap();
        assert_eq!(resp.decision, "approve");
    }

    /// Bare backtick `approve` in a negative context must NOT infer approval.
    /// e.g. "do not `approve`" previously matched the removed `lower.contains("`approve`")` check.
    #[test]
    fn infer_review_response_negation_do_not_approve_backtick() {
        let text = "There are unresolved issues — do not `approve` this PR until they are fixed.";
        assert!(
            parse_review_from_output(text).is_err(),
            "negative context with backtick approve must not infer approval"
        );
    }

    // ── parse_review_from_agent_output ──────────────────────────

    #[test]
    fn agent_output_claude_ndjson_review() {
        let ndjson = concat!(
            r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Reviewing..."}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"{\"decision\":\"approve\",\"notes\":\"LGTM\",\"test_results\":\"pass\",\"issues\":[]}","usage":{"input_tokens":100,"output_tokens":20}}"#,
        );
        let resp = parse_review_from_agent_output("claude", ndjson).unwrap();
        assert_eq!(resp.decision, "approve");
    }

    #[test]
    fn agent_output_opencode_ndjson_review() {
        let ndjson = concat!(
            r#"{"type":"step_start","timestamp":1000}"#,
            "\n",
            r#"{"type":"text","timestamp":1001,"part":{"type":"text","text":"{\"decision\":\"request_changes\",\"notes\":\"Fix it\",\"issues\":[]}"}}"#,
            "\n",
            r#"{"type":"step_finish","timestamp":1002,"part":{"type":"step-finish","reason":"stop","tokens":{"input":100,"output":10}}}"#,
        );
        let resp = parse_review_from_agent_output("opencode", ndjson).unwrap();
        assert_eq!(resp.decision, "request_changes");
    }

    #[test]
    fn agent_output_codex_ndjson_review() {
        let ndjson = concat!(
            r#"{"type":"thread.started","thread_id":"t1"}"#,
            "\n",
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"{\"decision\":\"approve\",\"notes\":\"All good\",\"issues\":[]}"}}"#,
            "\n",
            r#"{"type":"turn.completed"}"#,
        );
        let resp = parse_review_from_agent_output("codex", ndjson).unwrap();
        assert_eq!(resp.decision, "approve");
    }

    #[test]
    fn agent_output_plain_text_keyword_fallback() {
        let text = "All checks passed, LGTM.";
        let resp = parse_review_from_agent_output("claude", text).unwrap();
        assert_eq!(resp.decision, "approve");
    }

    #[test]
    fn agent_output_direct_json() {
        let json = r#"{"decision":"approve","notes":"LGTM","test_results":"pass","issues":[]}"#;
        let resp = parse_review_from_agent_output("opencode", json).unwrap();
        assert_eq!(resp.decision, "approve");
    }

    /// Regression: plain text containing JSON fragments should NOT trigger false
    /// positives. The per-agent extractor returns None for non-NDJSON text,
    /// so we fall through to keyword inference instead of generic NDJSON parsing.
    #[test]
    fn agent_output_text_with_json_fragment_no_false_positive() {
        let text = r#"I checked the file and found {"type":"text"} in the output. No issues."#;
        // This should NOT extract a review from the JSON fragment.
        // With the generic parser, this would match as a "text" NDJSON event.
        // With per-agent extraction, it returns None (not valid agent NDJSON).
        let result = parse_review_from_agent_output("claude", text);
        // Should either fail or infer from keywords, NOT parse the JSON fragment
        assert!(
            result.is_err() || result.as_ref().is_ok_and(|r| r.decision != "text"),
            "should not extract review from embedded JSON fragment"
        );
    }

    /// Prose containing the word "decision" and a JSON-like fragment must not
    /// be misidentified as a review response by `extract_review_response_object`.
    #[test]
    fn parse_review_response_ignores_prose_with_decision_word() {
        let text = r#"The decision was made. Here is a debug trace: {"trace":123}"#;
        assert!(
            parse_review_response(text).is_err(),
            "prose with 'decision' word and unrelated JSON should not parse as ReviewResponse"
        );
    }

    /// Plain text with an embedded JSON object that has unrelated keys should
    /// not be extracted as a review response.
    #[test]
    fn parse_review_response_ignores_embedded_json_without_decision_key() {
        let text = r#"Analysis complete. Config: {"timeout":30,"retries":3}. Done."#;
        assert!(
            parse_review_response(text).is_err(),
            "embedded JSON without 'decision' key should not parse as ReviewResponse"
        );
    }
}
