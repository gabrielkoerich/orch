# run_direct() Extraction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extract duplicated one-shot agent invocation logic from `control.rs` and `router/llm.rs` into a single `run_direct()` function in the runner module.

**Architecture:** Add `src/engine/runner/direct.rs` with `run_direct()` and `DirectResult`. Update `control.rs::invoke_agent()` and `router/llm.rs::call_router_llm()` to delegate to it. The router path uses `router_command()` (no temp files), while the control path uses `build_command()` with temp files — `run_direct()` must handle both via a `DirectInput` enum or by keeping two thin wrappers that share a common execution core.

**Tech Stack:** Rust, Tokio, existing `AgentRunner` trait (`build_command`, `router_command`, `parse_response`, `classify_error`)

---

## File Map

| File | Action | Purpose |
|------|--------|---------|
| `src/engine/runner/direct.rs` | **Create** | `DirectResult`, `run_direct()` public API |
| `src/engine/runner/mod.rs` | **Modify** | Add `pub mod direct;` |
| `src/control.rs` | **Modify** | Replace `invoke_agent()` body with `run_direct()` call |
| `src/engine/router/llm.rs` | **Modify** | Replace `call_router_llm()` body with `run_direct()` call |

---

## Task 1: Add `direct.rs` with `run_direct()`

**Files:**
- Create: `src/engine/runner/direct.rs`
- Modify: `src/engine/runner/mod.rs` (add `pub mod direct;`)

### Design Note

`invoke_agent()` in control.rs uses `build_command()` + temp files.
`call_router_llm()` in router uses `router_command()` (no temp files, returns `tokio::process::Command`).

These are different code paths. The common parts are: timeout wrapper, stdout/stderr capture, error classification on non-zero exit, and `parse_response()` call.

`run_direct()` will cover the `build_command()` path (used by `invoke_agent`). The router path will be refactored to call `run_direct_command()` — a lower-level helper that takes an already-built `tokio::process::Command`.

- [ ] **Step 1: Write the failing tests**

Create `src/engine/runner/direct.rs` with tests first:

