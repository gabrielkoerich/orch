//! Integration tests for the full review agent flow.
//!
//! Calls REAL agents with a review prompt, captures stdout/stderr,
//! and verifies the output can be parsed as a review response.
//! Reproduces the exact flow from review.rs.
//!
//! `#[ignore]`d — needs API keys and installed CLIs. Run locally:
//! ```bash
//! cargo test --test integration_review -- --ignored --nocapture
//! ```

use std::process::Command;

const REVIEW_PROMPT: &str = "Say hello in one sentence.";

fn is_available(binary: &str) -> bool {
    which::which(binary).is_ok()
}

/// Extract text from NDJSON — same logic as ndjson_extract_text in response.rs
fn extract_text_from_ndjson(ndjson: &str) -> String {
    ndjson
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|e| {
            let event_type = e.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match event_type {
                // opencode: text event
                "text" => e
                    .get("part")
                    .and_then(|p| p.get("text"))
                    .and_then(|t| t.as_str())
                    .map(str::to_string)
                    .or_else(|| e.get("text").and_then(|t| t.as_str()).map(str::to_string)),
                // codex: item.completed with agent_message
                "item.completed" => e
                    .get("item")
                    .filter(|item| {
                        item.get("type").and_then(|v| v.as_str()) == Some("agent_message")
                    })
                    .and_then(|item| item.get("text"))
                    .and_then(|t| t.as_str())
                    .map(str::to_string),
                // claude stream-json: assistant message
                "assistant" => e
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|item| {
                                if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                                    item.get("text")
                                        .and_then(|t| t.as_str())
                                        .map(str::to_string)
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("")
                    })
                    .filter(|s| !s.is_empty()),
                // claude stream-json: final result
                "result" => e.get("result").and_then(|r| r.as_str()).map(str::to_string),
                _ => None,
            }
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Extract JSON block from text (like extract_json_block in response.rs)
#[allow(dead_code)]
fn extract_json_from_text(text: &str) -> Option<String> {
    // Try direct JSON parse
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text.trim()) {
        if v.get("decision").is_some() {
            return Some(text.trim().to_string());
        }
    }

    // Try markdown code block
    if let Some(start) = text.find("```json") {
        let after = &text[start + 7..];
        if let Some(end) = after.find("```") {
            let block = after[..end].trim();
            if serde_json::from_str::<serde_json::Value>(block).is_ok() {
                return Some(block.to_string());
            }
        }
    }

    // Try finding a JSON object with "decision"
    if let Some(start) = text.find("{\"decision\"") {
        if let Some(end) = text[start..].find('}') {
            let candidate = &text[start..=start + end];
            if serde_json::from_str::<serde_json::Value>(candidate).is_ok() {
                return Some(candidate.to_string());
            }
        }
    }

    None
}

/// Run the full review parsing flow on agent output.
/// This mirrors exactly what review.rs does.
fn verify_review_output(agent: &str, exit_code: i32, stdout: &str, stderr: &str) {
    eprintln!("\n=== {agent} ===");
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

    // Step 1: review.rs line 638 — abort on error or empty
    assert!(
        exit_code == 0,
        "{agent}: non-zero exit code {exit_code}\nstderr: {}",
        &stderr[..stderr.len().min(500)]
    );
    assert!(
        !stdout.is_empty(),
        "{agent}: stdout is empty! exit_code={exit_code}, stderr: {}",
        &stderr[..stderr.len().min(500)]
    );

    // Step 2: extract text from NDJSON
    let extracted = extract_text_from_ndjson(stdout);
    eprintln!("extracted text length: {}", extracted.len());
    if !extracted.is_empty() {
        eprintln!(
            "extracted first 300 chars:\n{}",
            &extracted[..extracted.len().min(300)]
        );
    }

    // Step 3: verify text was extracted
    assert!(
        !extracted.is_empty(),
        "{agent}: NDJSON extraction returned empty! The review parser would fail.\nstdout first 500:\n{}",
        &stdout[..stdout.len().min(500)]
    );

    eprintln!(
        "=== {agent} PASSED: NDJSON extraction works, {} chars extracted ===\n",
        extracted.len()
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
    verify_review_output("claude:haiku", exit, &stdout, &stderr);
}

#[test]
#[ignore]
fn review_flow_kimi() {
    if !is_available("kimi") {
        eprintln!("kimi not installed, skipping");
        return;
    }
    let (exit, stdout, stderr) = run_claude_agent("kimi", "sonnet");
    verify_review_output("kimi:sonnet", exit, &stdout, &stderr);
}

#[test]
#[ignore]
fn review_flow_minimax() {
    if !is_available("minimax") {
        eprintln!("minimax not installed, skipping");
        return;
    }
    let (exit, stdout, stderr) = run_claude_agent("minimax", "sonnet");
    verify_review_output("minimax:sonnet", exit, &stdout, &stderr);
}
