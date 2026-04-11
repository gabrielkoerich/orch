//! Integration tests for the full review agent flow.
//!
//! ## Approach
//!
//! Real end-to-end review integration tests (spawning agents in tmux, reading
//! real GitHub PRs, etc.) require live API keys and installed CLIs. Instead,
//! this module tests the **review parsing pipeline** end-to-end using pre-captured
//! NDJSON fixtures that represent each agent's output format:
//!
//! ```text
//! Agent NDJSON output → find_agent_result() → extract text → parse_review_response() → ReviewResponse
//! ```
//!
//! This verifies the same code paths used by review.rs for each agent:
//!   - `find_agent_result(agent, ndjson)` — per-agent envelope extraction
//!   - `parse_review_response(&text)` — JSON/markdown parsing
//!   - `infer_review_response(&text)` — keyword-based fallback for plain text
//!
//! ## Fixtures
//!
//! Each fixture is a pre-captured NDJSON stream from a real agent invocation.
//! Fixtures cover both happy paths and error conditions:
//!
//! | Fixture | Agent | Decision |
//! |---------|-------|----------|
//! | `review_claude_approve.jsonl` | Claude | approve |
//! | `review_claude_request_changes.jsonl` | Claude | request_changes |
//! | `review_claude_rate_limit.jsonl` | Claude | (error) |
//! | `review_opencode_approve.jsonl` | OpenCode | approve |
//! | `review_opencode_request_changes.jsonl` | OpenCode | request_changes |
//! | `review_opencode_plain_text.jsonl` | OpenCode | (plain text → infer approve) |
//! | `review_codex_approve.jsonl` | Codex | approve |
//! | `review_codex_request_changes.jsonl` | Codex | request_changes |
//! | `review_codex_plain_text.jsonl` | Codex | (plain text → infer approve) |
//! | `review_kimi_approve.jsonl` | Kimi | approve |
//! | `review_minimax_request_changes.jsonl` | MiniMax | request_changes |
//!
//! ## Running
//!
//! ```bash
//! cargo test --test integration_review -- --nocapture
//! cargo test --test integration_review response_tests -- --nocapture  # subset
//! ```
//!
//! ## Adding new Fixtures
//!
//! To add a fixture for a new agent or format:
//! 1. Capture real agent NDJSON output (run agent with `--format json` / `stream-json`)
//! 2. Save as `tests/fixtures/review_{agent}_{decision}.jsonl`
//! 3. Add a test function that mirrors `verify_review_fixture()`
//!
//! ## End-to-End Tests (manual)
//!
//! For true end-to-end tests with live agents, the `#[ignore]` tests call
//! real agents. Run with:
//! ```bash
//! cargo test --test integration_review -- --ignored --nocapture
//! ```

use orch::engine::runner::agents::find_agent_result;
use orch::engine::runner::response::{
    infer_review_response, parse_review_response, ReviewResponse,
};

/// Full review parsing pipeline for an agent's NDJSON output.
///
/// Mirrors the production path in review.rs:
/// ```text
/// find_agent_result(agent, ndjson) → extract text → parse_review_response → infer_review_response
/// ```
fn parse_review_output(agent: &str, ndjson: &str) -> anyhow::Result<ReviewResponse> {
    let text = find_agent_result(agent, ndjson)
        .map(|r| r.result_text)
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| ndjson.to_string());

    parse_review_response(&text).or_else(|_| {
        infer_review_response(&text)
            .ok_or_else(|| anyhow::anyhow!("failed to parse review from {agent} output"))
    })
}

fn verify_review_fixture(agent: &str, fixture_name: &str, expected_decision: &str, ndjson: &str) {
    eprintln!("\n=== {agent} / {fixture_name} ===");
    eprintln!("expected decision: {expected_decision}");

    let result = find_agent_result(agent, ndjson);

    if let Some(ref r) = result {
        eprintln!("find_agent_result: is_error={}", r.is_error);
        eprintln!(
            "result_text (first 200 chars): {}",
            &r.result_text[..r.result_text.len().min(200)]
        );
        if let (Some(i), Some(o)) = (r.input_tokens, r.output_tokens) {
            eprintln!("tokens: input={i}, output={o}");
        }
    } else {
        eprintln!("find_agent_result returned None");
    }

    let review = parse_review_output(agent, ndjson);
    if let Ok(ref r) = review {
        eprintln!("decision: {:?} (expected: {expected_decision})", r.decision);
        eprintln!("notes: {:?}", r.notes);
        eprintln!("issues: {}", r.issues.len());
    } else if let Err(ref e) = review {
        eprintln!("parse FAILED: {e}");
    }

    let review = review.expect(
        "parse_review_output should succeed — if it fails, the review \
         pipeline cannot handle this agent's output format",
    );
    assert_eq!(
        review.decision, expected_decision,
        "{agent}/{fixture_name}: decision mismatch"
    );
    eprintln!("=== {agent} / {fixture_name} PASSED ===\n");
}

