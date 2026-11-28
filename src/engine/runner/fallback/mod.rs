//! Error recovery and fallback strategies.
//!
//! Extracted from `runner/mod.rs`. Handles the `Err(agent_err)` arm of the
//! parse result: error classification, model failover, free-model fallback,
//! and agent rerouting.

use crate::store;
use crate::store::TaskStore;
use std::collections::HashSet;
use std::sync::Arc;

use super::response;
use crate::cooldown;
use crate::cooldown::CreditExhaustionReason;
use crate::engine;
use crate::engine::agents;
use crate::sys::cmd_cache;

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
    // Byte cap for the first-line fallback summary below
    const FIRST_LINE_CAP: usize = 120;
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
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the saturating cast floors the delay to whole seconds"
        )]
        let retry_delay_s = retry_delay_s as u64;
        format!("status={error_status} after {attempt} attempts (last delay {retry_delay_s}s)")
    } else {
        let status_hint = extract_json_field(line, "\"status\":")
            .or_else(|| extract_json_field(line, "\"error\":"))
            .unwrap_or_else(|| "429".to_string());
        format!("status={status_hint} (api_retry fragment)")
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
                .map(ToString::to_string)
                .or_else(|| Some(num_str.to_string()))
        }
    })
}

/// How `run()` should proceed after error handling.
#[derive(Debug)]
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
#[expect(
    clippy::too_many_arguments,
    reason = "each caller picks a different mix of reroute options"
)]
#[expect(
    clippy::ref_option,
    reason = "the store helpers it calls take the store as &Option"
)]
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
    let tried_set: HashSet<&str> = tried_models.split(',').filter(|s| !s.is_empty()).collect();
    let next_free = free.iter().find(|m| {
        !exclude_models.contains(&m.as_str())
            && !tried_set.contains(m.as_str())
            && (!check_cooldowns
                || (!cooldown::is_agent_in_cooldown(agent_name)
                    && !response::is_model_in_cooldown(agent_name, m)))
    })?;
    tracing::info!(task_id, model = %next_free, "{}", reason_prefix);
    let new_tried = if tried_models.is_empty() {
        next_free.clone()
    } else {
        format!("{tried_models},{next_free}")
    };
    let msg = format!("{reason_prefix} {next_free}");
    let mut fields: Vec<(&str, serde_json::Value)> = vec![
        ("model", serde_json::json!(next_free.clone())),
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

/// Apply a vendor-provided future reset date as the model cooldown.
///
/// Skipped for credit exhaustion reasons other than billing cycle, which keep their agent-wide cooldowns.
/// Returns true when the cooldown was set.
async fn apply_vendor_reset_date(
    agent_name: &str,
    model_name: Option<&str>,
    message: &str,
) -> bool {
    let Some(model) = model_name else {
        return false;
    };
    match cooldown::detect_credit_exhaustion(message) {
        None | Some(CreditExhaustionReason::BillingCycleExhausted) => {}
        Some(_) => return false,
    }
    match cooldown::parse_retry_at(message) {
        Some(until) if until > chrono::Utc::now().timestamp() => {
            cooldown::set_model_cooldown_until(agent_name, model, until).await;
            true
        }
        _ => false,
    }
}

/// Handle an agent error: classify it, attempt recovery strategies, and update store.
///
/// Returns `Ok(ErrorHandleResult::EarlyReturn)` when the task was rerouted and
/// `run()` should record metrics and return immediately (skipping tmux cleanup).
/// Returns `Ok(ErrorHandleResult::Continue)` when `run()` should proceed to cleanup.
#[expect(
    clippy::too_many_arguments,
    reason = "the error path needs the task, agent, model and store context together"
)]
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

    // Map AgentError to RetryableError for handle_failover(). credit_recorded
    // skips the generic failure_count bump for already-recorded credit errors
    let mut credit_recorded = false;
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
            // Track whether the quota is model-scoped (BillingCycleExhausted + known model).
            // When true we return ModelUnavailable so handle_failover skips the
            // agent-level failure counter — the agent itself is healthy, only the
            // specific model's plan quota was exhausted (issue #3382).
            let mut billing_cycle_model_scoped = false;

            if apply_vendor_reset_date(agent_name, model_name, message).await {
                let reason = cooldown::detect_credit_exhaustion(message);
                credit_recorded = reason.is_some();
                billing_cycle_model_scoped =
                    reason == Some(CreditExhaustionReason::BillingCycleExhausted);
            } else if let Some(reason) = cooldown::detect_credit_exhaustion(message) {
                credit_recorded = true;
                // For billing cycle exhaustion where we know the specific model, apply
                // model-level persistent cooldown only. The quota is scoped to that
                // provider sub-model (e.g. github-copilot/gpt-5-mini), not the entire
                // agent account, so an agent-wide cooldown would incorrectly block
                // unrelated models on the same agent.
                if reason == CreditExhaustionReason::BillingCycleExhausted {
                    if let Some(model) = model_name {
                        cooldown::record_persistent_model_failure(agent_name, model).await;
                        billing_cycle_model_scoped = true;
                    } else {
                        cooldown::record_credit_exhaustion(agent_name, reason).await;
                    }
                } else {
                    cooldown::record_credit_exhaustion(agent_name, reason).await;
                }
            } else if let (Some(model), Some(window_secs)) =
                (model_name, cooldown::parse_relative_usage_window(message))
            {
                // Vendor-advertised relative usage window (e.g. "weekly (7-day)
                // usage limit") is authoritative — apply a model-specific cooldown
                // for that exact duration instead of the generic exponential
                // backoff. This is the same window-detection the `needs_review`
                // call site (cooldown::record_rate_limit()) already applies; this
                // in-task failover path bypassed it entirely, undoing the
                // #3572/#3574 fix for the most common rate-limit path (issue #3580).
                cooldown::set_model_cooldown(
                    agent_name,
                    model,
                    window_secs,
                    "rate_limit_usage_window",
                )
                .await;
                tracing::info!(
                    task_id,
                    agent = agent_name,
                    model,
                    window_secs,
                    "relative usage window detected: applying model-specific cooldown"
                );
            } else {
                cooldown::record_agent_failure_with_message(agent_name, message).await;
                // Also record the model-specific cooldown so model_for_complexity skips it.
                if let Some(model) = model_name {
                    if is_provider_returned_error {
                        cooldown::record_transient_model_failure(agent_name, model).await;
                    } else {
                        response::record_model_failure(agent_name, model).await;
                    }
                }
            }
            // Use ModelUnavailable when billing cycle is exhausted for a known model so
            // handle_failover does not call record_agent_failure_with_message (which would
            // set an unwanted agent-level cooldown). The agent's other models remain available.
            let retryable = if billing_cycle_model_scoped {
                response::RetryableError::ModelUnavailable
            } else {
                response::RetryableError::UsageLimit
            };
            (
                retryable,
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
            let mut billing_cycle_model_scoped = false;

            if apply_vendor_reset_date(agent_name, model_name, message).await {
                let reason = cooldown::detect_credit_exhaustion(message);
                credit_recorded = reason.is_some();
                billing_cycle_model_scoped =
                    reason == Some(CreditExhaustionReason::BillingCycleExhausted);
            } else if let Some(reason) = cooldown::detect_credit_exhaustion(message) {
                credit_recorded = true;
                // Billing cycle exhaustion scoped to a known model gets model-level
                // persistent cooldown only — agent-wide cooldown would block unrelated models.
                if reason == CreditExhaustionReason::BillingCycleExhausted {
                    if let Some(model) = model_name {
                        cooldown::record_persistent_model_failure(agent_name, model).await;
                        billing_cycle_model_scoped = true;
                    } else {
                        cooldown::record_credit_exhaustion(agent_name, reason).await;
                    }
                } else {
                    cooldown::record_credit_exhaustion(agent_name, reason).await;
                }
            } else {
                cooldown::record_agent_failure_with_message(agent_name, message).await;
                if let Some(model) = model_name {
                    response::record_model_failure(agent_name, model).await;
                }
            }
            // Use ModelUnavailable for model-scoped billing exhaustion so handle_failover
            // does not set an agent-level failure cooldown (issue #3382).
            let retryable = if billing_cycle_model_scoped {
                response::RetryableError::ModelUnavailable
            } else {
                response::RetryableError::AuthError
            };
            (retryable, format!("{agent_name} auth error: {message}"))
        }
        agents::AgentError::Timeout { elapsed } => {
            // A task time limit says the work was long, not that the model is
            // unavailable. Retry without a model cooldown or rate_limits row
            (
                response::RetryableError::Timeout,
                format!("{agent_name} timed out after {}s", elapsed.as_secs()),
            )
        }
        agents::AgentError::Interrupted { message } => (
            response::RetryableError::Interrupted,
            format!("{agent_name} interrupted: {message}"),
        ),
        agents::AgentError::MissingTool { tool } => (
            response::RetryableError::MissingTooling,
            format!("missing tool: {tool}"),
        ),
        agents::AgentError::ModelUnavailable { model, message } => {
            cooldown::record_model_unavailable(agent_name, model, message).await;

            // Try next model before switching agent, unless the agent is cooled
            // (an agent-wide cooldown must not be defeated by model substitution)
            let agent_cooled = cooldown::is_agent_in_cooldown(agent_name);
            let models = agent_runner.available_models();
            let current_model = model_name.unwrap_or("");
            let next_model = if agent_cooled {
                None
            } else {
                models.iter().find(|m| {
                    m.as_str() != current_model
                        && m.as_str() != model
                        && !response::is_model_in_cooldown(agent_name, m)
                })
            };
            if let Some(next) = next_model {
                tracing::info!(task_id, model = %next, "retrying with different model");
                let msg = format!("model {model} unavailable, trying {next}");
                if let Err(e) = store::store_set_result(
                    store,
                    repo,
                    task_id,
                    &[
                        ("model", serde_json::json!(next.clone())),
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
            if is_simple
                && let Some(result) = try_free_model_reroute(
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

            (
                response::RetryableError::ModelUnavailable,
                format!("model {model} unavailable"),
            )
        }
        agents::AgentError::ContextOverflow { message } => {
            // Context overflow is deterministic — the task's prompt/context is too large
            // for this model's context window. Cycling through other agents would hit the
            // same limit and waste API calls. Skip failover entirely and escalate
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
            // Credit-shaped errors that escaped the Auth/RateLimit classifiers still
            // need the credit cooldown and counter, not the generic 5 min backoff.
            if let Some(reason) = cooldown::detect_credit_exhaustion(message) {
                credit_recorded = true;
                match model_name {
                    Some(model) if reason == CreditExhaustionReason::BillingCycleExhausted => {
                        cooldown::record_persistent_model_failure(agent_name, model).await;
                    }
                    _ => cooldown::record_credit_exhaustion(agent_name, reason).await,
                }
            } else {
                if let Some(model) = model_name {
                    response::record_model_failure(agent_name, model).await;
                }
                cooldown::record_agent_failure_with_message(agent_name, message).await;
            }
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
        agents::AgentError::ThinkingBlockConflict { message } => {
            // Transient Anthropic API 400 — thinking blocks modified in multi-turn
            // tool-use. Not a model capacity or rate-limit problem.
            // Do NOT record failure counts or model/agent cooldowns.
            // Pass ModelUnavailable so handle_failover skips agent failure recording
            // while still updating the reroute chain to avoid re-selecting claude.
            (
                response::RetryableError::ModelUnavailable,
                format!("{agent_name} thinking block conflict: {message}"),
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
                    // A stopped session also exits 0 with no output, so the backoff stays short
                    cooldown::record_transient_model_failure(agent_name, m).await;
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
                    "{agent_name} exited with non-zero status (cost telemetry in stdout, no error message)"
                )
            } else {
                format!("{agent_name} exit {exit_code}: {message}")
            };

            (response::RetryableError::Failed, error_message)
        }
    };

    // Timeouts and interrupts clear the agent and skip the rate_limits row
    if matches!(
        retryable,
        response::RetryableError::Timeout | response::RetryableError::Interrupted
    ) && let Err(e) = store::store_set_result(
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

    // Record rate limit in store (sqlx). Timeouts and interrupted runs are
    // not usage limits, so they must not write a row
    {
        let error_type_str = match retryable {
            response::RetryableError::UsageLimit => Some("rate"),
            response::RetryableError::AuthError => Some("auth"),
            response::RetryableError::Timeout | response::RetryableError::Interrupted => None,
            _ => Some("error"),
        };
        if let Some(limit_type) = error_type_str
            && let Some(s) = store
            && let Err(e) = s
                .record_rate_limit(agent_name, limit_type, Some(task_id))
                .await
        {
            tracing::warn!(task_id, agent = %agent_name, err = %e, "failed to persist rate_limit event — router health check may miss this signal");
        }
    }

    // Try free models as last resort before giving up
    let chain = response::get_reroute_chain(task_id, store, repo).await;
    let available: Vec<String> = engine::configured_agents()
        .into_iter()
        .filter(|a| cmd_cache::command_exists(a))
        .collect();

    let all_agents_tried = {
        let chain_set: HashSet<&str> = if chain.is_empty() {
            HashSet::new()
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

    let outcome = response::handle_failover(
        task_id,
        agent_name,
        retryable,
        &error_msg,
        credit_recorded,
        store,
        repo,
    )
    .await;
    match (outcome.status.as_str(), outcome.next_agent.as_deref()) {
        ("new", Some(next)) => {
            tracing::info!(task_id, to = next, "failover: re-routing to fallback agent");
        }
        ("new", None) => {
            tracing::warn!(task_id, "failover exhausted (retryable), task reset to new");
        }
        ("blocked", _) => {
            tracing::warn!(task_id, "failover exhausted (hard failure), task blocked");
        }
        _ => {}
    }
    let status = outcome.status;

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
mod tests;
