# Eliminate N×2 DB Round-Trips in `orch task show` — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace sequential `store_get_field` / `opt_store_get_field` calls for a single task with one `get_by_external_id` call, reducing 18+ DB round-trips to 2 in `orch task show`, `orch task sessions`, and the review agent.

**Architecture:** `TaskStore::get_by_external_id` already exists and returns the full `Task` struct in one query. The callers currently call `store_get_field` once per field — each call resolves `task_id → store_id` (query 1) then loads the full row (query 2). Fix: load the task once, read all fields from the struct.

**Tech Stack:** Rust, sqlx, tokio

---

## File Map

| File | Change |
|------|--------|
| `src/engine/cleanup.rs` | Add `opt_store_get_task()` helper (returns `Option<Task>` in one query) |
| `src/cli/task.rs` | Replace 9 `opt_store_get_field` calls (lines ~328–388) with struct field access; replace 3 calls (lines ~1127–1143) similarly |
| `src/engine/review.rs` | Replace 4 `store_get_field` calls (lines ~611–638) with one `get_by_external_id` call |

---

## Task 1: Add `opt_store_get_task` helper to `cleanup.rs`

**Files:**
- Modify: `src/engine/cleanup.rs`

This helper replaces the pattern of calling `opt_store_get_field` multiple times for the same task — it loads the task once.

- [ ] **Step 1: Open the file and find where to insert**

  Read `src/engine/cleanup.rs` lines 19–30 (after `opt_store_get_field`). Add the new function directly after `opt_store_get_field`.

- [ ] **Step 2: Add the helper**

  Insert after `opt_store_get_field` (around line 30):

  ```rust
  /// Load a full task record from the store in a single query.
  ///
  /// Convenience wrapper over `get_by_external_id` that handles
  /// `Option<Arc<TaskStore>>`: returns `None` if store is absent or the
  /// task is not found.
  pub(crate) async fn opt_store_get_task(
      store: &Option<Arc<TaskStore>>,
      repo: &str,
      task_id: &str,
  ) -> Option<crate::store::Task> {
      if let Some(ref s) = store {
          s.get_by_external_id(repo, task_id).await.ok().flatten()
      } else {
          None
      }
  }
  ```

- [ ] **Step 3: Compile to verify**

  ```bash
  cd /Users/gb/.orch/worktrees/orch/gh-issue-795-refactor-eliminate-n-2-db-round-trips-in
  cargo check 2>&1 | head -30
  ```

  Expected: no errors.

- [ ] **Step 4: Commit**

  ```bash
  git add src/engine/cleanup.rs
  git commit -m "refactor: add opt_store_get_task helper — load full Task in one query"
  ```

---

## Task 2: Refactor `task show` — External task display (task.rs lines ~328–388)

**Files:**
- Modify: `src/cli/task.rs:328-398`

Currently 9 calls to `opt_store_get_field` + 2 calls to `get_token_usage`/`get_cost_estimate` (each doing 2 round trips) = ~22 DB round trips. Replace with one `opt_store_get_task` call.

The relevant block is inside `match task { Task::External(ext) => { ... } }`.

- [ ] **Step 1: Identify the exact block to replace**

  Read `src/cli/task.rs` lines 326–400. Confirm the block starts with:
  ```
  // Show agent/branch info if available
  if let Some(agent) = store_helpers::opt_store_get_field(&store, &repo, &ext.id.0, "agent").await
  ```
  and ends just before `println!("\n{}", ext.body);`.

