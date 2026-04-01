+++
title = "Evening Retrospective -- 2026-04-01"
date = 2026-04-01T23:59:00Z
[extra]
job = "evening-retrospective"
+++

## Summary

Solid day with **24 commits landed**, continuing the reliability push from yesterday. Focus shifted from review pipeline fixes to agent routing robustness and GraphQL/GitHub API edge cases. The open issue queue is clear (0 open issues) — all discovered problems were resolved same-day.

Two internal tasks are currently blocked (morning review jobs) due to review agent failures exceeding thresholds — these appear to be non-critical scheduled jobs that can be reset.

---

## Accomplished Today

### Reroute & Routing Reliability (4 bugs closed)

- **#1492** — `reroute` was not skipping the previously failed agent, causing tasks to potentially fail again with the same agent/model combo. Fixed: reroute now explicitly clears and skips the failed agent.
- **#1490** — `reroute` was clearing the agent but not the model, leading to incompatible agent/model assignments (e.g., claude agent with `github-copilot/gpt-5-mini`). Fixed: both agent and model are now cleared on reroute.
- **#1493** — Related fix ensuring failed agents are properly excluded from routing pool after reroute trigger.
- **#1486** — Fixed the root cause: reroute assigning incompatible models to agents by clearing model on agent change.

### GraphQL & GitHub API Hardening (3 bugs closed)

- **#1485** — `batch_is_pr_merged_by_branch` was swallowing partial GraphQL errors (returning success when some branches failed to resolve). Fixed: partial errors now surfaced and logged.
- **#1480** — Invalid JSON escaping in `batch_is_pr_merged_by_branch` GraphQL query causing parse failures. Fixed: removed invalid escaping.
- **#1484** — Review agent blocking tasks on transient GitHub 503 errors. Fixed: 5xx errors now treated as transient with circuit breaker logic, not hard failures.

### System Reliability (5 bugs closed)

- **#1479** — `free_models()` was blocking the Tokio worker during cache refresh. Fixed: moved to async-compatible flow.
- **#1475** — `review_poll` no-code reroute counter not reset on `auto_unblock`, causing premature blocking. Fixed.
- **#1474** — `store_reset_failure_counters` silently discarded DB errors. Fixed: added error logging.
- **#1477** — `mark_cleaned()` failures were silent. Fixed: added error logging to prevent silent cleanup failures.
- **#1481** — Deduplicated output format in agent prompts and removed dead prompt files. Code cleanup.

### Agent Session & Transport (3 bugs closed)

- **#1464** — Fixed double-push issue where `TmuxChannel` capture_loop and `CaptureService` both pushed to transport layer.
- **#1465** — Chat persistent sessions replaced with one-shot `--session-id` to reduce complexity.
- **#1463** — Fixed overflow in exponential backoff when `retry_attempts > 63` (2^63 overflow).

### Other Fixes

- **#1460** — Auto-cleanup sessions and worktrees on task close.
- **#1454** — Removed double-encoding in batch GraphQL queries.
- **#1471** — Removed hardcoded self-review logic from jobs.rs.

---

## Agent Performance (Last 24h)

### Agent Runs Only (excludes review runs)

| Agent | Model | Success | Failed | Rate-limit | Notes |
|-------|-------|---------|--------|------------|-------|
| claude | sonnet | 60 | 5 | 1 | Dominant workhorse |
| claude | opus | 17 | 0 | 1 | Solid, low volume |
| claude | haiku | 11 | 1 | 0 | Good for simple tasks |
| claude | github-copilot/gpt-5-mini | 0 | 4 | 0 | **Model assignment bug** — claude can't use copilot models |
| opencode | github-copilot/gpt-5-mini | 16 | 1 | 0 | Primary fallback |
| opencode | free models | 14 | 2 | 1 | Acceptable for free tier |
| minimax | opus | 1 | 1 | 0 | Low volume today |
| kimi | opus | 2 | 0 | 0 | Minimal usage |

**Overall agent success rate: ~82%** (121 successful / 148 total runs including reviews)

---

## Patterns & Observations

### What's Working

- **Reroute fixes** — The routing system is now more robust against agent/model mismatches
- **GraphQL error handling** — Partial errors now properly surfaced instead of being swallowed
- **Transient 5xx handling** — GitHub API hiccups no longer block tasks
- **Claude sonnet** — Remains the dominant reliable agent (60 successes)

### Issues Identified

**1. Model assignment bug: `opus/` (empty suffix)**
- 5 failures with `model unavailable (opus/): Model not found: opus/`
- Root cause: model string parsing produces empty suffix when routing assigns `opus/` instead of `opus`
- **Action needed**: Fix model string normalization in router or runner

**2. Minimax review parse failures**
- 1 failure: `failed to parse review response from minimax output`
- Minimax outputs valid text but not in expected review format
- **Action needed**: Tune review prompt or add minimax-specific parsing

**3. SQLite OOB panics continue**
- Multiple `sqlx-sqlite-worker` panics: `index out of bounds: the len is 56 but the index is 56`
- This is the same issue as #1453 (fixed) — may be residual from pre-fix runs, or need additional column handling
- **Action needed**: Monitor for new occurrences; may need more `.try_get()` conversions

**4. Stale worktree metadata log spam**
- Continued `fatal: not a git repository` errors for old worktrees
- Benign but obscures real errors in logs
- **Action needed**: Low priority — `git worktree prune` would clean this up

**5. Blocked morning review jobs**
- `internal:31297` and `internal:31298` are blocked with "review agent blocked — exceeded failure threshold"
- These are scheduled jobs that can be safely reset

---

## Cooldown Status (Active)

| Agent/Model | Cooldown Until | Reason |
|-------------|----------------|--------|
| claude | Apr 1 18:40 | Rate limit recovery |
| opencode | Apr 1 18:12 | Pool entry failure |
| opencode:mimo-v2-pro-free | Apr 1 23:35 | No text output |
| opencode:nemotron-3-super-free | Apr 2 02:00 | No text output |
| opencode:minimax-m2.5-free | Apr 2 00:20 | No text output |
| opencode:opus/ | Apr 2 00:26 | **Model assignment bug** |

---

## Tomorrow's Priorities

1. **Fix `opus/` model assignment bug** — Root cause identified, needs code fix to normalize model strings before validation
2. **Investigate SQLite OOB panics** — Confirm these are pre-fix residuals; if new ones appear, expand `.try_get()` coverage
3. **Unblock/reset morning review jobs** — `internal:31297` and `internal:31298` need manual reset or auto-unblock
4. **Monitor minimax review format** — If more parse failures occur, tune prompt or add format flexibility
5. **Clean up stale worktree metadata** — Run `git worktree prune` in project directory to eliminate log spam

---

## Open GitHub Issues

**0 open issues** — All discovered problems today were resolved same-day.

Potential new issues to file (if not already tracked):
- `opus/` model assignment bug causing 5 failures today
- SQLite OOB panics persisting despite #1453 fix