```rust
//! One-shot agent invocation without tmux.
//!
//! Used by the control session and the router LLM to run agents
//! in a fire-and-forget mode without a persistent tmux session.

use crate::engine::runner::agents::{get_runner, AgentError, PermissionRules};
use anyhow::Result;
use std::time::Duration;

/// Result of a one-shot agent invocation.
#[derive(Debug, Clone)]
pub struct DirectResult {
    pub text: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

/// Run an agent one-shot without tmux.
///
/// Writes temp files to `work_dir`, builds the CLI command via
/// `get_runner(agent).build_command()`, executes with `timeout`,
/// then parses with `parse_response()`.
///
/// `work_dir` should be an isolated directory (e.g., `~/.orch/state/control/{pid}/`).
pub async fn run_direct(
    agent: &str,
    model: Option<&str>,
    system_prompt: &str,
    message: &str,
    timeout: Duration,
    work_dir: &str,
) -> Result<DirectResult> {
    let sys_file = format!("{work_dir}/system.md");
    let msg_file = format!("{work_dir}/message.txt");

    tokio::fs::write(&sys_file, system_prompt).await?;
    tokio::fs::write(&msg_file, message).await?;

    let runner = get_runner(agent);
    let permissions = PermissionRules::default();
    let timeout_cmd = format!("timeout {}", timeout.as_secs());

    let shell_cmd = runner.build_command(
        model,
        &timeout_cmd,
        &sys_file,
        &msg_file,
        &permissions,
    );

    run_shell_command(&shell_cmd, work_dir, timeout, agent).await
}

/// Execute an already-built `tokio::process::Command` one-shot.
///
/// Used by the router which builds its own command via `router_command()`.
pub async fn run_direct_command(
    cmd: &mut tokio::process::Command,
    agent: &str,
    timeout: Duration,
) -> Result<DirectResult> {
    let output = tokio::time::timeout(timeout + Duration::from_secs(5), cmd.output())
        .await
        .map_err(|_| anyhow::anyhow!("agent timed out after {}s", timeout.as_secs()))?
        .map_err(|e| anyhow::anyhow!("spawning agent: {e}"))?;

    finish_output(output, agent)
}

/// Shared output-handling logic: parse stdout, classify errors.
fn finish_output(output: std::process::Output, agent: &str) -> Result<DirectResult> {
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code().unwrap_or(-1);

    let runner = get_runner(agent);

    if !output.status.success() {
        let err = runner.classify_error(exit_code, &stdout, &stderr);
        anyhow::bail!("{err}");
    }

    match runner.parse_response(&stdout) {
        Ok(parsed) => {
            let text = if !parsed.response.summary.is_empty() {
                parsed.response.summary.clone()
            } else {
                stdout.clone()
            };
            Ok(DirectResult {
                text,
                input_tokens: parsed.input_tokens,
                output_tokens: parsed.output_tokens,
            })
        }
        Err(AgentError::InvalidResponse { .. }) => {
            let (text, input_tokens, output_tokens) = extract_from_envelope(&stdout);
            Ok(DirectResult {
                text,
                input_tokens,
                output_tokens,
            })
        }
        Err(err) => anyhow::bail!("{err}"),
    }
}

async fn run_shell_command(
    shell_cmd: &str,
    work_dir: &str,
    timeout: Duration,
    agent: &str,
) -> Result<DirectResult> {
    let output = tokio::time::timeout(
        timeout + Duration::from_secs(5),
        tokio::process::Command::new("bash")
            .arg("-c")
            .arg(shell_cmd)
            .current_dir(work_dir)
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("agent timed out after {}s", timeout.as_secs()))?
    .map_err(|e| anyhow::anyhow!("spawning agent: {e}"))?;

    finish_output(output, agent)
}

/// Extract text and token usage from a Claude JSON envelope or raw output.
fn extract_from_envelope(raw: &str) -> (String, Option<u64>, Option<u64>) {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) {
        if let Some(obj) = value.as_object() {
            if obj.get("type").and_then(|v| v.as_str()) == Some("result") {
                let text = obj
                    .get("result")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let input_tokens = obj
                    .get("usage")
                    .and_then(|u| u.get("input_tokens"))
                    .and_then(|v| v.as_u64());
                let output_tokens = obj
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(|v| v.as_u64());
                return (text, input_tokens, output_tokens);
            }
        }
    }
    (raw.to_string(), None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_from_envelope_claude_json() {
        let raw = r#"{"type":"result","result":"Hello world","usage":{"input_tokens":10,"output_tokens":5}}"#;
        let (text, input, output) = extract_from_envelope(raw);
        assert_eq!(text, "Hello world");
        assert_eq!(input, Some(10));
        assert_eq!(output, Some(5));
    }

    #[test]
    fn test_extract_from_envelope_not_envelope() {
        let raw = "plain text response";
        let (text, input, output) = extract_from_envelope(raw);
        assert_eq!(text, "plain text response");
        assert_eq!(input, None);
        assert_eq!(output, None);
    }

    #[test]
    fn test_extract_from_envelope_wrong_type() {
        let raw = r#"{"type":"error","message":"something"}"#;
        let (text, input, output) = extract_from_envelope(raw);
        assert_eq!(text, raw);
        assert_eq!(input, None);
        assert_eq!(output, None);
    }

    #[test]
    fn test_direct_result_debug() {
        let r = DirectResult {
            text: "hi".to_string(),
            input_tokens: Some(1),
            output_tokens: Some(2),
        };
        assert!(format!("{r:?}").contains("hi"));
    }
}
```

- [ ] **Step 2: Run tests to verify they compile and pass**

```bash
cd /Users/gb/.orch/worktrees/orch/gh-issue-754-refactor-extract-run-direct-to-runner-re
cargo test engine::runner::direct 2>&1 | tail -20
```

Expected: FAIL — `direct` module not found yet.

- [ ] **Step 3: Wire `direct.rs` into `runner/mod.rs`**

Add to `src/engine/runner/mod.rs` after the existing `pub mod` declarations:

```rust
pub mod direct;
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test engine::runner::direct 2>&1 | tail -20
```

Expected: 4 tests PASS.

- [ ] **Step 5: Run full CI checks**

```bash
cargo fmt -- --check && cargo clippy --all-targets -- -D warnings && cargo nextest run
```

Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add src/engine/runner/direct.rs src/engine/runner/mod.rs
git commit -m "feat(runner): add run_direct() and run_direct_command() for one-shot agent invocations"
```

---

## Task 2: Migrate `control.rs::invoke_agent()` to `run_direct()`

**Files:**
- Modify: `src/control.rs`

### What to change

`invoke_agent()` currently:
1. Calls `prepare_temp_dir()` to get `work_dir`
2. Writes `system.md` and `message.txt` to `work_dir`
3. Calls `runner.build_command()` with a `timeout_cmd` string
4. Runs `bash -c <shell_cmd>` with `tokio::time::timeout`
5. Parses stdout with `runner.parse_response()` + `extract_from_envelope()`
6. Returns `InvokeResult`

After migration:
- Steps 2–5 move into `run_direct()`
- `invoke_agent()` becomes: `prepare_temp_dir()` → `run_direct()` → convert `DirectResult` to `InvokeResult`

`InvokeResult` and `extract_from_envelope()` in `control.rs` can stay — `InvokeResult` is the public type returned by `invoke_agent()` to callers in `chat.rs`. Do not rename it.

- [ ] **Step 1: Write a failing test proving the refactor compiles**

No new unit test needed — existing callers and compile-time checks cover this. Proceed to implementation.

- [ ] **Step 2: Replace `invoke_agent()` body**

In `src/control.rs`, replace the body of `invoke_agent()` (lines ~370–446) with:

```rust
pub async fn invoke_agent(
    agent: &str,
    model: &str,
    context: &str,
    message: &str,
) -> Result<InvokeResult> {
    let dir = prepare_temp_dir().await?;

    let result = crate::engine::runner::direct::run_direct(
        agent,
        Some(model),
        context,
        message,
        AGENT_TIMEOUT,
        &dir,
    )
    .await?;

    Ok(InvokeResult {
        text: result.text,
        input_tokens: result.input_tokens,
        output_tokens: result.output_tokens,
    })
}
```

Also remove the now-unused imports inside `invoke_agent()`:
- `use crate::engine::runner::agents::{get_runner, PermissionRules};`
- `use crate::engine::runner::agents::AgentError;`

And remove the now-unused private functions if nothing else references them:
- `extract_from_envelope()` (moved to `direct.rs`)

Check with `cargo clippy` — dead code warnings will identify anything unused.

- [ ] **Step 3: Compile and run tests**

```bash
cargo fmt -- --check && cargo clippy --all-targets -- -D warnings && cargo nextest run
```

Expected: all pass. No dead code warnings.

- [ ] **Step 4: Commit**

```bash
git add src/control.rs
git commit -m "refactor(control): delegate invoke_agent() to runner::direct::run_direct()"
```

---

## Task 3: Migrate `router/llm.rs::call_router_llm()` to `run_direct_command()`

**Files:**
- Modify: `src/engine/router/llm.rs`

### What to change

`call_router_llm()` currently:
1. Checks cooldown
2. Calls `runner.router_command(prompt, model)` → returns `tokio::process::Command`
3. Runs `cmd.output_with_context()` with `tokio::time::timeout`
4. Handles: timeout, spawn error, non-zero exit (rate limit detection), empty stdout
5. Returns raw `stdout: String`

After migration:
- Steps 3–4 move into `run_direct_command()`
- The rate-limit special case (detect from stdout, record cooldown) must stay in `call_router_llm()` because it's router-specific logic: it calls `record_agent_failure()` and logs a warning before bailing
- `call_router_llm()` becomes: cooldown check → `router_command()` → `run_direct_command()` → extract raw text

**Important:** `run_direct_command()` returns `DirectResult` with parsed text. `call_router_llm()` currently returns `Ok(stdout)` (raw string). The router parses this raw string later in `parse_llm_response()`. We need to return the raw text, not the parsed summary.

`DirectResult.text` may be the parsed summary (from `parse_response()`), not the raw JSON. This is wrong for the router which needs raw stdout to parse JSON route decisions.

**Solution:** Add a `raw_text: String` field to `DirectResult`, or use a different approach:

Option A — `raw_text` field: `DirectResult` carries both `text` (parsed summary) and `raw_text` (original stdout). Router uses `raw_text`.

Option B — `run_direct_command_raw()`: a variant that skips `parse_response()` and returns raw stdout directly. Simpler for the router's use case.

**Choose Option B** — it avoids coupling the router's raw-stdout need to the general DirectResult shape. Add a `run_direct_command_raw()` to `direct.rs` that skips `parse_response()`.

- [ ] **Step 1: Add `run_direct_command_raw()` to `direct.rs`**

```rust
/// Execute an already-built `tokio::process::Command` one-shot, returning raw stdout.
///
/// Unlike `run_direct_command()`, this does NOT call `parse_response()`.
/// Use this when the caller needs the raw stdout to parse itself (e.g., router).
pub async fn run_direct_command_raw(
    cmd: &mut tokio::process::Command,
    timeout: Duration,
) -> Result<String> {
    let output = tokio::time::timeout(timeout + Duration::from_secs(5), cmd.output())
        .await
        .map_err(|_| anyhow::anyhow!("agent timed out after {}s", timeout.as_secs()))?
        .map_err(|e| anyhow::anyhow!("spawning agent: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !output.status.success() {
        anyhow::bail!("agent failed (exit {}): {stderr}", output.status.code().unwrap_or(-1));
    }
    if stdout.is_empty() {
        anyhow::bail!("agent returned empty response");
    }
    Ok(stdout)
}
```

Add a test:

```rust
#[test]
fn test_direct_result_fields() {
    let r = DirectResult {
        text: "summary".to_string(),
        input_tokens: Some(100),
        output_tokens: Some(50),
    };
    assert_eq!(r.text, "summary");
    assert_eq!(r.input_tokens, Some(100));
}
```

- [ ] **Step 2: Refactor `call_router_llm()`**

Replace the body of `call_router_llm()` in `src/engine/router/llm.rs`:

```rust
async fn call_router_llm(&self, prompt: &str, config: &RouterConfig) -> anyhow::Result<String> {
    if crate::engine::runner::response::is_agent_in_cooldown(&config.router_agent) {
        anyhow::bail!("router LLM agent '{}' is on cooldown", config.router_agent);
    }

    let timeout = Duration::from_secs(config.timeout_seconds);
    let model = if config.router_model.is_empty() {
        None
    } else {
        Some(config.router_model.as_str())
    };

    let mut cmd = crate::engine::runner::agents::get_runner(&config.router_agent)
        .router_command(prompt, model)?;

    match crate::engine::runner::direct::run_direct_command_raw(&mut cmd, timeout).await {
        Ok(stdout) => Ok(stdout),
        Err(e) => {
            let msg = e.to_string();
            // Detect rate limit from error message (agent exits non-zero on API errors)
            if msg.contains("rate limit") || msg.contains("429") || msg.contains("overloaded") {
                tracing::warn!(
                    agent = %config.router_agent,
                    "router LLM rate limited — adding to cooldown"
                );
                crate::engine::runner::response::record_agent_failure(&config.router_agent);
                anyhow::bail!("router LLM rate limited: {}", config.router_agent);
            }
            tracing::warn!(error = %msg, "router LLM command failed");
            Err(e)
        }
    }
}
```

**Note:** The original code detected rate limits by calling `runner.parse_response(&stdout)` on a non-zero exit. `run_direct_command_raw()` doesn't do that — it bails with stderr. We need to preserve rate limit detection. Adjust `run_direct_command_raw()` to include stdout in the error when non-zero exit, or keep the rate-limit detection in `call_router_llm()` by checking the raw stdout before bailing.

**Better approach:** Return stdout even on non-zero exit so `call_router_llm()` can check it. Use a `Result<(String, bool), E>` or just expose the stdout in the error. Simplest: keep the original stdout-based detection.

Revised `run_direct_command_raw()`:

```rust
pub async fn run_direct_command_raw(
    cmd: &mut tokio::process::Command,
    timeout: Duration,
) -> Result<String, DirectCommandError> {
    let output = tokio::time::timeout(timeout + Duration::from_secs(5), cmd.output())
        .await
        .map_err(|_| DirectCommandError::Timeout { secs: timeout.as_secs() })?
        .map_err(|e| DirectCommandError::Spawn { message: e.to_string() })?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code().unwrap_or(-1);

    if !output.status.success() {
        return Err(DirectCommandError::NonZeroExit { exit_code, stdout, stderr });
    }
    if stdout.is_empty() {
        return Err(DirectCommandError::EmptyResponse { stderr });
    }
    Ok(stdout)
}

#[derive(Debug)]
pub enum DirectCommandError {
    Timeout { secs: u64 },
    Spawn { message: String },
    NonZeroExit { exit_code: i32, stdout: String, stderr: String },
    EmptyResponse { stderr: String },
}

impl std::fmt::Display for DirectCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout { secs } => write!(f, "agent timed out after {secs}s"),
            Self::Spawn { message } => write!(f, "spawning agent: {message}"),
            Self::NonZeroExit { exit_code, stderr, .. } => write!(f, "agent failed (exit {exit_code}): {stderr}"),
            Self::EmptyResponse { .. } => write!(f, "agent returned empty response"),
        }
    }
}

