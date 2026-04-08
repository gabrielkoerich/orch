//! Error recovery and fallback strategies.
//!
//! Extracted from `runner/mod.rs`. Handles the `Err(agent_err)` arm of the
//! parse result: error classification, model failover, free-model fallback,
//! and agent rerouting.

use crate::store;
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

/// Try to reroute the task to an untried free model.
///
/// Returns `Some(EarlyReturn { status: "routed" })` if a free model was found and the
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
    store::store_set(store, repo, task_id, &fields).await;
    if apply_backoff {
        response::wait_for_fallback_backoff(task_id, store, repo).await;
    }
    Some(ErrorHandleResult::EarlyReturn {
        status: "routed".to_string(),
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
                    response::record_model_failure(agent_name, model).await;
                }
            }
            (
                response::RetryableError::UsageLimit,
                format!("{agent_name} rate limit: {message}"),
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
        agents::AgentError::ModelUnavailable { model, .. } => {
            // Record model-specific cooldown with exponential backoff
            response::record_model_failure(agent_name, model).await;

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
                store::store_set(
                    store,
                    repo,
                    task_id,
                    &[
                        ("model", serde_json::json!(next.to_string())),
                        ("last_error", serde_json::json!(msg)),
                    ],
                )
                .await;
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
                response::RetryableError::Failed,
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
            store::store_set(
                store,
                repo,
                task_id,
                &[("last_error", serde_json::json!(msg))],
            )
            .await;
            return Ok(ErrorHandleResult::EarlyReturn {
                status: "needs_review".to_string(),
            });
        }
        agents::AgentError::WaitingForInput { message } => {
            // Requires human — skip failover, go straight to needs_review
            let msg = format!("waiting for input: {message}");
            store::store_set(
                store,
                repo,
                task_id,
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
        agents::AgentError::NetworkError { message } => {
            // Transient connectivity failure — retry same agent without rerouting,
            // but grow a dedicated streak counter so backoff keeps increasing.
            // Use "routed" so it re-dispatches without a full re-routing cycle.
            // After MAX_NETWORK_RETRIES consecutive network errors, escalate to
            // needs_review so a human is notified rather than looping forever.
            const MAX_NETWORK_RETRIES: u64 = 8;
            let msg = format!("{agent_name} network error: {message}");
            let retry_count =
                match store::store_increment(store, repo, task_id, "network_retries").await {
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
                        tracing::warn!(task_id, error = %e, "failed to increment network_retries");
                        0
                    }
                };
            store::store_set(
                store,
                repo,
                task_id,
                &[("last_error", serde_json::json!(msg))],
            )
            .await;
            if retry_count >= MAX_NETWORK_RETRIES {
                tracing::warn!(
                    task_id,
                    retry_count,
                    agent = agent_name,
                    "network retries exhausted, escalating to needs_review"
                );
                return Ok(ErrorHandleResult::Continue {
                    status: "needs_review".to_string(),
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
            if *exit_code == 0 && message.is_empty() {
                if let Some(m) = model_name {
                    // Use a 4-hour cooldown for silent exits instead of the default 1-hour
                    // model cooldown.  These models (especially github-copilot/* in opencode)
                    // fail consistently across multiple hours; a 1-hour window means ~12
                    // wasted 2-minute attempts per day per model.  4 hours cuts that to ~3.
                    crate::engine::cooldown::set_model_cooldown(agent_name, m, 4 * 3600);
                }
                // Before falling through to handle_failover() (which tries claude/codex),
                // check whether this agent has any free models that haven't been tried yet.
                // Only do this for simple-complexity tasks: free models are the right tier
                // for simple tasks.  For medium/complex tasks, fall through to
                // handle_failover() so the next agent is chosen at the same complexity
                // level (e.g. claude/sonnet or codex/gpt-5.2) instead of a weaker model.
                let is_simple = matches!(complexity, None | Some("simple"));
                let current = model_name.unwrap_or("");
                if is_simple {
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
            }
            (
                response::RetryableError::Failed,
                format!("{agent_name} exit {exit_code}: {message}"),
            )
        }
    };

    // If this was a timeout, clear the stored agent so the router can
    // pick a different executor on re-dispatch. Without this the same
    // slow agent can be picked again and will timeout repeatedly.
    if retryable == response::RetryableError::Timeout {
        store::store_set(
            store,
            repo,
            task_id,
            &[
                ("agent", serde_json::json!("")),
                ("last_error", serde_json::json!(error_msg.clone())),
            ],
        )
        .await;
    }

    // Record rate limit in store (sqlx)
    {
        let error_type_str = match retryable {
            response::RetryableError::UsageLimit => "rate",
            response::RetryableError::AuthError => "budget",
            _ => "error",
        };
        if let Some(ref s) = store {
            let _ = s
                .record_rate_limit(agent_name, error_type_str, Some(task_id))
                .await;
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

    Ok(ErrorHandleResult::Continue { status })
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
            matches!(result, ErrorHandleResult::EarlyReturn { ref status } if status == "routed"),
            "expected EarlyReturn{{status: routed}} for simple complexity — free model should be tried first"
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
            matches!(result, ErrorHandleResult::EarlyReturn { ref status } if status == "routed"),
            "expected EarlyReturn{{status: routed}} for unknown complexity — free model should be tried first"
        );
    }

    #[tokio::test]
    async fn silent_exit0_skips_free_model_for_medium_complexity() {
        let runner = MockRunner {
            free: vec!["opencode/mimo-v2-omni-free".to_string()],
        };
        let err = AgentError::Unknown {
            exit_code: 0,
            message: String::new(),
        };

        // complexity=medium → skip free model retry, fall through to agent failover
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
            matches!(result, ErrorHandleResult::Continue { .. }),
            "expected Continue for medium complexity — should fall through to agent failover, not retry with free model"
        );
    }

    #[tokio::test]
    async fn silent_exit0_skips_free_model_for_complex_complexity() {
        let runner = MockRunner {
            free: vec!["opencode/mimo-v2-omni-free".to_string()],
        };
        let err = AgentError::Unknown {
            exit_code: 0,
            message: String::new(),
        };

        // complexity=complex → skip free model retry, fall through to agent failover
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
            matches!(result, ErrorHandleResult::Continue { .. }),
            "expected Continue for complex complexity — should fall through to agent failover, not retry with free model"
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
}
