use std::time::Duration;

use super::*;
use crate::engine::agents::StreamEvent;
use crate::engine::agents::parser::AgentResponse;
use crate::engine::agents::{AgentError, AgentRunner, ParsedResponse, PermissionRules};
use chrono::Datelike;
use serial_test::serial;
use tokio::process::Command;

struct MockRunner {
    free: Vec<String>,
}

struct MockRunnerWithModels {
    models: Vec<String>,
}

impl AgentRunner for MockRunnerWithModels {
    fn stream_events(&self, _event: &serde_json::Value) -> Vec<StreamEvent> {
        vec![]
    }

    fn name(&self) -> &'static str {
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
                screenshots: vec![],
                screenshot_note: None,
            },
            input_tokens: None,
            output_tokens: None,
            cost_usd: None,
            duration_ms: None,
        })
    }

    fn classify_error(&self, _exit_code: i32, _stdout: &str, _stderr: &str) -> AgentError {
        AgentError::Unknown {
            exit_code: 0,
            message: String::new(),
        }
    }

    fn available_models(&self) -> Vec<String> {
        self.models.clone()
    }

    fn router_command(&self, _prompt: &str, _model: Option<&str>) -> anyhow::Result<Command> {
        anyhow::bail!("not implemented")
    }
}

impl AgentRunner for MockRunner {
    fn stream_events(&self, _event: &serde_json::Value) -> Vec<StreamEvent> {
        vec![]
    }

    fn name(&self) -> &'static str {
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
                screenshots: vec![],
                screenshot_note: None,
            },
            input_tokens: None,
            output_tokens: None,
            cost_usd: None,
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

    fn router_command(&self, _prompt: &str, _model: Option<&str>) -> anyhow::Result<Command> {
        anyhow::bail!("not implemented")
    }
}

/// Verify that a generic (non-credit-exhaustion) rate limit error sets the agent
/// cooldown immediately in `handle_error()`, before `handle_failover()` is called.
///
/// This prevents concurrent dispatches from starting new runs against the same
/// rate-limited agent while the first task is still in its error-handling path.
#[serial(cooldown_state)]
#[tokio::test]
async fn rate_limit_sets_agent_cooldown_immediately() {
    cooldown::reset_global_state().await;
    let runner = MockRunner { free: vec![] };
    let agent = "test-agent-1371-early-cooldown";

    // Confirm no cooldown before the error.
    assert!(
        !cooldown::is_agent_in_cooldown(agent),
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
        cooldown::is_agent_in_cooldown(agent),
        "agent should be in cooldown immediately after handle_error for RateLimit"
    );
}

/// Verify that a rate limit error sets BOTH the agent-level and model-level cooldowns.
///
/// This is critical: the router's model selection (model_for_complexity) checks
/// is_model_in_cooldown(agent, model) which uses the agent:model key format.
/// An agent-level cooldown alone does NOT prevent the same model from being
/// re-selected, causing the rate-limited model to fail repeatedly (issue #2153).
#[serial(cooldown_state)]
#[tokio::test]
async fn rate_limit_sets_both_agent_and_model_cooldowns() {
    cooldown::reset_global_state().await;
    let runner = MockRunner { free: vec![] };
    let agent = "test-agent-2153-model-cooldown";
    let model = "opencode/github-copilot/qwen3.6-plus-free";

    // Confirm no cooldowns before the error.
    assert!(
        !cooldown::is_agent_in_cooldown(agent),
        "agent should not be in cooldown before handle_error"
    );
    assert!(
        !cooldown::is_model_in_cooldown(agent, model),
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
        cooldown::is_agent_in_cooldown(agent),
        "agent should be in agent-level cooldown after RateLimit"
    );
    // Model-level cooldown must ALSO be set immediately — this is the fix for issue #2153.
    // Without this, model_for_complexity would still pick the same model.
    assert!(
        cooldown::is_model_in_cooldown(agent, model),
        "model should be in model-level cooldown after RateLimit (fixes issue #2153)"
    );
}