- [ ] **Step 2: Replace the block**

  Replace lines 327–397 (the entire series of `opt_store_get_field` + `get_token_usage` + `get_cost_estimate` calls) with:

  ```rust
  // Load all store fields in a single DB query
  if let Some(t) = store_helpers::opt_store_get_task(&store, &repo, &ext.id.0).await {
      if let Some(agent) = &t.agent {
          println!("Agent: {}", agent);
      }
      if let Some(model) = &t.model {
          println!("Model: {}", model);
      }
      if !t.complexity.is_empty() {
          println!("Complexity: {}", t.complexity);
      }
      if t.route_attempts > 0 {
          println!("Route attempts: {}", t.route_attempts);
      }
      if t.attempts > 0 {
          println!("Attempts: {}", t.attempts);
      }
      if t.review_cycles > 0 {
          println!("Review cycles: {}", t.review_cycles);
      }
      if let Some(pr) = t.pr_number {
          println!("PR: #{}", pr);
      }
      if !t.branch.is_empty() {
          println!("Branch: {}", t.branch);
      }
      if !t.last_error.is_empty() {
          println!("Last error: {}", truncate_err(&t.last_error, 200));
      }
      // Cost summary (if any tokens recorded)
      let total_tokens = (t.input_tokens + t.output_tokens) as u64;
      if total_tokens > 0 || t.total_cost_usd > 0.0 {
          println!(
              "Tokens: {} in / {} out — ${:.6}",
              t.input_tokens, t.output_tokens, t.total_cost_usd
          );
      }
  }
  ```

  Note: the `TokenUsage` / `CostEstimate` helpers are no longer needed here since we read directly from `Task` fields.

- [ ] **Step 3: Compile**

  ```bash
  cargo check 2>&1 | head -30
  ```

  Expected: no errors.

- [ ] **Step 4: Commit**

  ```bash
  git add src/cli/task.rs
  git commit -m "refactor: task show reads all External task fields from one DB query"
  ```

---

## Task 3: Refactor `task sessions` store fields (task.rs lines ~1123–1155)

**Files:**
- Modify: `src/cli/task.rs:1123-1155`

The `task sessions` subcommand loads agent/model/attempts + token/cost data via multiple `opt_store_get_field` calls on `task_key`. Replace with one `opt_store_get_task` call. The `get_recent_memory` helper also calls `resolve_task_id` + `get` internally — once we have the task struct we can call `store.recent_memory(task.id, max)` directly.

- [ ] **Step 1: Read the block**

  Read `src/cli/task.rs` lines 1123–1200. Confirm the "Task store fields" block structure:
  ```
  // Task store fields
  {
    {
      if let Some(agent) = store_helpers::opt_store_get_field(..., "agent").await { ... }
      if let Some(model) = store_helpers::opt_store_get_field(..., "model").await { ... }
      if let Some(attempts) = store_helpers::opt_store_get_field(..., "attempts").await { ... }
      let usage = store_helpers::get_token_usage(...).await;
      let cost = store_helpers::get_cost_estimate(...).await;
      ...
      let mem = store_helpers::get_recent_memory(...).await;
  ```

- [ ] **Step 2: Replace the block**

  Replace the "Task store fields" block (starting at `// Task store fields`) with a single `get_by_external_id` call. Since `task_key` can be `"internal:N"` or a numeric external ID (both work with `get_by_external_id` since internal tasks use `external_id = "internal:N"`), use `opt_store_get_task`:

  ```rust
  // Task store fields — single DB query for all fields
  if let Some(t) = store_helpers::opt_store_get_task(&store, &repo, &task_key).await {
      if let Some(agent) = &t.agent {
          println!("Agent: {}", agent);
      }
      if let Some(model) = &t.model {
          println!("Model: {}", model);
      }
      if t.attempts > 0 {
          println!("Attempts: {}", t.attempts);
      }

      // Token & cost summary
      let total_tokens = (t.input_tokens + t.output_tokens) as u64;
      if total_tokens > 0 || t.total_cost_usd > 0.0 {
          println!("\nCost summary:");
          println!("  input tokens:  {}", t.input_tokens);
          println!("  output tokens: {}", t.output_tokens);
          println!("  total tokens:  {}", total_tokens);
          println!("  estimated $:   ${:.6}", t.total_cost_usd);
      }

      // Memory (recent attempts) — use task DB id directly, no extra resolve
      if let Some(ref s) = store {
          if let Ok(mem) = s.recent_memory(t.id, 10).await {
              if !mem.is_empty() {
                  // ... print memory entries (preserve existing formatting)
              }
          }
      }
  }
  ```

  Keep all the memory printing code inside the `if !mem.is_empty()` block exactly as it was.

- [ ] **Step 3: Compile**

  ```bash
  cargo check 2>&1 | head -30
  ```

  Expected: no errors. If `get_recent_memory` was the only user of some import, clippy may warn — remove unused imports.

