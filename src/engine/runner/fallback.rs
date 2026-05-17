//! Error recovery and fallback strategies.
//!
//! Extracted from `runner/mod.rs`. Handles the `Err(agent_err)` arm of the
//! parse result: error classification, model failover, free-model fallback,
//! and agent rerouting.

use crate::store;
use crate::store::TaskStore;
use std::sync::Arc;

use super::{agents, response};

/// Summarizes a rate limit error by extracting key information from Claude's API retry JSON.
///
/// Looks for the last `{"type":"system","subtype":"api_retry",...}` line (or a partial
/// fragment containing `"subtype":"api_retry"`) and extracts `error_status`, `attempt`,
/// `retry_delay_ms` to return a compact summary.
/// Returns a compact fallback if no api_retry JSON is found.
///
/// Exposed `pub(crate)` so the review pipeline (`engine::review`) reuses the same
/// summariser as the task runner. Without this, review runs persisted raw
/// api_retry JSON fragments into `task_runs.error` (issue #3149).
pub(crate) fn summarize_rate_limit_error(raw_output: &str) -> String {
    // Find the last line containing "subtype":"api_retry"
    // This handles both complete JSON objects and truncated fragments that begin
    // mid-object (e.g. `,"subtype":"api_retry","attempt":5...`).
    let last_api_retry_line = raw_output
        .lines()
        .rev()
        .find(|line| line.contains("\"subtype\":\"api_retry\""));

    if let Some(line) = last_api_retry_line {
        return summarise_api_retry_line(line);
    }

    // No `"subtype":"api_retry"` marker but the payload may still be a truncated
    // api_retry NDJSON fragment that lost the `"subtype"` key earlier in the
    // stream. If any line contains the characteristic numeric fields
    // (`error_status`, `attempt`, `retry_delay_ms`), summarise from that line
    // instead of echoing the raw JSON into `task_runs.error` (issue #3149).
    let api_retry_field_markers = ["\"error_status\":", "\"attempt\":", "\"retry_delay_ms\":"];
    let field_line = raw_output
        .lines()
        .rev()
        .find(|line| api_retry_field_markers.iter().any(|m| line.contains(m)));
    if let Some(line) = field_line {
        return summarise_api_retry_line(line);
    }

    // No api_retry JSON or field markers found — emit a compact first-line
    // summary, never raw passthrough. Truncate to a tight byte cap so even
    // long single-line provider payloads cannot bloat `task_runs.error`.
    const FIRST_LINE_CAP: usize = 120;
    let first_line = raw_output.lines().next().unwrap_or(raw_output);
    if first_line.len() > FIRST_LINE_CAP {
        let cutoff = agents::truncate_at_char_boundary(first_line, FIRST_LINE_CAP);
        let remaining = raw_output.len().saturating_sub(cutoff);
        format!("{}... (+{} more chars)", &first_line[..cutoff], remaining)
    } else {
        first_line.to_string()
    }
}

/// Extract `error_status`, `attempt`, and `retry_delay_ms` from a single
/// `api_retry`-shaped JSON line (or fragment) and return a compact summary
/// such as `"status=429 after 7 attempts (last delay 35s)"`. Falls back to
/// `"status=… (api_retry fragment)"` when no numeric field is recoverable.
fn summarise_api_retry_line(line: &str) -> String {
    let error_status =
        extract_json_field(line, "\"error_status\":").unwrap_or_else(|| "unknown".to_string());
    let attempt = extract_json_field(line, "\"attempt\":").unwrap_or_else(|| "unknown".to_string());
    let retry_delay_ms =
        extract_json_field(line, "\"retry_delay_ms\":").unwrap_or_else(|| "0".to_string());

    let has_field = error_status != "unknown"
        || attempt != "unknown"
        || (retry_delay_ms != "0" && retry_delay_ms != "unknown");

    if has_field {
        let retry_delay_s = retry_delay_ms.parse::<f64>().unwrap_or(0.0) / 1000.0;
        let retry_delay_s = retry_delay_s as u64;
        format!(
            "status={} after {} attempts (last delay {}s)",
            error_status, attempt, retry_delay_s
        )
    } else {
        let status_hint = extract_json_field(line, "\"status\":")
            .or_else(|| extract_json_field(line, "\"error\":"))
            .unwrap_or_else(|| "429".to_string());
        format!("status={} (api_retry fragment)", status_hint)
    }
}

/// Extracts a JSON field value as a string, handling both quoted strings and raw numbers.
fn extract_json_field(json: &str, field: &str) -> Option<String> {
    json.find(field).and_then(|start| {
        let value_start = start + field.len();
        let value_str = &json[value_start..];

        // Handle quoted strings
        if let Some(rest) = value_str.strip_prefix('"') {
            let end = rest.find('"')?; // find closing quote
            Some(rest[..end].to_string())
        }
        // Handle raw numbers (integers or floats)
        else {
            let end = value_str
                .find(|c: char| !c.is_ascii_digit() && c != '.')
                .unwrap_or(value_str.len());
            let num_str = &value_str[..end];
            num_str
                .strip_suffix(',')
                .map(|s| s.to_string())
                .or_else(|| Some(num_str.to_string()))
        }
    })
}

/// How `run()` should proceed after error handling.
pub enum ErrorHandleResult {
    /// Task was rerouted — `run()` should record metrics then return early.
    EarlyReturn { status: String },
    /// Normal failover applied (or task marked needs_review) — proceed to cleanup + metrics.
    ///
    /// `error` is the current run's attributed error message (e.g. "kimi timed out after 1801s").
    /// Callers should pass this as `error_override` to `build_run_audit` so that stale
    /// `last_error` values from a previous agent's run are not mis-attributed.
    Continue { status: String, error: String },
}