/// Verify that rate limit for a specific model does NOT cool other models of the same agent.
#[serial(cooldown_state)]
#[tokio::test]
async fn rate_limit_cooldown_is_model_specific() {
    cooldown::reset_global_state().await;
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
        cooldown::is_model_in_cooldown(agent, cooled_model),
        "rate-limited model should be in cooldown"
    );
    // Other models of the same agent should NOT be cooled.
    assert!(
        !cooldown::is_model_in_cooldown(agent, other_model),
        "other models should not be cooled by a single model's rate limit"
    );
}

/**
 * Regression test for issue #3652: a credit-exhaustion error records its
 * own credit_failure_count cooldown, but handle_failover() used to also
 * bump the generic failure_count:<agent> counter. After a few credit
 * errors the next ordinary failure would start at the 24h/48h extended
 * backoff tier instead of the 5 minute base. The generic counter must be
 * untouched here while credit_failure_count is incremented. The agent
 * name is deliberately not a configured agent, so handle_failover() finds
 * it as a fallback and takes the recording path this test exercises
 **/
#[serial(cooldown_state)]
#[tokio::test]
async fn credit_exhaustion_does_not_bump_generic_failure_count() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.expect("in-memory store"));
    cooldown::init_cooldown_store(Arc::clone(&store)).await;

    let runner = MockRunner { free: vec![] };
    let agent = "test-agent-3652-credit-no-bump";

    let err = AgentError::RateLimit {
        message: "out of credits, please recharge your balance".to_string(),
    };

    let _result = handle_error(
        "test-3652-a",
        &err,
        agent,
        &runner,
        None,
        Some("simple"),
        1,
        &Some(Arc::clone(&store)),
        "owner/repo",
    )
    .await
    .unwrap();

    let credit_count = store
        .kv_get(&format!("credit_failure_count:{agent}"))
        .await
        .unwrap();
    assert!(
        credit_count.is_some(),
        "credit_failure_count should be incremented by the credit error"
    );
    let generic_count = store
        .kv_get(&format!("failure_count:{agent}"))
        .await
        .unwrap();
    assert!(
        generic_count.is_none(),
        "failure_count must not be bumped by credit errors, got {generic_count:?}"
    );
}

/// A timeout is a long task, not an unavailable model. Two timeouts in a
/// row must leave the model routable and write no rate_limits row
#[serial(cooldown_state)]
#[tokio::test]
async fn two_timeouts_leave_model_routable() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.expect("in-memory store"));
    cooldown::init_cooldown_store(Arc::clone(&store)).await;

    let runner = MockRunner { free: vec![] };
    let agent = "opencode";
    let model = "opencode/nemotron-3-ultra-free";

    let err = AgentError::Timeout {
        elapsed: Duration::from_mins(30),
    };

    for i in 1..=2 {
        let _result = handle_error(
            &format!("test-3876-timeout-{i}"),
            &err,
            agent,
            &runner,
            Some(model),
            Some("simple"),
            i,
            &Some(Arc::clone(&store)),
            "owner/repo",
        )
        .await
        .unwrap();
    }

    assert!(
        !cooldown::is_model_in_cooldown(agent, model),
        "timeouts must not cool the model"
    );
    assert!(
        !cooldown::is_agent_in_cooldown(agent),
        "timeouts must not cool the agent"
    );
    let counts = store.recent_rate_limit_counts(6).await.unwrap();
    assert_eq!(
        counts.get(agent).copied().unwrap_or(0),
        0,
        "timeouts must not write a rate_limits row"
    );
}

/// A run that exits without a result envelope retries without cooling the
/// model or writing a rate_limits row
#[serial(cooldown_state)]
#[tokio::test]
async fn interrupted_run_does_not_write_rate_limit() {
    cooldown::reset_global_state().await;
    let store = Arc::new(TaskStore::open_memory().await.expect("in-memory store"));
    cooldown::init_cooldown_store(Arc::clone(&store)).await;

    let runner = MockRunner { free: vec![] };
    let agent = "claude";
    let model = "sonnet";
    let err = AgentError::Interrupted {
        message: "exited without a result envelope".to_string(),
    };

    let _result = handle_error(
        "test-3876-interrupted",
        &err,
        agent,
        &runner,
        Some(model),
        Some("medium"),
        1,
        &Some(Arc::clone(&store)),
        "owner/repo",
    )
    .await
    .unwrap();

    assert!(
        !cooldown::is_model_in_cooldown(agent, model),
        "interrupted runs must not cool the model"
    );
    let counts = store.recent_rate_limit_counts(6).await.unwrap();
    assert_eq!(
        counts.get(agent).copied().unwrap_or(0),
        0,
        "interrupted runs must not write a rate_limits row"
    );
}

