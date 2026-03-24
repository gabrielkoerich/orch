# Model Pools Per Complexity Tier — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Allow each complexity tier to specify an array of models per agent; at dispatch time, pick one randomly while skipping models in cooldown.

**Architecture:** Change `model_map` internal type from `HashMap<String, HashMap<String, String>>` to `HashMap<String, HashMap<String, Vec<String>>>`. `model_for_complexity` keeps its `Option<String>` return type but internally picks a random non-cooled model from the pool. Config loading is updated to parse both single-string and YAML-array values. No new crates needed — pool shuffling reuses the existing `simple_hash_index_for` helper.

**Tech Stack:** Rust, `serde_yml`, existing `selection.rs` hash utilities, `response::is_model_in_cooldown` cooldown API.

---

## File Map

| File | Change |
|------|--------|
| `src/engine/router/config.rs` | Change `model_map` type, update `Default`, pool selection in `model_for_complexity`, parse arrays in `from_config` |
| `src/engine/router/selection.rs` | Change `simple_hash_index_for` visibility from `pub(super)` to `pub(crate)` so `config.rs` can call it |
| `src/engine/router/mod.rs` | Update `model_map_lookup` test to expect `Some(...)` for single-entry pools (no logic change) |
| `prompts/route.md` | Add routing hint to spread tasks to opencode/kimi/minimax |
| `~/.orch/config.yml` | Add model pool arrays under `model_map` |

---

## DO NOT TOUCH

- `src/github/token.rs` — settled, do not touch
- `src/engine/runner/` PTY runner — settled, do not touch
- Any auth flow code

---

## Required CI Checks (run before every commit)

```bash
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
cargo nextest run
```