impl std::error::Error for DirectCommandError {}
```

Then in `call_router_llm()`:

```rust
use crate::engine::runner::direct::{run_direct_command_raw, DirectCommandError};

match run_direct_command_raw(&mut cmd, timeout).await {
    Ok(stdout) => Ok(stdout),
    Err(DirectCommandError::NonZeroExit { stdout, stderr, .. }) => {
        // Detect rate limit from stdout (agent exits non-zero on API errors)
        use crate::engine::runner::agents::AgentError;
        let runner = crate::engine::runner::agents::get_runner(&config.router_agent);
        if let Err(AgentError::RateLimit { .. }) = runner.parse_response(&stdout) {
            tracing::warn!(
                agent = %config.router_agent,
                "router LLM rate limited — adding to cooldown"
            );
            crate::engine::runner::response::record_agent_failure(&config.router_agent);
            anyhow::bail!("router LLM rate limited: {}", config.router_agent);
        }
        tracing::warn!(stderr = %stderr, stdout = %stdout, "router LLM command failed");
        anyhow::bail!("router LLM failed: {stderr}");
    }
    Err(DirectCommandError::Timeout { secs }) => {
        anyhow::bail!("router LLM timed out after {secs}s");
    }
    Err(DirectCommandError::EmptyResponse { stderr }) => {
        tracing::warn!(stderr = %stderr, "router LLM returned empty stdout");
        anyhow::bail!("router LLM returned empty response");
    }
    Err(e) => Err(e.into()),
}
```

- [ ] **Step 3: Compile and run tests**

```bash
cargo fmt -- --check && cargo clippy --all-targets -- -D warnings && cargo nextest run
```

Expected: all pass.

- [ ] **Step 4: Commit**

```bash
git add src/engine/runner/direct.rs src/engine/router/llm.rs
git commit -m "refactor(router): delegate call_router_llm() to runner::direct::run_direct_command_raw()"
```

---

## Task 4: Final verification

- [ ] **Step 1: Run all CI checks**

```bash
cargo fmt -- --check && cargo clippy --all-targets -- -D warnings && cargo nextest run
```

Expected: all pass, no warnings.

- [ ] **Step 2: Verify dead code removed from `control.rs`**

```bash
grep -n "extract_from_envelope\|get_runner\|PermissionRules\|AgentError" src/control.rs
```

Expected: none of these symbols remain (they moved to `direct.rs`).

- [ ] **Step 3: Final commit if any cleanup needed**

If no changes, skip. Otherwise:

```bash
git add -p
git commit -m "chore: remove dead code after run_direct() extraction"
```
