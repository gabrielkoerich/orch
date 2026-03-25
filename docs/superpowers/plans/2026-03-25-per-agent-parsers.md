# Per-Agent Response Parsers — Refactor Plan

## Problem

All agents share the same generic response parser. Each agent returns different NDJSON structures, error formats, and edge cases. The generic parser keeps failing because it can't handle all variations.

### Agent Output Differences (observed in production)

| Agent | Binary | NDJSON Format | Error Format | Exit Code on Failure |
|-------|--------|---------------|--------------|---------------------|
| claude | claude | `type:system`, `type:assistant`, `type:result` | `is_error:true` in result event | 0 or 1 |
| kimi | claude (kimi alias) | Same as claude | `is_error:true` + `authentication_failed` + `permission_error` | 0 (even on auth failure!) |
| minimax | claude (minimax alias) | Same as claude | `is_error:true` + `modelUsage:{}` (empty = failed) | 0 or 1 |
| opencode | opencode | `type:text`, `type:step_start`, `type:step_finish` | `error` event, `step_finish.reason=error` | 0 or non-zero |
| codex | codex | `type:item.completed`, `type:turn.failed` | `turn.failed` event | 0 or non-zero |

### What Goes Wrong Today

1. **kimi exits 0 with `is_error:true`** — parser skips error path because exit code is 0
2. **claude NDJSON `type:assistant` not extracted** — `ndjson_extract_text` only handled opencode/codex
3. **minimax produces valid review but `classify_error` scans full output** for rate limit patterns
4. **opencode `type:text` events have two sub-formats** — `part.text` vs direct `text` field
5. **Review result in `type:result.result` field** — needs unwrapping from the envelope

## Plan

### Phase 1: Extract `find_result_event()` helper

A shared function that all agents can use to find the final result event in NDJSON:

```rust
pub struct AgentResult {
    pub is_error: bool,
    pub result_text: String,  // The inner result content
    pub exit_code: Option<i32>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
}

/// Find the type:result event in NDJSON, extract key fields.
/// Works for claude/kimi/minimax (all use claude binary).
pub fn find_claude_result(ndjson: &str) -> Option<AgentResult> {
    // Find last line with type:result
    // Extract is_error, result, usage
}

/// Find the agent text from opencode NDJSON.
pub fn find_opencode_result(ndjson: &str) -> Option<AgentResult> {
    // Find type:text events, concatenate
    // Find type:step_finish for tokens/cost
    // Check for error events
}

/// Find the agent text from codex NDJSON.
pub fn find_codex_result(ndjson: &str) -> Option<AgentResult> {
    // Find item.completed with type:agent_message
    // Check for turn.failed events
}
```

### Phase 2: Per-agent review response extraction

Replace the generic `parse_response` + fallback chain in `review.rs` with agent-specific extraction:

```rust
let result = match review_agent.as_str() {
    "claude" | "kimi" | "minimax" => find_claude_result(&raw_output),
    "opencode" => find_opencode_result(&raw_output),
    "codex" => find_codex_result(&raw_output),
    _ => find_claude_result(&raw_output), // fallback
};

match result {
    Some(r) if r.is_error => {
        // Agent reported error — cooldown and retry
        cooldown_agent(&review_agent, &r.result_text);
        return ReviewDecision::Failed(r.result_text);
    }
    Some(r) => {
        // Agent produced output — parse review from result_text
        let review = parse_review_response(&r.result_text)?;
        // ... post comment, merge, etc.
    }
    None => {
        // No result event found — truly empty output
        return ReviewDecision::Failed("no output");
    }
}
```

### Phase 3: Per-agent task response extraction

Same pattern for task agents (not just review). The `parse_success_output` in `runner/mod.rs` should use agent-specific extractors.

### Phase 4: Kimi/minimax-specific handling

Even though kimi and minimax use the claude binary, they have different behavior:

- **kimi**: Returns `is_error:true` on billing cycle limit, exit 0. The `result` field contains the error message from the Kimi API (not Claude API).
- **minimax**: Returns valid output but `modelUsage:{}` when the model doesn't exist. Exit 1 with full NDJSON output.

The per-agent parser should check:
- kimi: `is_error` flag + `result` text for "usage limit" / "quota" / "permission_error"
- minimax: `modelUsage` empty object = model failure

### Phase 5: Integration tests per agent

For each agent, test:
1. Successful response → parsed correctly
2. Rate limit response → detected, cooled
3. Auth failure → detected, cooled
4. Empty/malformed response → handled gracefully
5. Plain text response → synthesized

## Files

| File | Change |
|------|--------|
| `src/engine/runner/agents/claude.rs` | Add `find_claude_result()` |
| `src/engine/runner/agents/opencode.rs` | Add `find_opencode_result()` |
| `src/engine/runner/agents/codex.rs` | Add `find_codex_result()` |
| `src/engine/runner/agents/mod.rs` | `AgentResult` struct, shared trait |
| `src/engine/review.rs` | Use per-agent extractors instead of generic parse |
| `src/engine/runner/mod.rs` | Use per-agent extractors for task responses |
| `src/engine/runner/response.rs` | `ndjson_extract_text` → delegate to per-agent |
| `tests/integration_review.rs` | Per-agent integration tests |
| `tests/fixtures/` | Add fixtures for each agent's error/success formats |
