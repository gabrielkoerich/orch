//! Integration tests that invoke real agent CLIs.
//!
//! These tests are `#[ignore]`d by default — they need real API keys,
//! installed CLIs, and cost money to run.
//!
//! Run all locally:
//! ```bash
//! cargo test --test integration_agents -- --ignored --nocapture
//! ```
//!
//! Run a single agent:
//! ```bash
//! cargo test --test integration_agents claude -- --ignored --nocapture
//! cargo test --test integration_agents codex -- --ignored --nocapture
//! cargo test --test integration_agents opencode -- --ignored --nocapture
//! cargo test --test integration_agents kimi -- --ignored --nocapture
//! cargo test --test integration_agents minimax -- --ignored --nocapture
//! cargo test --test integration_agents router -- --ignored --nocapture
//! ```

use std::process::Command;

/// Simple prompt that should produce a parseable JSON response.
const SIMPLE_PROMPT: &str = r#"Reply with ONLY this JSON, no markdown, no explanation:
{"status":"done","summary":"integration test","accomplished":["replied"],"remaining":[],"files":[]}"#;

fn is_available(binary: &str) -> bool {
    which::which(binary).is_ok()
}

/// Assert a Claude JSON envelope is valid and extract the inner result.
fn assert_claude_envelope(stdout: &str) -> String {
    let val: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("not valid JSON: {e}\nraw: {stdout}"));

    assert_eq!(
        val.get("type").and_then(|v| v.as_str()),
        Some("result"),
        "expected Claude envelope with type=result, got: {val}"
    );

    let is_error = val
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    assert!(!is_error, "Claude envelope has is_error=true: {val}");

    val.get("result")
        .and_then(|v| v.as_str())
        .expect("envelope missing 'result' field")
        .to_string()
}

/// Assert NDJSON stream has at least one valid line, return all parsed events.
fn assert_ndjson(stdout: &str) -> Vec<serde_json::Value> {
    let mut events = Vec::new();
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(val) => {
                let event_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("?");
                eprintln!("  event: {event_type}");
                events.push(val);
            }
            Err(e) => {
                eprintln!("  unparseable: {e}: {}", &line[..line.len().min(100)]);
            }
        }
    }
    assert!(!events.is_empty(), "no valid NDJSON lines in output");
    events
}

// ── Claude ────────────────────────────────────────────────────────

#[test]
#[ignore]
fn claude_responds_with_json_envelope() {
    if !is_available("claude") {
        eprintln!("SKIP: claude not in PATH");
        return;
    }

    let output = Command::new("claude")
        .args([
            "--output-format",
            "json",
            "--print",
            "--model",
            "haiku",
            SIMPLE_PROMPT,
        ])
        .output()
        .expect("failed to execute claude");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    eprintln!("exit: {}", output.status.code().unwrap_or(-1));
    eprintln!(
        "stdout ({} bytes): {}",
        stdout.len(),
        &stdout[..stdout.len().min(500)]
    );
    eprintln!(
        "stderr ({} bytes): {}",
        stderr.len(),
        &stderr[..stderr.len().min(500)]
    );

    assert!(output.status.success(), "claude failed: {stderr}");
    assert!(!stdout.is_empty(), "claude returned empty stdout");

    let result = assert_claude_envelope(&stdout);
    eprintln!("inner result: {result}");

    // Inner result should contain our JSON response
    assert!(
        result.contains("status") || result.contains("done"),
        "result doesn't look like expected JSON: {result}"
    );
}

#[test]
#[ignore]
fn claude_stdin_pipe_mode() {
    if !is_available("claude") {
        eprintln!("SKIP: claude not in PATH");
        return;
    }

    // Test -p with stdin redirect — matches real task agent invocation
    let output = Command::new("claude")
        .args(["-p", "--model", "haiku", "--output-format", "json"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                stdin.write_all(SIMPLE_PROMPT.as_bytes()).ok();
            }
            drop(child.stdin.take()); // close stdin
            child.wait_with_output()
        })
        .expect("failed to execute claude");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    eprintln!("exit: {}", output.status.code().unwrap_or(-1));
    eprintln!(
        "stdout ({} bytes): {}",
        stdout.len(),
        &stdout[..stdout.len().min(500)]
    );
    eprintln!(
        "stderr ({} bytes): {}",
        stderr.len(),
        &stderr[..stderr.len().min(500)]
    );

    assert!(output.status.success(), "claude -p failed: {stderr}");
    assert!(!stdout.is_empty(), "claude -p returned empty stdout");

    let result = assert_claude_envelope(&stdout);
    eprintln!("inner result: {result}");
}

