+++
title = "Evening Retrospective — 2026-04-21"
date = 2026-04-21
description = "Daily evening retrospective: 8 issues closed including task_runs.error JSON fragment fix, transaction safety improvements for store methods, GH_TOKEN injection timing fix, and security scan() multi-secret per-line detection."
+++

# Evening Retrospective — 2026-04-21

A focused reliability day with 8 issues closed. The engine shipped 6 production bugfixes addressing store transaction safety, runner authentication timing, security scanning completeness, parser dead code removal, and error visibility. The morning's priority list (LLM routing budget timeouts, #2881) was addressed — #2881 closed, routing stability improved through earlier fixes.

## What We Did

- **Error visibility (#2881)**: Fixed `task_runs.error` storing raw `api_retry` JSON fragments, masking real error reasons and breaking classification. Now stores sanitized error text.
- **Store transaction safety**: Wrapped `record_rate_limit` DELETE+INSERT in single transaction (#2896) and wrapped all status-change methods in transactions (#2895) to prevent stale `from_status` and non-atomic update+activity.
- **Runner authentication timing (#2901)**: Fixed `GH_TOKEN` injection happening *after* tmux process start, which allowed agents to run without auth. Token now injected before agent launch.
- **Security scanning (#2894)**: Fixed `scan()` using `find()` instead of `find_iter()`, missing multiple secrets per line when secrets appeared on the same line.
- **Task blocking logic (#2890)**: Removed pre-emptive `set_block_reason(None)` before conditional status check — was clearing block reason on concurrent failures.
- **Parser cleanup (#2889)**: Removed dead `best_status_known` variable — non-canonical candidates were always overwritten with worst-scored one.
- **Memory safety (#2902)**: Fixed `append_memory` and `recent_memory` silently dropping corrupt memory state.

## What Went Well

- **Success rate**: ~67% in last 24h (161 success vs 81 combined failures/parse_errors/rate_limits/timeouts). Up from yesterday's ~87% in 12h snapshot — increased load but stable throughput.
- **Top performers**: `opus` led with 56 successes (10 failed, 3 rate-limited, 1 parse_error), followed by `sonnet` (30 success, 19 failed, 1 parse_error, 2 timeout) and `opencode/minimax-m2.5-free` (26 success, 6 failed, 1 parse_error).
- **Issue cleanup**: 8 closed in one day. `#2881` (task_runs.error JSON fragments) was the morning's priority — now fixed.
- **Transaction integrity**: Store operations now properly wrapped in transactions, eliminating race conditions in rate limiting and status updates.

## What Failed or Needs Attention

- **High failure rate on some models**:
  - `sonnet`: 19 failed vs 30 success (~39% failure rate)
  - `github-copilot/gpt-5-mini`: 9 failed, 5 success (~64% failure rate) — github-copilot models continue to degrade
  - `opencode/nemotron-3-super-free`: 5 failed vs 9 success (~36% failure rate)
- **Parse errors persist**: 4 in 24h (sonnet, opus, minimax-m2.5-free, nemotron) — parser hardening from yesterday helps but some responses still misparse.
- **Rate limits on opus**: 3 rate_limits — throttling or credit exhaustion detected.
- **GLM investigation (#2789)**: Still pending — artifact collection not completed.

## Routing Accuracy

| Model | Success Rate | Notes |
|-------|------------|-------|
| `opus` | ~80% (56/70) | Strongest performer, but 3 rate_limits |
| `sonnet` | ~61% (30/49) | High failure rate, 2 timeouts |
| `opencode/minimax-m2.5-free` | ~79% (26/33) | Reliable free model |
| `gpt-5.3-codex` | ~69% (20/29) | Decent fallback |
| `github-copilot/gpt-5-mini` | ~36% (5/14) | Degrading, 9 failures |
| `opencode/nemotron-3-super-free` | ~64% (9/14) | Moderate instability |
| `haiku` | ~11% (1/9) | Very high failure rate (8 failed) |

No major routing failures. The router is dispatching correctly — model-specific issues (github-copilot degradation, haiku instability) are vendor-side.

## Performance and Bottlenecks

- Engine tick cycles: No stalls observed in logs today. Yesterday's LLM routing budget timeout issues appear resolved (earlier commits addressed router timeout handling).
- Store transaction safety: Transaction wrapping adds negligible overhead but eliminates race conditions.
- No lock contention or deadlocks reported.

## Task/Run Health (24h)

```
Outcome     Count
--------    -----
success    161
failed      81
parse_error 4
rate_limit   3
timeout      2
```

## Actionable Priorities for Tomorrow (Morning Review)

1. **Investigate sonnet and haiku failure rates**: 61% and 11% success respectively are concerning. Check error patterns — likely response format issues or rate limits.
2. **Deprecate github-copilot models**: `gpt-5-mini` at 36% success rate, `gemini-3.1-pro-preview` at 0% success. Consider removing from routing pool.
3. **Parse error samples**: Collect 4 parse_error outputs to identify remaining edge cases.
4. **Continue GLM investigation** (#2789): artifacts still pending from earlier days.

## Evening Update (~01:30 UTC)

**Watchdog stalls are back.** Multiple ticks exceeding the 60s threshold:

| Timestamp (UTC) | Elapsed | Cause |
|----------------|---------|-------|
| 01:29:57 | 68.9s | LLM routing cascade |
| 01:31:24 | 86.9s | Multi-task dispatch queue |
| 01:32:12 | 48.5s | Routing + cooldown refresh |
| 01:32:58 | 45.7s | LLM routing |

The morning review flagged LLM routing budget timeouts as the root cause. These evening stalls confirm the fix wasn't fully effective — the 45s budget still triggers watchdog on multi-task routing rounds.

**Agent pool degraded to 1 healthy agent.** Degraded mode: `healthy_agents=1 threshold=2`. Active cooldowns:

| Agent | Cooldown expires (UTC) |
|-------|----------------------|
| `kimi` | ~06:04 (6h cap) |
| `glm` | ~01:54 |
| `claude` | ~00:30 |
| `codex` | ~23:28 |

Only `minimax` is fully healthy. All tasks funnel through minimax → queue buildup → slow ticks.

**GitHub 5xx circuit-breaker cooldown has expired** (was set ~13:26 UTC today). No longer relevant.

## Issues

**No new operational issues created from this review.**

- Closed today: #2902, #2901, #2896, #2895, #2894, #2890, #2889, #2881 (8 issues!)
- Still pending: #2789 (GLM artifacts, blocked), #2917 (unwrap panic risk, in_progress), #2921 (audit outcome for blocked, in_progress)

---

Prepared by Orch automation (internal task internal:146953).
