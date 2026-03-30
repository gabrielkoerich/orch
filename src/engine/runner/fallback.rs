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
            // Check for credit exhaustion - these require longer agent-wide cooldowns
            if let Some(reason) = crate::engine::cooldown::detect_credit_exhaustion(message) {
                crate::engine::cooldown::record_credit_exhaustion(agent_name, reason);
            }
            (
                response::RetryableError::UsageLimit,
                format!("{agent_name} rate limit: {message}"),
            )
        }
        agents::AgentError::Auth { message } => {
            // Check for credit exhaustion in auth errors (billing-related)
            if let Some(reason) = crate::engine::cooldown::detect_credit_exhaustion(message) {
                crate::engine::cooldown::record_credit_exhaustion(agent_name, reason);
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
            // Transient connectivity failure — retry same agent, no reroute chain update.
            let msg = format!("{agent_name} network error: {message}");
            store::store_set(
                store,
                repo,
                task_id,
                &[("last_error", serde_json::json!(msg))],
            )
            .await;
            return Ok(ErrorHandleResult::EarlyReturn {
                status: "new".to_string(),
            });
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
                let free = agent_runner.free_models();
                if is_simple && !free.is_empty() {
                    let tried_models: String = store::opt_store_get_task(store, repo, task_id)
                        .await
                        .map(|t| t.model_reroute_chain)
                        .unwrap_or_default();
                    let tried_set: std::collections::HashSet<&str> =
                        tried_models.split(',').filter(|s| !s.is_empty()).collect();
                    let current = model_name.unwrap_or("");
                    if let Some(next_free) = free.iter().find(|m| {
                        m.as_str() != current
                            && !tried_set.contains(m.as_str())
                            && !response::is_model_in_cooldown(agent_name, m)
                    }) {
                        let new_tried = if tried_models.is_empty() {
                            next_free.clone()
                        } else {
                            format!("{tried_models},{next_free}")
                        };
                        let msg = format!("silent exit 0, retrying with free model {next_free}");
                        tracing::info!(
                            task_id,
                            model = %next_free,
                            "silent exit-0: retrying with free model"
                        );
                        store::store_set(
                            store,
                            repo,
                            task_id,
                            &[
                                ("agent", serde_json::json!("opencode")),
                                ("model", serde_json::json!(next_free.to_string())),
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

    let is_simple = matches!(complexity, None | Some("simple"));
    if all_agents_tried && is_simple {
        // All agents exhausted — try free models via opencode (only for simple tasks)
        let free = agent_runner.free_models();
        if !free.is_empty() {
            let tried_models: String = store::opt_store_get_task(store, repo, task_id)
                .await
                .map(|t| t.model_reroute_chain)
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
                store::store_set(
                    store,
                    repo,
                    task_id,
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