// ── Codex ─────────────────────────────────────────────────────────

#[test]
#[ignore]
fn codex_responds_with_ndjson() {
    if !is_available("codex") {
        eprintln!("SKIP: codex not in PATH");
        return;
    }

    let output = Command::new("codex")
        .args([
            "--ask-for-approval",
            "never",
            "--sandbox",
            "workspace-write",
            "exec",
            "--json",
            SIMPLE_PROMPT,
        ])
        .output()
        .expect("failed to execute codex");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    eprintln!("exit: {}", output.status.code().unwrap_or(-1));
    eprintln!(
        "stdout ({} bytes):\n{}",
        stdout.len(),
        &stdout[..stdout.len().min(1000)]
    );
    eprintln!(
        "stderr ({} bytes): {}",
        stderr.len(),
        &stderr[..stderr.len().min(500)]
    );

    assert!(output.status.success(), "codex failed: {stderr}");
    assert!(!stdout.is_empty(), "codex returned empty stdout");

    let events = assert_ndjson(&stdout);

    // Check for error events
    let errors: Vec<_> = events
        .iter()
        .filter(|e| {
            let t = e.get("type").and_then(|v| v.as_str()).unwrap_or("");
            t == "error" || t == "turn.failed"
        })
        .collect();

    if !errors.is_empty() {
        for err in &errors {
            eprintln!("  ERROR EVENT: {err}");
        }
        panic!("codex returned error events");
    }

    // Should have an agent_message
    let has_message = events.iter().any(|e| {
        e.get("type").and_then(|v| v.as_str()) == Some("item.completed")
            && e.get("item")
                .and_then(|i| i.get("type"))
                .and_then(|v| v.as_str())
                == Some("agent_message")
    });
    assert!(has_message, "codex produced no agent_message events");
}

#[test]
#[ignore]
fn codex_stdin_pipe_mode() {
    if !is_available("codex") {
        eprintln!("SKIP: codex not in PATH");
        return;
    }

    // Test piped stdin — matches real task invocation: cat msg | codex ... exec --json -
    let output = Command::new("codex")
        .args([
            "--ask-for-approval",
            "never",
            "--sandbox",
            "workspace-write",
            "exec",
            "--json",
            "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                stdin.write_all(SIMPLE_PROMPT.as_bytes()).ok();
            }
            drop(child.stdin.take());
            child.wait_with_output()
        })
        .expect("failed to execute codex");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    eprintln!("exit: {}", output.status.code().unwrap_or(-1));
    eprintln!(
        "stdout ({} bytes):\n{}",
        stdout.len(),
        &stdout[..stdout.len().min(1000)]
    );
    eprintln!(
        "stderr ({} bytes): {}",
        stderr.len(),
        &stderr[..stderr.len().min(500)]
    );

    assert!(output.status.success(), "codex stdin failed: {stderr}");
    assert!(!stdout.is_empty(), "codex stdin returned empty stdout");
    assert_ndjson(&stdout);
}

// ── OpenCode ──────────────────────────────────────────────────────

#[test]
#[ignore]
fn opencode_responds_with_ndjson() {
    if !is_available("opencode") {
        eprintln!("SKIP: opencode not in PATH");
        return;
    }

    let output = Command::new("opencode")
        .args(["run", "--format", "json", SIMPLE_PROMPT])
        .output()
        .expect("failed to execute opencode");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    eprintln!("exit: {}", output.status.code().unwrap_or(-1));
    eprintln!(
        "stdout ({} bytes):\n{}",
        stdout.len(),
        &stdout[..stdout.len().min(1000)]
    );
    eprintln!(
        "stderr ({} bytes): {}",
        stderr.len(),
        &stderr[..stderr.len().min(500)]
    );

    assert!(output.status.success(), "opencode failed: {stderr}");
    assert!(!stdout.is_empty(), "opencode returned empty stdout");
    assert_ndjson(&stdout);
}

// ── Kimi ──────────────────────────────────────────────────────────