fn error_fixture(agent: &str, fixture_name: &str, ndjson: &str) {
    eprintln!("\n=== {agent} / {fixture_name} (error) ===");
    let result = find_agent_result(agent, ndjson).expect("should find result");
    assert!(
        result.is_error,
        "{agent}/{fixture_name}: expected is_error=true, got false"
    );
    eprintln!("is_error: true (correct)");
    eprintln!(
        "error text (first 200 chars): {}",
        &result.result_text[..result.result_text.len().min(200)]
    );
    eprintln!("=== {agent} / {fixture_name} PASSED ===\n");
}

// ── Claude fixtures ─────────────────────────────────────────────────────────────

#[test]
fn review_claude_approve() {
    let ndjson = include_str!("fixtures/review_claude_approve.jsonl");
    verify_review_fixture("claude", "approve", "approve", ndjson);
}

#[test]
fn review_claude_request_changes() {
    let ndjson = include_str!("fixtures/review_claude_request_changes.jsonl");
    verify_review_fixture("claude", "request_changes", "request_changes", ndjson);
}

#[test]
fn review_claude_rate_limit() {
    let ndjson = include_str!("fixtures/review_claude_rate_limit.jsonl");
    error_fixture("claude", "rate_limit", ndjson);
}

// ── OpenCode fixtures ────────────────────────────────────────────────────────────

#[test]
fn review_opencode_approve() {
    let ndjson = include_str!("fixtures/review_opencode_approve.jsonl");
    verify_review_fixture("opencode", "approve", "approve", ndjson);
}

#[test]
fn review_opencode_request_changes() {
    let ndjson = include_str!("fixtures/review_opencode_request_changes.jsonl");
    verify_review_fixture("opencode", "request_changes", "request_changes", ndjson);
}

#[test]
fn review_opencode_plain_text() {
    // Plain-text review: OpenCode returns "Review complete. All tests passed, LGTM."
    // Should infer "approve" via keyword detection.
    let ndjson = include_str!("fixtures/review_opencode_plain_text.jsonl");
    verify_review_fixture("opencode", "plain_text", "approve", ndjson);
}

// ── Codex fixtures ───────────────────────────────────────────────────────────────

#[test]
fn review_codex_approve() {
    let ndjson = include_str!("fixtures/review_codex_approve.jsonl");
    verify_review_fixture("codex", "approve", "approve", ndjson);
}

#[test]
fn review_codex_request_changes() {
    let ndjson = include_str!("fixtures/review_codex_request_changes.jsonl");
    verify_review_fixture("codex", "request_changes", "request_changes", ndjson);
}

#[test]
fn review_codex_plain_text() {
    // Plain-text review: Codex returns "All checks passed, LGTM."
    // Should infer "approve" via keyword detection.
    let ndjson = include_str!("fixtures/review_codex_plain_text.jsonl");
    verify_review_fixture("codex", "plain_text", "approve", ndjson);
}

// ── Kimi fixtures ─────────────────────────────────────────────────────────────

#[test]
fn review_kimi_approve() {
    let ndjson = include_str!("fixtures/review_kimi_approve.jsonl");
    verify_review_fixture("kimi", "approve", "approve", ndjson);
}

// ── MiniMax fixtures ───────────────────────────────────────────────────────────

#[test]
fn review_minimax_request_changes() {
    let ndjson = include_str!("fixtures/review_minimax_request_changes.jsonl");
    verify_review_fixture("minimax", "request_changes", "request_changes", ndjson);
}

// ── End-to-end tests (manual only — require live API keys and installed CLIs) ──
//
// These tests spawn real agents in tmux with a review prompt. They are `#[ignore]`d
// because they require API keys, installed CLIs, and a real GitHub PR. Run locally with:
//
// ```bash
// cargo test --test integration_review -- --ignored --nocapture
// ```

use std::process::Command;

const REVIEW_PROMPT: &str = r#"You are a code review agent. Review this trivial change and respond with ONLY this JSON (no markdown, no explanation):
{"decision":"approve","notes":"LGTM","test_results":"pass","issues":[]}"#;

fn is_available(binary: &str) -> bool {
    which::which(binary).is_ok()
}