Or combined:

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo nextest run
```

---

## Task 0: Expose `simple_hash_index_for` to the whole crate

**Files:**
- Modify: `src/engine/router/selection.rs`

`simple_hash_index_for` is currently `pub(super)` (only visible to the `router` module's `mod.rs`). We need it in `config.rs` (a sibling submodule), so change its visibility to `pub(crate)`.

- [ ] **Step 1: Change visibility**

In `src/engine/router/selection.rs`, change line 35:
```rust
// Before:
pub(super) fn simple_hash_index_for(len: usize, task_id: &str) -> usize {

// After:
pub(crate) fn simple_hash_index_for(len: usize, task_id: &str) -> usize {
```

(If `simple_hash_fraction_for` is also `pub(super)` and used by `weights.rs`, leave it as-is or change to `pub(crate)` only if needed. Only `simple_hash_index_for` is needed by `config.rs`.)

- [ ] **Step 2: Confirm it compiles**

```bash
cargo build 2>&1 | head -20
```

Expected: builds cleanly (no visibility errors).

- [ ] **Step 3: Commit**

```bash
git add src/engine/router/selection.rs
git commit -m "refactor(router): expose simple_hash_index_for as pub(crate)"
```

---

## Task 1: Change `model_map` type and update `Default`

**Files:**
- Modify: `src/engine/router/config.rs`

The `RouterConfig.model_map` field changes from `HashMap<String, HashMap<String, String>>` to `HashMap<String, HashMap<String, Vec<String>>>`. All `.insert(agent, "model-string")` calls in `Default::default()` become `.insert(agent, vec!["model-string".to_string()])`.

- [ ] **Step 1: Write the failing tests for pool type and single-entry backward compat**

Add to the `#[cfg(test)]` block at the bottom of `src/engine/router/config.rs`:

```rust
#[test]
fn model_pool_single_entry_returns_model() {
    let config = RouterConfig::default();
    // Single-entry pool always returns the one model (not in cooldown)
    let model = config.model_for_complexity("claude", "simple");
    assert_eq!(model, Some("claude-haiku-4-5-20251001".to_string()));
}

#[test]
fn model_pool_all_cooled_returns_first() {
    use crate::engine::runner::response::record_model_failure;
    let config = RouterConfig::default();
    // Put the single claude/simple model in cooldown
    record_model_failure("claude_pool_test", "model-a");
    record_model_failure("claude_pool_test", "model-b");

    // Build a config with a 2-entry pool for a synthetic agent
    let mut cfg = RouterConfig::default();
    let pool = vec!["model-a".to_string(), "model-b".to_string()];
    cfg.model_map
        .entry("simple".to_string())
        .or_default()
        .insert("claude_pool_test".to_string(), pool);

    // Both cooled — should return first entry (index 0) as deterministic fallback
    let model = cfg.model_for_complexity("claude_pool_test", "simple");
    assert_eq!(model, Some("model-a".to_string()),
        "all-cooled fallback must return pool[0]");
}

#[test]
fn model_pool_skips_cooled_model() {
    use crate::engine::runner::response::{record_model_failure, is_model_in_cooldown};
    let mut cfg = RouterConfig::default();
    let pool = vec!["model-x".to_string(), "model-y".to_string()];
    cfg.model_map
        .entry("simple".to_string())
        .or_default()
        .insert("agent_pool_skip_test".to_string(), pool);

    // Put model-x in cooldown
    record_model_failure("agent_pool_skip_test", "model-x");
    assert!(is_model_in_cooldown("agent_pool_skip_test", "model-x"));

    // Should always return model-y (model-x is cooled)
    for _ in 0..10 {
        let model = cfg.model_for_complexity("agent_pool_skip_test", "simple");
        assert_eq!(model, Some("model-y".to_string()));
    }
}
```

- [ ] **Step 2: Run tests to confirm they fail**

```bash
cargo nextest run --test-threads 1 -E 'test(model_pool)'
```

Expected: compile error or test failures (`model_for_complexity` signature mismatch or return-type mismatch).

- [ ] **Step 3: Update `model_map` field type and `Default::default()`**

In `src/engine/router/config.rs`:

Change the field declaration:
```rust
// Before:
pub model_map: HashMap<String, HashMap<String, String>>,

// After:
pub model_map: HashMap<String, HashMap<String, Vec<String>>>,
```

In `Default::default()`, change every `insert` to wrap the model in `vec![]`:
```rust
// Before:
simple.insert("claude".to_string(), "claude-haiku-4-5-20251001".to_string());

// After:
simple.insert("claude".to_string(), vec!["claude-haiku-4-5-20251001".to_string()]);
```

Apply this pattern to all 20 `.insert(...)` calls in `default()` (simple/medium/complex/review for each of the 5 agents).

- [ ] **Step 4: Update `model_for_complexity` to pick from the pool**

Replace the current implementation:

```rust
pub fn model_for_complexity(&self, agent: &str, complexity: &str) -> Option<String> {
    self.model_map
        .get(complexity)
        .and_then(|m| m.get(agent))
        .cloned()
}
```

With:

```rust
pub fn model_for_complexity(&self, agent: &str, complexity: &str) -> Option<String> {
    let pool = self.model_map.get(complexity)?.get(agent)?;
    if pool.is_empty() {
        return None;
    }
    if pool.len() == 1 {
        return Some(pool[0].clone());
    }

    // Pick a random starting index to distribute load across pool models.
    // Uses the existing hash helper to avoid adding a rand dependency.
    let start = super::selection::simple_hash_index_for(pool.len(), agent);

    // Scan from start, skipping models in cooldown.
    for offset in 0..pool.len() {
        let idx = (start + offset) % pool.len();
        let model = &pool[idx];
        if !crate::engine::runner::response::is_model_in_cooldown(agent, model) {
            return Some(model.clone());
        }
    }

    // All models in cooldown — return the first (index 0) as a deterministic
    // fallback. It will likely hit rate limit, but that triggers the normal
    // model-failover path.
    Some(pool[0].clone())
}
```

- [ ] **Step 5: Run tests**

```bash
cargo nextest run --test-threads 1 -E 'test(model_pool)'
```

Expected: all 3 new tests pass.

- [ ] **Step 6: Run full test suite + lint**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo nextest run
```

Expected: all pass. Fix any clippy warnings before continuing.

- [ ] **Step 7: Commit**

```bash
git add src/engine/router/config.rs
git commit -m "feat(router): change model_map to Vec<String> pools with cooldown-aware selection"
```

---

## Task 2: Update `from_config` to parse array values

**Files:**
- Modify: `src/engine/router/config.rs`

The existing `from_config` loading reads model map entries via `crate::config::get("model_map.{complexity}.{agent}")`. When the YAML value is a sequence, `config::get` serializes it via `serde_yml::to_string` and returns a YAML string like `"- model-a\n- model-b\n"`. We need to parse this back into `Vec<String>`.

- [ ] **Step 1: Write a failing unit test for array parsing**

Add to the test block in `src/engine/router/config.rs`:

```rust
#[test]
fn parse_model_pool_from_yaml_array_string() {
    // This is what serde_yml::to_string produces for a sequence
    let yaml_array = "- github-copilot/gpt-5-mini\n- opencode/minimax-m2.5-free\n";
    let pool = RouterConfig::parse_model_pool(yaml_array);
    assert_eq!(pool, vec![
        "github-copilot/gpt-5-mini".to_string(),
        "opencode/minimax-m2.5-free".to_string(),
    ]);
}

#[test]
fn parse_model_pool_from_plain_string() {
    let plain = "openai/gpt-4.1-mini";
    let pool = RouterConfig::parse_model_pool(plain);
    assert_eq!(pool, vec!["openai/gpt-4.1-mini".to_string()]);
}
```

- [ ] **Step 2: Run test to confirm failure**

```bash
cargo nextest run --test-threads 1 -E 'test(parse_model_pool)'
```

Expected: compile error (function doesn't exist).

- [ ] **Step 3: Add `parse_model_pool` helper and update `from_config`**

Add this private helper to the `RouterConfig` impl block (above `from_config`):

```rust
/// Parse a config value into a pool of model strings.
///
/// Handles two formats:
/// - A plain string: `"openai/gpt-4.1-mini"` → `vec!["openai/gpt-4.1-mini"]`
/// - A YAML sequence (as serialized by `serde_yml::to_string`):
///   `"- model-a\n- model-b\n"` → `vec!["model-a", "model-b"]`
fn parse_model_pool(value: &str) -> Vec<String> {
    // Try to parse as a YAML sequence first
    if let Ok(arr) = serde_yml::from_str::<Vec<String>>(value) {
        if !arr.is_empty() {
            return arr;
        }
    }
    // Fallback: treat the whole value as a single model name
    let trimmed = value.trim().to_string();
    if trimmed.is_empty() {
        vec![]
    } else {
        vec![trimmed]
    }
}
```

Then update the model_map loading section in `from_config` to use `Vec<String>` instead of `String`:

```rust
// Before (in from_config):
let key = format!("model_map.{complexity}.{agent}");
if let Ok(model) = crate::config::get(&key) {
    if !model.is_empty() {
        config
            .model_map
            .entry(complexity.clone())
            .or_default()
            .insert(agent.to_string(), model);
    }
}

// After:
let key = format!("model_map.{complexity}.{agent}");
if let Ok(model_val) = crate::config::get(&key) {
    if !model_val.is_empty() {
        let pool = Self::parse_model_pool(&model_val);
        if !pool.is_empty() {
            config
                .model_map
                .entry(complexity.clone())
                .or_default()
                .insert(agent.to_string(), pool);
        }
    }
}
```

- [ ] **Step 4: Run the new tests + full suite**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo nextest run
```

Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add src/engine/router/config.rs
git commit -m "feat(router): parse YAML arrays in model_map config loading (backward compat)"
```

---

## Task 3: Update `model_map_lookup` test

**Files:**
- Modify: `src/engine/router/mod.rs`

The existing `model_map_lookup` test calls `config.model_for_complexity(...)` which still returns `Option<String>`. Since each pool now has exactly one model (the default), the test assertions still hold. However, we must verify the test still compiles and passes with the type change.

- [ ] **Step 1: Run the existing test**

```bash
cargo nextest run --test-threads 1 -E 'test(model_map_lookup)'
```

Expected: passes (single-entry pools always return that one model). If it fails due to a type issue, fix in this step.

- [ ] **Step 2: No changes needed if test passes — skip to Task 4**

If `model_map_lookup` does fail (only possible if there's a direct HashMap access in the test body), update the assertions to call `model_for_complexity` which still returns `Option<String>`.

---

## Task 4: Update routing prompt

**Files:**
- Modify: `prompts/route.md`

Add guidance to spread load to opencode/kimi/minimax for simple and medium tasks, noting that these agents have broader model availability (especially via GitHub Copilot free tier and free models).

- [ ] **Step 1: Add agent distribution hint to route.md**

Append after the "Complexity controls model tier" section:

```markdown
Agent selection guidance:
- opencode has access to GitHub Copilot models (gpt-5, gemini-2.5-pro, claude-sonnet-4.6) AND free models (opencode/minimax-m2.5-free, opencode/nemotron-3-super-free, opencode/mimo-v2-pro-free). Prefer opencode for simple and medium tasks when available — it has the widest model variety and free fallbacks.
- kimi and minimax are effective for medium tasks and have no rate limits when configured.
- claude and codex have harder rate limits. Reserve them for complex tasks or when opencode/kimi/minimax are unavailable.
- Distribute tasks broadly across agents when multiple are available; avoid routing everything to the same agent.
```

- [ ] **Step 2: Run full test suite to confirm no regressions**

```bash
cargo nextest run
```

- [ ] **Step 3: Commit**

```bash
git add prompts/route.md
git commit -m "docs(prompts): encourage wider agent distribution in routing prompt"
```

---

## Task 5: Update `~/.orch/config.yml` with model pools

**Files:**
- Modify: `~/.orch/config.yml`

This is a local file (not in the repo). Add model pool arrays for `opencode`, `kimi`, and `minimax` under `model_map`.

> **Note:** This change is local config only — not committed to the repo. The repo ships reasonable defaults in `RouterConfig::default()`.

- [ ] **Step 1: Read the current config**

```bash
cat ~/.orch/config.yml
```

- [ ] **Step 2: Add model pool arrays**

Under `model_map`, update `opencode`, `kimi`, and `minimax` entries to arrays. Example:

```yaml
model_map:
  simple:
    opencode:
      - github-copilot/gpt-5-mini
      - github-copilot/gemini-3-flash-preview
      - opencode/minimax-m2.5-free
      - opencode/nemotron-3-super-free
    kimi: k2p5
    minimax: MiniMax-M2.5-highspeed
  medium:
    opencode:
      - github-copilot/gpt-5.4-mini
      - github-copilot/claude-sonnet-4-6
      - github-copilot/gemini-2.5-pro
      - opencode/mimo-v2-pro-free
    kimi: kimi-k2-thinking
    minimax: MiniMax-M2.7
  complex:
    opencode:
      - github-copilot/gpt-5.4
      - github-copilot/claude-opus-4-6
      - github-copilot/gpt-5.2
    kimi: kimi-k2-thinking
    minimax: MiniMax-M2.7
  review:
    opencode:
      - github-copilot/gpt-5.4
      - github-copilot/claude-sonnet-4-6
      - github-copilot/gemini-2.5-pro
    kimi: kimi-k2-thinking
    minimax: MiniMax-M2.7
```

Keep existing `claude` and `codex` entries unchanged.

- [ ] **Step 3: Restart the service to pick up config changes**

```bash
orch service restart
```

- [ ] **Step 4: Verify routing uses the new pools**

```bash
orch task list --status new | head -5
# Or trigger a test task and watch the log:
tail -f ~/.orch/state/orch.log | grep -E 'model|opencode'
```

---

## Task 6: Final verification

- [ ] **Step 1: Run full CI checks**

```bash
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
cargo nextest run
```

All must pass with zero warnings.

- [ ] **Step 2: Verify no regressions in key tests**

```bash
cargo nextest run --test-threads 1 -E 'test(model_map_lookup) | test(model_pool) | test(parse_model_pool)'
```

Expected output: all 5+ tests pass.

- [ ] **Step 3: Commit if any stragglers**

If clean, no commit needed — prior tasks already committed.
