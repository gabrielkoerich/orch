# Opencode Silent Exit-0 Free-Model Retry Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When opencode exits 0 with empty output (silent failure), retry with another free model instead of falling through to claude/codex.

**Architecture:** The fix lives entirely in `src/engine/runner/fallback.rs`. In the `Unknown { exit_code: 0, message: "" }` arm, after recording the model cooldown, we check `agent_runner.free_models()` for an untried model not in cooldown. If found, we update the store with the new model and return `EarlyReturn { status: "new" }` to re-dispatch to opencode with the new model. Only if no free models remain do we fall through to `handle_failover()` (which would pick claude/codex).

**Tech Stack:** Rust, async/await, existing `store`, `response`, and `agents` modules.

---

### Task 1: Implement free-model retry in silent exit-0 arm

**Files:**
- Modify: `src/engine/runner/fallback.rs:144-157`

The current `Unknown { exit_code, message }` arm records the model cooldown but immediately falls through to `handle_failover()`. We need to insert a free-model retry between the cooldown recording and the fallthrough.

- [ ] **Step 1: Read the current Unknown arm**

The current code at lines 144–157 in `fallback.rs`:

```rust
agents::AgentError::Unknown { exit_code, message } => {
    // Exit 0 with empty output is a silent failure (common with GitHub
    // Copilot models in opencode).  Record a model-specific cooldown so
    // the same model is not retried on every subsequent task.
    if *exit_code == 0 && message.is_empty() {
        if let Some(m) = model_name {
            response::record_model_failure(agent_name, m);
        }
    }
    (
        response::RetryableError::Failed,
        format!("{agent_name} exit {exit_code}: {message}"),
    )
}
```

- [ ] **Step 2: Replace the Unknown arm with free-model retry logic**

Replace lines 144–157 with:

```rust
agents::AgentError::Unknown { exit_code, message } => {
    // Exit 0 with empty output is a silent failure (common with GitHub
    // Copilot models in opencode).  Record a model-specific cooldown so
    // the same model is not retried on every subsequent task.
    if *exit_code == 0 && message.is_empty() {
        if let Some(m) = model_name {
            response::record_model_failure(agent_name, m);
        }
        // Before falling through to handle_failover() (which tries claude/codex),
        // check whether this agent has any free models that haven't been tried yet.
        // This keeps silent failures contained within the free-model pool and
        // preserves claude/codex capacity for tasks that genuinely need them.
        let free = agent_runner.free_models();
        if !free.is_empty() {
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
                tracing::info!(task_id, model = %next_free, "silent exit-0: retrying with free model");
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
```

- [ ] **Step 3: Verify it compiles**

```bash
cargo check 2>&1 | head -30
```

Expected: no errors.

- [ ] **Step 4: Commit checkpoint**

```bash
git add src/engine/runner/fallback.rs
git commit -m "fix: retry free model on opencode silent exit-0 before falling through to claude"
```

---

### Task 2: Add unit test for silent exit-0 free-model retry

**Files:**
- Modify: `src/engine/runner/fallback.rs` (add `#[cfg(test)] mod tests` section at the bottom)

The test verifies that `handle_error()` returns `EarlyReturn { status: "new" }` when:
- Agent error is `Unknown { exit_code: 0, message: "" }`
- The agent runner reports free models
- The store is `None` (so `model_reroute_chain` is empty — all free models available)

It also verifies that when all free models are in cooldown, it returns `Continue` (falls through to failover).

- [ ] **Step 1: Write the failing test**

Add this at the bottom of `src/engine/runner/fallback.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::runner::agents::{AgentError, AgentRunner, ParsedResponse, PermissionRules};
    use crate::parser::AgentResponse;

    /// Minimal mock runner for fallback tests.
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
    async fn silent_exit0_retries_free_model_before_failover() {
        // Given: opencode exits 0 with empty output, and there is one free model available
        let runner = MockRunner {
            free: vec!["opencode/mimo-v2-omni-free".to_string()],
        };
        let err = AgentError::Unknown {
            exit_code: 0,
            message: String::new(),
        };

        let result = handle_error(
            "test-task-1",
            &err,
            "opencode",
            &runner,
            Some("opencode/gpt-5.4-mini"),
            1,
            &None,
            "owner/repo",
        )
        .await
        .unwrap();

        // Should retry with the free model, not fall through to claude
        assert!(
            matches!(result, ErrorHandleResult::EarlyReturn { ref status } if status == "new"),
            "expected EarlyReturn{{status: new}}, got Continue"
        );
    }

    #[tokio::test]
    async fn silent_exit0_falls_through_when_no_free_models() {
        // Given: opencode exits 0 with empty output, but no free models are available
        let runner = MockRunner { free: vec![] };
        let err = AgentError::Unknown {
            exit_code: 0,
            message: String::new(),
        };

        let result = handle_error(
            "test-task-2",
            &err,
            "opencode",
            &runner,
            Some("opencode/gpt-5.4-mini"),
            1,
            &None,
            "owner/repo",
        )
        .await
        .unwrap();

        // Should fall through to normal failover (Continue)
        assert!(
            matches!(result, ErrorHandleResult::Continue { .. }),
            "expected Continue, got EarlyReturn"
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail first**

```bash
cargo nextest run engine::runner::fallback::tests 2>&1
```

Expected: compilation error or test not found (since code doesn't exist yet). After Task 1 is done, the tests should compile. Run after Task 1 is applied.

- [ ] **Step 3: Run the tests to verify they pass**

```bash
cargo nextest run engine::runner::fallback::tests 2>&1
```

Expected output:
```
PASS engine::runner::fallback::tests::silent_exit0_retries_free_model_before_failover
PASS engine::runner::fallback::tests::silent_exit0_falls_through_when_no_free_models
```

- [ ] **Step 4: Commit**

```bash
git add src/engine/runner/fallback.rs
git commit -m "test: verify silent exit-0 retries free model before falling through to claude"
```

---

### Task 3: Full CI checks and final commit

**Files:** None (verification only)

- [ ] **Step 1: Run formatter**

```bash
cargo fmt -- --check
```

Expected: no output (already formatted). If it fails, run `cargo fmt` and re-check.

- [ ] **Step 2: Run clippy**

```bash
cargo clippy --all-targets -- -D warnings 2>&1 | tail -20
```

Expected: no warnings or errors.

- [ ] **Step 3: Run full test suite**

```bash
cargo nextest run 2>&1 | tail -20
```

Expected: all tests pass.

- [ ] **Step 4: Verify the fix is complete**

Confirm the log message `"silent exit-0: retrying with free model"` will appear instead of `"failover: switching to fallback agent from=opencode to=claude"` when opencode exits 0 with empty output and free models are available.
