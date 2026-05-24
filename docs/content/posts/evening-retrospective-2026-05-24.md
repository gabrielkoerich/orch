+++
title = "Evening Retrospective — 2026-05-24"
date = 2026-05-24
description = "Daily evening retrospective and operational summary."
+++

# Evening Retrospective — 2026-05-24

## Summary

Today was a significant cleanup day followed by two targeted bug fixes. Two major refactors landed earlier: complete removal of all budget tracking features and a jobs system refactor. Late in the day, two additional bug fixes addressed production failures: cascading router timeouts stalling the tick loop for 2+ minutes, and a codex 0.133.0 API breakage (`--ask-for-approval` flag removed).

## What Happened Today

**Commits (since last retrospective):**

| Commit | Description |
|--------|-------------|
| `e59a1dda` | fix(codex): replace removed --ask-for-approval with -c approval_policy= for codex 0.133.0 (#3190) |
| `ec66fb1e` | fix(router): stop cascading timeouts within a single tick (#3189) |
| `dcce8bce` | build(deps): bump openssl in the cargo group (#3183) |
| `d4b1e74e` | refactor(jobs): load jobs from prompts/jobs/*.md files (#3182) |
| `eb564ceb` | refactor: remove all budget tracking features (#3181) |

**Closed issues (today):**
- #3188 — codex 0.133.0 rejects `--ask-for-approval` — fixed in #3190
- #3187 — per-entry router timeout cascade stalls tick loop for minutes — fixed in #3189
- #3176 — retryable-blocked classifier broadened (closed yesterday, confirmed working)
- #3175 — codex index.lock regression (closed)
- #3169 — unavailable opencode models still selected after warning-only validation (closed)

## What Was Accomplished

### Codex 0.133.0 compatibility fix (#3190)

Codex CLI removed the `--ask-for-approval` flag in version 0.133.0. The runner was still generating commands with this flag, causing all codex dispatch to fail immediately. Fixed by replacing the flag with the config-based equivalent:
- Autonomous mode: `--sandbox workspace-write -c 'approval_policy="never"'`
- Supervised mode: `-c 'approval_policy="on-request"' --sandbox {sandbox}`
- Full access: `--dangerously-bypass-approvals-and-sandbox` (unchanged)

All runner tests updated to assert the new flag shape with negative assertions guarding against regression.

### Router timeout cascade fix (#3189)

Production logs showed the tick loop stalling 70s, 130s, and 170s — multiples of `router.timeout_seconds`. When a pool entry timed out, the router was continuing to try additional entries in the same tick, accumulating N × timeout_seconds of wall-clock stall per iteration. Fixed by immediately advancing the pool index and returning an error on timeout, so the next tick starts at the next pool entry. Architecture preserved: no new semaphore, no new knob, `max_tasks_per_tick=1` still governs concurrency. WATCHDOG stall alerts should cease.

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

- WATCHDOG tick stalls (70s–170s) identified as router timeout cascade — **now fixed** in #3189. No more multi-minute stalls expected.
- Codex dispatch failures from 0.133.0 API breakage — **now fixed** in #3190.

## Routing & Agent Health

- Budget tracking removal eliminates a class of routing degradation observed previously (#3167: LLM routing budget fallback recurring throughout day).
- Router timeout cascade fix (#3189) directly addresses WATCHDOG stall alerts that were occurring throughout the day.
- Core agents (claude, codex, opencode) remain healthy.
- Codex dispatch was broken by 0.133.0 API change; fix landed same day.
- Opencode stale model WARN noise should decrease now that unavailable models are pruned at config load (#3169 fix).

## Priorities For Tomorrow's Morning Review

1. Verify WATCHDOG stall alerts have ceased after the router timeout cascade fix (#3189).
2. Confirm codex dispatch is healthy with the new `approval_policy` flag shape (#3190).
3. Monitor LLM routing stability through the full day — verify budget exhaustion fallback to round-robin no longer occurs.
4. Operator triage: `internal:149337` SSH agent signing failure — restart SSH agent and re-add keys.
5. Operator: prune stale opencode model entries (`github-copilot/gpt-5.3`, `github-copilot/claude-opus-4.6`) from config to eliminate WARN noise.

---

Prepared by Orch automation (internal:150277).
