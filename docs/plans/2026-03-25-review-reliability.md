# Review Agent Reliability — Fix Plan

## Problems Found (2026-03-24/25)

### 1. Startup InReview reset reads GitHub labels instead of SQLite
- SQLite says `in_review`, GitHub says `needs_review` (label out of sync)
- `backend.list_by_status(InReview)` reads GitHub → finds nothing
- Tasks stuck in `in_review` forever after restart
- **Fix**: Read from SQLite store, not backend, for startup reset

### 2. Review output parsing — per-agent format handling
- Kimi returns `is_error: true` with exit 0 (auth failure looks like success)
- Claude returns `type:result` with `result` field containing review text
- OpenCode returns `type:text` events with review content
- Minimax same as Claude (uses claude binary)
- The review parser scans raw NDJSON for rate limit patterns and misclassifies valid reviews
- **Fix**: Parse the `type:result` event properly — extract `result` field, check `is_error`, THEN parse review content

### 3. Output file overwritten by retry attempts
- All review attempts use `attempt: 1` — same output file
- Second attempt overwrites first attempt's output
- Review code from first attempt reads stale data
- **Fix**: Use incrementing attempt numbers for review, or read output immediately after session ends

### 4. Kimi/minimax billing cycle cooldown
- "usage limit for this billing cycle" needs 5h cooldown, not 30min
- `parse_retry_at` doesn't handle "billing cycle" / "next cycle" patterns
- **Fix**: Already done in cooldown.rs — detect billing cycle, cooldown 5h

### 5. review_session_expected flag not set on all paths
- If the subscriber sets `in_review` but the spawn fails, the flag stays false
- Startup reset skips tasks with `review_session_expected = false` and age < 10min
- **Fix**: Set the flag BEFORE spawning, not after

### 6. Startup worktree reconciliation
- On restart, iterate all worktrees in `~/.orch/worktrees/{project}/`
- For each worktree, find the corresponding task in SQLite
- If task is done/cancelled → delete worktree + local branch + remote branch
- If task is active → try `git rebase origin/main`
- If rebase fails (conflicts) → delete worktree + branches, reset task to `new` for fresh start
- This prevents stale worktrees accumulating and conflicting branches blocking agents

## Implementation Tasks

### Task 1: Read SQLite for startup InReview reset (CRITICAL)

In `src/engine/mod.rs`, change the startup reset from:
```rust
// BROKEN: reads from GitHub labels which may be out of sync
backend.list_by_status(Status::InReview)
```
To:
```rust
// CORRECT: read from SQLite (source of truth)
store.list_by_status(repo, TaskStatus::InReview)
```

### Task 2: Per-agent review response extraction

In `src/engine/review.rs`, replace the generic parse flow with agent-specific extraction:

```rust
// Step 1: Find the type:result event in NDJSON
let result_event = find_result_event(&raw_output);

// Step 2: Check is_error
if result_event.is_error {
    // Cooldown agent, return Failed
    return handle_review_error(result_event);
}

// Step 3: Extract review text from result.result field
let review_text = result_event.result;

// Step 4: Parse review JSON from the text (handles markdown blocks)
let review = parse_review_response(&review_text);
```

This is simpler than the current 3-stage parse with fallbacks.

### Task 3: Fix attempt numbering for reviews

In `src/engine/review.rs` line 540, change `attempt: 1` to use an incrementing counter:
```rust
let review_attempts = store_increment(&Some(store.clone()), repo, &task.id.0, "review_attempts").await;
let review_attempt_dir = task_attempt_dir(repo, &review_task_id, review_attempts as u32)?;
```

### Task 4: Set review_session_expected before spawn

In `src/engine/subscribers/review.rs`, move `set_review_session_expected(true)` to BEFORE `spawn_in_tmux`, not after.

### Task 5: Integration test for full review cycle

Extend `tests/integration_review.rs`:
1. Spawn agent in tmux with review prompt
2. Wait for completion
3. Read output file
4. Parse with the same code path as review.rs
5. Verify decision is extracted
6. Test with each agent: claude, kimi, minimax, opencode
