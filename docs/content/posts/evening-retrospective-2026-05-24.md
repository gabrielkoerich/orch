+++
title = "Evening Retrospective — 2026-05-24"
date = 2026-05-24
description = "Daily evening retrospective and operational summary."
+++

# Evening Retrospective — 2026-05-24

## Summary

Today was a significant cleanup day. Two major refactors landed: complete removal of all budget tracking features (per-task token budgets, wall-clock route wrappers, budget_warning/budget_exceeded columns) and a jobs system refactor that moves inline job definitions out of `.orch.yml` into discoverable markdown files under `prompts/jobs/`. Both are structural improvements that reduce configuration complexity and remove code that was actively causing failures.

## What Happened Today

**Commits (since last retrospective):**

| Commit | Description |
|--------|-------------|
| `d4b1e74e` | refactor(jobs): load jobs from prompts/jobs/*.md files (#3182) |
| `eb564ceb` | refactor: remove all budget tracking features (#3181) |
| `756550ff` | Daily morning review (#3180) |

**Closed issues (last 24h):**
- #3176 — retryable-blocked classifier broadened (closed yesterday, confirmed working)
- #3175 — codex index.lock regression (closed)
- #3169 — unavailable opencode models still selected after warning-only validation (closed)

No new bugs filed today; the two refactors were proactive improvements based on patterns that had caused recurring failures.

## What Was Accomplished

### Budget tracking removal (#3181)
Dropped the entire token-budget subsystem that had been generating false signals and causing routing fallbacks throughout the day. Specifically removed:
- `tasks.budget_warning` and `tasks.budget_exceeded` columns (migration 027)
- `check_token_budget()` pre-run guard in the runner
- `TokenBudgetExceeded` failure category in sync
- `BudgetCheckOutcome` enum and runner branches
- `router.llm_budget_secs` + `llm_bypass_*` knobs and counters
- `tokio::time::timeout` wall-clock wrapper around the route cascade
- Budget warning PR comments

This directly addressed the recurring pattern where the router LLM budget was being exhausted by mid-day, forcing fallback to round-robin routing. The system already has `router.timeout_seconds` per-call and `max_tasks_per_tick` for concurrency — the budget layer was redundant and harmful.

### Jobs system refactor (#3182)
Moved inline job definitions from `.orch.yml` into markdown files under `prompts/jobs/`. `load_jobs()` now merges inline definitions with file-discovered jobs, rejecting duplicate IDs. This makes job definitions more maintainable, discoverable, and editable without touching config files. Also serialized tests that share global cooldown state to eliminate test flakiness, adding `reset_global_state()` helpers in cooldown and opencode runner modules.

## Failures, Retries, and Ongoing Issues

- **Environment blockers (no change):**
  - `internal:149337` SSH agent signing failure during pushes — operator action required
  - `#3110` Claude 401 Invalid authentication credentials — ongoing, owner action required

- No new systemic routing regressions observed. Budget removal is expected to improve LLM routing stability throughout the day (previously budget exhaustion triggered round-robin fallback).

## Routing & Agent Health

- Budget tracking removal eliminates a class of routing degradation observed previously (#3167: LLM routing budget fallback recurring throughout day).
- Core agents (claude, codex, opencode) remain healthy.
- Opencode stale model WARN noise should decrease now that unavailable models are pruned at config load (#3169 fix, from yesterday).

## Priorities For Tomorrow's Morning Review

1. Monitor LLM routing stability through the full day — verify that budget exhaustion fallback to round-robin no longer occurs.
2. Confirm job loading from `prompts/jobs/*.md` is working correctly in production (new job files should be picked up without config changes).
3. Operator triage: `internal:149337` SSH agent signing failure — if persists, restart SSH agent and re-add keys.
4. Monitor whether any tests flake due to global state — the serialization fix in #3182 addresses known cases but watch for others.

---

Prepared by Orch automation (internal:150193).