#[test]
#[ignore]
fn kimi_responds_like_claude() {
    if !is_available("kimi") {
        eprintln!("SKIP: kimi not in PATH");
        return;
    }

    let output = Command::new("kimi")
        .args([
            "-p",
            "--output-format",
            "json",
            "--model",
            "haiku",
            SIMPLE_PROMPT,
        ])
        .output()
        .expect("failed to execute kimi");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    eprintln!("exit: {}", output.status.code().unwrap_or(-1));
    eprintln!(
        "stdout ({} bytes): {}",
        stdout.len(),
        &stdout[..stdout.len().min(500)]
    );
    eprintln!(
        "stderr ({} bytes): {}",
        stderr.len(),
        &stderr[..stderr.len().min(500)]
    );

    assert!(output.status.success(), "kimi failed: {stderr}");
    assert!(!stdout.is_empty(), "kimi returned empty stdout");

    let result = assert_claude_envelope(&stdout);
    eprintln!("inner result: {result}");
}

// ── MiniMax ───────────────────────────────────────────────────────

#[test]
#[ignore]
fn minimax_responds_like_claude() {
    if !is_available("minimax") {
        eprintln!("SKIP: minimax not in PATH");
        return;
    }

    let output = Command::new("minimax")
        .args([
            "-p",
            "--output-format",
            "json",
            "--model",
            "haiku",
            SIMPLE_PROMPT,
        ])
        .output()
        .expect("failed to execute minimax");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    eprintln!("exit: {}", output.status.code().unwrap_or(-1));
    eprintln!(
        "stdout ({} bytes): {}",
        stdout.len(),
        &stdout[..stdout.len().min(500)]
    );
    eprintln!(
        "stderr ({} bytes): {}",
        stderr.len(),
        &stderr[..stderr.len().min(500)]
    );

    assert!(output.status.success(), "minimax failed: {stderr}");
    assert!(!stdout.is_empty(), "minimax returned empty stdout");

    let result = assert_claude_envelope(&stdout);
    eprintln!("inner result: {result}");
}

// ── Router LLM ───────────────────────────────────────────────────

#[test]
#[ignore]
fn router_claude_haiku_returns_valid_routing() {
    if !is_available("claude") {
        eprintln!("SKIP: claude not in PATH");
        return;
    }

    let router_prompt = r#"You are a task router. Given the task below, respond with ONLY a JSON object (no markdown, no explanation):

Task: "Fix typo in README.md"

Required JSON format:
{"executor":"claude","complexity":"simple","reason":"trivial text fix","profile":{"role":"editor","skills":[],"tools":[],"constraints":[]},"selected_skills":[]}"#;

    let output = Command::new("claude")
        .args([
            "--output-format",
            "json",
            "--print",
            "--model",
            "haiku",
            router_prompt,
        ])
        .output()
        .expect("failed to execute claude");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    eprintln!("exit: {}", output.status.code().unwrap_or(-1));
    eprintln!(
        "stdout ({} bytes): {}",
        stdout.len(),
        &stdout[..stdout.len().min(500)]
    );
    eprintln!(
        "stderr ({} bytes): {}",
        stderr.len(),
        &stderr[..stderr.len().min(500)]
    );

    assert!(output.status.success(), "router failed: {stderr}");
    assert!(!stdout.is_empty(), "router returned empty stdout");

    // Unwrap Claude envelope
    let result = assert_claude_envelope(&stdout);
    eprintln!("inner result: {result}");

    // The inner result should be parseable as JSON with an "executor" field
    // (might be in a code block or raw JSON)
    let text = result.trim();
    let json_str = if let Some(start) = text.find("```json") {
        let after = &text[start + 7..];
        &after[..after.find("```").unwrap_or(after.len())]
    } else if let Some(start) = text.find("```") {
        let after = &text[start + 3..];
        &after[..after.find("```").unwrap_or(after.len())]
    } else if let Some(start) = text.find('{') {
        &text[start..=text.rfind('}').unwrap_or(text.len() - 1)]
    } else {
        text
    };

    let route: serde_json::Value = serde_json::from_str(json_str.trim())
        .unwrap_or_else(|e| panic!("inner result is not valid JSON: {e}\nraw: {json_str}"));

    assert!(
        route.get("executor").is_some(),
        "route response missing 'executor' field: {route}"
    );

    eprintln!(
        "route: executor={}, complexity={}",
        route
            .get("executor")
            .and_then(|v| v.as_str())
            .unwrap_or("?"),
        route
            .get("complexity")
            .and_then(|v| v.as_str())
            .unwrap_or("?"),
    );
}
