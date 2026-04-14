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

/// Maximum output file size to read (10 MB).
/// Prevents runaway outputs from consuming excessive memory.
const MAX_OUTPUT_SIZE_BYTES: u64 = 10 * 1024 * 1024;

/// Read a file with a size limit, returning truncated content if the file is too large.
///
/// This prevents runaway agent outputs from causing memory issues. If the file
/// exceeds the limit, reads from the end of the file to capture the most recent
/// output (which is typically where the result or error message is).
fn read_file_with_limit<P: AsRef<Path>>(path: P, max_bytes: u64) -> anyhow::Result<String> {
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom};

    let path = path.as_ref();
    let metadata = std::fs::metadata(path)?;

    if !metadata.is_file() {
        anyhow::bail!("not a file: {}", path.display());
    }

    let file_size = metadata.len();

    // If file is within limits, read the whole thing
    if file_size <= max_bytes {
        return std::fs::read_to_string(path).map_err(|e| e.into());
    }

    // File is too large - read from the end to get the most recent output
    let mut file = File::open(path)?;
    let start_pos = file_size.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start_pos))?;

    let mut buffer = Vec::with_capacity(max_bytes as usize);
    file.read_to_end(&mut buffer)?;

    // Try to convert to UTF-8, replacing invalid sequences
    let content = String::from_utf8_lossy(&buffer);

    // Log a warning about truncation
    tracing::warn!(
        path = %path.display(),
        file_size = file_size,
        max_bytes = max_bytes,
        "output file truncated: exceeded size limit"
    );

    Ok(content.into_owned())
}

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

    // Add jitter: ±30% using a hash-based pseudo-random value.
    // Mix system time (nanoseconds) with thread ID so concurrent retries on
    // different threads get different delays even when called in the same
    // microsecond, avoiding the thundering-herd problem.
    let jitter_range = (capped as f64 * JITTER_FACTOR) as u64;
    let jitter = {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        std::time::SystemTime::now().hash(&mut h);
        std::thread::current().id().hash(&mut h);
        h.finish() % (jitter_range * 2 + 1)
    };

    capped.saturating_sub(jitter_range) + jitter
}

fn fallback_retry_count(task: Option<&crate::store::Task>) -> usize {
    task.map(|t| {
        let chain_retry_count = t
            .limit_reroute_chain
            .split(',')
            .filter(|entry| !entry.is_empty())
            .count();
        let network_retry_count = t.network_retries.max(0) as usize;
        chain_retry_count.max(network_retry_count)
    })
    .unwrap_or(0)
}

