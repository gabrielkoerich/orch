use std::env;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::process;
use std::sync::Arc;

use super::super::TaskRunner;
use super::*;
use crate::backends::{ExternalId, ExternalTask};
use crate::cooldown;
use crate::engine::agents::{AgentError, find_agent_result, patterns};
use crate::engine::router::{Router, RouterConfig};
use crate::store::TaskStatus;
use serial_test::serial;

#[serial(cooldown_state)]
#[test]
fn calculate_backoff_delay_jitter_is_centered() {
    // Run many samples and verify the range is [0.7*capped, 1.3*capped].
    // We can't control `now` directly, but calling the function many times
    // exercises different microsecond values and lets us check bounds.
    let base = config::retry_base_delay_ms();
    let max = config::retry_max_delay_ms();

    // retry_count=0 → exponential = base, capped = base (assuming base < max)
    let capped = base.min(max);
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "mirrors the jitter range in calculate_backoff_delay"
    )]
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

#[serial(cooldown_state)]
#[test]
fn fallback_retry_count_uses_larger_network_retry_streak() {
    let task = Task {
        id: 1,
        external_id: None,
        repo: "owner/repo".to_string(),
        origin: "internal".to_string(),
        title: "test".to_string(),
        body: String::new(),
        status: TaskStatus::New,
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
        route_attempts: 0,
        attempts: 0,
        branch: String::new(),
        worktree: String::new(),
        worktree_cleaned: false,
        owner_node: None,
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
        timeout_retries: 0,
        review_agent_failures: 0,
        review_resets: 0,
        review_cycles: 0,
        review_invocations: 0,
        needs_review_refires: 0,
        input_tokens: 0,
        output_tokens: 0,
        total_cost_usd: 0.0,
        model_reroute_chain: String::new(),
        limit_reroute_chain: "claude".to_string(),
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

#[serial(cooldown_state)]
#[test]
fn retryable_type_str_returns_correct_labels() {
    assert_eq!(RetryableError::Timeout.type_str(), "timeout");
    assert_eq!(RetryableError::Interrupted.type_str(), "interrupted");
    assert_eq!(RetryableError::UsageLimit.type_str(), "rate_limit");
    assert_eq!(RetryableError::AuthError.type_str(), "auth_error");
    assert_eq!(RetryableError::Failed.type_str(), "failed");
    assert_eq!(RetryableError::MissingTooling.type_str(), "failed");
}

#[serial(cooldown_state)]
#[test]
fn is_usage_limit_detects_rate_limit() {
    assert!(patterns::detect_rate_limit("Error: rate limit exceeded").is_some());
    assert!(patterns::detect_rate_limit("HTTP 429 Too Many Requests").is_some());
    assert!(patterns::detect_rate_limit("quota exceeded for model").is_some());
    // context_length_exceeded is handled by detect_context_overflow, not detect_rate_limit
    assert!(patterns::detect_context_overflow("context_length_exceeded").is_some());
}

#[serial(cooldown_state)]
#[test]
fn is_usage_limit_rejects_normal_text() {
    assert!(patterns::detect_rate_limit("task completed successfully").is_none());
    assert!(patterns::detect_rate_limit("").is_none());
}

#[serial(cooldown_state)]
#[test]
fn is_auth_error_detects_common_patterns() {
    assert!(patterns::detect_auth_error("401 Unauthorized").is_some());
    assert!(patterns::detect_auth_error("invalid api key provided").is_some());
    assert!(patterns::detect_auth_error("Your billing plan has expired").is_some());
    assert!(patterns::detect_auth_error("Error: 403 Forbidden").is_some());
}

#[serial(cooldown_state)]
#[test]
fn is_auth_error_rejects_normal_text() {
    assert!(patterns::detect_auth_error("task completed successfully").is_none());
    assert!(patterns::detect_auth_error("").is_none());
}

#[serial(cooldown_state)]
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

#[serial(cooldown_state)]
#[test]
fn detect_missing_tooling_returns_none_for_normal() {
    assert!(patterns::detect_missing_tool("everything works fine").is_none());
    assert!(patterns::detect_missing_tool("").is_none());
}

// ── Cooldown tests ────────────────────────────────────────────

#[serial(cooldown_state)]
#[tokio::test]
async fn record_and_check_agent_cooldown() {
    cooldown::reset_global_state().await;
    // Use unique names to avoid interference from other tests
    let agent = "test_cooldown_agent_1";
    assert!(!is_agent_in_cooldown(agent));
    record_agent_failure_with_message(agent, "").await;
    assert!(is_agent_in_cooldown(agent));
}

#[serial(cooldown_state)]
#[tokio::test]
async fn record_and_check_model_cooldown() {
    cooldown::reset_global_state().await;
    let agent = "test_cooldown_agent_2";
    let model = "test_model_x";
    assert!(!is_model_in_cooldown(agent, model));
    record_model_failure(agent, model).await;
    assert!(is_model_in_cooldown(agent, model));
    // Different model should not be in cooldown
    assert!(!is_model_in_cooldown(agent, "other_model"));
}

#[serial(cooldown_state)]
#[tokio::test]
async fn clear_expired_does_not_remove_fresh_entries() {
    cooldown::reset_global_state().await;
    let agent = "test_cooldown_agent_3";
    record_agent_failure_with_message(agent, "").await;
    clear_expired_cooldowns();
    // Should still be in cooldown (just recorded)
    assert!(is_agent_in_cooldown(agent));
}

// Use fake agent names so other tests don't interfere.
#[serial(cooldown_state)]
#[test]
fn pick_fallback_skips_current_agent() {
    let available = vec!["test_agent_a".to_string(), "test_agent_b".to_string()];
    let result = pick_fallback_agent("test_agent_a", "", &available);
    assert_eq!(result, Some("test_agent_b".to_string()));
}

#[serial(cooldown_state)]
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

#[serial(cooldown_state)]
#[test]
fn pick_fallback_returns_none_when_exhausted() {
    let available = vec!["test_agent_a".to_string(), "test_agent_b".to_string()];
    let result = pick_fallback_agent("test_agent_a", "test_agent_a,test_agent_b", &available);
    assert!(result.is_none());
}

#[serial(cooldown_state)]
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

#[serial(cooldown_state)]
#[test]
fn parse_review_response_direct_json() {
    let json = r#"{"decision":"approve","notes":"LGTM","test_results":"pass","issues":[]}"#;
    let resp = parse_review_response(json).unwrap();
    assert_eq!(resp.decision, "approve");
    assert_eq!(resp.notes, "LGTM");
    assert!(resp.issues.is_empty(), "{:?}", resp.issues);
}

#[serial(cooldown_state)]
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

#[serial(cooldown_state)]
#[test]
fn parse_review_response_embedded_json_object() {
    let text = "Review complete.\n\n{\"decision\":\"approve\",\"notes\":\"LGTM\",\"test_results\":\"pass\",\"issues\":[]}\nThanks!";
    let resp = parse_review_response(text).unwrap();
    assert_eq!(resp.decision, "approve");
    assert_eq!(resp.notes, "LGTM");
}

#[serial(cooldown_state)]
#[test]
fn parse_review_response_template_echo_infers_from_notes() {
    let json = r#"{"decision":"approve|request_changes","notes":"LGTM","issues":[]}"#;
    assert_eq!(parse_review_response(json).unwrap().decision, "approve");

    let no_signal = r#"{"decision":"approve|request_changes","notes":"hmm","issues":[]}"#;
    assert!(parse_review_response(no_signal).is_err());
}

#[serial(cooldown_state)]
#[test]
fn parse_review_response_invalid() {
    let result = parse_review_response("not json at all");
    assert!(result.is_err());
}

#[serial(cooldown_state)]
#[test]
fn extract_json_block_basic() {
    let md = "text\n```json\n{\"key\": \"value\"}\n```\nmore";
    let result = extract_json_block(md);
    assert!(result.is_some());
    assert!(result.unwrap().contains("\"key\""));
}

#[serial(cooldown_state)]
#[test]
fn extract_json_block_skips_non_json_blocks() {
    let md = "```json\nnot-json-array\n```\n\n```json\n{\"real\": true}\n```";
    let result = extract_json_block(md);
    assert!(result.is_some());
    assert!(result.unwrap().contains("\"real\""));
}

#[serial(cooldown_state)]
#[test]
fn extract_json_block_none_when_missing() {
    assert!(extract_json_block("no code blocks here").is_none());
}

#[serial(cooldown_state)]
#[test]
fn extract_json_block_skips_malformed_intermediate_blocks() {
    let md = "prefix ```json{\"broken\": true}\n\n```json\n{\"real\": true}\n```";
    let result = extract_json_block(md);
    assert_eq!(result.as_deref(), Some("{\"real\": true}"));
}

#[serial(cooldown_state)]
#[test]
fn extract_json_block_skips_multiple_malformed_intermediate_blocks() {
    let md =
        "prefix ```json{\"broken\": true}\nmid ```json[1, 2, 3]\n\n```json\n{\"real\": true}\n```";
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

#[serial(cooldown_state)]
#[test]
fn parse_review_from_output_direct_json() {
    let json = r#"{"decision":"approve","notes":"LGTM","test_results":"pass","issues":[]}"#;
    let resp = parse_review_response(json).unwrap();
    assert_eq!(resp.decision, "approve");
}

#[serial(cooldown_state)]
#[test]
fn parse_review_from_output_markdown() {
    let md = "Review complete.\n\n```json\n{\"decision\":\"request_changes\",\"notes\":\"Fix it\",\"issues\":[]}\n```\n";
    let resp = parse_review_response(md).unwrap();
    assert_eq!(resp.decision, "request_changes");
}

#[serial(cooldown_state)]
#[test]
fn parse_review_from_output_plain_text_fallback() {
    let text = "The review is done. LGTM.";
    let resp = parse_review_plain(text).unwrap();
    assert_eq!(resp.decision, "approve");
    assert!(resp.issues.is_empty(), "{:?}", resp.issues);
}

/// opencode NDJSON stream — text events use the `part.text` format.
#[serial(cooldown_state)]
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
#[serial(cooldown_state)]
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
#[serial(cooldown_state)]
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

#[serial(cooldown_state)]
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
#[serial(cooldown_state)]
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
#[serial(cooldown_state)]
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
#[serial(cooldown_state)]
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
#[serial(cooldown_state)]
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
#[serial(cooldown_state)]
#[test]
fn infer_review_response_negation_not_approved() {
    assert!(
        parse_review_plain("The changes were not approved.").is_err(),
        "\"not approved\" should not infer approval"
    );
}

#[serial(cooldown_state)]
#[test]
fn infer_review_response_negation_unapproved() {
    assert!(
        parse_review_plain("Unapproved changes found in the diff.").is_err(),
        "\"unapproved\" should not infer approval"
    );
}

#[serial(cooldown_state)]
#[test]
fn infer_review_response_negation_not_approving() {
    assert!(
        parse_review_plain("I am not approving this PR.").is_err(),
        "\"not approving\" should not infer approval"
    );
}

#[serial(cooldown_state)]
#[test]
fn infer_review_response_negation_cannot_be_approved() {
    assert!(
        parse_review_plain("This PR cannot be approved until the race in tick.rs is fixed.")
            .is_err(),
        "\"cannot be approved\" should not infer approval"
    );
}

#[serial(cooldown_state)]
#[test]
fn infer_review_response_negation_should_not_be_approved() {
    assert!(
        parse_review_plain("The diff breaks the build and should not be approved.").is_err(),
        "\"should not be approved\" should not infer approval"
    );
}

#[serial(cooldown_state)]
#[test]
fn infer_review_response_negation_not_yet_approved() {
    assert!(
        parse_review_plain("This change is not yet approved.").is_err(),
        "\"not yet approved\" should not infer approval"
    );
}

#[serial(cooldown_state)]
#[test]
fn infer_review_response_negation_wont_approve() {
    assert!(
        parse_review_plain("I won't approve this until tests cover the retry path.").is_err(),
        "\"won't approve\" should not infer approval"
    );
}

#[serial(cooldown_state)]
#[test]
fn infer_review_response_bare_approved() {
    assert!(
        parse_review_plain("The PR was approved.").is_err(),
        "bare \"approved\" should not infer approval"
    );
}

/// Positive plain-text approval signals must still work.
#[serial(cooldown_state)]
#[test]
fn infer_review_response_lgtm() {
    let resp = parse_review_plain("LGTM, everything looks fine.").unwrap();
    assert_eq!(resp.decision, "approve");
}

/// Evidence phrases are not a decision token
#[serial(cooldown_state)]
#[test]
fn infer_review_response_looks_good() {
    assert!(
        parse_review_plain("Looks good to me!").is_err(),
        "\"looks good\" should not infer approval"
    );
}

#[serial(cooldown_state)]
#[test]
fn infer_review_response_all_checks_passed() {
    assert!(
        parse_review_plain(
            "The background task completed but I already got the CI results directly. All checks passed — no action needed.",
        )
        .is_err(),
        "\"all checks passed\" should not infer approval"
    );
}

#[serial(cooldown_state)]
#[test]
fn infer_review_response_checks_passed() {
    assert!(
        parse_review_plain("All checks passed (fmt ✓, clippy ✓, tests ✓).").is_err(),
        "\"all checks passed\" should not infer approval"
    );
}

#[serial(cooldown_state)]
#[test]
fn infer_review_response_all_tests_passed() {
    assert!(
        parse_review_plain("All tests passed and the implementation looks correct.").is_err(),
        "\"all tests passed\" should not infer approval"
    );
}

#[serial(cooldown_state)]
#[test]
fn infer_review_response_no_issues_found() {
    assert!(
        parse_review_plain("Review complete. No issues found in the diff.").is_err(),
        "\"no issues found\" should not infer approval"
    );
}

/// Partial "tests passed" mid-sentence must NOT infer approval without "all" prefix.
#[serial(cooldown_state)]
#[test]
fn infer_review_response_partial_tests_passed_no_approve() {
    let text = "The existing tests passed before this change but the new payment logic has a null-pointer risk on line 42.";
    assert!(parse_review_plain(text).is_err());
}

/// Partial "checks passed" mid-sentence must NOT infer approval without "all" prefix.
#[serial(cooldown_state)]
#[test]
fn infer_review_response_partial_checks_passed_no_approve() {
    let text = "CI checks passed for the base branch but this PR adds a broken path.";
    assert!(parse_review_plain(text).is_err());
}

/// LLMs sometimes echo the JSON decision field value literally: "Decision is `approve`."
#[serial(cooldown_state)]
#[test]
fn infer_review_response_decision_is_approve_backtick() {
    let text = "Already retrieved the output and completed the review above. The CI test check passed (3m11s) and all other checks are green. Decision is `approve`.";
    let resp = parse_review_plain(text).unwrap();
    assert_eq!(resp.decision, "approve");
}

/// "Decision: approve" without backticks should also match.
#[serial(cooldown_state)]
#[test]
fn infer_review_response_decision_colon_approve() {
    let text = "Reviewed the PR. No blocking issues found. Decision: approve";
    let resp = parse_review_plain(text).unwrap();
    assert_eq!(resp.decision, "approve");
}

/// Bare backtick `approve` in a negative context must NOT infer approval.
/// e.g. "do not `approve`" previously matched the removed `lower.contains("`approve`")` check.
#[serial(cooldown_state)]
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

#[serial(cooldown_state)]
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

#[serial(cooldown_state)]
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

#[serial(cooldown_state)]
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

#[serial(cooldown_state)]
#[test]
fn agent_output_plain_text_keyword_fallback() {
    let text = "All checks passed, LGTM.";
    let resp = parse_via_agent_path("claude", text).unwrap();
    assert_eq!(resp.decision, "approve");
}

#[serial(cooldown_state)]
#[test]
fn agent_output_direct_json() {
    let json = r#"{"decision":"approve","notes":"LGTM","test_results":"pass","issues":[]}"#;
    let resp = parse_via_agent_path("opencode", json).unwrap();
    assert_eq!(resp.decision, "approve");
}

/// Regression: plain text containing JSON fragments should NOT trigger false
/// positives. The per-agent extractor returns None for non-NDJSON text,
/// so we fall through to keyword inference instead of generic NDJSON parsing.
#[serial(cooldown_state)]
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
#[serial(cooldown_state)]
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
#[serial(cooldown_state)]
#[test]
fn parse_review_response_ignores_embedded_json_without_decision_key() {
    let text = r#"Analysis complete. Config: {"timeout":30,"retries":3}. Done."#;
    assert!(
        parse_review_response(text).is_err(),
        "embedded JSON without 'decision' key should not parse as ReviewResponse"
    );
}

// ── read_file_with_limit tests ───────────────────────────────────────────

#[serial(cooldown_state)]
#[test]
fn read_file_with_limit_reads_small_file_completely() {
    let temp_dir = env::temp_dir();
    let file_path = temp_dir.join(format!("orch_test_small_{}.txt", process::id()));
    let content = "Hello, World!";
    fs::write(&file_path, content).unwrap();

    let result = read_file_with_limit(&file_path, 1024);
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), content);

    let _ = fs::remove_file(&file_path);
}

#[serial(cooldown_state)]
#[test]
fn read_file_with_limit_truncates_large_file() {
    let temp_dir = env::temp_dir();
    let file_path = temp_dir.join(format!("orch_test_large_{}.txt", process::id()));

    // Create a file larger than the limit (100 bytes)
    let large_content = "x".repeat(200);
    fs::write(&file_path, &large_content).unwrap();

    // Read with a 50-byte limit
    let result = read_file_with_limit(&file_path, 50);
    assert!(result.is_ok());

    let read_content = result.unwrap();
    // Should only read 50 bytes (the tail of the file)
    assert_eq!(read_content.len(), 50);
    assert_eq!(read_content, "x".repeat(50));

    let _ = fs::remove_file(&file_path);
}

#[serial(cooldown_state)]
#[test]
fn read_file_with_limit_handles_empty_file() {
    let temp_dir = env::temp_dir();
    let file_path = temp_dir.join(format!("orch_test_empty_{}.txt", process::id()));
    fs::write(&file_path, "").unwrap();

    let result = read_file_with_limit(&file_path, 1024);
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "");

    let _ = fs::remove_file(&file_path);
}

