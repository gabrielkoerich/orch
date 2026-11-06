//! Recorded agent runs through the same parse and classify path the runner takes

use std::fs;
use std::path::Path;

use super::agents;
use super::agents::AgentError;
use super::agents::claude;
use super::agents::fixture;
use super::parse_session_output;
use super::response;
use crate::engine::cooldown;
use crate::parser::AgentResponse;
use crate::security;

fn run(agent: &str, raw: &str) -> Result<agents::ParsedResponse, AgentError> {
    let fx = fixture::load(raw);
    let runner = agents::get_runner(agent);
    parse_session_output("1", agent, &*runner, &fx.session_output())
}

fn done(agent: &str, raw: &str) -> AgentResponse {
    match run(agent, raw) {
        Ok(parsed) => parsed.response,
        Err(e) => panic!("expected a parsed response, got {e:?}"),
    }
}

#[test]
fn claude_success() {
    let raw = include_str!("../../../tests/fixtures/claude_stream_success.jsonl");
    let parsed = run("claude", raw).unwrap();
    assert_eq!(parsed.response.status, "done");
    assert_eq!(parsed.response.files, vec!["hello.txt"]);
    assert!(parsed.input_tokens.is_some_and(|t| t > 0));
    assert!(parsed.output_tokens.is_some_and(|t| t > 0));
    assert!(parsed.cost_usd.is_some_and(|c| c > 0.0));
}

#[test]
fn claude_success_reports_an_allowed_rate_limit_window() {
    let raw = include_str!("../../../tests/fixtures/claude_stream_success.jsonl");
    let windows = claude::rate_limit_windows(&fixture::load(raw).stdout);
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].status, "allowed");
    assert_eq!(windows[0].window_type, "seven_day");
    assert!(windows[0].resets_at.is_some());
}

#[test]
fn claude_rate_limit_carries_the_reset_time() {
    let raw = include_str!("../../../tests/fixtures/claude_stream_rate_limit.jsonl");
    let err = run("claude", raw).unwrap_err();
    assert!(matches!(err, AgentError::RateLimit { .. }), "got {err:?}");
    let windows = claude::rate_limit_windows(&fixture::load(raw).stdout);
    assert_eq!(windows.len(), 2);
    assert_eq!(windows[0].status, "rejected");
    assert_eq!(windows[0].window_type, "seven_day");
    assert_eq!(windows[0].utilization, Some(1.0));
    assert_eq!(windows[0].resets_at, Some(1_791_864_000));
    assert_eq!(windows[1].window_type, "five_hour");
    assert_eq!(windows[1].utilization, Some(0.06));
}

#[test]
fn claude_expired_oauth_token_is_auth() {
    let raw = include_str!("../../../tests/fixtures/claude_stream_auth_expired.jsonl");
    let err = run("claude", raw).unwrap_err();
    assert!(matches!(err, AgentError::Auth { .. }), "got {err:?}");
}

// The CLI writes a second result once the background work ends, and the run keeps going until then
#[test]
fn claude_background_task_reads_the_last_result() {
    let raw = include_str!("../../../tests/fixtures/claude_stream_background_task.jsonl");
    let stdout = fixture::load(raw).stdout;
    assert_eq!(stdout.matches("\"type\":\"result\"").count(), 2);
    let response = done("claude", raw);
    assert_eq!(response.status, "done");
    assert!(
        response.summary.contains("finished"),
        "{}",
        response.summary
    );
}

#[test]
fn claude_background_subagent_reads_the_last_result() {
    let raw = include_str!("../../../tests/fixtures/claude_stream_background_subagent.jsonl");
    let stdout = fixture::load(raw).stdout;
    assert_eq!(stdout.matches("\"type\":\"result\"").count(), 2);
    let response = done("claude", raw);
    assert_eq!(response.status, "done");
    assert!(
        response.summary.contains("finished"),
        "{}",
        response.summary
    );
}

#[test]
fn claude_killed_mid_run_is_a_timeout() {
    let raw = include_str!("../../../tests/fixtures/claude_stream_killed.jsonl");
    let err = run("claude", raw).unwrap_err();
    assert!(matches!(err, AgentError::Timeout { .. }), "got {err:?}");
}

#[test]
fn codex_success() {
    let raw = include_str!("../../../tests/fixtures/codex_success.jsonl");
    let parsed = run("codex", raw).unwrap();
    assert_eq!(parsed.response.status, "done");
    assert_eq!(parsed.response.files, vec!["hello.txt"]);
}

#[test]
fn codex_usage_limit_carries_the_reset_time() {
    let raw = include_str!("../../../tests/fixtures/codex_rate_limit.jsonl");
    let err = run("codex", raw).unwrap_err();
    let AgentError::RateLimit { message } = &err else {
        panic!("expected RateLimit, got {err:?}");
    };
    assert!(cooldown::parse_retry_at(message).is_some(), "{message}");
}

// No opencode provider was reachable when the fixtures were recorded, so success is an archived review run
#[test]
fn opencode_success() {
    let raw = include_str!("../../../tests/fixtures/opencode_success.jsonl");
    let stdout = fixture::load(raw).stdout;
    let result = agents::find_agent_result("opencode", &stdout).unwrap();
    assert!(!result.is_error);
    assert!(result.input_tokens.is_some_and(|t| t > 0));
    let review = response::parse_review_response(&result.result_text).unwrap();
    assert_eq!(review.decision, "approve");
}

#[test]
fn opencode_free_usage_limit_is_a_rate_limit() {
    let raw = include_str!("../../../tests/fixtures/opencode_rate_limit.jsonl");
    let err = run("opencode", raw).unwrap_err();
    assert!(matches!(err, AgentError::RateLimit { .. }), "got {err:?}");
}

#[test]
fn opencode_insufficient_balance_is_auth() {
    let raw = include_str!("../../../tests/fixtures/opencode_out_of_credits.jsonl");
    let err = run("opencode", raw).unwrap_err();
    assert!(matches!(err, AgentError::Auth { .. }), "got {err:?}");
}

#[test]
fn agent_fixtures_hold_no_secrets() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            !security::has_leaks(&text),
            "{} matches a leak pattern",
            path.display()
        );
    }
}