/// Wait with exponential backoff before retrying with a fallback agent.
/// Returns the delay applied in milliseconds.
pub async fn wait_for_fallback_backoff(
    task_id: &str,
    store: &Option<Arc<TaskStore>>,
    repo: &str,
) -> u64 {
    let task = store::opt_store_get_task(store, repo, task_id).await;

    // Normal failover uses reroute chain length. NetworkError retries bypass
    // failover, so track their own streak and use whichever retry source is larger.
    let retry_count = fallback_retry_count(task.as_ref());

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
/// Output is truncated to `MAX_OUTPUT_SIZE_BYTES` to prevent runaway memory usage.
pub async fn read_output_file(task_id: &str, primary_path: &Path, repo: &str) -> String {
    // Primary: explicit output file (already points to attempt dir)
    if let Some(content) = tokio::task::spawn_blocking({
        let p = primary_path.to_path_buf();
        move || read_file_with_limit(p, MAX_OUTPUT_SIZE_BYTES)
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
    if let Ok(task_dir) = crate::home::task_dir_async(repo, task_id).await {
        let attempts_dir = task_dir.join("attempts");
        // Use async metadata for is_dir check
        if tokio::fs::metadata(&attempts_dir)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false)
        {
            let mut attempt_nums: Vec<u32> = tokio::task::spawn_blocking({
                let a = attempts_dir.clone();
                move || {
                    match std::fs::read_dir(&a) {
                        Ok(entries) => {
                            let mut nums = Vec::new();
                            for entry in entries {
                                match entry {
                                    Ok(e) => {
                                        if let Some(n) =
                                            e.file_name().to_str().and_then(|n| n.parse().ok())
                                        {
                                            nums.push(n);
                                        }
                                    }
                                    Err(e) => tracing::warn!(
                                        path = %a.display(),
                                        err = %e,
                                        "error reading attempt dir entry in output fallback"
                                    ),
                                }
                            }
                            nums
                        }
                        Err(e) => {
                            tracing::warn!(path = %a.display(), err = %e, "failed to read attempts dir in output fallback");
                            Vec::new()
                        }
                    }
                }
            })
            .await
            .unwrap_or_default();

            attempt_nums.sort_unstable_by(|a, b| b.cmp(a)); // newest first
            for n in attempt_nums {
                let p = attempts_dir.join(n.to_string()).join("output.json");
                if let Some(content) = tokio::task::spawn_blocking({
                    let p = p.clone();
                    move || read_file_with_limit(p, MAX_OUTPUT_SIZE_BYTES)
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
            move || read_file_with_limit(p, MAX_OUTPUT_SIZE_BYTES)
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
    let available: Vec<String> = crate::engine::configured_agents()
        .into_iter()
        .filter(|a| crate::cmd_cache::command_exists(a))
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
        if let Err(e) = store::store_set_result(
            store,
            repo,
            task_id,
            &[("last_error", serde_json::json!(msg))],
        )
        .await
        {
            tracing::warn!(task_id, err = %e, "failed to write agents_exhausted last_error to store");
        }
        // If this was a timeout, ensure the agent is placed into cooldown
        // so the router's round-robin avoids it for a while.
        if matches!(error_type, RetryableError::Timeout) {
            record_agent_failure_with_message(agent_name, error_message).await;
        }

        // Log reroute activity for exhausted agents
        let details = serde_json::json!({
            "failure_reason": error_type.type_str(),
            "error_message": error_message,
            "chain": chain,
            "agents_exhausted": true,
        });
        crate::store::store_log_activity(
            store,
            repo,
            task_id,
            "rerouted",
            Some("in_progress"),
            Some("needs_review"),
            None::<&str>,
            None::<&str>,
            Some(&details),
        )
        .await;

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
        set_agent_cooldown(agent_name, 120).await;

        let msg = format!("{error_message}, clearing agent/model for re-route");
        if let Err(e) = store::store_set_result(
            store,
            repo,
            task_id,
            &[
                ("agent", serde_json::json!("")),
                ("model", serde_json::json!("")),
                ("last_error", serde_json::json!(msg)),
            ],
        )
        .await
        {
            tracing::warn!(task_id, err = %e, "failed to write agent/model clear to store");
        }

        // Apply exponential backoff with jitter before retry.
        // Return "new" (not "routed") so the task goes through Phase 3a routing
        // again. This ensures the router actually re-selects a compatible agent+model
        // via model_for_complexity() instead of bypassing routing and defaulting to
        // claude in the dispatch loop (#1604).
        wait_for_fallback_backoff(task_id, store, repo).await;

        // Log reroute activity for successful failover
        let details = serde_json::json!({
            "failure_reason": error_type.type_str(),
            "error_message": error_message,
            "from_agent": agent_name,
            "to_agent": next,
            "chain": chain,
        });
        crate::store::store_log_activity(
            store,
            repo,
            task_id,
            "rerouted",
            Some("in_progress"),
            Some("new"),
            Some(&next),
            None::<&str>,
            Some(&details),
        )
        .await;

        return "new".to_string();
    }

    // No fallback available
    tracing::warn!(task_id, agent = agent_name, "no fallback agents available");

    // Record agent failure for cooldown tracking
    // Skip cooldown for MissingTooling — it's permanent, not transient
    if !matches!(error_type, RetryableError::MissingTooling) {
        record_agent_failure_with_message(agent_name, error_message).await;
    }

    let msg = format!("{error_message}, no fallback agents");
    if let Err(e) = store::store_set_result(
        store,
        repo,
        task_id,
        &[("last_error", serde_json::json!(msg))],
    )
    .await
    {
        tracing::warn!(task_id, err = %e, "failed to write no_fallback last_error to store");
    }

    // Log reroute activity for no fallback available
    let details = serde_json::json!({
        "failure_reason": error_type.type_str(),
        "error_message": error_message,
        "chain": chain,
        "no_fallback_available": true,
    });
    crate::store::store_log_activity(
        store,
        repo,
        task_id,
        "rerouted",
        Some("in_progress"),
        Some("needs_review"),
        None::<&str>,
        None::<&str>,
        Some(&details),
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

    if let Err(e) = store::store_set_result(
        store,
        repo,
        task_id,
        &[("limit_reroute_chain", serde_json::json!(chain))],
    )
    .await
    {
        tracing::warn!(task_id, err = %e, "failed to write limit_reroute_chain to store");
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

/// Public entry point for keyword-based review response inference.
///
/// Used by `review.rs` when `parse_review_response` fails on the extracted text.
pub fn infer_review_response(text: &str) -> Option<ReviewResponse> {
    infer_review_response_from_text(text)
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
                        debug_assert!(
                            start.is_some(),
                            "start must be Some when depth returns to 0"
                        );
                        let start_idx = start.unwrap_or(0);
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
    raw_stdout: &str,
) {
    // Use agent self-reported files; fall back to NDJSON tool-call extraction when empty.
    let files_modified = if response.files.is_empty() {
        let extracted = crate::cli::ndjson::extract_files_changed(raw_stdout);
        if !extracted.is_empty() {
            tracing::debug!(
                task_id,
                count = extracted.len(),
                "files_modified: fell back to NDJSON extraction (agent reported none)"
            );
        }
        extracted
    } else {
        response.files.clone()
    };

    // Build the memory entry
    let entry = crate::store::MemoryEntry {
        attempt,
        agent: agent.to_string(),
        model: model.map(String::from),
        learnings: response.learnings.clone(),
        error: error.map(String::from),
        files_modified,
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
    use crate::engine::runner::agents::{find_agent_result, patterns, AgentError};

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
    fn fallback_retry_count_uses_larger_network_retry_streak() {
        let task = crate::store::Task {
            id: 1,
            external_id: None,
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "test".to_string(),
            body: String::new(),
            status: crate::store::TaskStatus::New,
            source: String::new(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
            labels: vec![],
            agent: None,
            model: None,
            complexity: "simple".to_string(),
            estimate: 0,
            route_reason: String::new(),
            agent_profile: String::new(),
            selected_skills: String::new(),
            route_attempts: 0,
            attempts: 0,
            branch: String::new(),
            worktree: String::new(),
            worktree_cleaned: false,
            summary: String::new(),
            last_error: String::new(),
            parent_id: None,
            block_reason: None,
            pr_number: None,
            pr_review_context: String::new(),
            last_review_ts: String::new(),
            review_ts_map: "{}".to_string(),
            last_comment_review_ts: String::new(),
            merge_conflict_retries: 0,
            ci_merge_failures: 0,
            pr_create_failures: 0,
            push_failures: 0,
            network_retries: 3,
            review_agent_failures: 0,
            review_cycles: 0,
            review_invocations: 0,
            needs_review_refires: 0,
            review_session_expected: false,
            input_tokens: 0,
            output_tokens: 0,
            input_cost_usd: 0.0,
            output_cost_usd: 0.0,
            total_cost_usd: 0.0,
            model_reroute_chain: String::new(),
            limit_reroute_chain: "claude".to_string(),
            budget_warning: String::new(),
            budget_exceeded: false,
            memory: vec![],
            delegations: vec![],
            auto_unblock_count: 0,
            auto_unblock_last_at: String::new(),
            auto_unblock_last_reason: String::new(),
            ci_recovery_count: 0,
            no_code_reroutes: 0,
            no_code_last_agent: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
        };

        assert_eq!(fallback_retry_count(Some(&task)), 3);
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

    // ── parse_review_response + infer_review_response (production path) ─

    /// Helper that mirrors the production plain-text path in review.rs:
    /// parse_review_response → infer_review_response.
    fn parse_review_plain(text: &str) -> anyhow::Result<ReviewResponse> {
        parse_review_response(text).or_else(|_| {
            infer_review_response(text)
                .ok_or_else(|| anyhow::anyhow!("failed to parse review response"))
        })
    }

    #[test]
    fn parse_review_from_output_direct_json() {
        let json = r#"{"decision":"approve","notes":"LGTM","test_results":"pass","issues":[]}"#;
        let resp = parse_review_response(json).unwrap();
        assert_eq!(resp.decision, "approve");
    }

    #[test]
    fn parse_review_from_output_markdown() {
        let md = "Review complete.\n\n```json\n{\"decision\":\"request_changes\",\"notes\":\"Fix it\",\"issues\":[]}\n```\n";
        let resp = parse_review_response(md).unwrap();
        assert_eq!(resp.decision, "request_changes");
    }

    #[test]
    fn parse_review_from_output_plain_text_fallback() {
        let text = "The review is done — all tests passed and the PR was approved.";
        let resp = parse_review_plain(text).unwrap();
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
        let resp = parse_via_agent_path("opencode", ndjson).unwrap();
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
        let resp = parse_via_agent_path("opencode", ndjson).unwrap();
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
        let resp = parse_via_agent_path("opencode", ndjson).unwrap();
        assert_eq!(resp.decision, "approve");
    }

    #[test]
    fn parse_review_from_output_ndjson_prefers_final_json_text_event() {
        let ndjson = concat!(
            r#"{"type":"text","timestamp":1001,"part":{"type":"text","text":"Working on it..."}}"#,
            "\n",
            r#"{"type":"text","timestamp":1002,"part":{"type":"text","text":"{\"decision\":\"approve\",\"notes\":\"LGTM\",\"test_results\":\"pass\",\"issues\":[]}"}}"#,
        );
        let resp = parse_via_agent_path("opencode", ndjson).unwrap();
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
        let resp = parse_via_agent_path("opencode", ndjson).unwrap();
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
        let resp = parse_via_agent_path("codex", ndjson).unwrap();
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
        let resp = parse_via_agent_path("codex", ndjson).unwrap();
        assert_eq!(resp.decision, "request_changes");
        assert_eq!(resp.notes, "Fix the tests");
    }

    /// codex reasoning items must NOT be extracted as review text — only agent_message is used.
    #[test]
    fn codex_skips_reasoning_items() {
        let ndjson = concat!(
            r#"{"type":"item.completed","item":{"type":"reasoning","text":"internal thoughts..."}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"{\"decision\":\"approve\",\"notes\":\"LGTM\",\"issues\":[]}"}}"#,
        );
        let resp = parse_via_agent_path("codex", ndjson).unwrap();
        assert_eq!(resp.decision, "approve");
        assert_eq!(resp.notes, "LGTM");
    }

    /// Negation patterns must NOT trigger a false approval.
    #[test]
    fn infer_review_response_negation_not_approved() {
        assert!(
            parse_review_plain("The changes were not approved.").is_err(),
            "\"not approved\" should not infer approval"
        );
    }

    #[test]
    fn infer_review_response_negation_unapproved() {
        assert!(
            parse_review_plain("Unapproved changes found in the diff.").is_err(),
            "\"unapproved\" should not infer approval"
        );
    }

    #[test]
    fn infer_review_response_negation_not_approving() {
        assert!(
            parse_review_plain("I am not approving this PR.").is_err(),
            "\"not approving\" should not infer approval"
        );
    }

    /// Positive plain-text approval signals must still work.
    #[test]
    fn infer_review_response_lgtm() {
        let resp = parse_review_plain("LGTM, everything looks fine.").unwrap();
        assert_eq!(resp.decision, "approve");
    }

    #[test]
    fn infer_review_response_looks_good() {
        let resp = parse_review_plain("Looks good to me!").unwrap();
        assert_eq!(resp.decision, "approve");
    }

    /// "All checks passed" should infer approval (real failure observed in task 16393).
    #[test]
    fn infer_review_response_all_checks_passed() {
        let resp = parse_review_plain(
            "The background task completed but I already got the CI results directly. All checks passed — no action needed.",
        )
        .unwrap();
        assert_eq!(resp.decision, "approve");
    }

    #[test]
    fn infer_review_response_checks_passed() {
        let resp = parse_review_plain("All checks passed (fmt ✓, clippy ✓, tests ✓).").unwrap();
        assert_eq!(resp.decision, "approve");
    }

    #[test]
    fn infer_review_response_all_tests_passed() {
        let resp =
            parse_review_plain("All tests passed and the implementation looks correct.").unwrap();
        assert_eq!(resp.decision, "approve");
    }

    #[test]
    fn infer_review_response_no_issues_found() {
        let resp = parse_review_plain("Review complete. No issues found in the diff.").unwrap();
        assert_eq!(resp.decision, "approve");
    }

    /// Partial "tests passed" mid-sentence must NOT infer approval without "all" prefix.
    #[test]
    fn infer_review_response_partial_tests_passed_no_approve() {
        let text = "The existing tests passed before this change but the new payment logic has a null-pointer risk on line 42.";
        assert!(parse_review_plain(text).is_err());
    }

    /// Partial "checks passed" mid-sentence must NOT infer approval without "all" prefix.
    #[test]
    fn infer_review_response_partial_checks_passed_no_approve() {
        let text = "CI checks passed for the base branch but this PR adds a broken path.";
        assert!(parse_review_plain(text).is_err());
    }

    /// LLMs sometimes echo the JSON decision field value literally: "Decision is `approve`."
    #[test]
    fn infer_review_response_decision_is_approve_backtick() {
        let text = "Already retrieved the output and completed the review above. The CI test check passed (3m11s) and all other checks are green. Decision is `approve`.";
        let resp = parse_review_plain(text).unwrap();
        assert_eq!(resp.decision, "approve");
    }

    /// "Decision: approve" without backticks should also match.
    #[test]
    fn infer_review_response_decision_colon_approve() {
        let text = "Reviewed the PR. No blocking issues found. Decision: approve";
        let resp = parse_review_plain(text).unwrap();
        assert_eq!(resp.decision, "approve");
    }

    /// Bare backtick `approve` in a negative context must NOT infer approval.
    /// e.g. "do not `approve`" previously matched the removed `lower.contains("`approve`")` check.
    #[test]
    fn infer_review_response_negation_do_not_approve_backtick() {
        let text = "There are unresolved issues — do not `approve` this PR until they are fixed.";
        assert!(
            parse_review_plain(text).is_err(),
            "negative context with backtick approve must not infer approval"
        );
    }

    // ── review parsing via find_agent_result + parse_review_response ────

    /// Helper that mirrors the production path in review.rs:
    /// find_agent_result → parse_review_response → infer_review_response.
    fn parse_via_agent_path(agent: &str, output: &str) -> anyhow::Result<ReviewResponse> {
        use crate::engine::runner::agents::find_agent_result;
        let text = find_agent_result(agent, output)
            .map(|r| r.result_text)
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| output.to_string());
        if let Ok(r) = parse_review_response(&text) {
            return Ok(r);
        }
        infer_review_response_from_text(&text)
            .ok_or_else(|| anyhow::anyhow!("failed to parse review response from {agent} output"))
    }

    #[test]
    fn agent_output_claude_ndjson_review() {
        let ndjson = concat!(
            r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Reviewing..."}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"{\"decision\":\"approve\",\"notes\":\"LGTM\",\"test_results\":\"pass\",\"issues\":[]}","usage":{"input_tokens":100,"output_tokens":20}}"#,
        );
        let resp = parse_via_agent_path("claude", ndjson).unwrap();
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
        let resp = parse_via_agent_path("opencode", ndjson).unwrap();
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
        let resp = parse_via_agent_path("codex", ndjson).unwrap();
        assert_eq!(resp.decision, "approve");
    }

    #[test]
    fn agent_output_plain_text_keyword_fallback() {
        let text = "All checks passed, LGTM.";
        let resp = parse_via_agent_path("claude", text).unwrap();
        assert_eq!(resp.decision, "approve");
    }

    #[test]
    fn agent_output_direct_json() {
        let json = r#"{"decision":"approve","notes":"LGTM","test_results":"pass","issues":[]}"#;
        let resp = parse_via_agent_path("opencode", json).unwrap();
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
        let result = parse_via_agent_path("claude", text);
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

    // ── read_file_with_limit tests ───────────────────────────────────────────

    #[test]
    fn read_file_with_limit_reads_small_file_completely() {
        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join(format!("orch_test_small_{}.txt", std::process::id()));
        let content = "Hello, World!";
        std::fs::write(&file_path, content).unwrap();

        let result = read_file_with_limit(&file_path, 1024);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), content);

        let _ = std::fs::remove_file(&file_path);
    }

    #[test]
    fn read_file_with_limit_truncates_large_file() {
        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join(format!("orch_test_large_{}.txt", std::process::id()));

        // Create a file larger than the limit (100 bytes)
        let large_content = "x".repeat(200);
        std::fs::write(&file_path, &large_content).unwrap();

        // Read with a 50-byte limit
        let result = read_file_with_limit(&file_path, 50);
        assert!(result.is_ok());

        let read_content = result.unwrap();
        // Should only read 50 bytes (the tail of the file)
        assert_eq!(read_content.len(), 50);
        assert_eq!(read_content, "x".repeat(50));

        let _ = std::fs::remove_file(&file_path);
    }

    #[test]
    fn read_file_with_limit_handles_empty_file() {
        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join(format!("orch_test_empty_{}.txt", std::process::id()));
        std::fs::write(&file_path, "").unwrap();

        let result = read_file_with_limit(&file_path, 1024);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "");

        let _ = std::fs::remove_file(&file_path);
    }

    #[test]
    fn read_file_with_limit_handles_nonexistent_file() {
        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join("orch_test_nonexistent_xyz123.txt");

        let result = read_file_with_limit(&file_path, 1024);
        assert!(result.is_err());
    }

    #[test]
    fn read_file_with_limit_reads_exact_limit() {
        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join(format!("orch_test_exact_{}.txt", std::process::id()));

        // Create exactly 100 bytes
        let content = "y".repeat(100);
        std::fs::write(&file_path, &content).unwrap();

        // Read with exactly 100-byte limit (should read all)
        let result = read_file_with_limit(&file_path, 100);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 100);

        let _ = std::fs::remove_file(&file_path);
    }

    #[test]
    fn read_file_with_limit_handles_unicode_truncation() {
        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join(format!("orch_test_unicode_{}.txt", std::process::id()));

        // Create content with multi-byte UTF-8 characters
        let emoji = "🎉".repeat(100); // Each emoji is 4 bytes
        std::fs::write(&file_path, &emoji).unwrap();

        // Read with a limit that cuts into the emoji sequence
        let result = read_file_with_limit(&file_path, 50);
        assert!(result.is_ok());

        let read_content = result.unwrap();
        // from_utf8_lossy should handle invalid sequences
        assert!(!read_content.is_empty());

        let _ = std::fs::remove_file(&file_path);
    }

    // ── Fixture-based review integration tests ──────────────────────────────────

    /// Full review pipeline: find_agent_result → parse_review_response → infer_review_response.
    fn parse_review_via_agent(agent: &str, output: &str) -> anyhow::Result<ReviewResponse> {
        let text = find_agent_result(agent, output)
            .map(|r| r.result_text)
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| output.to_string());
        if let Ok(r) = parse_review_response(&text) {
            return Ok(r);
        }
        infer_review_response(&text)
            .ok_or_else(|| anyhow::anyhow!("failed to parse review from {agent} output"))
    }

    // ── Claude review fixtures ─────────────────────────────────────────────────

    #[test]
    fn fixture_claude_review_approve() {
        let raw = include_str!("../../../tests/fixtures/review_claude_approve.jsonl");
        let resp = parse_review_via_agent("claude", raw).unwrap();
        assert_eq!(resp.decision, "approve");
        assert_eq!(resp.notes, "All checks pass");
        assert_eq!(resp.test_results.as_deref(), Some("pass"));
        assert!(resp.issues.is_empty());
    }

    #[test]
    fn fixture_claude_review_request_changes() {
        let raw = include_str!("../../../tests/fixtures/review_claude_request_changes.jsonl");
        let resp = parse_review_via_agent("claude", raw).unwrap();
        assert_eq!(resp.decision, "request_changes");
        assert_eq!(resp.notes, "Fix the null deref on line 42");
        assert_eq!(resp.test_results.as_deref(), Some("fail"));
        assert_eq!(resp.issues.len(), 1);
        assert_eq!(resp.issues[0].file, "src/main.rs");
        assert_eq!(resp.issues[0].line, Some(42));
        assert_eq!(resp.issues[0].severity, "error");
    }

    #[test]
    fn fixture_claude_review_rate_limit_returns_error() {
        // find_claude_result on an is_error=true result returns Some with is_error=true.
        // The integration test verifies this is captured, not that it's a ReviewResponse.
        let raw = include_str!("../../../tests/fixtures/review_claude_rate_limit.jsonl");
        let result = find_agent_result("claude", raw);
        assert!(
            result.is_some(),
            "should find result even for error envelope"
        );
        let result = result.unwrap();
        assert!(result.is_error, "rate limit should be flagged as error");
        assert!(result.result_text.contains("rate limit"));
    }

    // ── OpenCode review fixtures ────────────────────────────────────────────────

    #[test]
    fn fixture_opencode_review_approve() {
        let raw = include_str!("../../../tests/fixtures/review_opencode_approve.jsonl");
        let resp = parse_review_via_agent("opencode", raw).unwrap();
        assert_eq!(resp.decision, "approve");
        assert_eq!(resp.notes, "All checks pass");
        assert!(resp.issues.is_empty());
    }

    #[test]
    fn fixture_opencode_review_request_changes() {
        let raw = include_str!("../../../tests/fixtures/review_opencode_request_changes.jsonl");
        let resp = parse_review_via_agent("opencode", raw).unwrap();
        assert_eq!(resp.decision, "request_changes");
        assert_eq!(resp.notes, "Fix the memory leak");
        assert_eq!(resp.test_results.as_deref(), Some("fail"));
        assert_eq!(resp.issues.len(), 1);
        assert_eq!(resp.issues[0].file, "src/lib.rs");
        assert_eq!(resp.issues[0].line, Some(100));
    }

    #[test]
    fn fixture_opencode_review_plain_text_approve() {
        // OpenCode with plain-text review (no JSON) — should infer approval via keywords.
        let raw = include_str!("../../../tests/fixtures/review_opencode_plain_text.jsonl");
        let resp = parse_review_via_agent("opencode", raw).unwrap();
        assert_eq!(resp.decision, "approve");
    }

    // ── Codex review fixtures ────────────────────────────────────────────────────

    #[test]
    fn fixture_codex_review_approve() {
        let raw = include_str!("../../../tests/fixtures/review_codex_approve.jsonl");
        let resp = parse_review_via_agent("codex", raw).unwrap();
        assert_eq!(resp.decision, "approve");
        assert_eq!(resp.notes, "Clean implementation");
        assert!(resp.issues.is_empty());
    }

    #[test]
    fn fixture_codex_review_request_changes() {
        let raw = include_str!("../../../tests/fixtures/review_codex_request_changes.jsonl");
        let resp = parse_review_via_agent("codex", raw).unwrap();
        assert_eq!(resp.decision, "request_changes");
        assert_eq!(resp.notes, "Race condition on line 15");
        assert_eq!(resp.issues.len(), 1);
        assert_eq!(resp.issues[0].file, "src/server.rs");
        assert_eq!(resp.issues[0].line, Some(15));
    }

    #[test]
    fn fixture_codex_review_plain_text_approve() {
        // Codex with plain-text review — should infer approval via "LGTM" keyword.
        let raw = include_str!("../../../tests/fixtures/review_codex_plain_text.jsonl");
        let resp = parse_review_via_agent("codex", raw).unwrap();
        assert_eq!(resp.decision, "approve");
    }

    // ── Kimi review fixtures ────────────────────────────────────────────────────

    #[test]
    fn fixture_kimi_review_approve() {
        let raw = include_str!("../../../tests/fixtures/review_kimi_approve.jsonl");
        let resp = parse_review_via_agent("kimi", raw).unwrap();
        assert_eq!(resp.decision, "approve");
        assert_eq!(resp.notes, "LGTM");
        assert_eq!(resp.test_results.as_deref(), Some("pass"));
        assert!(resp.issues.is_empty());
    }

    // ── MiniMax review fixtures ────────────────────────────────────────────────

    #[test]
    fn fixture_minimax_review_request_changes() {
        let raw = include_str!("../../../tests/fixtures/review_minimax_request_changes.jsonl");
        let resp = parse_review_via_agent("minimax", raw).unwrap();
        assert_eq!(resp.decision, "request_changes");
        assert_eq!(resp.notes, "Fix the bug");
        assert_eq!(resp.test_results.as_deref(), Some("fail"));
        assert_eq!(resp.issues.len(), 1);
        assert_eq!(resp.issues[0].file, "src/foo.rs");
        assert_eq!(resp.issues[0].line, Some(42));
        assert_eq!(resp.issues[0].severity, "error");
    }

    // ── Cross-agent token/metadata extraction ───────────────────────────────────

    /// Verify that find_agent_result extracts token counts from each agent's format.
    #[test]
    fn find_agent_result_extracts_tokens_claude() {
        let raw = include_str!("../../../tests/fixtures/review_claude_approve.jsonl");
        let result = find_agent_result("claude", raw).expect("should find result");
        assert_eq!(result.input_tokens, Some(200));
        assert_eq!(result.output_tokens, Some(30));
        assert!(result.cost_usd.is_none()); // review fixtures don't include cost
    }

    #[test]
    fn find_agent_result_extracts_tokens_opencode() {
        let raw = include_str!("../../../tests/fixtures/review_opencode_approve.jsonl");
        let result = find_agent_result("opencode", raw).expect("should find result");
        assert_eq!(result.input_tokens, Some(2900));
        assert_eq!(result.output_tokens, Some(100));
        assert_eq!(result.cost_usd, Some(0.002));
    }

    /// Verify that find_agent_result returns None for empty/unparseable input.
    #[test]
    fn find_agent_result_returns_none_for_empty() {
        assert!(find_agent_result("claude", "").is_none());
        assert!(find_agent_result("opencode", "").is_none());
        assert!(find_agent_result("codex", "").is_none());
        assert!(find_agent_result("kimi", "").is_none());
        assert!(find_agent_result("minimax", "").is_none());
    }

    #[test]
    fn find_agent_result_returns_none_for_plain_text() {
        assert!(find_agent_result("claude", "just some plain text").is_none());
        assert!(find_agent_result("codex", "just some plain text").is_none());
        // OpenCode is a special case — it can sometimes return a result for plain text.
        // This is acceptable because opencode's extract_ndjson_text falls back gracefully.
    }

    // ── Error propagation via find_agent_result ─────────────────────────────────

    /// Verify that find_agent_result surfaces rate-limit errors from Claude.
    #[test]
    fn find_agent_result_propagates_claude_rate_limit() {
        let raw = include_str!("../../../tests/fixtures/review_claude_rate_limit.jsonl");
        let result = find_agent_result("claude", raw).expect("should find result");
        assert!(result.is_error, "should be flagged as error");
        assert!(result.result_text.contains("rate limit"));
    }
}