- [ ] **Step 4: Commit**

  ```bash
  git add src/cli/task.rs
  git commit -m "refactor: task sessions reads all store fields from one DB query"
  ```

---

## Task 4: Refactor `review.rs` (lines ~611–638)

**Files:**
- Modify: `src/engine/review.rs:611-639`

Currently 4 calls to `store_get_field` for worktree, branch, summary, pr_number — each making 2 DB queries = 8 round trips. Replace with one `store.get_by_external_id` call.

- [ ] **Step 1: Read the block**

  Read `src/engine/review.rs` lines 608–645. Confirm:
  ```rust
  let worktree = super::cleanup::store_get_field(store, repo, &task.id.0, "worktree").await;
  let branch   = super::cleanup::store_get_field(store, repo, &task.id.0, "branch").await;
  let agent_summary = super::cleanup::store_get_field(store, repo, &task.id.0, "summary").await.unwrap_or_default();
  // ... worktree/branch early-exit checks ...
  let stored_pr_number = super::cleanup::store_get_field(store, repo, &task.id.0, "pr_number").await...
  ```

- [ ] **Step 2: Replace with single get_by_external_id call**

  Replace the 4 `store_get_field` calls with:

  ```rust
  // Load all needed fields in a single DB query
  let task_row = store
      .get_by_external_id(repo, &task.id.0)
      .await
      .ok()
      .flatten();

  let worktree = task_row.as_ref().and_then(|t| {
      if t.worktree.is_empty() { None } else { Some(t.worktree.clone()) }
  });
  let branch = task_row.as_ref().and_then(|t| {
      if t.branch.is_empty() { None } else { Some(t.branch.clone()) }
  });
  let agent_summary = task_row
      .as_ref()
      .map(|t| t.summary.clone())
      .unwrap_or_default();
  ```

  Then replace the `stored_pr_number` derivation with:
  ```rust
  let stored_pr_number = task_row
      .as_ref()
      .and_then(|t| t.pr_number)
      .filter(|&n| n > 0)
      .map(|n| n as u64);
  ```

  Note: the original `stored_pr_number` was `Option<u64>`. `t.pr_number` is `Option<i32>`, so cast via `.map(|n| n as u64)`.

- [ ] **Step 3: Compile**

  ```bash
  cargo check 2>&1 | head -30
  ```

  Expected: no errors.

- [ ] **Step 4: Commit**

  ```bash
  git add src/engine/review.rs
  git commit -m "refactor: review.rs loads worktree/branch/summary/pr_number in one DB query"
  ```

---

## Task 5: Run CI checks and verify

**Files:** none (verification only)

- [ ] **Step 1: Format check**

  ```bash
  cd /Users/gb/.orch/worktrees/orch/gh-issue-795-refactor-eliminate-n-2-db-round-trips-in
  cargo fmt -- --check
  ```

  If it fails, run `cargo fmt` then re-check.

- [ ] **Step 2: Clippy**

  ```bash
  cargo clippy --all-targets -- -D warnings 2>&1 | head -50
  ```

  Fix any warnings. Common ones after this refactor:
  - Unused imports (`use crate::store::{TokenUsage, CostEstimate}` if no longer needed in task.rs)
  - Dead code warnings on removed call sites

- [ ] **Step 3: Tests**

  ```bash
  cargo nextest run 2>&1 | tail -30
  ```

  Expected: all pass. The existing `store_get_field_*` tests in `cleanup.rs` still cover the helper itself; no new tests are needed since the refactor removes call sites without changing behavior.

- [ ] **Step 4: Final commit (fmt fixes if needed)**

  ```bash
  git add -p
  git commit -m "style: cargo fmt after DB round-trip refactor"
  ```

---

## Notes

- `get_by_external_id` works for internal task IDs too — they're stored with `external_id = "internal:N"`.
- The old `store_get_field` / `opt_store_get_field` helpers remain in `cleanup.rs` — they're still used in `engine/mod.rs` and other places that load only one field. Don't remove them.
- `stored_pr_number` in review.rs was originally parsed from a string. After this refactor it comes from `t.pr_number: Option<i32>`, so the `.parse::<u64>()` step is replaced by a cast.