/// Run the full review parsing flow on agent output.
/// Uses the same code path as review.rs: find_agent_result → parse_review_from_output.
fn verify_review_output(
    agent_binary: &str,
    label: &str,
    exit_code: i32,
    stdout: &str,
    stderr: &str,
) {
    eprintln!("\n=== {label} ===");
    eprintln!("exit_code: {exit_code}");
    eprintln!("stdout length: {}", stdout.len());
    eprintln!("stderr length: {}", stderr.len());
    if !stdout.is_empty() {
        eprintln!(
            "stdout first 300 chars:\n{}",
            &stdout[..stdout.len().min(300)]
        );
    }
    if !stderr.is_empty() {
        eprintln!(
            "stderr first 200 chars:\n{}",
            &stderr[..stderr.len().min(200)]
        );
    }

    // Step 1: abort on non-zero exit or empty output (mirrors review.rs line 638)
    assert!(
        exit_code == 0,
        "{label}: non-zero exit code {exit_code}\nstderr: {}",
        &stderr[..stderr.len().min(500)]
    );
    assert!(
        !stdout.is_empty(),
        "{label}: stdout is empty! exit_code={exit_code}, stderr: {}",
        &stderr[..stderr.len().min(500)]
    );

    // Step 2: per-agent envelope extraction — same as review.rs stage 1
    let text = match find_agent_result(agent_binary, stdout) {
        Some(result) if result.is_error => {
            panic!(
                "{label}: agent reported is_error=true: {}",
                result.result_text
            );
        }
        Some(result) if !result.result_text.is_empty() => {
            eprintln!("extracted text length: {}", result.result_text.len());
            eprintln!(
                "extracted first 300 chars:\n{}",
                &result.result_text[..result.result_text.len().min(300)]
            );
            result.result_text
        }
        _ => {
            eprintln!("find_agent_result returned None/empty, falling back to raw stdout");
            stdout.to_string()
        }
    };

    assert!(
        !text.is_empty(),
        "{label}: text is empty after extraction! The review parser would fail.\nstdout first 500:\n{}",
        &stdout[..stdout.len().min(500)]
    );

    // Step 3: parse as ReviewResponse — same as review.rs stage 2
    let review = parse_review_response(&text)
        .ok()
        .or_else(|| infer_review_response(&text));
    assert!(
        review.is_some(),
        "{label}: parse_review_response and infer_review_response both failed\nextracted text:\n{}",
        &text[..text.len().min(500)]
    );

    eprintln!(
        "=== {label} PASSED: extracted {} chars, parsed review decision={:?} ===\n",
        text.len(),
        review.unwrap().decision
    );
}

/// Run a claude-family agent (claude, kimi, minimax)
fn run_claude_agent(binary: &str, model: &str) -> (i32, String, String) {
    eprintln!("Running {binary} --model {model}...");

    let mut cmd = Command::new(binary);
    cmd.env_remove("CLAUDECODE");
    cmd.args([
        "-p",
        "--verbose",
        "--output-format",
        "stream-json",
        "--permission-mode",
        "bypassPermissions",
        "--model",
        model,
    ]);

    // Pipe prompt via stdin
    let mut echo = Command::new("printf")
        .arg(REVIEW_PROMPT)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("printf");

    cmd.stdin(echo.stdout.take().unwrap());
    let _ = echo.wait(); // avoid zombie
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("{binary}: failed to run: {e}"));

    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

#[test]
#[ignore]
fn review_flow_claude_haiku() {
    if !is_available("claude") {
        eprintln!("claude not installed, skipping");
        return;
    }
    let (exit, stdout, stderr) = run_claude_agent("claude", "haiku");
    verify_review_output("claude", "claude:haiku", exit, &stdout, &stderr);
}

#[test]
#[ignore]
fn review_flow_kimi() {
    if !is_available("kimi") {
        eprintln!("kimi not installed, skipping");
        return;
    }
    let (exit, stdout, stderr) = run_claude_agent("kimi", "sonnet");
    verify_review_output("kimi", "kimi:sonnet", exit, &stdout, &stderr);
}

#[test]
#[ignore]
fn review_flow_minimax() {
    if !is_available("minimax") {
        eprintln!("minimax not installed, skipping");
        return;
    }
    let (exit, stdout, stderr) = run_claude_agent("minimax", "sonnet");
    verify_review_output("minimax", "minimax:sonnet", exit, &stdout, &stderr);
}

fn run_opencode_agent(model: &str) -> (i32, String, String) {
    eprintln!("Running opencode run --model {model}...");

    let mut cmd = Command::new("opencode");
    cmd.env_remove("CLAUDECODE");
    cmd.args(["run", "--format", "json"]);
    if !model.is_empty() {
        cmd.arg("--model").arg(model);
    }

    // Pipe prompt via stdin
    let mut echo = Command::new("printf");
    echo.arg(REVIEW_PROMPT).stdout(std::process::Stdio::piped());

    let mut echo_child = echo.spawn().expect("printf");
    cmd.stdin(echo_child.stdout.take().unwrap());
    let _ = echo_child.wait(); // avoid zombie
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("opencode: failed to run: {e}"));

    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

#[test]
#[ignore]
fn review_flow_opencode() {
    if !is_available("opencode") {
        eprintln!("opencode not installed, skipping");
        return;
    }
    let (exit, stdout, stderr) = run_opencode_agent("openai/gpt-4.1");
    verify_review_output("opencode", "opencode:gpt-4.1", exit, &stdout, &stderr);
}