#[serial(cooldown_state)]
#[test]
fn read_file_with_limit_handles_nonexistent_file() {
    let temp_dir = env::temp_dir();
    let file_path = temp_dir.join("orch_test_nonexistent_xyz123.txt");

    let result = read_file_with_limit(&file_path, 1024);
    assert!(result.is_err());
}

#[serial(cooldown_state)]
#[test]
fn read_file_with_limit_reads_exact_limit() {
    let temp_dir = env::temp_dir();
    let file_path = temp_dir.join(format!("orch_test_exact_{}.txt", process::id()));

    // Create exactly 100 bytes
    let content = "y".repeat(100);
    fs::write(&file_path, &content).unwrap();

    // Read with exactly 100-byte limit (should read all)
    let result = read_file_with_limit(&file_path, 100);
    assert!(result.is_ok());
    assert_eq!(result.unwrap().len(), 100);

    let _ = fs::remove_file(&file_path);
}

#[serial(cooldown_state)]
#[test]
fn read_file_with_limit_handles_unicode_truncation() {
    let temp_dir = env::temp_dir();
    let file_path = temp_dir.join(format!("orch_test_unicode_{}.txt", process::id()));

    // Create content with multi-byte UTF-8 characters
    let emoji = "🎉".repeat(100); // Each emoji is 4 bytes
    fs::write(&file_path, &emoji).unwrap();

    // Read with a limit that cuts into the emoji sequence
    let result = read_file_with_limit(&file_path, 50);
    assert!(result.is_ok());

    let read_content = result.unwrap();
    // from_utf8_lossy should handle invalid sequences
    assert_ne!(read_content, "");

    let _ = fs::remove_file(&file_path);
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

#[serial(cooldown_state)]
#[test]
fn fixture_claude_review_approve() {
    let raw = include_str!("../../../../tests/fixtures/review_claude_approve.jsonl");
    let resp = parse_review_via_agent("claude", raw).unwrap();
    assert_eq!(resp.decision, "approve");
    assert_eq!(resp.notes, "All checks pass");
    assert_eq!(resp.test_results.as_deref(), Some("pass"));
    assert!(resp.issues.is_empty(), "{:?}", resp.issues);
}

#[serial(cooldown_state)]
#[test]
fn fixture_claude_review_request_changes() {
    let raw = include_str!("../../../../tests/fixtures/review_claude_request_changes.jsonl");
    let resp = parse_review_via_agent("claude", raw).unwrap();
    assert_eq!(resp.decision, "request_changes");
    assert_eq!(resp.notes, "Fix the null deref on line 42");
    assert_eq!(resp.test_results.as_deref(), Some("fail"));
    assert_eq!(resp.issues.len(), 1);
    assert_eq!(resp.issues[0].file, "src/main.rs");
    assert_eq!(resp.issues[0].line, Some(42));
    assert_eq!(resp.issues[0].severity, "error");
}

#[serial(cooldown_state)]
#[test]
fn fixture_claude_review_rate_limit_returns_error() {
    // find_claude_result on an is_error=true result returns Some with is_error=true.
    // The integration test verifies this is captured, not that it's a ReviewResponse.
    let raw = include_str!("../../../../tests/fixtures/review_claude_rate_limit.jsonl");
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

#[serial(cooldown_state)]
#[test]
fn fixture_opencode_review_approve() {
    let raw = include_str!("../../../../tests/fixtures/review_opencode_approve.jsonl");
    let resp = parse_review_via_agent("opencode", raw).unwrap();
    assert_eq!(resp.decision, "approve");
    assert_eq!(resp.notes, "All checks pass");
    assert!(resp.issues.is_empty(), "{:?}", resp.issues);
}

#[serial(cooldown_state)]
#[test]
fn fixture_opencode_review_request_changes() {
    let raw = include_str!("../../../../tests/fixtures/review_opencode_request_changes.jsonl");
    let resp = parse_review_via_agent("opencode", raw).unwrap();
    assert_eq!(resp.decision, "request_changes");
    assert_eq!(resp.notes, "Fix the memory leak");
    assert_eq!(resp.test_results.as_deref(), Some("fail"));
    assert_eq!(resp.issues.len(), 1);
    assert_eq!(resp.issues[0].file, "src/lib.rs");
    assert_eq!(resp.issues[0].line, Some(100));
}

#[serial(cooldown_state)]
#[test]
fn fixture_opencode_review_plain_text_approve() {
    // OpenCode with plain-text review (no JSON) — should infer approval via keywords.
    let raw = include_str!("../../../../tests/fixtures/review_opencode_plain_text.jsonl");
    let resp = parse_review_via_agent("opencode", raw).unwrap();
    assert_eq!(resp.decision, "approve");
}

// ── Codex review fixtures ────────────────────────────────────────────────────

#[serial(cooldown_state)]
#[test]
fn fixture_codex_review_approve() {
    let raw = include_str!("../../../../tests/fixtures/review_codex_approve.jsonl");
    let resp = parse_review_via_agent("codex", raw).unwrap();
    assert_eq!(resp.decision, "approve");
    assert_eq!(resp.notes, "Clean implementation");
    assert!(resp.issues.is_empty(), "{:?}", resp.issues);
}

#[serial(cooldown_state)]
#[test]
fn fixture_codex_review_request_changes() {
    let raw = include_str!("../../../../tests/fixtures/review_codex_request_changes.jsonl");
    let resp = parse_review_via_agent("codex", raw).unwrap();
    assert_eq!(resp.decision, "request_changes");
    assert_eq!(resp.notes, "Race condition on line 15");
    assert_eq!(resp.issues.len(), 1);
    assert_eq!(resp.issues[0].file, "src/server.rs");
    assert_eq!(resp.issues[0].line, Some(15));
}

#[serial(cooldown_state)]
#[test]
fn fixture_codex_review_plain_text_approve() {
    // Codex with plain-text review — should infer approval via "LGTM" keyword.
    let raw = include_str!("../../../../tests/fixtures/review_codex_plain_text.jsonl");
    let resp = parse_review_via_agent("codex", raw).unwrap();
    assert_eq!(resp.decision, "approve");
}

// ── Kimi review fixtures ────────────────────────────────────────────────────

#[serial(cooldown_state)]
#[test]
fn fixture_kimi_review_approve() {
    let raw = include_str!("../../../../tests/fixtures/review_kimi_approve.jsonl");
    let resp = parse_review_via_agent("kimi", raw).unwrap();
    assert_eq!(resp.decision, "approve");
    assert_eq!(resp.notes, "LGTM");
    assert_eq!(resp.test_results.as_deref(), Some("pass"));
    assert!(resp.issues.is_empty(), "{:?}", resp.issues);
}

// ── MiniMax review fixtures ────────────────────────────────────────────────

#[serial(cooldown_state)]
#[test]
fn fixture_minimax_review_request_changes() {
    let raw = include_str!("../../../../tests/fixtures/review_minimax_request_changes.jsonl");
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
#[serial(cooldown_state)]
#[test]
fn find_agent_result_extracts_tokens_claude() {
    let raw = include_str!("../../../../tests/fixtures/review_claude_approve.jsonl");
    let result = find_agent_result("claude", raw).expect("should find result");
    assert_eq!(result.input_tokens, Some(200));
    assert_eq!(result.output_tokens, Some(30));
    assert!(result.cost_usd.is_none()); // review fixtures don't include cost
}

#[serial(cooldown_state)]
#[test]
fn find_agent_result_extracts_tokens_opencode() {
    let raw = include_str!("../../../../tests/fixtures/review_opencode_approve.jsonl");
    let result = find_agent_result("opencode", raw).expect("should find result");
    assert_eq!(result.input_tokens, Some(2900));
    assert_eq!(result.output_tokens, Some(100));
    assert_eq!(result.cost_usd, Some(0.002));
}

/// Verify that find_agent_result returns None for empty/unparseable input.
#[serial(cooldown_state)]
#[test]
fn find_agent_result_returns_none_for_empty() {
    assert!(find_agent_result("claude", "").is_none());
    assert!(find_agent_result("opencode", "").is_none());
    assert!(find_agent_result("codex", "").is_none());
    assert!(find_agent_result("kimi", "").is_none());
    assert!(find_agent_result("minimax", "").is_none());
}

#[serial(cooldown_state)]
#[test]
fn find_agent_result_returns_none_for_plain_text() {
    assert!(find_agent_result("claude", "just some plain text").is_none());
    assert!(find_agent_result("codex", "just some plain text").is_none());
    // OpenCode is a special case — it can sometimes return a result for plain text.
    // This is acceptable because opencode's extract_ndjson_text falls back gracefully.
}

// ── Error propagation via find_agent_result ─────────────────────────────────

/// Verify that find_agent_result surfaces rate-limit errors from Claude.
#[serial(cooldown_state)]
#[test]
fn find_agent_result_propagates_claude_rate_limit() {
    let raw = include_str!("../../../../tests/fixtures/review_claude_rate_limit.jsonl");
    let result = find_agent_result("claude", raw).expect("should find result");
    assert!(result.is_error, "should be flagged as error");
    assert!(result.result_text.contains("rate limit"));
}

/// Two fake agents on PATH, a config that names them and a 1 ms backoff. Each nextest test
/// runs in its own process, so the environment is not restored
#[cfg(unix)]
fn failover_env() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    for agent in ["claude", "codex"] {
        let path = bin.join(agent);
        fs::write(&path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let home = dir.path().join("home");
    fs::create_dir(&home).unwrap();
    fs::write(
        home.join("config.yml"),
        "agents: [claude, codex]\ndispatch:\n  retry_base_delay_ms: 1\n  retry_max_delay_ms: 1\nworkflow:\n  max_timeout_retries: 3\n",
    )
    .unwrap();
    let mut path = bin.as_os_str().to_os_string();
    path.push(":");
    path.push(env::var_os("PATH").unwrap_or_default());
    // SAFETY: nextest runs each test in its own process, so no other test touches the environment
    unsafe { env::set_var("PATH", path) };
    // SAFETY: same as above
    unsafe { env::set_var("ORCH_HOME", &home) };
    cmd_cache::reset_for_test();
    dir
}

async fn failover_task() -> (Arc<TaskStore>, String) {
    let store = Arc::new(TaskStore::open_memory().await.expect("in-memory store"));
    let id = store
        .create_internal("owner/repo", "t", "b", "test", "", None)
        .await
        .unwrap();
    (store, format!("internal:{id}"))
}

/// Every other agent is in cooldown, so the timed-out task retries from `new` after the backoff
#[cfg(unix)]
#[serial(cooldown_state)]
#[tokio::test]
async fn timeout_with_every_other_agent_cooled_retries_as_new() {
    let _env = failover_env();
    cooldown::reset_global_state().await;
    set_agent_cooldown("codex", 600).await;
    let (store, task_id) = failover_task().await;
    let store = Some(store);

    let outcome = handle_failover(
        &task_id,
        "claude",
        RetryableError::Timeout,
        "claude timed out after 1804s",
        false,
        &store,
        "owner/repo",
    )
    .await;

    assert_eq!(outcome.status, "new");
    assert!(outcome.next_agent.is_none());
    let task = store::opt_store_get_task(&store, "owner/repo", &task_id)
        .await
        .unwrap();
    assert!(task.limit_reroute_chain.is_empty(), "chain must be cleared");
    assert!(
        task.last_error.contains("will retry"),
        "{}",
        task.last_error
    );
    assert_eq!(task.timeout_retries, 1);
}

/// Another agent is free, so the timeout still reroutes to it
#[cfg(unix)]
#[serial(cooldown_state)]
#[tokio::test]
async fn timeout_with_a_free_agent_reroutes_to_it() {
    let _env = failover_env();
    cooldown::reset_global_state().await;
    let (store, task_id) = failover_task().await;
    let store = Some(store);

    let outcome = handle_failover(
        &task_id,
        "claude",
        RetryableError::Timeout,
        "claude timed out after 1804s",
        false,
        &store,
        "owner/repo",
    )
    .await;

    assert_eq!(outcome.status, "new");
    assert_eq!(outcome.next_agent.as_deref(), Some("codex"));
    let task = store::opt_store_get_task(&store, "owner/repo", &task_id)
        .await
        .unwrap();
    assert_eq!(task.limit_reroute_chain, "claude");
    assert_eq!(task.agent.as_deref(), Some("codex"));
    assert!(
        task.model.as_deref().unwrap_or("").is_empty(),
        "model must be cleared so routing picks one for the fallback, got {:?}",
        task.model
    );
    assert!(
        task.last_error.contains("switching to codex"),
        "{}",
        task.last_error
    );
}

/// The run_with_context reroute clear must keep the fallback while status is still InProgress
#[cfg(unix)]
#[serial(cooldown_state)]
#[tokio::test]
async fn timeout_failover_keeps_fallback_after_reroute_clear() {
    let _env = failover_env();
    cooldown::reset_global_state().await;
    let (store_arc, task_id) = failover_task().await;
    let store_id = store_arc
        .resolve_task_id("owner/repo", &task_id)
        .await
        .unwrap()
        .unwrap();
    store_arc
        .update_status(store_id, TaskStatus::InProgress)
        .await
        .unwrap();
    store_arc
        .set_fields(store_id, &[("agent", serde_json::json!("claude"))])
        .await
        .unwrap();

    let store = Some(Arc::clone(&store_arc));
    let outcome = handle_failover(
        &task_id,
        "claude",
        RetryableError::Timeout,
        "claude timed out after 1804s",
        false,
        &store,
        "owner/repo",
    )
    .await;
    assert_eq!(outcome.status, "new");
    assert_eq!(outcome.next_agent.as_deref(), Some("codex"));

    let runner = TaskRunner::new("owner/repo".to_string()).with_store(Arc::clone(&store_arc));
    runner
        .clear_agent_on_reroute_unless_fallback(&task_id, "claude")
        .await;

    let task = store::opt_store_get_task(&store, "owner/repo", &task_id)
        .await
        .unwrap();
    assert_eq!(task.agent.as_deref(), Some("codex"));
    assert_eq!(task.status, TaskStatus::InProgress);
    assert!(
        task.model.as_deref().unwrap_or("").is_empty(),
        "model must stay empty so routing picks one for the fallback, got {:?}",
        task.model
    );

    let mut config = RouterConfig::default();
    let medium = config.model_map.entry("medium".to_string()).or_default();
    medium.insert("claude".to_string(), vec!["haiku".to_string()]);
    medium.insert("codex".to_string(), vec!["gpt-test".to_string()]);
    let agents = vec!["claude".to_string(), "codex".to_string()];
    let mut router = Router::new_for_test(config, agents);
    let labeled = ExternalTask {
        id: ExternalId(task_id.clone()),
        title: "Test".to_string(),
        body: "Test body".to_string(),
        state: "open".to_string(),
        labels: vec!["agent:claude".to_string()],
        author: "test".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        url: format!("https://github.com/owner/repo/issues/{task_id}"),
    };
    let result = router
        .route(&labeled, &store_arc, "owner/repo")
        .await
        .unwrap();
    assert_eq!(result.agent, "codex");
}

/// A task that times out on every run blocks after `max_timeout_retries` and keeps its issue open
#[cfg(unix)]
#[serial(cooldown_state)]
#[tokio::test]
async fn repeated_timeouts_block_the_task() {
    let _env = failover_env();
    cooldown::reset_global_state().await;
    set_agent_cooldown("codex", 600).await;
    let (store, task_id) = failover_task().await;
    let store = Some(store);

    let mut statuses = Vec::new();
    for _ in 0..3 {
        statuses.push(
            handle_failover(
                &task_id,
                "claude",
                RetryableError::Timeout,
                "claude timed out after 1804s",
                false,
                &store,
                "owner/repo",
            )
            .await
            .status,
        );
    }

    assert_eq!(statuses, ["new", "new", "blocked"]);
    let task = store::opt_store_get_task(&store, "owner/repo", &task_id)
        .await
        .unwrap();
    assert_eq!(task.block_reason.as_deref(), Some("timed out 3 times"));
    assert_eq!(task.timeout_retries, 3);
}