/// Try to reroute the task to an untried free model.
///
/// Returns `Some(EarlyReturn { status: "new" })` if a free model was found and the
/// task was rerouted, `None` otherwise (caller should fall through to normal failover).
///
/// - `exclude_models`: models to skip in addition to already-tried ones
/// - `check_cooldowns`: whether to skip models currently in cooldown
/// - `set_agent`: if `Some`, also overwrite the task's `agent` field
/// - `apply_backoff`: whether to pace the reroute with `wait_for_fallback_backoff`
/// - `reason_prefix`: prefix for the `last_error` message (free model name is appended)
#[allow(clippy::too_many_arguments)]
async fn try_free_model_reroute(
    task_id: &str,
    agent_name: &str,
    agent_runner: &dyn agents::AgentRunner,
    exclude_models: &[&str],
    check_cooldowns: bool,
    set_agent: Option<&str>,
    apply_backoff: bool,
    reason_prefix: &str,
    store: &Option<Arc<TaskStore>>,
    repo: &str,
) -> Option<ErrorHandleResult> {
    let free = agent_runner.free_models();
    if free.is_empty() {
        return None;
    }
    let tried_models: String = store::opt_store_get_task(store, repo, task_id)
        .await
        .map(|t| t.model_reroute_chain)
        .unwrap_or_default();
    let tried_set: std::collections::HashSet<&str> =
        tried_models.split(',').filter(|s| !s.is_empty()).collect();
    let next_free = free.iter().find(|m| {
        !exclude_models.contains(&m.as_str())
            && !tried_set.contains(m.as_str())
            && (!check_cooldowns || !response::is_model_in_cooldown(agent_name, m))
    })?;
    tracing::info!(task_id, model = %next_free, "{}", reason_prefix);
    let new_tried = if tried_models.is_empty() {
        next_free.clone()
    } else {
        format!("{tried_models},{next_free}")
    };
    let msg = format!("{reason_prefix} {next_free}");
    let mut fields: Vec<(&str, serde_json::Value)> = vec![
        ("model", serde_json::json!(next_free.to_string())),
        ("model_reroute_chain", serde_json::json!(new_tried)),
        ("last_error", serde_json::json!(msg)),
    ];
    if let Some(agent) = set_agent {
        fields.push(("agent", serde_json::json!(agent)));
    }
    if let Err(e) = store::store_set_result(store, repo, task_id, &fields).await {
        tracing::warn!(task_id, err = %e, "try_free_model_reroute: failed to persist model update — not rerouting");
        return None; // let caller fall through to normal failover
    }
    if apply_backoff {
        response::wait_for_fallback_backoff(task_id, store, repo).await;
    }
    Some(ErrorHandleResult::EarlyReturn {
        status: "new".to_string(),
    })
}

