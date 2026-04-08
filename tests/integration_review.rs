//! Integration tests for the full review agent flow.
//!
//! Calls REAL agents with a review prompt, captures stdout/stderr,
//! and verifies the output can be parsed using the SAME code path as review.rs:
//!   1. `agents::find_agent_result(agent, stdout)` — per-agent envelope extraction
//!   2. `parse_review_response(&text)` / `infer_review_response(&text)` — JSON/plain-text review parse
//!
//! `#[ignore]`d — needs API keys and installed CLIs. Run locally:
//! ```bash
//! cargo test --test integration_review -- --ignored --nocapture
//! ```

use orch::engine::runner::agents::find_agent_result;
use orch::engine::runner::response::{infer_review_response, parse_review_response};
use std::process::Command;

const REVIEW_PROMPT: &str = r#"You are a code review agent. Review this trivial change and respond with ONLY this JSON (no markdown, no explanation):
{"decision":"approve","summary":"looks good","concerns":[]}"#;

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
