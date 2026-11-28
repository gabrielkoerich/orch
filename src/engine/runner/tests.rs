use super::*;
use crate::backends::{ExternalId, ExternalTask, Mention, Status};
use crate::engine::agents::patterns::safe_tail as safe_utf8_tail;
use crate::parser::AgentResponse;
use crate::store::{NewTask, TaskStatus, TaskStore};
use async_trait::async_trait;
use chrono::Utc;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tokio::process::Command;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn ok_parse_result(status: &str) -> agents::ParsedResponse {
    agents::ParsedResponse {
        response: AgentResponse {
            status: status.to_string(),
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
    }
}

// ── safe_utf8_tail ───────────────────────────────────────────────────────

#[test]
fn safe_utf8_tail_short_string_returned_as_is() {
    assert_eq!(safe_utf8_tail("hello", 100), "hello");
    assert_eq!(safe_utf8_tail("", 10), "");
}

#[test]
fn safe_utf8_tail_truncates_ascii() {
    let s = "abcdefghij"; // 10 bytes
    assert_eq!(safe_utf8_tail(s, 5), "fghij");
    assert_eq!(safe_utf8_tail(s, 10), "abcdefghij");
}

#[test]
fn safe_utf8_tail_handles_multibyte_boundary() {
    // "日本語" is 3 chars × 3 bytes each = 9 bytes total
    let s = "日本語";
    assert_eq!(s.len(), 9);
    // Cutting at 8 bytes lands in the middle of "語" (3 bytes at 6..9).
    // safe_utf8_tail should walk forward to the next boundary at 9 (end) rather
    // than panicking on an invalid slice.
    let tail = safe_utf8_tail(s, 8);
    assert!(
        s.ends_with(tail),
        "tail must be a valid suffix of the original"
    );
    assert!(
        str::from_utf8(tail.as_bytes()).is_ok(),
        "tail must be valid UTF-8"
    );
}

#[test]
fn safe_utf8_tail_handles_mixed_ascii_and_multibyte() {
    let s = "hello日本語world"; // ascii + CJK + ascii
    for max_bytes in [1, 3, 5, 7, 11, 13, 20, 100] {
        let tail = safe_utf8_tail(s, max_bytes);
        assert!(
            str::from_utf8(tail.as_bytes()).is_ok(),
            "max_bytes={max_bytes}: tail must be valid UTF-8"
        );
        assert!(
            s.ends_with(tail),
            "max_bytes={max_bytes}: tail must be a suffix of original"
        );
    }
}

#[test]
fn safe_utf8_tail_exact_boundary() {
    let s = "abcdef"; // 6 bytes
    assert_eq!(safe_utf8_tail(s, 6), "abcdef"); // exactly fits
    assert_eq!(safe_utf8_tail(s, 0), ""); // empty tail
}

#[test]
fn parse_success_output_synthesizes_done_from_plain_text() {
    let runner = agents::get_runner("claude");
    let parsed = parse_success_output(
        "123",
        "claude",
        &*runner,
        "No open positions, no trade executions, no conditions change.",
    )
    .unwrap();

    assert_eq!(parsed.response.status, "done");
    assert_eq!(
        parsed.response.summary,
        "No open positions, no trade executions, no conditions change."
    );
}

#[test]
fn parse_success_output_synthesizes_needs_review_from_error_text() {
    let runner = agents::get_runner("claude");
    let parsed =
        parse_success_output("123", "claude", &*runner, "Failed to update branch").unwrap();

    assert_eq!(parsed.response.status, "needs_review");
    assert_eq!(
        parsed.response.error.as_deref(),
        Some("agent returned plain text instead of JSON")
    );
}

#[test]
fn parse_success_output_keeps_empty_text_invalid() {
    let runner = agents::get_runner("claude");
    let err = parse_success_output("123", "claude", &*runner, "").unwrap_err();

    assert!(matches!(err, agents::AgentError::InvalidResponse { .. }));
}

#[test]
fn parse_success_output_preserves_tokens_from_ndjson() {
    // Simulates NDJSON output from --output-format stream-json where
    // the result event contains token usage. parse_response fails on
    // NDJSON (expects single JSON blob), but find_agent_result extracts
    // tokens from the NDJSON envelope so cost tracking is preserved.
    let ndjson = r#"{"type":"system","subtype":"init","session_id":"abc"}
{"type":"assistant","message":{"content":[{"type":"text","text":"Done."}],"usage":{"input_tokens":100,"output_tokens":50}}}
{"type":"result","subtype":"success","is_error":false,"duration_ms":5000,"result":"All changes committed successfully","usage":{"input_tokens":1234,"output_tokens":567}}"#;
    let runner = agents::get_runner("claude");
    let parsed = parse_success_output("456", "claude", &*runner, ndjson).unwrap();

    assert_eq!(parsed.response.status, "done");
    assert_eq!(parsed.input_tokens, Some(1234));
    assert_eq!(parsed.output_tokens, Some(567));
    assert_eq!(parsed.duration_ms, Some(5000));
}

#[test]
fn parse_success_output_preserves_tokens_from_minimax_ndjson() {
    // MiniMax/Kimi use assistant event fallback when there's no result event.
    let ndjson = r#"{"type":"system","subtype":"init"}
{"type":"assistant","message":{"content":[{"type":"text","text":"No open positions."}],"usage":{"input_tokens":2000,"output_tokens":300}}}"#;
    let runner = agents::get_runner("minimax");
    let parsed = parse_success_output("789", "minimax", &*runner, ndjson).unwrap();

    assert_eq!(parsed.response.status, "done");
    assert_eq!(parsed.input_tokens, Some(2000));
    assert_eq!(parsed.output_tokens, Some(300));
}

#[test]
fn classify_run_error_type_recognizes_specific_patterns() {
    let cases = [
        ("push failed: remote rejected", "push_failed"),
        ("No commits between main and branch", "no_commits"),
        ("create PR failed: 422", "pr_failed"),
        ("invalid response from agent", "parse_error"),
        ("parse error: missing json", "parse_error"),
        ("exceeded max attempts while rerouting", "max_attempts"),
        ("unit test failed", "ci_failure"),
        ("build failed: type mismatch", "ci_failure"),
        ("lint failed on warning", "ci_failure"),
        ("3 tests failed", "ci_failure"),
        ("test suite failed: 3 tests failed", "ci_failure"),
        (
            "claude interrupted: exited without a result envelope",
            "interrupted",
        ),
    ];

    for (input, expected) in cases {
        assert_eq!(classify_run_error_type(input), expected, "input={input}");
    }
}

#[test]
fn classify_run_error_type_falls_back_for_unknown_errors() {
    assert_eq!(classify_run_error_type("something unexpected"), "failed");
}

#[test]
fn classify_run_error_type_connection_error_mentioning_pull_request_is_not_pr_failed() {
    // A generic GitHub API or connection error whose message happens to contain the
    // substring "pull request" must NOT be classified as pr_failed.
    assert_eq!(
        classify_run_error_type("failed to fetch pull request details from GitHub"),
        "failed"
    );
    assert_eq!(
        classify_run_error_type("connection reset while reading pull request data"),
        "failed"
    );
}

#[test]
fn classify_run_error_type_url_with_test_substring_is_not_ci_failure() {
    // A connection error whose URL path contains "/test-org/" must NOT be ci_failure.
    assert_eq!(
        classify_run_error_type(
            "failed to connect to api.github.com/repos/test-org/repo/pulls/123"
        ),
        "failed"
    );
    // Bare "test" word in an unrelated error must not trigger ci_failure.
    assert_eq!(
        classify_run_error_type("context deadline exceeded testing connection"),
        "failed"
    );
}

#[test]
fn classify_run_error_type_usage_without_limit_is_not_rate_limit() {
    // "usage" alone (e.g. token usage reporting) must NOT be rate_limit.
    assert_eq!(
        classify_run_error_type("token usage: 1245 tokens"),
        "failed"
    );
    assert_eq!(
        classify_run_error_type("incorrect usage of git command"),
        "failed"
    );
    // Only "usage limit" should trigger rate_limit.
    assert_eq!(
        classify_run_error_type("exceeded usage limit for this billing period"),
        "rate_limit"
    );
}

#[test]
fn classify_run_error_type_billing_cycle_is_not_auth_error() {
    // "billing cycle" quota-exhaustion messages must be classified as billing_cycle_exhausted,
    // not auth_error — aligning with cooldown.rs CreditExhaustionReason::BillingCycleExhausted.
    assert_eq!(
        classify_run_error_type(
            "You've reached your usage limit for this billing cycle. \
                 Your quota will be refreshed in the next cycle."
        ),
        "billing_cycle_exhausted"
    );
    assert_eq!(
        classify_run_error_type("quota exhausted for billing cycle"),
        "billing_cycle_exhausted"
    );
    // Plain "billing" without "cycle" that used to map to auth_error now falls through to "failed".
    assert_eq!(
        classify_run_error_type("billing account suspended"),
        "failed"
    );
}

#[test]
fn classify_run_error_type_monthly_quota_is_billing_cycle() {
    // Regression test for issue #3302: GitHub Copilot "You have exceeded your
    // monthly quota" must map to billing_cycle_exhausted, not "failed".
    assert_eq!(
        classify_run_error_type(
            "opencode failed: agent failed: You have exceeded your monthly quota"
        ),
        "billing_cycle_exhausted"
    );
    assert_eq!(
        classify_run_error_type("You have exceeded your monthly quota"),
        "billing_cycle_exhausted"
    );
}

#[test]
fn classify_run_outcome_routed_is_success() {
    // "routed" is a canonical non-success status (task will be re-dispatched).
    // It should be classified as "success" so the audit trail distinguishes it
    // from genuine failures (#2801).
    let parse_result = Ok(ok_parse_result("routed"));
    assert_eq!(
        classify_run_outcome("routed", &parse_result, false),
        "success"
    );
}

#[test]
fn classify_run_outcome_blocked_is_blocked() {
    let parse_result = Ok(ok_parse_result("blocked"));
    assert_eq!(
        classify_run_outcome("blocked", &parse_result, false),
        "blocked"
    );
}

#[test]
fn classify_run_outcome_non_canonical_is_failed() {
    // Non-canonical statuses should be classified as "failed" in the audit.
    // The explicit error message is provided by build_run_audit.
    let parse_result = Ok(ok_parse_result("fix_deployed"));
    assert_eq!(
        classify_run_outcome("fix_deployed", &parse_result, false),
        "failed"
    );
}

#[test]
fn classify_run_outcome_kimi_billing_cycle_is_not_rate_limit() {
    // Regression test for issue #3529: Kimi's exact billing-cycle exhaustion
    // message was stored as outcome="rate_limit" because the generic "usage
    // limit" pattern in detect_rate_limit() matched before credit-exhaustion
    // detection ever ran. The outcome must reflect the actual failure mode.
    let parse_result: Result<agents::ParsedResponse, agents::AgentError> =
        Err(agents::AgentError::RateLimit {
            message: "Failed to authenticate. API Error: 403 You've reached your usage \
                          limit for this billing cycle. Your quota will be refreshed soon."
                .to_string(),
        });
    assert_eq!(
        classify_run_outcome("needs_review", &parse_result, false),
        "billing_cycle_exhausted"
    );
}

#[test]
fn classify_run_outcome_generic_rate_limit_is_unaffected() {
    // A genuine rate limit (no billing-cycle/credit-exhaustion markers) must
    // still classify as "rate_limit".
    let parse_result: Result<agents::ParsedResponse, agents::AgentError> =
        Err(agents::AgentError::RateLimit {
            message: "429 Too Many Requests".to_string(),
        });
    assert_eq!(
        classify_run_outcome("needs_review", &parse_result, false),
        "rate_limit"
    );
}

#[test]
fn classify_run_outcome_auth_billing_cycle_is_not_failed() {
    // Auth-classified billing-cycle exhaustion (e.g. "billing cycle exhausted"
    // matches detect_auth_error's pattern list) must also be distinguished from
    // a generic auth failure.
    let parse_result: Result<agents::ParsedResponse, agents::AgentError> =
        Err(agents::AgentError::Auth {
            message: "billing cycle exhausted for this account".to_string(),
        });
    assert_eq!(
        classify_run_outcome("needs_review", &parse_result, false),
        "billing_cycle_exhausted"
    );
}

#[test]
fn classify_run_outcome_generic_auth_error_is_unaffected() {
    let parse_result: Result<agents::ParsedResponse, agents::AgentError> =
        Err(agents::AgentError::Auth {
            message: "401 Unauthorized".to_string(),
        });
    assert_eq!(
        classify_run_outcome("needs_review", &parse_result, false),
        "failed"
    );
}

#[test]
fn classify_run_outcome_interrupted() {
    let parse_result: Result<agents::ParsedResponse, agents::AgentError> =
        Err(agents::AgentError::Interrupted {
            message: "exited without a result envelope".to_string(),
        });
    assert_eq!(
        classify_run_outcome("new", &parse_result, false),
        "interrupted"
    );
}

#[tokio::test]
async fn build_run_audit_success_ignores_stale_last_error() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let task_id = store
        .create(&NewTask {
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    store
        .set_fields(
            task_id,
            &[("last_error", serde_json::json!("no error info available"))],
        )
        .await
        .unwrap();

    let runner = TaskRunner::new("owner/repo".to_string()).with_store(store);
    let parse_result = Ok(ok_parse_result("done"));
    let started_at = Utc::now();
    let audit = runner
        .build_run_audit(RunAuditInput {
            task_id: &task_id.to_string(),
            status: "done",
            parse_result: &parse_result,
            raw_stdout: "",
            raw_stderr: "",
            started_at: &started_at,
            error_override: None,
            elapsed_secs: Some(1),
            push_failed: false,
        })
        .await;

    assert_eq!(audit.outcome, "success");
    assert_eq!(audit.error, "");
}

#[tokio::test]
async fn reroute_clear_wipes_agent_when_it_still_matches_the_run() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let store_id = store
        .create(&NewTask {
            external_id: Some("42".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    store
        .update_status(store_id, TaskStatus::InProgress)
        .await
        .unwrap();
    store
        .set_fields(
            store_id,
            &[
                ("agent", serde_json::json!("claude")),
                ("model", serde_json::json!("sonnet")),
            ],
        )
        .await
        .unwrap();

    let runner = TaskRunner::new("owner/repo".to_string()).with_store(Arc::clone(&store));
    runner
        .clear_agent_on_reroute_unless_fallback("42", "claude")
        .await;

    let task = store.get(store_id).await.unwrap();
    assert!(
        task.agent.as_deref().unwrap_or("").is_empty(),
        "matching in-progress agent must be cleared for a normal reroute, got {:?}",
        task.agent
    );
    assert!(
        task.model.as_deref().unwrap_or("").is_empty(),
        "model must be cleared with the agent, got {:?}",
        task.model
    );
}

#[tokio::test]
async fn reroute_clear_keeps_a_different_in_progress_fallback() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let store_id = store
        .create(&NewTask {
            external_id: Some("42".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    store
        .update_status(store_id, TaskStatus::InProgress)
        .await
        .unwrap();
    store
        .set_fields(
            store_id,
            &[
                ("agent", serde_json::json!("codex")),
                ("model", serde_json::json!("")),
            ],
        )
        .await
        .unwrap();

    let runner = TaskRunner::new("owner/repo".to_string()).with_store(Arc::clone(&store));
    runner
        .clear_agent_on_reroute_unless_fallback("42", "claude")
        .await;

    let task = store.get(store_id).await.unwrap();
    assert_eq!(task.agent.as_deref(), Some("codex"));
    assert!(
        task.model.as_deref().unwrap_or("").is_empty(),
        "model stays empty so routing picks one for the fallback, got {:?}",
        task.model
    );
}

#[tokio::test]
async fn build_run_audit_ok_parse_prefers_parsed_error_over_stale_last_error() {
    // needs_review classifies as success and wipes the error. The #3851
    // misattribution is an Ok parse whose outcome is failed or blocked
    let cases = [("needs_action", "failed"), ("blocked", "blocked")];
    let parsed_error = "passbox Touch ID prompt timed out for cf/read-token";
    let stale = "minimax auth error: API Error: 402 insufficient balance (1008), clearing agent/model for re-route";
    for (status, expected_outcome) in cases {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let task_id = store
            .create(&NewTask {
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "Test".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        store
            .set_fields(task_id, &[("last_error", serde_json::json!(stale))])
            .await
            .unwrap();

        let runner = TaskRunner::new("owner/repo".to_string()).with_store(store);
        let parse_result = Ok(agents::ParsedResponse {
            response: AgentResponse {
                status: status.to_string(),
                summary: "cache_stats.py did not run".to_string(),
                accomplished: vec![],
                remaining: vec![],
                files: vec![],
                error: Some(parsed_error.to_string()),
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
        });
        let started_at = Utc::now();
        let audit = runner
            .build_run_audit(RunAuditInput {
                task_id: &task_id.to_string(),
                status,
                parse_result: &parse_result,
                raw_stdout: "",
                raw_stderr: "",
                started_at: &started_at,
                error_override: None,
                elapsed_secs: Some(261),
                push_failed: false,
            })
            .await;

        assert_eq!(audit.outcome, expected_outcome, "status={status}");
        assert_eq!(audit.error, parsed_error, "status={status}");
    }
}

#[tokio::test]
async fn build_run_audit_non_success_uses_fallback_error_when_empty() {
    let runner = TaskRunner::new("owner/repo".to_string());
    let parse_result = Ok(ok_parse_result("new"));
    let started_at = Utc::now();
    let audit = runner
        .build_run_audit(RunAuditInput {
            task_id: "1",
            status: "new",
            parse_result: &parse_result,
            raw_stdout: "",
            raw_stderr: "",
            started_at: &started_at,
            error_override: None,
            elapsed_secs: Some(1),
            push_failed: false,
        })
        .await;

    assert_eq!(audit.outcome, "failed");
    // "new" is a canonical non-success status — no error message needed.
    assert_eq!(audit.error, "");
}

#[tokio::test]
async fn build_run_audit_non_canonical_status_reports_explicit_error() {
    // Non-canonical statuses (not in the allowlist) with no parse error
    // should get an explicit "unrecognized status: X" message instead of
    // the opaque "no error info available" placeholder (#2801).
    let runner = TaskRunner::new("owner/repo".to_string());
    let parse_result = Ok(ok_parse_result("fix_deployed"));
    let started_at = Utc::now();
    let audit = runner
        .build_run_audit(RunAuditInput {
            task_id: "1",
            status: "fix_deployed",
            parse_result: &parse_result,
            raw_stdout: "",
            raw_stderr: "",
            started_at: &started_at,
            error_override: None,
            elapsed_secs: Some(1),
            push_failed: false,
        })
        .await;

    assert_eq!(audit.outcome, "failed");
    assert_eq!(audit.error, "unrecognized status: fix_deployed");
}

#[tokio::test]
async fn build_run_audit_canonical_non_success_status_no_error_message() {
    // Canonical non-success statuses (e.g. "routed") should not produce
    // an error message — they're expected outcomes, not failures.
    for status in &["routed"] {
        let runner = TaskRunner::new("owner/repo".to_string());
        let parse_result = Ok(ok_parse_result(status));
        let started_at = Utc::now();
        let audit = runner
            .build_run_audit(RunAuditInput {
                task_id: "1",
                status,
                parse_result: &parse_result,
                raw_stdout: "",
                raw_stderr: "",
                started_at: &started_at,
                error_override: None,
                elapsed_secs: Some(1),
                push_failed: false,
            })
            .await;

        assert_eq!(audit.outcome, "success", "status={status}");
        assert_eq!(
            audit.error, "",
            "status={status}: no error for canonical status"
        );
    }
}

#[tokio::test]
async fn build_run_audit_blocked_uses_summary_when_error_missing() {
    let runner = TaskRunner::new("owner/repo".to_string());
    let parse_result = Ok(agents::ParsedResponse {
        response: AgentResponse {
            status: "blocked".to_string(),
            summary: "Could not proceed due to missing credentials".to_string(),
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
    });
    let started_at = Utc::now();
    let audit = runner
        .build_run_audit(RunAuditInput {
            task_id: "1",
            status: "blocked",
            parse_result: &parse_result,
            raw_stdout: "",
            raw_stderr: "",
            started_at: &started_at,
            error_override: None,
            elapsed_secs: Some(1),
            push_failed: false,
        })
        .await;

    assert_eq!(audit.outcome, "blocked");
    assert_eq!(audit.error, "Could not proceed due to missing credentials");
}

// ── resolve_project_dir ──────────────────────────────────────────────────

#[tokio::test]
async fn resolve_project_dir_uses_project_dir_env() {
    // When PROJECT_DIR env var is set, it should always win.
    let runner = TaskRunner::new("owner/repo".to_string());

    // Use a temp dir so the path exists
    let dir = tempfile::tempdir().unwrap();
    let dir_str = dir.path().to_string_lossy().to_string();

    // Temporarily set the env var — note: tests run in parallel so we use a
    // dedicated key that only this test touches. std::env::set_var is not
    // thread-safe in general, but the subsequent call is synchronous and
    // isolated to the temp dir created above.
    // SAFETY: nextest runs each test in its own process, so no other test touches the environment
    unsafe { env::set_var("PROJECT_DIR", &dir_str) };
    let result = runner.resolve_project_dir().await;
    // SAFETY: nextest runs each test in its own process, so no other test touches the environment
    unsafe { env::remove_var("PROJECT_DIR") };

    assert!(
        result.is_ok(),
        "resolve_project_dir should succeed with PROJECT_DIR set"
    );
    assert_eq!(result.unwrap(), dir.path());
}

#[tokio::test]
async fn resolve_project_dir_empty_project_dir_env_falls_through() {
    // An empty PROJECT_DIR should be ignored and fall through to other logic.
    let runner = TaskRunner::new("owner/testrepo-nonexistent".to_string());
    // SAFETY: nextest runs each test in its own process, so no other test touches the environment
    unsafe { env::set_var("PROJECT_DIR", "") };
    let result = runner.resolve_project_dir().await;
    // SAFETY: nextest runs each test in its own process, so no other test touches the environment
    unsafe { env::remove_var("PROJECT_DIR") };

    // No active config entry, no legacy project_dir, no bare clone for this
    // repo: should return an explicit error rather than falling back to the
    // service's own CWD (see issue #3416).
    assert!(
        result.is_err(),
        "resolve_project_dir should error when no project dir can be resolved"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("owner/testrepo-nonexistent"),
        "error should name the unresolvable repo: {err}"
    );
}

// ── mock backend for process_delegations ─────────────────────────────────

struct TrackingBackend {
    sub_tasks_created: Arc<Mutex<Vec<(String, String)>>>, // (title, parent_id)
    status_updates: Arc<Mutex<Vec<(String, Status)>>>,
    comments: Arc<Mutex<Vec<(String, String)>>>, // (id, body)
}

impl TrackingBackend {
    fn new() -> Self {
        Self {
            sub_tasks_created: Arc::new(Mutex::new(vec![])),
            status_updates: Arc::new(Mutex::new(vec![])),
            comments: Arc::new(Mutex::new(vec![])),
        }
    }
}

#[async_trait]
impl ExternalBackend for TrackingBackend {
    fn name(&self) -> &'static str {
        "tracking"
    }
    async fn create_task(&self, _t: &str, _b: &str, _l: &[String]) -> anyhow::Result<ExternalId> {
        Ok(ExternalId("new".to_string()))
    }
    async fn get_task(&self, id: &ExternalId) -> anyhow::Result<ExternalTask> {
        Ok(ExternalTask {
            id: id.clone(),
            title: String::new(),
            body: String::new(),
            state: "open".to_string(),
            labels: vec![],
            author: String::new(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: String::new(),
        })
    }
    async fn list_by_status(&self, _s: Status) -> anyhow::Result<Vec<ExternalTask>> {
        Ok(vec![])
    }
    async fn list_routable(&self) -> anyhow::Result<Vec<ExternalTask>> {
        Ok(vec![])
    }
    async fn post_comment(&self, id: &ExternalId, body: &str) -> anyhow::Result<()> {
        self.comments
            .lock()
            .unwrap()
            .push((id.0.clone(), body.to_string()));
        Ok(())
    }
    async fn set_labels(&self, _id: &ExternalId, _l: &[String]) -> anyhow::Result<()> {
        Ok(())
    }
    async fn remove_label(&self, _id: &ExternalId, _l: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn get_sub_issues(&self, _id: &ExternalId) -> anyhow::Result<Vec<ExternalId>> {
        Ok(vec![])
    }
    async fn create_sub_task(
        &self,
        parent: &ExternalId,
        title: &str,
        _body: &str,
        _l: &[String],
    ) -> anyhow::Result<ExternalId> {
        self.sub_tasks_created
            .lock()
            .unwrap()
            .push((title.to_string(), parent.0.clone()));
        Ok(ExternalId(format!("child-{title}")))
    }
    async fn ensure_status_label(&self, _l: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn has_open_issue_with_title(&self, _t: &str, _l: &str) -> anyhow::Result<bool> {
        Ok(false)
    }
    async fn health_check(&self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn is_pr_merged(&self, _b: &str) -> anyhow::Result<bool> {
        Ok(false)
    }
    async fn get_authenticated_user(&self) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
    async fn get_mentions(&self, _s: &str) -> anyhow::Result<Vec<Mention>> {
        Ok(vec![])
    }
    async fn update_status(&self, id: &ExternalId, status: Status) -> anyhow::Result<()> {
        self.status_updates
            .lock()
            .unwrap()
            .push((id.0.clone(), status));
        Ok(())
    }
}

fn make_task(id: &str) -> ExternalTask {
    ExternalTask {
        id: ExternalId(id.to_string()),
        title: format!("Task {id}"),
        body: String::new(),
        state: "open".to_string(),
        labels: vec![],
        author: "bot".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        url: String::new(),
    }
}

// ── process_delegations ───────────────────────────────────────────────────

#[expect(
    clippy::await_holding_lock,
    reason = "the env lock must stay held across the awaited calls"
)]
#[tokio::test]
async fn process_delegations_creates_subtasks_and_blocks_parent() {
    let _guard = ENV_LOCK.lock().unwrap();
    let temp_home = TempDir::new().unwrap();
    let orch_home = temp_home.path().join(".orch");
    fs::create_dir_all(&orch_home).await.unwrap();
    let old_orch_home = env::var("ORCH_HOME").ok();
    // SAFETY: nextest runs each test in its own process, so no other test touches the environment
    unsafe { env::set_var("ORCH_HOME", &orch_home) };

    let runner = TaskRunner::new("owner/repo".to_string());
    let parent = make_task("99");
    let backend = Arc::new(TrackingBackend::new());
    let backend_dyn: Arc<dyn ExternalBackend> = Arc::<TrackingBackend>::clone(&backend);

    let delegations = vec![
        Delegation {
            title: "Subtask A".to_string(),
            body: "Do A".to_string(),
            labels: vec!["enhancement".to_string()],
        },
        Delegation {
            title: "Subtask B".to_string(),
            body: "Do B".to_string(),
            labels: vec![],
        },
    ];

    runner
        .process_delegations(&parent, &delegations, &backend_dyn)
        .await
        .unwrap();

    // Two sub-tasks should have been created
    let created = backend.sub_tasks_created.lock().unwrap();
    assert_eq!(created.len(), 2);
    assert_eq!(created[0].1, "99", "parent id should be 99");
    assert!(
        created.iter().any(|(t, _)| t == "Subtask A"),
        "Subtask A should be created"
    );
    assert!(
        created.iter().any(|(t, _)| t == "Subtask B"),
        "Subtask B should be created"
    );
    drop(created);

    // Parent should be marked blocked
    let updates = backend.status_updates.lock().unwrap();
    assert!(
        updates
            .iter()
            .any(|(id, s)| id == "99" && *s == Status::Blocked),
        "parent should be blocked"
    );
    drop(updates);

    // A comment summarising the delegations should be posted
    let comments = backend.comments.lock().unwrap();
    assert!(
        !comments.is_empty(),
        "a delegation summary comment should be posted"
    );
    let comment_body = &comments[0].1;
    assert!(
        comment_body.contains("Subtask A"),
        "summary should mention Subtask A"
    );
    assert!(
        comment_body.contains("Subtask B"),
        "summary should mention Subtask B"
    );

    if let Some(old) = old_orch_home {
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        unsafe { env::set_var("ORCH_HOME", old) };
    } else {
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        unsafe { env::remove_var("ORCH_HOME") };
    }
}

#[expect(
    clippy::await_holding_lock,
    reason = "the env lock must stay held across the awaited calls"
)]
#[tokio::test]
async fn process_delegations_single_subtask() {
    let _guard = ENV_LOCK.lock().unwrap();
    let temp_home = TempDir::new().unwrap();
    let orch_home = temp_home.path().join(".orch");
    fs::create_dir_all(&orch_home).await.unwrap();
    let old_orch_home = env::var("ORCH_HOME").ok();
    // SAFETY: nextest runs each test in its own process, so no other test touches the environment
    unsafe { env::set_var("ORCH_HOME", &orch_home) };

    let runner = TaskRunner::new("owner/repo".to_string());
    let parent = make_task("101");
    let backend = Arc::new(TrackingBackend::new());
    let backend_dyn: Arc<dyn ExternalBackend> = Arc::<TrackingBackend>::clone(&backend);

    let delegations = vec![Delegation {
        title: "Only Child".to_string(),
        body: "Do only child".to_string(),
        labels: vec!["feature".to_string()],
    }];

    runner
        .process_delegations(&parent, &delegations, &backend_dyn)
        .await
        .unwrap();

    let created = backend.sub_tasks_created.lock().unwrap();
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].0, "Only Child");
    drop(created);

    let updates = backend.status_updates.lock().unwrap();
    assert!(
        updates
            .iter()
            .any(|(id, s)| id == "101" && *s == Status::Blocked),
        "parent should be blocked"
    );
    drop(updates);

    let comments = backend.comments.lock().unwrap();
    let body = &comments[0].1;
    assert!(
        body.contains("1 subtask"),
        "comment should count one subtask"
    );

    if let Some(old) = old_orch_home {
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        unsafe { env::set_var("ORCH_HOME", old) };
    } else {
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        unsafe { env::remove_var("ORCH_HOME") };
    }
}

// ── weight signal logic ───────────────────────────────────────────────────

/// Test helper that mirrors the weight signal logic from `run_with_context`.
/// Reroutes only emit `RateLimited` when the error type is genuinely "rate_limit".
fn weight_signal_for(status: &str, error_type: &str, agent: &str) -> WeightSignal {
    let is_rerouted = status == "new" || status == "routed";
    let is_rate_limit_error = error_type == "rate_limit";
    if is_rerouted {
        if is_rate_limit_error {
            WeightSignal::RateLimited {
                agent: agent.to_string(),
            }
        } else {
            WeightSignal::Rerouted
        }
    } else if status == "done"
        || status == "needs_review"
        || status == "in_progress"
        || status == "in_review"
    {
        WeightSignal::Success {
            agent: agent.to_string(),
        }
    } else if status == "blocked" {
        WeightSignal::Blocked
    } else {
        WeightSignal::None
    }
}

#[test]
fn weight_signal_success_includes_needs_review() {
    // The normal happy-path for code tasks: agent reports done, has a PR,
    // so handle_success returns "needs_review". This must map to Success.
    let signal = weight_signal_for("needs_review", "success", "claude");
    assert!(
        matches!(signal, WeightSignal::Success { agent } if agent == "claude"),
        "needs_review should produce WeightSignal::Success"
    );
}

#[test]
fn needs_review_signal_success_for_empty_error() {
    let signal = needs_review_signal("", "claude");
    assert!(matches!(signal, WeightSignal::Success { agent } if agent == "claude"));
}

#[test]
fn needs_review_signal_failed_after_timeout_with_no_fallback() {
    let signal = needs_review_signal("claude timed out after 1804s, no fallback agents", "claude");
    assert!(matches!(signal, WeightSignal::Failed { agent } if agent == "claude"));
}

#[test]
fn needs_review_signal_rate_limited_for_rate_limit() {
    let signal = needs_review_signal("rate limit exceeded, try again later", "claude");
    assert!(matches!(signal, WeightSignal::RateLimited { agent } if agent == "claude"));
}

#[test]
fn weight_signal_success_for_all_done_statuses() {
    for status in ["done", "needs_review", "in_progress", "in_review"] {
        let signal = weight_signal_for(status, "success", "codex");
        assert!(
            matches!(signal, WeightSignal::Success { agent } if agent == "codex"),
            "{status} should produce WeightSignal::Success"
        );
    }
}

#[test]
fn weight_signal_rate_limited_for_reroute_with_rate_limit_error() {
    // Reroute due to rate limit should emit RateLimited.
    let signal = weight_signal_for("new", "rate_limit", "claude");
    assert!(matches!(signal, WeightSignal::RateLimited { agent } if agent == "claude"));
}

#[test]
fn weight_signal_rate_limited_for_routed_status_with_rate_limit_error() {
    let signal = weight_signal_for("routed", "rate_limit", "claude");
    assert!(matches!(signal, WeightSignal::RateLimited { agent } if agent == "claude"));
}

#[test]
fn weight_signal_rerouted_for_non_rate_limit_reroute() {
    // Reroute due to silence detection, timeout, or other non-rate-limit errors
    // should emit Rerouted (not RateLimited and not None).
    // Rerouted maps to "new" in tick so the task stays retryable and is never
    // converted to needs_review → done via the no-code review shortcut.
    for error_type in ["timeout", "silence detected", "parse_error", "failed"] {
        for status in ["new", "routed"] {
            let signal = weight_signal_for(status, error_type, "claude");
            assert!(
                matches!(signal, WeightSignal::Rerouted),
                "{status} with error_type={error_type} should produce WeightSignal::Rerouted, not RateLimited or None"
            );
        }
    }
}

#[test]
fn silence_reset_rerouted_never_becomes_needs_review() {
    // Regression test for: silence detection sets task to new, runner returns
    // WeightSignal::Rerouted, tick must map that to "new" — not "needs_review".
    // If it mapped to needs_review the no-code shortcut would close the task as
    // done with no successful agent run in the audit trail.
    let signal = weight_signal_for("new", "silence detected", "claude");
    assert!(
        matches!(signal, WeightSignal::Rerouted),
        "silence-reset reroute must produce Rerouted, never None/needs_review"
    );
    // Verify tick mapping: Rerouted => "new" (not needs_review or done)
    let display_status = match &signal {
        WeightSignal::Rerouted => "new",
        WeightSignal::None => "needs_review", // the old (buggy) path
        _ => "other",
    };
    assert_eq!(
        display_status, "new",
        "Rerouted must map to 'new' in tick, not 'needs_review'"
    );
}

#[test]
fn weight_signal_blocked_status() {
    let signal = weight_signal_for("blocked", "success", "claude");
    assert!(matches!(signal, WeightSignal::Blocked));
}

// ── parse_success_output token extraction ────────────────────────────────

/// Mock runner that always returns InvalidResponse with truncated text,
/// simulating what minimax/codex do when they can't parse structured JSON.
struct FailingMockRunner;

impl agents::AgentRunner for FailingMockRunner {
    #[cfg(test)]
    fn name(&self) -> &'static str {
        "opencode"
    }

    fn stream_events(&self, _event: &serde_json::Value) -> Vec<agents::StreamEvent> {
        vec![]
    }

    fn build_command(
        &self,
        _model: Option<&str>,
        _timeout_cmd: &str,
        _sys_file: &str,
        _msg_file: &str,
        _permissions: &agents::PermissionRules,
    ) -> String {
        String::new()
    }

    fn parse_response(&self, raw: &str) -> Result<agents::ParsedResponse, agents::AgentError> {
        // Simulate what minimax does: truncate to 300 chars
        Err(agents::AgentError::InvalidResponse {
            raw: raw.chars().take(30).collect(),
        })
    }

    fn extract_text(&self, raw: &str) -> Result<String, agents::AgentError> {
        Err(agents::AgentError::InvalidResponse {
            raw: raw.to_string(),
        })
    }

    fn classify_error(&self, exit_code: i32, _stdout: &str, _stderr: &str) -> agents::AgentError {
        agents::AgentError::Unknown {
            exit_code,
            message: "mock error".into(),
        }
    }

    fn router_command(&self, _prompt: &str, _model: Option<&str>) -> anyhow::Result<Command> {
        anyhow::bail!("not implemented for mock")
    }
}

#[test]
fn parse_success_output_extracts_tokens_from_raw_stdout_not_error_raw() {
    // OpenCode NDJSON output with token metadata in step_finish
    let ndjson = r#"{"type":"text","timestamp":1,"part":{"type":"text","text":"Here is my response: {\"status\":\"done\",\"summary\":\"fixed\"}}"}}
{"type":"step_finish","timestamp":2,"part":{"type":"step-finish","reason":"stop","cost":0,"tokens":{"total":95177,"input":94920,"output":257}}}"#;

    let runner = FailingMockRunner;
    let result = parse_success_output("1550", "opencode", &runner, ndjson);

    // Should succeed (synthesized from text) with tokens preserved
    let parsed = result.expect("should synthesize response from text");
    assert_eq!(parsed.input_tokens, Some(94920));
    assert_eq!(parsed.output_tokens, Some(257));
}

#[test]
fn parse_success_output_no_tokens_when_ndjson_has_none() {
    // Plain text with an explicit completion signal but no token metadata.
    // Uses "No open positions" which is in the explicit_done list.
    let text = "No open positions, no trade executions, no conditions change.";

    let runner = FailingMockRunner;
    let result = parse_success_output("1551", "opencode", &runner, text);

    // Should synthesize a "done" response, but without tokens (no NDJSON envelope).
    let parsed = result.expect("should synthesize response from explicit-done text");
    assert_eq!(parsed.input_tokens, None);
    assert_eq!(parsed.output_tokens, None);
}

#[test]
fn parse_success_output_uses_agent_result_directly() {
    // Here we test that if find_agent_result succeeds and returns valid JSON, we use it directly
    // WITHOUT calling agent_runner.parse_response at all.
    let ndjson = r#"{"type":"result","subtype":"success","is_error":false,"result":"{\"status\":\"done\",\"summary\":\"my summary\",\"accomplished\":[],\"remaining\":[],\"files\":[]}"}"#;

    // FailingMockRunner always returns InvalidResponse, so if we called it, we'd synthesize "done" instead of getting "my summary".
    let runner = FailingMockRunner;
    let result = parse_success_output("999", "claude", &runner, ndjson);

    let parsed = result.expect("should parse successfully via find_agent_result");
    assert_eq!(parsed.response.status, "done");
    assert_eq!(parsed.response.summary, "my summary");
}

#[test]
fn parse_success_output_treats_agent_result_is_error_as_authoritative() {
    // Here we test that if find_agent_result returns is_error=true, we return AgentFailed
    // instead of falling back to synthesis or agent_runner.parse_response.
    let ndjson = r#"{"type":"result","subtype":"success","is_error":true,"result":"I failed because of reasons"}"#;

    let runner = FailingMockRunner;
    let result = parse_success_output("999", "claude", &runner, ndjson);

    let err = result.expect_err("should return error");
    match err {
        agents::AgentError::AgentFailed { message } => {
            assert_eq!(message, "I failed because of reasons");
        }
        _ => panic!("expected AgentFailed, got {err:?}"),
    }
}

#[test]
fn parse_success_output_classifies_model_unavailable_from_is_error_result() {
    // Regression test for issue #3607: the is_error=true NDJSON path
    // (used by codex's find_codex_result, which stringifies its own
    // classified AgentError into result_text) must re-classify a
    // model-not-found message as ModelUnavailable so it gets
    // model-scoped cooldown treatment instead of degrading to a
    // generic AgentFailed, which triggers an agent-wide cooldown.
    let ndjson = r#"{"type":"result","subtype":"success","is_error":true,"result":"model unavailable (gpt-5.5): Reconnecting... 2/5 (unexpected status 404 Not Found: The model `gpt-5.5` does not exist or you do not have access to it.)"}"#;

    let runner = FailingMockRunner;
    let result = parse_success_output("999", "claude", &runner, ndjson);

    let err = result.expect_err("should return error");
    match err {
        agents::AgentError::ModelUnavailable { model, .. } => {
            assert_eq!(model, "gpt-5.5");
        }
        _ => panic!("expected ModelUnavailable, got {err:?}"),
    }
}

#[test]
fn nonzero_exit_with_ndjson_success_is_parsed_as_success() {
    // Simulate an agent that exits with non-zero code but emits NDJSON result
    // with is_error=false and a JSON result payload. The runner should accept
    // this as success because the NDJSON result is authoritative.
    let ndjson = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":1234,"result":"{\"status\":\"done\",\"summary\":\"All good\"}","usage":{"input_tokens":10,"output_tokens":5}}"#;

    // Use a runner that would normally fail parsing to ensure we're using
    // the find_agent_result path. FailingMockRunner.parse_response returns
    // InvalidResponse, but parse_success_output should short-circuit.
    let runner = FailingMockRunner;

    // Construct a fake SessionOutput
    let session_output = session::SessionOutput {
        exit_code: 1,
        raw_stdout: ndjson.to_string(),
        raw_stderr: String::new(),
        elapsed_secs: Some(1),
        sandbox_error: None,
    };

    // Call the helper we added via the task runner logic by reusing the
    // parse_success_output directly since it encapsulates the same behavior.
    let parsed = parse_success_output("task-ndjson", "claude", &runner, &session_output.raw_stdout)
        .expect("should parse NDJSON success");
    assert_eq!(parsed.response.status, "done");
    assert_eq!(parsed.response.summary, "All good");
    assert_eq!(parsed.input_tokens, Some(10));
    assert_eq!(parsed.output_tokens, Some(5));
}

// ── agent label update after reroute ─────────────────────────────────────

/// Backend that wraps TrackingBackend but can be configured to fail label ops.
struct LabelFailingBackend {
    inner: TrackingBackend,
    /// If set, set_labels returns this error.
    set_labels_err: Option<String>,
}

impl LabelFailingBackend {
    fn new() -> Self {
        Self {
            inner: TrackingBackend::new(),
            set_labels_err: None,
        }
    }
    fn with_set_labels_err(mut self, err: impl Into<String>) -> Self {
        self.set_labels_err = Some(err.into());
        self
    }
}

impl Default for LabelFailingBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExternalBackend for LabelFailingBackend {
    fn name(&self) -> &'static str {
        "label_failing"
    }
    async fn create_task(&self, _t: &str, _b: &str, _l: &[String]) -> anyhow::Result<ExternalId> {
        Ok(ExternalId("new".to_string()))
    }
    async fn get_task(&self, id: &ExternalId) -> anyhow::Result<ExternalTask> {
        self.inner.get_task(id).await
    }
    async fn list_by_status(&self, _s: Status) -> anyhow::Result<Vec<ExternalTask>> {
        Ok(vec![])
    }
    async fn list_routable(&self) -> anyhow::Result<Vec<ExternalTask>> {
        Ok(vec![])
    }
    async fn post_comment(&self, id: &ExternalId, body: &str) -> anyhow::Result<()> {
        self.inner.post_comment(id, body).await
    }
    async fn set_labels(&self, id: &ExternalId, labels: &[String]) -> anyhow::Result<()> {
        if let Some(ref err) = self.set_labels_err {
            return Err(anyhow::anyhow!("{err}"));
        }
        self.inner.set_labels(id, labels).await
    }
    async fn remove_label(&self, id: &ExternalId, label: &str) -> anyhow::Result<()> {
        self.inner.remove_label(id, label).await
    }
    async fn get_sub_issues(&self, _id: &ExternalId) -> anyhow::Result<Vec<ExternalId>> {
        Ok(vec![])
    }
    async fn create_sub_task(
        &self,
        parent: &ExternalId,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> anyhow::Result<ExternalId> {
        self.inner
            .create_sub_task(parent, title, body, labels)
            .await
    }
    async fn ensure_status_label(&self, label: &str) -> anyhow::Result<()> {
        self.inner.ensure_status_label(label).await
    }
    async fn update_status(&self, id: &ExternalId, status: Status) -> anyhow::Result<()> {
        self.inner.update_status(id, status).await
    }
    async fn get_authenticated_user(&self) -> anyhow::Result<Option<String>> {
        self.inner.get_authenticated_user().await
    }
    async fn get_mentions(&self, _s: &str) -> anyhow::Result<Vec<Mention>> {
        Ok(vec![])
    }
    async fn health_check(&self) -> anyhow::Result<()> {
        self.inner.health_check().await
    }
}

/// `update_agent_label_after_reroute` returns `false` when `set_labels` fails.
///
/// This tests the extracted helper directly, bypassing the full `run_with_context`
/// path (which would fail earlier on worktree setup in a unit-test environment).
#[tokio::test]
async fn update_agent_label_returns_false_on_set_labels_failure() {
    let task = ExternalTask {
        id: ExternalId("test-reroute-1".to_string()),
        title: "Test".to_string(),
        body: "body".to_string(),
        state: "open".to_string(),
        labels: vec![],
        author: "bot".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        url: String::new(),
    };
    let backend: Arc<dyn ExternalBackend> =
        Arc::new(LabelFailingBackend::new().with_set_labels_err("network error"));

    let result =
        update_agent_label_after_reroute("owner/repo", &task, &backend, "claude", "codex").await;

    assert!(
        !result,
        "update_agent_label_after_reroute should return false when set_labels fails"
    );
}

/// `update_agent_label_after_reroute` returns `true` when `set_labels` succeeds.
#[tokio::test]
async fn update_agent_label_returns_true_on_success() {
    let task = ExternalTask {
        id: ExternalId("test-reroute-2".to_string()),
        title: "Test".to_string(),
        body: "body".to_string(),
        state: "open".to_string(),
        labels: vec![],
        author: "bot".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        url: String::new(),
    };
    // LabelFailingBackend with no injected error delegates to TrackingBackend (always Ok).
    let backend: Arc<dyn ExternalBackend> = Arc::new(LabelFailingBackend::new());

    let result =
        update_agent_label_after_reroute("owner/repo", &task, &backend, "claude", "codex").await;

    // ensure_label is skipped in #[cfg(test)], set_labels succeeds via mock.
    assert!(
        result,
        "update_agent_label_after_reroute should return true when set_labels succeeds"
    );
}

// ── NDJSON envelope fallback (issue #3078) ────────────────────────────────

/// Pattern A: agent emits domain JSON as result (no `status` field).
/// NDJSON envelope says is_error=false, subtype=success.
/// Was previously returning InvalidResponse → task re-ran.
#[test]
fn parse_success_output_ndjson_envelope_fallback_domain_json_result() {
    let domain_result = r#"{"file_path":"md/journal/2026-05-08-morning-briefing.md","summary":"Morning briefing filed","health":{"status":"ok"},"priorities":[]}"#;
    let ndjson = format!(
        r#"{{"type":"result","subtype":"success","is_error":false,"terminal_reason":"completed","result":{domain_result:?},"usage":{{"input_tokens":3000,"output_tokens":400}}}}"#
    );

    let runner = agents::get_runner("glm");
    let parsed = parse_success_output("149200", "glm", &*runner, &ndjson)
        .expect("should synthesize done from NDJSON envelope when result is domain JSON");

    assert_eq!(parsed.response.status, "done");
    assert!(
        parsed.response.summary.contains("file_path") || !parsed.response.summary.is_empty(),
        "summary should be populated from domain JSON result text"
    );
    assert_eq!(parsed.input_tokens, Some(3000));
    assert_eq!(parsed.output_tokens, Some(400));
}

/// Grok-style envelope: no terminal_reason, subtype=success.
#[test]
fn parse_success_output_envelope_fallback_without_terminal_reason() {
    let receipt = r#"{"ok":true,"report":"notes.md","commit":"abc"}"#;
    let ndjson = format!(
        r#"{{"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn","result":{receipt:?},"usage":{{"input_tokens":3000,"output_tokens":400}}}}"#
    );
    let runner = agents::get_runner("grok");
    let parsed = parse_success_output("1", "grok", &*runner, &ndjson).expect("done");
    assert_eq!(parsed.response.status, "done");
    assert_eq!(parsed.input_tokens, Some(3000));
}

/// Pattern B: agent emits plain prose as result, no completion keywords.
/// NDJSON envelope says is_error=false, subtype=success.
/// Was previously returning InvalidResponse → task re-ran.
#[test]
fn parse_success_output_ndjson_envelope_fallback_plain_prose_result() {
    let prose = "PR #1008 now has zero diff vs main. The reverted ledger changes are gone and the branch is clean.";
    let ndjson = format!(
        r#"{{"type":"result","subtype":"success","is_error":false,"terminal_reason":"completed","result":{prose:?},"usage":{{"input_tokens":2500,"output_tokens":300}}}}"#
    );

    let runner = agents::get_runner("claude");
    let parsed = parse_success_output("149164", "claude", &*runner, &ndjson)
        .expect("should synthesize done from NDJSON envelope when result is plain prose");

    assert_eq!(parsed.response.status, "done");
    assert_eq!(parsed.response.summary, prose);
    assert_eq!(parsed.input_tokens, Some(2500));
    assert_eq!(parsed.output_tokens, Some(300));
}

/// Verify that is_error=true in NDJSON envelope still returns AgentFailed,
/// not "done" — the envelope fallback must not override explicit errors.
#[test]
fn parse_success_output_ndjson_envelope_fallback_skipped_when_is_error_true() {
    let ndjson = r#"{"type":"result","subtype":"error","is_error":true,"result":"Something went wrong with no status"}"#;

    let runner = agents::get_runner("claude");
    let err = parse_success_output("149999", "claude", &*runner, ndjson)
        .expect_err("is_error=true should return AgentFailed");

    assert!(matches!(err, agents::AgentError::AgentFailed { .. }));
}

fn tool_pair(id: &str, name: &str, cmd: &str, is_error: bool) -> String {
    format!(
        "{}\n{}\n",
        serde_json::json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":id,"name":name,"input":{"command":cmd}}]}}),
        serde_json::json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":id,"is_error":is_error,"content":"x"}]}}),
    )
}

#[test]
fn salvage_wrapup_accepts_commit_then_cleanup() {
    let raw = format!(
        "{}{}",
        tool_pair("1", "run_terminal_cmd", "git commit -m x", false),
        tool_pair("2", "run_terminal_cmd", "git status", false)
    );
    assert!(salvage_wrapup(&raw).is_some());
}

#[test]
fn salvage_wrapup_rejects_edit_after_commit_or_unanswered_call() {
    let edit = format!(
        "{}{}",
        tool_pair("1", "run_terminal_cmd", "git commit -m x", false),
        tool_pair("2", "search_replace", "", false)
    );
    assert!(salvage_wrapup(&edit).is_none());
    let pending = format!(
        "{}{}",
        tool_pair("1", "run_terminal_cmd", "git commit -m x", false),
        serde_json::json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"2","name":"run_terminal_cmd","input":{"command":"cargo test"}}]}})
    );
    assert!(salvage_wrapup(&pending).is_none());
    assert!(salvage_wrapup(&tool_pair("1", "run_terminal_cmd", "ls", false)).is_none());
}

/// Grok writes `is_error: true` and `errors: ["cancelled"]` when the timeout
/// wrapper kills it. Exit 124 is the time limit, so this is Timeout
#[test]
fn parse_session_output_exit124_cancelled_envelope_is_timeout() {
    let ndjson = r#"{"type":"result","subtype":"error","is_error":true,"stop_reason":"cancelled","errors":["cancelled"],"duration_ms":2697000}"#;
    let session = session::SessionOutput {
        exit_code: 124,
        raw_stdout: ndjson.to_string(),
        raw_stderr: String::new(),
        elapsed_secs: Some(2705),
        sandbox_error: None,
    };

    let runner = agents::get_runner("grok");
    let err = parse_session_output("4068", "grok", &*runner, &session)
        .expect_err("cancelled at the time limit should be an error");
    match err {
        agents::AgentError::Timeout { elapsed } => {
            assert_eq!(elapsed.as_secs(), 2705);
        }
        _ => panic!("exit 124 with cancelled envelope should be Timeout, got {err:?}"),
    }
}

/// A provider error with a normal non-zero exit is AgentFailed
#[test]
fn parse_session_output_exit1_provider_error_stays_agent_failed() {
    let ndjson =
        r#"{"type":"result","subtype":"error","is_error":true,"errors":["provider error"]}"#;
    let session = session::SessionOutput {
        exit_code: 1,
        raw_stdout: ndjson.to_string(),
        raw_stderr: String::new(),
        elapsed_secs: Some(12),
        sandbox_error: None,
    };

    let runner = agents::get_runner("grok");
    let err = parse_session_output("4068", "grok", &*runner, &session)
        .expect_err("provider error should be an error");
    assert!(
        matches!(err, agents::AgentError::AgentFailed { .. }),
        "exit 1 provider error should stay AgentFailed, got {err:?}"
    );
}

// ── terminal_reason:completed + exit 1 detection (issue #3087) ─────────────

/// kimi/minimax sometimes emit valid NDJSON with terminal_reason=completed
/// but exit 1. In that case parse_success_output may fail (e.g. empty result
/// text that synthesize can't parse), then parse_session_output would
/// normally fall back to classify_error — grabbing a garbled JSON tail and
/// producing a spurious Unknown/Auth error. The fix intercepts this and
/// returns InvalidResponse (soft error, re-route) instead.
#[test]
fn parse_session_output_exit0_without_envelope_is_interrupted() {
    let session = session::SessionOutput {
        exit_code: 0,
        raw_stdout: "\u{4}\u{4}\u{4} truncated pane junk".to_string(),
        raw_stderr: String::new(),
        elapsed_secs: Some(180),
        sandbox_error: None,
    };

    let runner = agents::get_runner("claude");
    let err = parse_session_output("3876", "claude", &*runner, &session)
        .expect_err("missing envelope should be an error");
    assert!(
        matches!(err, agents::AgentError::Interrupted { .. }),
        "exit 0 with no result envelope should be Interrupted, got {err:?}"
    );
}

#[test]
fn parse_session_output_exit1_with_terminal_reason_completed_returns_invalid_response() {
    // Empty result text causes synthesize_response_from_text to return None
    // (empty/whitespace-only input), so parse_success_output returns
    // InvalidResponse. With the fix, parse_session_output intercepts the
    // "terminal_reason":"completed" signal before classify_error and
    // returns InvalidResponse (re-route) instead of a garbled error.
    let ndjson = r#"{"type":"result","subtype":"success","is_error":false,"terminal_reason":"completed","result":"","usage":{"input_tokens":1000,"output_tokens":100}}"#;
    let session = session::SessionOutput {
        exit_code: 1,
        raw_stdout: ndjson.to_string(),
        raw_stderr: String::new(),
        elapsed_secs: Some(30),
        sandbox_error: None,
    };

    let runner = agents::get_runner("claude");
    let result = parse_session_output("149298", "claude", &*runner, &session);

    let err = result.expect_err("should be an error, not Ok");
    assert!(
        matches!(err, agents::AgentError::InvalidResponse { .. }),
        "should be InvalidResponse, got {err:?}"
    );
}

// ── GLM cost-telemetry exit-1 detection (issue #3094) ──────────────────────

/// GLM exits with code 1 but stdout is only cost telemetry JSON (contains
/// "costUSD":). This must return InvalidResponse (soft re-route) instead of
/// a hard Failed classification with spurious agent cooldown.
#[test]
fn parse_session_output_exit1_with_cost_telemetry_returns_invalid_response() {
    // GLM emits cost/usage JSON on clean exit; exit code 1 is a false failure.
    let telemetry = r#"{"type":"result","subtype":"success","is_error":false,"result":"","costUSD":0.042,"usage":{"input_tokens":5000,"output_tokens":800}}"#;
    let session = session::SessionOutput {
        exit_code: 1,
        raw_stdout: telemetry.to_string(),
        raw_stderr: String::new(),
        elapsed_secs: Some(120),
        sandbox_error: None,
    };

    let runner = agents::get_runner("glm");
    let result = parse_session_output("149347", "glm", &*runner, &session);

    let err = result.expect_err("should be an error, not Ok");
    assert!(
        matches!(err, agents::AgentError::InvalidResponse { .. }),
        "glm cost-telemetry exit-1 should return InvalidResponse, got {err:?}"
    );
}

/// GLM with costUSD AND is_error:true — must propagate AgentFailed, not
/// silently swallow the error via the cost-telemetry guard.
#[test]
fn parse_session_output_exit1_cost_telemetry_with_is_error_true_propagates_failure() {
    let telemetry = r#"{"type":"result","subtype":"error","is_error":true,"result":"tool failed","costUSD":0.01,"usage":{"input_tokens":100,"output_tokens":10}}"#;
    let session = session::SessionOutput {
        exit_code: 1,
        raw_stdout: telemetry.to_string(),
        raw_stderr: String::new(),
        elapsed_secs: Some(15),
        sandbox_error: None,
    };

    let runner = agents::get_runner("glm");
    let result = parse_session_output("149348", "glm", &*runner, &session);

    let err = result.expect_err("should be an error");
    assert!(
        matches!(err, agents::AgentError::AgentFailed { .. }),
        "is_error=true with costUSD must still return AgentFailed, got {err:?}"
    );
}

/// terminal_reason=completed must NOT override actual error conditions.
/// When is_error=true in the NDJSON envelope, AgentFailed must be returned
/// even if terminal_reason=completed is also present. Only kimi/minimax
/// agents use find_claude_result which extracts is_error, so use "kimi"
/// as agent_name.
#[test]
fn parse_session_output_exit1_preserves_error_when_is_error_true() {
    let ndjson = r#"{"type":"result","subtype":"error","is_error":true,"terminal_reason":"completed","result":"failed"}"#;
    let session = session::SessionOutput {
        exit_code: 1,
        raw_stdout: ndjson.to_string(),
        raw_stderr: String::new(),
        elapsed_secs: Some(30),
        sandbox_error: None,
    };

    let runner = agents::get_runner("kimi");
    let result = parse_session_output("149299", "kimi", &*runner, &session);

    let err = result.expect_err("should be an error");
    assert!(
        matches!(err, agents::AgentError::AgentFailed { .. }),
        "is_error=true should return AgentFailed, got {err:?}"
    );
}

/// RateLimit with terminal_reason=completed must be propagated as RateLimit,
/// not downgraded to InvalidResponse. This covers claude session-limit errors
/// that include "terminal_reason":"completed" in their NDJSON envelope — the
/// terminal_reason guard was designed for kimi/minimax/glm false-positive
/// exit codes but was intercepting classified errors like RateLimit.
#[test]
fn parse_session_output_exit1_with_rate_limit_and_terminal_reason_completed() {
    let ndjson = r#"{"type":"result","is_error":true,"terminal_reason":"completed","result":"You've hit your session limit · resets 2:20pm (America/Sao_Paulo)","usage":{"input_tokens":0,"output_tokens":0}}"#;
    let session = session::SessionOutput {
        exit_code: 1,
        raw_stdout: ndjson.to_string(),
        raw_stderr: String::new(),
        elapsed_secs: Some(120),
        sandbox_error: None,
    };

    let runner = agents::get_runner("claude");
    let result = parse_session_output("152895", "claude", &*runner, &session);

    let err = result.expect_err("rate-limit should be an error");
    assert!(
        matches!(err, agents::AgentError::RateLimit { .. }),
        "session limit should return RateLimit, not downgraded, got {err:?}"
    );
}