/// Handle an agent error: classify it, attempt recovery strategies, and update store.
///
/// Returns `Ok(ErrorHandleResult::EarlyReturn)` when the task was rerouted and
/// `run()` should record metrics and return immediately (skipping tmux cleanup).
/// Returns `Ok(ErrorHandleResult::Continue)` when `run()` should proceed to cleanup.
#[allow(clippy::too_many_arguments)]
pub async fn handle_error(
    task_id: &str,
    agent_err: &agents::AgentError,
    agent_name: &str,
    agent_runner: &dyn agents::AgentRunner,
    model_name: Option<&str>,
    complexity: Option<&str>,
    new_attempts: u32,
    store: &Option<Arc<TaskStore>>,
    repo: &str,
) -> anyhow::Result<ErrorHandleResult> {
    tracing::warn!(
        task_id,
        agent = agent_name,
        error = %agent_err,
        "agent error, attempting recovery"
    );

    // Map AgentError to RetryableError for the existing handle_failover()
    let (retryable, error_msg) = match agent_err {
        agents::AgentError::RateLimit { message, .. } => {
            let is_provider_returned_error = message
                .to_ascii_lowercase()
                .contains("provider returned error");
            // Check for credit exhaustion - these require longer agent-wide cooldowns.
            // For all other rate limits, record both the agent-level and model-level
            // cooldowns immediately so the router can observe them before any concurrent
            // tasks start their runs. handle_failover() also records the cooldown later,
            // but setting it here means the in-memory map is updated right away — without
            // this, concurrent dispatches that check cooldowns between now and
            // handle_failover() will see no cooldown and waste another run against the
            // same rate-limited model.
            //
            // Setting BOTH cooldowns is critical: the router's model selection
            // (model_for_complexity) checks is_model_in_cooldown(agent, model) which
            // uses the agent:model key, so a model-level cooldown is needed to prevent
            // the same model from being re-selected even when the agent-level cooldown
            // is set. Without the model-level cooldown, the model continues to be
            // picked and fails repeatedly (issue #2153).
            if let Some(reason) = crate::engine::cooldown::detect_credit_exhaustion(message) {
                crate::engine::cooldown::record_credit_exhaustion(agent_name, reason).await;
            } else {
                crate::engine::cooldown::record_agent_failure_with_message(agent_name, message)
                    .await;
                // Also record the model-specific cooldown so model_for_complexity skips it.
                if let Some(model) = model_name {
                    if is_provider_returned_error {
                        // Opaque provider failures can remain unstable for hours.
                        // Use longer exponential model backoff to avoid repeated runs.
                        crate::engine::cooldown::record_persistent_model_failure(agent_name, model)
                            .await;
                    } else {
                        response::record_model_failure(agent_name, model).await;
                    }
                }
            }
            (
                response::RetryableError::UsageLimit,
                format!(
                    "{agent_name} rate limit: {}",
                    summarize_rate_limit_error(message)
                ),
            )
        }
        agents::AgentError::Auth { message } => {
            // Check for credit exhaustion first — those need longer agent-wide cooldowns.
            // For plain auth failures (expired key, wrong token, etc.) record both the
            // agent-level and model-level cooldowns so the router can observe them before
            // concurrent tasks start another run against the same unavailable model.
            if let Some(reason) = crate::engine::cooldown::detect_credit_exhaustion(message) {
                crate::engine::cooldown::record_credit_exhaustion(agent_name, reason).await;
            } else {
                crate::engine::cooldown::record_agent_failure_with_message(agent_name, message)
                    .await;
                if let Some(model) = model_name {
                    response::record_model_failure(agent_name, model).await;
                }
            }
            (
                response::RetryableError::AuthError,
                format!("{agent_name} auth error: {message}"),
            )
        }
        agents::AgentError::Timeout { elapsed } => (
            response::RetryableError::Timeout,
            format!("{agent_name} timed out after {}s", elapsed.as_secs()),
        ),
        agents::AgentError::MissingTool { tool } => (
            response::RetryableError::MissingTooling,
            format!("missing tool: {tool}"),
        ),
        agents::AgentError::ModelUnavailable { model, message } => {
            // "Model not found" is deterministic — the model is decommissioned and
            // will never return. Skip the escalating backoff and jump straight to the
            // 7-day max to prevent 3-4 wasteful retries over the 4h→12h→36h ramp.
            // All other model-unavailable signals use the standard escalating backoff.
            let lower = message.to_lowercase();
            let is_permanently_gone = lower.contains("not found");
            if is_permanently_gone {
                crate::engine::cooldown::record_model_permanently_gone(agent_name, model).await;
            } else {
                response::record_model_failure(agent_name, model).await;
            }

            // Try next model before switching agent
            let models = agent_runner.available_models();
            let current_model = model_name.unwrap_or("");
            let next_model = models.iter().find(|m| {
                m.as_str() != current_model
                    && m.as_str() != model
                    && !response::is_model_in_cooldown(agent_name, m)
            });
            if let Some(next) = next_model {
                tracing::info!(task_id, model = %next, "retrying with different model");
                let msg = format!("model {model} unavailable, trying {next}");
                if let Err(e) = store::store_set_result(
                    store,
                    repo,
                    task_id,
                    &[
                        ("model", serde_json::json!(next.to_string())),
                        ("last_error", serde_json::json!(msg)),
                    ],
                )
                .await
                {
                    tracing::warn!(task_id, err = %e, "failed to write model failover to store");
                }
                // Skip normal failover — we're retrying same agent with different model.
                // Use "routed" (not "new") since agent/model are already stored;
                // this avoids a redundant LLM re-routing cycle.
                response::wait_for_fallback_backoff(task_id, store, repo).await;
                return Ok(ErrorHandleResult::EarlyReturn {
                    status: "routed".to_string(),
                });
            }

            // No other standard models available — try free models for simple tasks
            let is_simple = matches!(complexity, None | Some("simple"));
            if is_simple {
                if let Some(result) = try_free_model_reroute(
                    task_id,
                    agent_name,
                    agent_runner,
                    &[current_model, model.as_str()],
                    true,
                    None,
                    true,
                    &format!("model {model} unavailable, trying free model"),
                    store,
                    repo,
                )
                .await
                {
                    return Ok(result);
                }
            }

            (
                response::RetryableError::ModelUnavailable,
                format!("model {model} unavailable"),
            )
        }
        agents::AgentError::ContextOverflow { message } => {
            // Context overflow is deterministic — the task's prompt/context is too large
            // for this model's context window. Cycling through other agents would hit the
            // same limit and waste API budget. Skip failover entirely and escalate
            // immediately to needs_review so a human can intervene (e.g., split the task,
            // reduce context, or manually assign a model with a larger context window).
            let msg = format!("{agent_name} context overflow: {message}");
            if let Err(e) = store::store_set_result(
                store,
                repo,
                task_id,
                &[("last_error", serde_json::json!(msg))],
            )
            .await
            {
                tracing::warn!(task_id, err = %e, "failed to write context_overflow last_error to store");
            }
            return Ok(ErrorHandleResult::EarlyReturn {
                status: "needs_review".to_string(),
            });
        }
        agents::AgentError::WaitingForInput { message } => {
            // Requires human — skip failover, go straight to needs_review
            let msg = format!("waiting for input: {message}");
            if let Err(e) = store::store_set_result(
                store,
                repo,
                task_id,
                &[("last_error", serde_json::json!(msg))],
            )
            .await
            {
                tracing::warn!(task_id, err = %e, "failed to write waiting_for_input last_error to store");
            }
            return Ok(ErrorHandleResult::EarlyReturn {
                status: "needs_review".to_string(),
            });
        }
        agents::AgentError::PermissionDenied { message } => (
            response::RetryableError::Failed,
            format!("permission denied: {message}"),
        ),
        agents::AgentError::InvalidResponse { .. } => {
            // Record model-specific cooldown so the same model that produced an
            // unparseable response isn't immediately retried on the next task.
            if let Some(model) = model_name {
                response::record_model_failure(agent_name, model).await;
            }
            (
                response::RetryableError::Failed,
                format!("{agent_name} invalid response"),
            )
        }
        agents::AgentError::AgentFailed { message } => {
            // "Provider returned error" (opencode) and similar upstream failures.
            // Record model+agent cooldowns so the router skips this model on retry,
            // then route through handle_failover() to get proper reroute-chain
            // tracking, exhaustion checks, and fallback pacing.
            if let Some(model) = model_name {
                response::record_model_failure(agent_name, model).await;
            }
            crate::engine::cooldown::record_agent_failure_with_message(agent_name, message).await;
            (
                response::RetryableError::Failed,
                format!("{agent_name} failed: {message}"),
            )
        }
        agents::AgentError::NetworkError { message } => {
            // Transient connectivity failure — retry same agent without rerouting,
            // but grow a dedicated streak counter so backoff keeps increasing.
            // Use "routed" so it re-dispatches without a full re-routing cycle.
            // After MAX_NETWORK_RETRIES consecutive network errors, escalate to
            // needs_review so a human is notified rather than looping forever.
            const MAX_NETWORK_RETRIES: u64 = 8;
            let msg = format!("{agent_name} network error: {message}");
            let retry_count = match store::store_increment(store, repo, task_id, "network_retries")
                .await
            {
                Ok(count) => {
                    tracing::info!(
                        task_id,
                        retry_count = count,
                        agent = agent_name,
                        "network retry scheduled"
                    );
                    count
                }
                Err(e) => {
                    tracing::warn!(
                        task_id,
                        error = %e,
                        "failed to increment network_retries — escalating to needs_review"
                    );
                    return Ok(ErrorHandleResult::Continue {
                        status: "needs_review".to_string(),
                        error: format!("{agent_name} network error (store unavailable): {message}"),
                    });
                }
            };
            if let Err(e) = store::store_set_result(
                store,
                repo,
                task_id,
                &[("last_error", serde_json::json!(msg))],
            )
            .await
            {
                tracing::warn!(task_id, err = %e, "failed to write network_error last_error to store");
            }
            if retry_count >= MAX_NETWORK_RETRIES {
                tracing::warn!(
                    task_id,
                    retry_count,
                    agent = agent_name,
                    "network retries exhausted, escalating to needs_review"
                );
                return Ok(ErrorHandleResult::Continue {
                    status: "needs_review".to_string(),
                    error: msg,
                });
            }
            // Apply backoff to pace retries
            response::wait_for_fallback_backoff(task_id, store, repo).await;
            return Ok(ErrorHandleResult::EarlyReturn {
                status: "routed".to_string(),
            });
        }
        agents::AgentError::StaleSession { session_id } => {
            // Stale session errors only occur in the orch chat control session, not
            // in task runners (tasks don't use --session-id / --resume). Treat as a
            // generic agent failure so the task is retried from scratch.
            (
                response::RetryableError::Failed,
                format!("{agent_name} stale session: {session_id}"),
            )
        }
        agents::AgentError::Unknown { exit_code, message } => {
            // Exit 0 with empty output is a silent failure (common with GitHub
            // Copilot models in opencode).  Record a model-specific cooldown so
            // the same model is not retried on every subsequent task.
            // Also matches when message == "empty-output-exit0" (from classify_from_text
            // or the direct empty-output branch in mod.rs).
            if *exit_code == 0
                && (message.is_empty()
                    || message == "empty-output-exit0"
                    || message.starts_with("empty-output-exit0:"))
            {
                if let Some(m) = model_name {
                    // Silent exits can remain broken for long periods. Start with 4h,
                    // then escalate using exponential backoff for persistent failures.
                    crate::engine::cooldown::record_persistent_model_failure(agent_name, m).await;
                }
                // Before falling through to handle_failover() (which tries claude/codex),
                // check whether this agent has any free models that haven't been tried yet.
                // Silent exit-0 is usually model/provider-specific; retrying other free
                // models first avoids wasting paid claude/codex failovers.
                let current = model_name.unwrap_or("");
                if let Some(result) = try_free_model_reroute(
                    task_id,
                    agent_name,
                    agent_runner,
                    &[current],
                    true,
                    Some("opencode"),
                    true,
                    "silent exit 0, retrying with free model",
                    store,
                    repo,
                )
                .await
                {
                    return Ok(result);
                }
            }

            // Handle JSON cost telemetry in stdout for non-zero exits
            // If the message looks like Claude's cost JSON, replace with a generic message
            let error_message = if *exit_code != 0 && message.contains("\"costUSD\":") {
                // Extract just the essential info: exit code and that cost telemetry was present
                format!(
                    "{} exited with non-zero status (cost telemetry in stdout, no error message)",
                    agent_name
                )
            } else {
                format!("{agent_name} exit {exit_code}: {message}")
            };

            (response::RetryableError::Failed, error_message)
        }
    };

    // If this was a timeout, clear the stored agent so the router can
    // pick a different executor on re-dispatch. Without this the same
    // slow agent can be picked again and will timeout repeatedly.
    if retryable == response::RetryableError::Timeout {
        if let Err(e) = store::store_set_result(
            store,
            repo,
            task_id,
            &[
                ("agent", serde_json::json!("")),
                ("last_error", serde_json::json!(error_msg.clone())),
            ],
        )
        .await
        {
            tracing::warn!(task_id, err = %e, "failed to write timeout agent clear to store");
        }
    }

    // Record rate limit in store (sqlx)
    {
        let error_type_str = match retryable {
            response::RetryableError::UsageLimit => "rate",
            response::RetryableError::AuthError => "budget",
            _ => "error",
        };
        if let Some(ref s) = store {
            if let Err(e) = s
                .record_rate_limit(agent_name, error_type_str, Some(task_id))
                .await
            {
                tracing::warn!(task_id, agent = %agent_name, err = %e, "failed to persist rate_limit event — router health check may miss this signal");
            }
        }
    }

    // Try free models as last resort before giving up
    let chain = response::get_reroute_chain(task_id, store, repo).await;
    let available: Vec<String> = crate::engine::configured_agents()
        .into_iter()
        .filter(|a| crate::cmd_cache::command_exists(a))
        .collect();

    let all_agents_tried = {
        let chain_set: std::collections::HashSet<&str> = if chain.is_empty() {
            std::collections::HashSet::new()
        } else {
            chain.split(',').collect()
        };
        !available
            .iter()
            .any(|a| a.as_str() != agent_name && !chain_set.contains(a.as_str()))
    };

    let is_simple = matches!(complexity, None | Some("simple"));
    if all_agents_tried && is_simple {
        // All agents exhausted — try free models via opencode (only for simple tasks)
        if let Some(result) = try_free_model_reroute(
            task_id,
            agent_name,
            agent_runner,
            &[],
            true, // fix: check cooldowns (was previously missing, inconsistent with other paths)
            Some("opencode"),
            false,
            "all agents exhausted, trying free model",
            store,
            repo,
        )
        .await
        {
            return Ok(result);
        }
    }

    let status =
        response::handle_failover(task_id, agent_name, retryable, &error_msg, store, repo).await;
    if status == "needs_review" {
        tracing::warn!(task_id, "failover exhausted, task marked needs_review");
    }

    // Store failure memory for retry learning
    response::store_failure_memory(
        task_id,
        new_attempts,
        agent_name,
        model_name,
        &error_msg,
        store,
        repo,
    )
    .await;

    Ok(ErrorHandleResult::Continue {
        status,
        error: error_msg,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::runner::agents::{AgentError, AgentRunner, ParsedResponse, PermissionRules};
    use crate::parser::AgentResponse;

    struct MockRunner {
        free: Vec<String>,
    }

    impl AgentRunner for MockRunner {
        fn name(&self) -> &str {
            "opencode"
        }

        fn build_command(
            &self,
            _model: Option<&str>,
            _timeout_cmd: &str,
            _sys_file: &str,
            _msg_file: &str,
            _permissions: &PermissionRules,
        ) -> String {
            String::new()
        }

        fn parse_response(&self, _raw: &str) -> Result<ParsedResponse, AgentError> {
            Ok(ParsedResponse {
                response: AgentResponse {
                    status: "done".to_string(),
                    summary: String::new(),
                    accomplished: vec![],
                    remaining: vec![],
                    files: vec![],
                    error: None,
                    input_tokens: None,
                    output_tokens: None,
                    learnings: vec![],
                    delegations: vec![],
                },
                input_tokens: None,
                output_tokens: None,
                duration_ms: None,
            })
        }

        fn classify_error(&self, _exit_code: i32, _stdout: &str, _stderr: &str) -> AgentError {
            AgentError::Unknown {
                exit_code: 0,
                message: String::new(),
            }
        }

        fn free_models(&self) -> Vec<String> {
            self.free.clone()
        }

        fn router_command(
            &self,
            _prompt: &str,
            _model: Option<&str>,
        ) -> anyhow::Result<tokio::process::Command> {
            anyhow::bail!("not implemented")
        }
    }

    /// Verify that a generic (non-credit-exhaustion) rate limit error sets the agent
    /// cooldown immediately in `handle_error()`, before `handle_failover()` is called.
    ///
    /// This prevents concurrent dispatches from starting new runs against the same
    /// rate-limited agent while the first task is still in its error-handling path.
    #[tokio::test]
    async fn rate_limit_sets_agent_cooldown_immediately() {
        let runner = MockRunner { free: vec![] };
        let agent = "test-agent-1371-early-cooldown";

        // Confirm no cooldown before the error.
        assert!(
            !crate::engine::cooldown::is_agent_in_cooldown(agent),
            "agent should not be in cooldown before handle_error"
        );

        let err = AgentError::RateLimit {
            message: "429 Too Many Requests".to_string(),
        };

        // Run handle_error — no store so handle_failover will find no agents and return Continue.
        let _result = handle_error(
            "test-1371-a",
            &err,
            agent,
            &runner,
            Some("sonnet"),
            Some("medium"),
            1,
            &None,
            "owner/repo",
        )
        .await
        .unwrap();

        // Cooldown must be set immediately after handle_error returns, not deferred.
        assert!(
            crate::engine::cooldown::is_agent_in_cooldown(agent),
            "agent should be in cooldown immediately after handle_error for RateLimit"
        );
    }

    /// Verify that a rate limit error sets BOTH the agent-level and model-level cooldowns.
    ///
    /// This is critical: the router's model selection (model_for_complexity) checks
    /// is_model_in_cooldown(agent, model) which uses the agent:model key format.
    /// An agent-level cooldown alone does NOT prevent the same model from being
    /// re-selected, causing the rate-limited model to fail repeatedly (issue #2153).
    #[tokio::test]
    async fn rate_limit_sets_both_agent_and_model_cooldowns() {
        let runner = MockRunner { free: vec![] };
        let agent = "test-agent-2153-model-cooldown";
        let model = "opencode/github-copilot/qwen3.6-plus-free";

        // Confirm no cooldowns before the error.
        assert!(
            !crate::engine::cooldown::is_agent_in_cooldown(agent),
            "agent should not be in cooldown before handle_error"
        );
        assert!(
            !crate::engine::cooldown::is_model_in_cooldown(agent, model),
            "model should not be in cooldown before handle_error"
        );

        let err = AgentError::RateLimit {
            message: "Upstream error from Alibaba: Request rate increased too quickly.".to_string(),
        };

        let _result = handle_error(
            "test-2153-a",
            &err,
            agent,
            &runner,
            Some(model),
            Some("simple"),
            1,
            &None,
            "owner/repo",
        )
        .await
        .unwrap();

        // Agent-level cooldown must be set immediately.
        assert!(
            crate::engine::cooldown::is_agent_in_cooldown(agent),
            "agent should be in agent-level cooldown after RateLimit"
        );
        // Model-level cooldown must ALSO be set immediately — this is the fix for issue #2153.
        // Without this, model_for_complexity would still pick the same model.
        assert!(
            crate::engine::cooldown::is_model_in_cooldown(agent, model),
            "model should be in model-level cooldown after RateLimit (fixes issue #2153)"
        );
    }

    /// Verify that rate limit for a specific model does NOT cool other models of the same agent.
    #[tokio::test]
    async fn rate_limit_cooldown_is_model_specific() {
        let runner = MockRunner { free: vec![] };
        let agent = "test-agent-2153-model-specific";
        let cooled_model = "opencode/github-copilot/qwen3.6-plus-free";
        let other_model = "opencode/minimax-m2.5-free";

        let err = AgentError::RateLimit {
            message: "rate limit exceeded".to_string(),
        };

        let _result = handle_error(
            "test-2153-b",
            &err,
            agent,
            &runner,
            Some(cooled_model),
            Some("simple"),
            1,
            &None,
            "owner/repo",
        )
        .await
        .unwrap();

        // The specific model that hit the rate limit should be cooled.
        assert!(
            crate::engine::cooldown::is_model_in_cooldown(agent, cooled_model),
            "rate-limited model should be in cooldown"
        );
        // Other models of the same agent should NOT be cooled.
        assert!(
            !crate::engine::cooldown::is_model_in_cooldown(agent, other_model),
            "other models should not be cooled by a single model's rate limit"
        );
    }

    #[tokio::test]
    async fn provider_returned_error_sets_extended_model_cooldown() {
        let runner = MockRunner { free: vec![] };
        let agent = "opencode";
        let model = "opencode/nemotron-3-super-free";
        let key = format!("{agent}:{model}");

        let err = AgentError::RateLimit {
            message: "Provider returned error".to_string(),
        };

        let _result = handle_error(
            "test-2478-provider-cooldown",
            &err,
            agent,
            &runner,
            Some(model),
            Some("simple"),
            1,
            &None,
            "owner/repo",
        )
        .await
        .unwrap();

        let now = chrono::Utc::now().timestamp();
        let until = crate::engine::cooldown::cooldown_until(&key)
            .expect("provider error should set model cooldown");
        let remaining = until.saturating_sub(now);

        // Allow a wider margin for scheduler/runtime jitter in concurrent test runs.
        assert!(
            remaining >= crate::engine::cooldown::PERSISTENT_MODEL_BACKOFF_BASE_SECS - 30,
            "expected ~4h model cooldown, got {remaining}s"
        );
    }

    #[tokio::test]
    async fn silent_exit0_retries_free_model_for_simple_complexity() {
        let runner = MockRunner {
            free: vec!["opencode/mimo-v2-omni-free".to_string()],
        };
        let err = AgentError::Unknown {
            exit_code: 0,
            message: String::new(),
        };

        // complexity=simple → free model retry is appropriate
        let result = handle_error(
            "test-1112-a",
            &err,
            "opencode",
            &runner,
            Some("opencode/gpt-5.4-mini"),
            Some("simple"),
            1,
            &None,
            "owner/repo",
        )
        .await
        .unwrap();

        assert!(
            matches!(result, ErrorHandleResult::EarlyReturn { ref status } if status == "new"),
            "expected EarlyReturn{{status: new}} for simple complexity — free model should be tried first"
        );
    }

    #[tokio::test]
    async fn silent_exit0_retries_free_model_when_complexity_unknown() {
        let runner = MockRunner {
            free: vec!["opencode/mimo-v2-omni-free".to_string()],
        };
        let err = AgentError::Unknown {
            exit_code: 0,
            message: String::new(),
        };

        // complexity=None (unknown) → treated as simple → free model retry
        let result = handle_error(
            "test-1112-a2",
            &err,
            "opencode",
            &runner,
            Some("opencode/gpt-5.4-mini"),
            None,
            1,
            &None,
            "owner/repo",
        )
        .await
        .unwrap();

        assert!(
            matches!(result, ErrorHandleResult::EarlyReturn { ref status } if status == "new"),
            "expected EarlyReturn{{status: new}} for unknown complexity — free model should be tried first"
        );
    }

    #[tokio::test]
    async fn silent_exit0_retries_free_model_for_medium_complexity() {
        let runner = MockRunner {
            free: vec!["opencode/mimo-v2-omni-free".to_string()],
        };
        let err = AgentError::Unknown {
            exit_code: 0,
            message: String::new(),
        };

        // complexity=medium → still retry free model before cross-agent failover
        let result = handle_error(
            "test-1195-medium",
            &err,
            "opencode",
            &runner,
            Some("opencode/copilot-gpt-5.4-mini"),
            Some("medium"),
            1,
            &None,
            "owner/repo",
        )
        .await
        .unwrap();

        assert!(
            matches!(result, ErrorHandleResult::EarlyReturn { ref status } if status == "new"),
            "expected EarlyReturn{{status: new}} for medium complexity — free model should be tried before failover"
        );
    }

    #[tokio::test]
    async fn silent_exit0_retries_free_model_for_complex_complexity() {
        let runner = MockRunner {
            free: vec!["opencode/mimo-v2-omni-free".to_string()],
        };
        let err = AgentError::Unknown {
            exit_code: 0,
            message: String::new(),
        };

        // complexity=complex → still retry free model before cross-agent failover
        let result = handle_error(
            "test-1195-complex",
            &err,
            "opencode",
            &runner,
            Some("opencode/copilot-gpt-5.4-mini"),
            Some("complex"),
            1,
            &None,
            "owner/repo",
        )
        .await
        .unwrap();

        assert!(
            matches!(result, ErrorHandleResult::EarlyReturn { ref status } if status == "new"),
            "expected EarlyReturn{{status: new}} for complex complexity — free model should be tried before failover"
        );
    }

    /// Context overflow is deterministic — the same context will overflow every agent.
    /// Verify that it escalates directly to needs_review without cycling through fallback agents.
    #[tokio::test]
    async fn context_overflow_escalates_to_needs_review_immediately() {
        let runner = MockRunner { free: vec![] };
        let err = AgentError::ContextOverflow {
            message: "prompt is too long: 250000 tokens, max 200000".to_string(),
        };

        let result = handle_error(
            "test-2129-a",
            &err,
            "claude",
            &runner,
            Some("sonnet"),
            Some("medium"),
            1,
            &None,
            "owner/repo",
        )
        .await
        .unwrap();

        assert!(
            matches!(result, ErrorHandleResult::EarlyReturn { ref status } if status == "needs_review"),
            "context overflow must escalate directly to needs_review without cycling through fallback agents"
        );
    }

    /// When store is None, store_set_result returns Ok(()) — reroute proceeds normally.
    /// This verifies the happy path is not broken by the error-propagation change.
    #[tokio::test]
    async fn free_model_reroute_succeeds_without_store() {
        let runner = MockRunner {
            free: vec!["opencode/mimo-v2-omni-free".to_string()],
        };
        let err = AgentError::Unknown {
            exit_code: 0,
            message: String::new(),
        };

        // store=None → store_set_result returns Ok(()) → reroute proceeds
        let result = handle_error(
            "test-2293-a",
            &err,
            "opencode",
            &runner,
            Some("opencode/gpt-5.4-mini"),
            Some("simple"),
            1,
            &None,
            "owner/repo",
        )
        .await
        .unwrap();

        assert!(
            matches!(result, ErrorHandleResult::EarlyReturn { ref status } if status == "new"),
            "expected EarlyReturn{{status: new}} — store_set_result with None store should not prevent reroute"
        );
    }

    #[tokio::test]
    async fn silent_exit0_falls_through_when_no_free_models() {
        let runner = MockRunner { free: vec![] };
        let err = AgentError::Unknown {
            exit_code: 0,
            message: String::new(),
        };

        let result = handle_error(
            "test-1112-b",
            &err,
            "opencode",
            &runner,
            Some("opencode/gpt-5.4-mini"),
            Some("simple"),
            1,
            &None,
            "owner/repo",
        )
        .await
        .unwrap();

        assert!(
            matches!(result, ErrorHandleResult::Continue { .. }),
            "expected Continue (fallthrough to failover) when no free models available"
        );
    }

    /// Verify that InvalidResponse applies model-level cooldown.
    ///
    /// This fixes issue #2750: InvalidResponse used to skip model-level cooldown,
    /// allowing models that produced unparseable responses to be immediately
    /// retried on the next task without any backoff.
    #[tokio::test]
    async fn invalid_response_sets_model_cooldown() {
        let runner = MockRunner { free: vec![] };
        let agent = "opencode";
        let model = "opencode/nemotron-3-super-free";

        // Confirm no model cooldown before the error.
        assert!(
            !crate::engine::cooldown::is_model_in_cooldown(agent, model),
            "model should not be in cooldown before handle_error"
        );

        let err = AgentError::InvalidResponse {
            raw: "invalid json output".to_string(),
        };

        let _result = handle_error(
            "test-2750-invalid-response",
            &err,
            agent,
            &runner,
            Some(model),
            Some("simple"),
            1,
            &None,
            "owner/repo",
        )
        .await
        .unwrap();

        // Model-level cooldown must be set after InvalidResponse.
        assert!(
            crate::engine::cooldown::is_model_in_cooldown(agent, model),
            "model should be in cooldown after InvalidResponse (fixes issue #2750)"
        );
    }

    #[tokio::test]
    async fn summarize_rate_limit_error_extracts_api_retry_info() {
        let raw_output = r#"some log lines
{"type":"system","subtype":"api_retry","attempt":6,"max_retries":10,"retry_delay_ms":17266.789,"error_status":429,"error":"rate_limit","session_id":"331c099b-..."}
{"type":"system","subtype":"api_retry","attempt":7,"max_retries":10,"retry_delay_ms":35162.66,"error_status":429,"error":"rate_limit","session_id":"331c099b-..."}
more logs"#;

        let summarized = super::summarize_rate_limit_error(raw_output);
        assert_eq!(summarized, "status=429 after 7 attempts (last delay 35s)");
    }

    #[tokio::test]
    async fn summarize_rate_limit_error_returns_original_when_no_api_retry() {
        let raw_output = "some regular error message without api_retry JSON";

        let summarized = super::summarize_rate_limit_error(raw_output);
        assert_eq!(summarized, raw_output);
    }

    #[tokio::test]
    async fn summarize_rate_limit_error_handles_malformed_json_gracefully() {
        let raw_output = r#"some logs
{"type":"system","subtype":"api_retry","attempt":"not_a_number","retry_delay_ms":invalid,"error_status":429}
more logs"#;

        let summarized = super::summarize_rate_limit_error(raw_output);
        // Should still return a formatted string even with invalid numbers
        assert!(summarized.starts_with("status="));
        assert!(summarized.contains("after"));
        assert!(summarized.contains("attempts"));
        assert!(summarized.contains("(last delay"));
    }

    #[tokio::test]
    async fn summarize_rate_limit_error_works_with_single_line() {
        let raw_output = r#"{"type":"system","subtype":"api_retry","attempt":3,"max_retries":10,"retry_delay_ms":5000,"error_status":429,"error":"rate_limit"}"#;

        let summarized = super::summarize_rate_limit_error(raw_output);
        assert_eq!(summarized, "status=429 after 3 attempts (last delay 5s)");
    }

    /// Verify extract_json_field correctly handles quoted string values.
    /// Previously it stripped from the wrong end (strip_suffix), returning None for
    /// any field that isn't the last one in the JSON object (fixes issue #2817).
    #[tokio::test]
    async fn extract_json_field_handles_quoted_strings() {
        let json = r#"{"error_status":429,"attempt":7,"retry_delay_ms":35162.66,"error":"rate_limit","session_id":"331c099b-..."}"#;

        assert_eq!(
            super::extract_json_field(json, "\"error\":"),
            Some("rate_limit".to_string()),
            "quoted string field mid-object should be extracted correctly"
        );
        assert_eq!(
            super::extract_json_field(json, "\"session_id\":"),
            Some("331c099b-...".to_string()),
            "quoted string field at end of object (no trailing comma) should be extracted"
        );
        // Numeric fields should still work (regression check)
        assert_eq!(
            super::extract_json_field(json, "\"error_status\":"),
            Some("429".to_string())
        );
        assert_eq!(
            super::extract_json_field(json, "\"attempt\":"),
            Some("7".to_string())
        );
        assert_eq!(
            super::extract_json_field(json, "\"retry_delay_ms\":"),
            Some("35162.66".to_string())
        );
    }

    /// Truncated fragment starting mid-object with ,"subtype":"api_retry"...
    /// Should NOT return raw JSON blob (regression test for issue #2839).
    #[tokio::test]
    async fn summarize_rate_limit_error_handles_truncated_fragment_with_subtype() {
        // Real-world truncated fragment from GLM runs (no "type":"system" prefix)
        let raw_output = r#"some log context
,"subtype":"api_retry","attempt":5,"retry_delay_ms":20000,"error_status":429,"error":"rate_limit"
more context"#;

        let summarized = super::summarize_rate_limit_error(raw_output);
        assert!(
            !summarized.contains("subtype"),
            "summarized output must not contain raw 'subtype' JSON fragment"
        );
        assert!(
            !summarized.contains("{"),
            "summarized output must not contain JSON braces"
        );
        assert!(
            summarized.starts_with("status="),
            "summarized output should start with 'status=' — got: {summarized}"
        );
        // Note: "api_retry" is only included when fields are not extractable
        // (the fragment fallback path). When attempt/delay are extractable,
        // the summary is "status=X after N attempts (last delay Ys)" with no "api_retry".
    }

    /// Fragment with attempt and delay fields extractable from mid-fragment.
    #[tokio::test]
    async fn summarize_rate_limit_error_handles_fragment_with_extractable_fields() {
        let raw_output = r#"log output
,"subtype":"api_retry","attempt":5,"retry_delay_ms":20000,"error_status":429,"error":"rate_limit","session_id":"abc123","max_retries":10}
rest of output"#;

        let summarized = super::summarize_rate_limit_error(raw_output);
        assert_eq!(summarized, "status=429 after 5 attempts (last delay 20s)");
    }

    /// Complete JSON (no truncation) still works after removing "type":"system" requirement.
    #[tokio::test]
    async fn summarize_rate_limit_error_complete_json_still_works() {
        let raw_output = r#"some log lines
{"type":"system","subtype":"api_retry","attempt":6,"max_retries":10,"retry_delay_ms":17266.789,"error_status":429,"error":"rate_limit","session_id":"331c099b-..."}
{"type":"system","subtype":"api_retry","attempt":7,"max_retries":10,"retry_delay_ms":35162.66,"error_status":429,"error":"rate_limit","session_id":"331c099b-..."}
more logs"#;

        let summarized = super::summarize_rate_limit_error(raw_output);
        assert_eq!(summarized, "status=429 after 7 attempts (last delay 35s)");
    }

    /// When no api_retry JSON is found at all, should NOT return raw passthrough.
    #[tokio::test]
    async fn summarize_rate_limit_error_no_api_retry_no_raw_passthrough() {
        let raw_output = "some regular error message without api_retry JSON that is quite long and should be truncated";

        let summarized = super::summarize_rate_limit_error(raw_output);
        // Should be truncated to first line, not the full raw_output
        assert!(
            summarized.len() <= 140,
            "expected truncated first-line, got: {summarized}"
        );
    }

    /// Short error without api_retry: should return the line as-is (no truncation needed).
    #[tokio::test]
    async fn summarize_rate_limit_error_no_api_retry_short() {
        let raw_output = "rate limit exceeded";

        let summarized = super::summarize_rate_limit_error(raw_output);
        assert_eq!(summarized, raw_output);
    }

    /// "Model not found" is a permanent failure — verify it applies the persistent (4h base)
    /// cooldown rather than the standard transient (5min base) cooldown.
    ///
    /// Regression test for issue #2941: dead models were retried every 4h indefinitely
    /// because "not found" was treated the same as a transient unavailability.
    #[tokio::test]
    async fn model_not_found_applies_persistent_cooldown() {
        let runner = MockRunner { free: vec![] };
        let agent = "opencode-2941-not-found";
        let model = "github-copilot/claude-opus-4.6";
        let key = format!("{agent}:{model}");

        let err = AgentError::ModelUnavailable {
            model: model.to_string(),
            message: "Model not found: github-copilot/claude-opus-4.6".to_string(),
        };

        let _result = handle_error(
            "test-2941-a",
            &err,
            agent,
            &runner,
            Some(model),
            Some("complex"),
            1,
            &None,
            "owner/repo",
        )
        .await
        .unwrap();

        let now = chrono::Utc::now().timestamp();
        let until = crate::engine::cooldown::cooldown_until(&key)
            .expect("model not found should set model cooldown");
        let remaining = until.saturating_sub(now);

        // Persistent cooldown starts at 4h; transient starts at 5min.
        // Verify we applied the persistent (4h base) path, not the short one.
        assert!(
            remaining >= crate::engine::cooldown::PERSISTENT_MODEL_BACKOFF_BASE_SECS - 30,
            "expected ~4h (persistent) cooldown for 'Model not found', got {remaining}s — \
             may have applied transient 5min cooldown instead"
        );
    }

    /// Transient model unavailability (not "not found") should use the standard short cooldown.
    #[tokio::test]
    async fn model_unavailable_transient_applies_short_cooldown() {
        let runner = MockRunner { free: vec![] };
        let agent = "opencode-2941-transient";
        let model = "github-copilot/gpt-4o";
        let key = format!("{agent}:{model}");

        let err = AgentError::ModelUnavailable {
            model: model.to_string(),
            message: "No endpoints found for github-copilot/gpt-4o.".to_string(),
        };

        let _result = handle_error(
            "test-2941-b",
            &err,
            agent,
            &runner,
            Some(model),
            Some("medium"),
            1,
            &None,
            "owner/repo",
        )
        .await
        .unwrap();

        let now = chrono::Utc::now().timestamp();
        let until = crate::engine::cooldown::cooldown_until(&key)
            .expect("transient model unavailability should set model cooldown");
        let remaining = until.saturating_sub(now);

        // Standard (transient) backoff base is 5min. Persistent base is 4h.
        // Confirm we did NOT apply the long 4h cooldown for a transient error.
        assert!(
            remaining < crate::engine::cooldown::PERSISTENT_MODEL_BACKOFF_BASE_SECS,
            "transient unavailability should use short cooldown (<4h), got {remaining}s"
        );
    }
}