/// Regression test for issue #3580: the in-task failover path (this
/// function) previously never called `parse_relative_usage_window()`, so a
/// vendor-advertised window like "weekly (7-day) usage limit" fell through
/// to the generic exponential backoff (5min base) instead of the ~7-day
/// window the vendor advertised. Only the `needs_review` call site in
/// `runner/mod.rs` applied the window fix from #3572/#3574 — this path did
/// not. Verify the model-specific cooldown now matches the parsed window
/// rather than the short exponential-backoff base.
///
/// Note: `handle_failover()` still applies its own short agent-level
/// cooldown when switching to a fallback agent — that bookkeeping is
/// unrelated to and unaffected by this fix, so it isn't asserted here.
#[serial(cooldown_state)]
#[tokio::test]
async fn rate_limit_with_relative_window_sets_window_length_model_cooldown() {
    cooldown::reset_global_state().await;
    let runner = MockRunner { free: vec![] };
    let agent = "test-agent-3580-kimi";
    let model = "opus";
    let key = format!("{agent}:{model}");

    let err = AgentError::RateLimit {
        message: "You've reached your weekly (7-day) usage limit. Your quota will reset when the current 7-day window expires.".to_string(),
    };

    let _result = handle_error(
        "test-3580-a",
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
    let until =
        cooldown::cooldown_until(&key).expect("relative usage window should set model cooldown");
    let remaining = until.saturating_sub(now);

    assert!(
        remaining >= 7 * 86400 - 30,
        "expected ~7-day model cooldown from the parsed window, got {remaining}s"
    );
    assert_eq!(
        cooldown::cooldown_reason(&key).as_deref(),
        Some("rate_limit_usage_window"),
        "relative usage-window cooldown must be labeled as a rate limit, not silence"
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn provider_returned_error_sets_short_model_cooldown() {
    cooldown::reset_global_state().await;
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
    let until = cooldown::cooldown_until(&key).expect("provider error should set model cooldown");
    let remaining = until.saturating_sub(now);

    assert!(
        remaining <= cooldown::TRANSIENT_MODEL_BACKOFF_BASE_SECS,
        "expected a 5 minute model cooldown, got {remaining}s"
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn model_unavailable_sets_persistent_model_cooldown_for_codex_style() {
    cooldown::reset_global_state().await;
    let runner = MockRunner { free: vec![] };
    let agent = "codex";
    let model = "gpt-5.3-codex";

    // Simulate Codex provider JSON error: message contains "model is not supported"
    let err = AgentError::ModelUnavailable {
        model: model.to_string(),
        message: "{\"detail\":\"The 'gpt-5.3-codex' model is not supported\"}".to_string(),
    };

    let _result = handle_error(
        "test-codex-model-unsupported",
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

    // The persistent backoff base is ~4h; ensure cooldown exists and is at least that long minus a margin
    let key = format!("{agent}:{model}");
    let now = chrono::Utc::now().timestamp();
    let until = cooldown::cooldown_until(&key).expect("model cooldown should be set");
    let remaining = until.saturating_sub(now);

    assert!(
        remaining >= cooldown::PERSISTENT_MODEL_BACKOFF_BASE_SECS - 30,
        "expected persistent model cooldown (~4h) for codex model-unavailable, got {remaining}s"
    );
}

async fn vendor_reset_remaining(agent: &str, model: &str, message: &str) -> i64 {
    cooldown::reset_global_state().await;
    let runner = MockRunner { free: vec![] };
    let err = AgentError::RateLimit {
        message: message.to_string(),
    };
    let _ = handle_error(
        "test-3649",
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
    let until = cooldown::cooldown_until(&format!("{agent}:{model}"))
        .expect("model cooldown should be set");
    until - chrono::Utc::now().timestamp()
}

#[serial(cooldown_state)]
#[tokio::test]
async fn vendor_reset_date_overrides_weekly_window() {
    let t = chrono::Local::now() + chrono::Duration::days(2);
    let msg = format!(
        "You've hit your weekly limit · resets {} {} at 1am (America/Sao_Paulo)",
        t.format("%b"),
        t.day()
    );
    let remaining = vendor_reset_remaining("claude", "sonnet", &msg).await;
    assert!(
        remaining > 3600 && remaining < 3 * 86400,
        "expected ~2d cooldown, got {remaining}s"
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn vendor_reset_date_overrides_billing_cycle_backoff() {
    let t = chrono::Local::now() + chrono::Duration::days(3);
    let msg = format!(
        "You've hit your usage limit. Upgrade to Pro (https://x), or try again at {}.",
        t.format("%b %d, %Y %I:%M %p")
    );
    let remaining = vendor_reset_remaining("codex", "gpt-5.4", &msg).await;
    assert!(
        remaining > 2 * 86400 + 80000 - 3600 && remaining < 4 * 86400,
        "expected ~3d cooldown, got {remaining}s"
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn agent_failed_insufficient_balance_sets_credit_cooldown() {
    cooldown::reset_global_state().await;
    let runner = MockRunner { free: vec![] };
    let agent = "test-agent-3642-insufficient-balance";

    // Confirm no cooldown before the error.
    assert!(
        !cooldown::is_agent_in_cooldown(agent),
        "agent should not be in cooldown before handle_error"
    );

    let err = agents::AgentError::AgentFailed {
        message: "402 Payment Required: insufficient balance".to_string(),
    };

    // Run handle_error — no store so handle_failover will find no agents and return Continue.
    let _result = handle_error(
        "test-3642-a",
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

    // The credit exhaustion path (from #3642) should set a cooldown of at least
    // CREDIT_BACKOFF_BASE_SECS (1h), not the generic 5-min backoff.
    // Without #3642, the AgentFailed arm would have gone to the else branch
    // and called record_agent_failure_with_message, setting a 5-min cooldown.
    assert!(
        cooldown::is_agent_in_cooldown(agent),
        "agent should be in cooldown after handle_error for AgentFailed with insufficient balance"
    );

    let until = cooldown::cooldown_until(agent).expect("agent cooldown should be active");
    let remaining = until - chrono::Utc::now().timestamp();
    assert!(
        remaining >= cooldown::CREDIT_BACKOFF_BASE_SECS - 30,
        "credit cooldown should be at least the 1h base, got {remaining}s"
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn silent_exit0_retries_free_model_for_simple_complexity() {
    cooldown::reset_global_state().await;
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

#[serial(cooldown_state)]
#[tokio::test]
async fn silent_exit0_retries_free_model_when_complexity_unknown() {
    cooldown::reset_global_state().await;
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

#[serial(cooldown_state)]
#[tokio::test]
async fn silent_exit0_retries_free_model_for_medium_complexity() {
    cooldown::reset_global_state().await;
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

#[serial(cooldown_state)]
#[tokio::test]
async fn silent_exit0_retries_free_model_for_complex_complexity() {
    cooldown::reset_global_state().await;
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
#[serial(cooldown_state)]
#[tokio::test]
async fn context_overflow_escalates_to_needs_review_immediately() {
    cooldown::reset_global_state().await;
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
#[serial(cooldown_state)]
#[tokio::test]
async fn free_model_reroute_succeeds_without_store() {
    cooldown::reset_global_state().await;
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

#[serial(cooldown_state)]
#[tokio::test]
async fn silent_exit0_falls_through_when_no_free_models() {
    cooldown::reset_global_state().await;
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
#[serial(cooldown_state)]
#[tokio::test]
async fn invalid_response_sets_model_cooldown() {
    cooldown::reset_global_state().await;
    let runner = MockRunner { free: vec![] };
    let agent = "opencode";
    let model = "opencode/nemotron-3-super-free";

    // Confirm no model cooldown before the error.
    assert!(
        !cooldown::is_model_in_cooldown(agent, model),
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
        cooldown::is_model_in_cooldown(agent, model),
        "model should be in cooldown after InvalidResponse (fixes issue #2750)"
    );
}

/// BillingCycleExhausted with a known model must set ONLY the model-level cooldown.
///
/// Regression test for issue #3382: "upgrade to pro" messages (added to
/// billing_cycle_patterns in #3380) were triggering an agent-wide 24h cooldown
/// when model_name was None, blocking all codex models even though only the
/// specific model's plan quota was exhausted.
#[serial(cooldown_state)]
#[tokio::test]
async fn billing_cycle_exhausted_with_known_model_sets_only_model_cooldown() {
    cooldown::reset_global_state().await;
    let runner = MockRunner { free: vec![] };
    let agent = "codex-3382-billing-cycle";
    let model = "gpt-5.5";

    let err = AgentError::RateLimit {
        message: "rate limit: You've hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro).".to_string(),
    };

    let _result = handle_error(
        "test-3382-a",
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

    // Agent-level credit cooldown must NOT be set — the quota is model-scoped.
    assert!(
        !cooldown::is_agent_in_cooldown(agent),
        "agent must NOT be in agent-level cooldown when BillingCycleExhausted has a known model (issue #3382)"
    );
    // Model-level cooldown MUST be set.
    assert!(
        cooldown::is_model_in_cooldown(agent, model),
        "model must be in cooldown after BillingCycleExhausted (issue #3382)"
    );
    // Other models on the same agent must NOT be cooled.
    assert!(
        !cooldown::is_model_in_cooldown(agent, "gpt-5.4"),
        "other models on the same agent must not be cooled by a single model's billing exhaustion"
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn summarize_rate_limit_error_extracts_api_retry_info() {
    cooldown::reset_global_state().await;
    let raw_output = r#"some log lines
{"type":"system","subtype":"api_retry","attempt":6,"max_retries":10,"retry_delay_ms":17266.789,"error_status":429,"error":"rate_limit","session_id":"331c099b-..."}
{"type":"system","subtype":"api_retry","attempt":7,"max_retries":10,"retry_delay_ms":35162.66,"error_status":429,"error":"rate_limit","session_id":"331c099b-..."}
more logs"#;

    let summarized = super::summarize_rate_limit_error(raw_output);
    assert_eq!(summarized, "status=429 after 7 attempts (last delay 35s)");
}

#[serial(cooldown_state)]
#[tokio::test]
async fn summarize_rate_limit_error_returns_original_when_no_api_retry() {
    cooldown::reset_global_state().await;
    let raw_output = "some regular error message without api_retry JSON";

    let summarized = super::summarize_rate_limit_error(raw_output);
    assert_eq!(summarized, raw_output);
}

#[serial(cooldown_state)]
#[tokio::test]
async fn summarize_rate_limit_error_handles_malformed_json_gracefully() {
    cooldown::reset_global_state().await;
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

#[serial(cooldown_state)]
#[tokio::test]
async fn summarize_rate_limit_error_works_with_single_line() {
    cooldown::reset_global_state().await;
    let raw_output = r#"{"type":"system","subtype":"api_retry","attempt":3,"max_retries":10,"retry_delay_ms":5000,"error_status":429,"error":"rate_limit"}"#;

    let summarized = super::summarize_rate_limit_error(raw_output);
    assert_eq!(summarized, "status=429 after 3 attempts (last delay 5s)");
}

/// Verify extract_json_field correctly handles quoted string values.
/// Previously it stripped from the wrong end (strip_suffix), returning None for
/// any field that isn't the last one in the JSON object (fixes issue #2817).
#[serial(cooldown_state)]
#[tokio::test]
async fn extract_json_field_handles_quoted_strings() {
    cooldown::reset_global_state().await;
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
#[serial(cooldown_state)]
#[tokio::test]
async fn summarize_rate_limit_error_handles_truncated_fragment_with_subtype() {
    cooldown::reset_global_state().await;
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
        !summarized.contains('{'),
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
#[serial(cooldown_state)]
#[tokio::test]
async fn summarize_rate_limit_error_handles_fragment_with_extractable_fields() {
    cooldown::reset_global_state().await;
    let raw_output = r#"log output
,"subtype":"api_retry","attempt":5,"retry_delay_ms":20000,"error_status":429,"error":"rate_limit","session_id":"abc123","max_retries":10}
rest of output"#;

    let summarized = super::summarize_rate_limit_error(raw_output);
    assert_eq!(summarized, "status=429 after 5 attempts (last delay 20s)");
}

/// Complete JSON (no truncation) still works after removing "type":"system" requirement.
#[serial(cooldown_state)]
#[tokio::test]
async fn summarize_rate_limit_error_complete_json_still_works() {
    cooldown::reset_global_state().await;
    let raw_output = r#"some log lines
{"type":"system","subtype":"api_retry","attempt":6,"max_retries":10,"retry_delay_ms":17266.789,"error_status":429,"error":"rate_limit","session_id":"331c099b-..."}
{"type":"system","subtype":"api_retry","attempt":7,"max_retries":10,"retry_delay_ms":35162.66,"error_status":429,"error":"rate_limit","session_id":"331c099b-..."}
more logs"#;

    let summarized = super::summarize_rate_limit_error(raw_output);
    assert_eq!(summarized, "status=429 after 7 attempts (last delay 35s)");
}

/// When no api_retry JSON is found at all, should NOT return raw passthrough.
#[serial(cooldown_state)]
#[tokio::test]
async fn summarize_rate_limit_error_no_api_retry_no_raw_passthrough() {
    cooldown::reset_global_state().await;
    let raw_output = "some regular error message without api_retry JSON that is quite long and should be truncated";

    let summarized = super::summarize_rate_limit_error(raw_output);
    // Should be truncated to first line, not the full raw_output
    assert!(
        summarized.len() <= 140,
        "expected truncated first-line, got: {summarized}"
    );
}

/// Short error without api_retry: should return the line as-is (no truncation needed).
#[serial(cooldown_state)]
#[tokio::test]
async fn summarize_rate_limit_error_no_api_retry_short() {
    cooldown::reset_global_state().await;
    let raw_output = "rate limit exceeded";

    let summarized = super::summarize_rate_limit_error(raw_output);
    assert_eq!(summarized, raw_output);
}

/// "Model not found" is a permanent failure — verify it applies the persistent (4h base)
/// cooldown rather than the standard transient (5min base) cooldown.
///
/// Regression test for issue #2941: dead models were retried every 4h indefinitely
/// because "not found" was treated the same as a transient unavailability.
#[serial(cooldown_state)]
#[tokio::test]
async fn model_not_found_applies_persistent_cooldown() {
    cooldown::reset_global_state().await;
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
    let until = cooldown::cooldown_until(&key).expect("model not found should set model cooldown");
    let remaining = until.saturating_sub(now);

    // Persistent cooldown starts at 4h; transient starts at 5min.
    // Verify we applied the persistent (4h base) path, not the short one.
    assert!(
        remaining >= cooldown::PERSISTENT_MODEL_BACKOFF_BASE_SECS - 30,
        "expected ~4h (persistent) cooldown for 'Model not found', got {remaining}s — \
             may have applied transient 5min cooldown instead"
    );
}

/// "Model not supported" is a permanent account-type mismatch — verify it applies the
/// persistent (4h base → 7d max) cooldown, same as "not found".
///
/// Regression test for issue #3215: "not supported" was not matched by the
/// `is_permanently_gone` check, so it fell through to the 5min→4h transient backoff
/// and the model kept being retried every ~4h indefinitely.
#[serial(cooldown_state)]
#[tokio::test]
async fn model_not_supported_applies_persistent_cooldown() {
    cooldown::reset_global_state().await;
    let runner = MockRunner { free: vec![] };
    let agent = "codex-3215-not-supported";
    let model = "gpt-5.2-codex";
    let key = format!("{agent}:{model}");

    let err = AgentError::ModelUnavailable {
        model: model.to_string(),
        message:
            "The 'gpt-5.2-codex' model is not supported when using Codex with a ChatGPT account."
                .to_string(),
    };

    let _result = handle_error(
        "test-3215-a",
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
    let until =
        cooldown::cooldown_until(&key).expect("model not supported should set model cooldown");
    let remaining = until.saturating_sub(now);

    assert!(
        remaining >= cooldown::PERSISTENT_MODEL_BACKOFF_BASE_SECS - 30,
        "expected ~4h (persistent) cooldown for 'not supported', got {remaining}s — \
             may have applied transient 5min cooldown instead (issue #3215)"
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn model_not_available_in_country_applies_persistent_cooldown() {
    cooldown::reset_global_state().await;
    let runner = MockRunner { free: vec![] };
    let agent = "opencode-3650-country";
    let model = "some-region-locked-model";
    let key = format!("{agent}:{model}");

    // issue #3650: region/country blocks need the persistent backoff, not the 5min one
    for message in [
        "This model is not available in your country.",
        "This model is not available in your region.",
    ] {
        cooldown::reset_global_state().await;
        let err = AgentError::ModelUnavailable {
            model: model.to_string(),
            message: message.to_string(),
        };

        let _result = handle_error(
            "test-3650",
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
        let until = cooldown::cooldown_until(&key).expect("region block should set model cooldown");
        let remaining = until.saturating_sub(now);

        assert!(
            remaining >= cooldown::PERSISTENT_MODEL_BACKOFF_BASE_SECS - 30,
            "expected ~4h (persistent) cooldown for '{message}', got {remaining}s — \
                 may have applied transient 5min cooldown instead (issue #3650)"
        );
    }
}

/// ThinkingBlockConflict must NOT set any agent or model cooldown.
///
/// This error is a transient Claude CLI 400 (thinking blocks modified in multi-turn
/// tool-use) — not a model capacity problem. Penalising claude:opus with failure
/// counts would prematurely push it into extended-backoff for unrelated reasons.
#[serial(cooldown_state)]
#[tokio::test]
async fn thinking_block_conflict_does_not_set_cooldown() {
    cooldown::reset_global_state().await;
    let runner = MockRunner { free: vec![] };
    let agent = "claude-3204-thinking-block";
    let model = "opus";

    assert!(
        !cooldown::is_agent_in_cooldown(agent),
        "agent must not be in cooldown before handle_error"
    );
    assert!(
        !cooldown::is_model_in_cooldown(agent, model),
        "model must not be in cooldown before handle_error"
    );

    let err = AgentError::ThinkingBlockConflict {
        message: "`thinking` or `redacted_thinking` blocks in the latest assistant message cannot be modified.".to_string(),
    };

    let _result = handle_error(
        "test-3204-thinking",
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

    assert!(
        !cooldown::is_agent_in_cooldown(agent),
        "agent must NOT be in cooldown after ThinkingBlockConflict (fixes issue #3204)"
    );
    assert!(
        !cooldown::is_model_in_cooldown(agent, model),
        "model must NOT be in cooldown after ThinkingBlockConflict (fixes issue #3204)"
    );
}

/// Transient model unavailability (not "not found") should use the standard short cooldown.
#[serial(cooldown_state)]
#[tokio::test]
async fn model_unavailable_transient_applies_short_cooldown() {
    cooldown::reset_global_state().await;
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
    let until = cooldown::cooldown_until(&key)
        .expect("transient model unavailability should set model cooldown");
    let remaining = until.saturating_sub(now);

    // Standard (transient) backoff base is 5min. Persistent base is 4h.
    // Confirm we did NOT apply the long 4h cooldown for a transient error.
    assert!(
        remaining < cooldown::PERSISTENT_MODEL_BACKOFF_BASE_SECS,
        "transient unavailability should use short cooldown (<4h), got {remaining}s"
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn model_unavailable_does_not_reuse_same_agent_when_agent_cooled() {
    cooldown::reset_global_state().await;
    let agent = "codex-3620";
    let failing_model = "gpt-5.4";
    let other_model = "gpt-5.5";
    let runner = MockRunnerWithModels {
        models: vec![failing_model.to_string(), other_model.to_string()],
    };

    cooldown::set_agent_cooldown(agent, 3600).await;

    let err = AgentError::ModelUnavailable {
        model: failing_model.to_string(),
        message: "model gpt-5.4 unavailable".to_string(),
    };

    let result = handle_error(
        "test-3620",
        &err,
        agent,
        &runner,
        Some(failing_model),
        Some("medium"),
        1,
        &None,
        "owner/repo",
    )
    .await
    .unwrap();

    if let ErrorHandleResult::EarlyReturn { status } = result {
        assert_ne!(
            status, "routed",
            "agent-wide cooldown was defeated by same-agent model substitution"
        );
    }
}
