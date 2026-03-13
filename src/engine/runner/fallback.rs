//! Error recovery and fallback strategies.
//!
//! Extracted from `runner/mod.rs`. Handles the `Err(agent_err)` arm of the
//! parse result: error classification, model failover, free-model fallback,
//! and agent rerouting.

use crate::db::Db;
use crate::store::TaskStore;
use std::sync::Arc;

use super::{agents, response};

/// How `run()` should proceed after error handling.
pub enum ErrorHandleResult {
    /// Task was rerouted — `run()` should record metrics then return early.
    EarlyReturn { status: String },
    /// Normal failover applied (or task marked needs_review) — proceed to cleanup + metrics.
    Continue { status: String },
}

/// Handle an agent error: classify it, attempt recovery strategies, and update sidecar.
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
    new_attempts: u32,
    db: Option<&Arc<Db>>,
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
        agents::AgentError::RateLimit { message, .. } => (
            response::RetryableError::UsageLimit,
            format!("{agent_name} rate limit: {message}"),
        ),
        agents::AgentError::Auth { message } => (
            response::RetryableError::AuthError,
            format!("{agent_name} auth error: {message}"),
        ),
        agents::AgentError::Timeout { elapsed } => (
            response::RetryableError::Timeout,
            format!("{agent_name} timed out after {}s", elapsed.as_secs()),
        ),
        agents::AgentError::MissingTool { tool } => (
            response::RetryableError::MissingTooling,
            format!("missing tool: {tool}"),
        ),
        agents::AgentError::ModelUnavailable { model, .. } => {
            // Record model-specific cooldown (1 hour ban)
            response::record_model_failure(agent_name, model);

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
                crate::engine::cleanup::store_and_sidecar_set(
                    store,
                    repo,
                    task_id,
                    &[
                        format!("model={next}"),
                        format!("last_error={msg}"),
                        "error_type=failed".to_string(),
                    ],
                    &[
                        ("model", serde_json::json!(next.to_string())),
                        ("last_error", serde_json::json!(msg)),
                    ],
                )
                .await;
                // Skip normal failover — we're retrying same agent with different model
                return Ok(ErrorHandleResult::EarlyReturn {
                    status: "new".to_string(),
                });
            }
            (
                response::RetryableError::Failed,
                format!("model {model} unavailable"),
            )
        }
        agents::AgentError::ContextOverflow { .. } => {
            // Could truncate and retry, but for now treat as failed
            (
                response::RetryableError::Failed,
                format!("{agent_name} context overflow"),
            )
        }
        agents::AgentError::WaitingForInput { message } => {
            // Requires human — skip failover, go straight to needs_review
            let msg = format!("waiting for input: {message}");
            crate::engine::cleanup::store_and_sidecar_set(
                store,
                repo,
                task_id,
                &[format!("last_error={msg}"), "error_type=failed".to_string()],
                &[("last_error", serde_json::json!(msg))],
            )
            .await;
            return Ok(ErrorHandleResult::EarlyReturn {
                status: "needs_review".to_string(),
            });
        }
        agents::AgentError::PermissionDenied { message } => (
            response::RetryableError::Failed,
            format!("permission denied: {message}"),
        ),
        agents::AgentError::InvalidResponse { .. } => (
            response::RetryableError::Failed,
            format!("{agent_name} invalid response"),
        ),
        agents::AgentError::AgentFailed { message } => (
            response::RetryableError::Failed,
            format!("{agent_name} failed: {message}"),
        ),
        agents::AgentError::Unknown { exit_code, message } => (
            response::RetryableError::Failed,
            format!("{agent_name} exit {exit_code}: {message}"),
        ),
    };

    // Record rate limit in DB for rate-limit and auth errors
    if let Some(db) = db {
        let error_type_str = match retryable {
            response::RetryableError::UsageLimit => "rate",
            response::RetryableError::AuthError => "budget",
            _ => "error",
        };
        let _ = db
            .record_rate_limit(agent_name, error_type_str, Some(task_id))
            .await;
    }

    // Try free models as last resort before giving up
    let chain = response::get_reroute_chain(task_id, store, repo).await;
    let available: Vec<String> = ["claude", "codex", "opencode", "kimi", "minimax"]
        .iter()
        .filter(|a| crate::cmd_cache::command_exists(a))
        .map(|s| s.to_string())
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

    if all_agents_tried {
        // All agents exhausted — try free models via opencode
        let free = agent_runner.free_models();
        if !free.is_empty() {
            let tried_models: String = crate::engine::cleanup::opt_store_or_sidecar(
                store,
                repo,
                task_id,
                "model_reroute_chain",
            )
            .await
            .unwrap_or_default();
            let tried_set: std::collections::HashSet<&str> =
                tried_models.split(',').filter(|s| !s.is_empty()).collect();

            if let Some(free_model) = free.iter().find(|m| !tried_set.contains(m.as_str())) {
                tracing::info!(task_id, model = %free_model, "last resort: trying free model via opencode");
                let new_tried = if tried_models.is_empty() {
                    free_model.clone()
                } else {
                    format!("{tried_models},{free_model}")
                };
                let msg = format!("all agents exhausted, trying free model {free_model}");
                crate::engine::cleanup::store_and_sidecar_set(
                    store,
                    repo,
                    task_id,
                    &[
                        "agent=opencode".to_string(),
                        format!("model={free_model}"),
                        format!("model_reroute_chain={new_tried}"),
                        format!("last_error={msg}"),
                        format!("error_type={}", retryable.type_str()),
                    ],
                    &[
                        ("agent", serde_json::json!("opencode")),
                        ("model", serde_json::json!(free_model.to_string())),
                        ("model_reroute_chain", serde_json::json!(new_tried)),
                        ("last_error", serde_json::json!(msg)),
                    ],
                )
                .await;
                return Ok(ErrorHandleResult::EarlyReturn {
                    status: "new".to_string(),
                });
            }
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

    Ok(ErrorHandleResult::Continue { status })
}
