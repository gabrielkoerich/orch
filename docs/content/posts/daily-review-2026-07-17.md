+++
title = "Daily Review — 2026-07-17"
date = 2026-07-17
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-07-17

## What Shipped (Last 24h)

**1 commit** landed on `main`:

| Commit | PR | Summary |
|--------|----|---------|
| `8aad8e12` | #3410 | fix(router): preserve opencode discovery cache on empty refresh — closes #3409 |

The fix prevents a transient `opencode models` CLI failure from overwriting the free-model discovery cache with an empty list. Previously a single failed refresh poisoned the cache for the full 1h TTL, causing opencode to be falsely marked "all models cooled" for ~60 minutes at a time (observed 4x in the prior 30h, ~3-4h/day of lost routing capacity). The cache update now preserves the prior non-empty list when a refresh comes back empty, with regression tests covering both the free-model and all-models caches.

A second fix landed just outside the strict 24h window but is worth closing the loop on since it wasn't covered in a prior report: `1d4b2182` (#3408, merged 2026-07-16) added a global routing sweep so `new` tasks from inactive/removed repos get routed even when their project isn't in the active tick loop. Two tasks had been stuck in `new` for 46+ days before this landed — see Stuck Tasks below for the result.

---

## Operational Health

### Throughput

`task_activity` over the last 24 hours:

| Event | Count |
|------|------:|
| `status_change` | 143 |
| `dispatch` | 57 |
| `push` | 48 |
| `branch_delete` | 30 |
| `pr_create` | 28 |
| `routed` | 26 |
| `review_start` | 19 |
| `review_decision` | 19 |

**Zero `error` or `rerouted` events in the last 24 hours** — the first fully clean window in recent reports.

### Task Run Outcomes

`task_runs` shows **50 runs** over the last 24 hours:

| Agent | Model | Outcome | Count |
|------|-------|---------|------:|
| claude | sonnet | success | 20 |
| codex | gpt-5.4 | success | 11 |
| kimi | opus | success | 7 |
| opencode | mimo-v2.5-free | success | 5 |
| opencode | deepseek-v4-flash-free | success | 4 |
| opencode | nemotron-3-ultra-free | success | 1 |
| claude | sonnet | (in progress) | 2 |

**48 of 48 completed runs succeeded (100%).** No failures, rate limits, silence detections, or parse errors in the window. The two in-progress rows are this task and the internal evening-retrospective task, both dispatched at 23:00 UTC.

### Active Cooldowns

| Key | Remaining | Reason |
|-----|-----------|--------|
| `minimax:opus` | ~5d11h | persisted billing-cycle exhaustion, 7-day cap correctly applied |

Only one active cooldown, and it's the expected persistent one from prior billing-cycle exhaustion. No new cooldowns triggered today.

### Routing Accuracy

LLM router again selected `minimax` for both internal tasks dispatched at 23:00 UTC (this review and the evening retrospective) despite `minimax:opus` being in an active 5+ day cooldown — same known pattern as recent days. Both correctly and immediately rerouted to `claude/sonnet`. Fallback works as designed; the LLM occasionally picks a cooled agent when it doesn't have live cooldown state, but the router's pre-emptive check catches it before dispatch every time. Not filing — this is the same behavior noted in the last several reports and doesn't cause any dropped work.

### Logs and Service Health

- `orch.error.log` is **0 bytes** — no service-level errors
- Sync ticks stayed in the healthy **1.4s–3.1s** range throughout the window, no watchdog alerts
- One transient GitHub connectivity gap around 22:19–22:38 UTC (HTTP send failures to `api.github.com`, 5xx circuit breaker opening/closing repeatedly with `attempt=12` through `attempt=19`) — resolved on its own once connectivity returned; polling fallback and circuit breaker behaved exactly as designed, no tasks lost or stuck as a result

---

## What Failed

Nothing. Zero failed, rate-limited, timed-out, or parse-error task runs in the last 24 hours.

---

## Stuck Tasks

| Status | Count |
|-------|------:|
| `done` | 5,068 |
| `blocked` | 50 |
| `in_progress` | 2 |
| `routed` | 2 |
| `new` | 0 |

**`new` count is now 0** — down from 2 tasks that had been stuck for 46+ days (closed by #3407/#3408's global routing sweep, which merged 2026-07-16). Confirms the fix took effect: no orphaned tasks from inactive repos remain.

The 50 `blocked` tasks are all downstream CI-related auto-merge blocks (per settled architecture — blocking is scoped to the single task at merge time, not the whole repo). No new blocked tasks appeared in the last 24h; this count is unchanged from recent reports.

For the orch project itself: 1,966 `done`, 1 `in_progress` (this task), 0 `blocked`, 0 `new`.

---

## Issues

**0 open issues** in `gabrielkoerich/orch` — the queue is fully drained. #3407 and #3409 (both closed in the last ~26h) were the last two open items, and both fixes are now merged to `main`.

No new issues filed this run — nothing in today's operational data indicates a bug or gap not already handled by an existing mechanism. The GitHub connectivity blip resolved on its own via existing polling-fallback/circuit-breaker logic, and the one routing quirk (LLM occasionally selecting a cooled agent) is already covered by the pre-emptive routability check and doesn't need a new report.

---

## Priorities for Tomorrow

1. **Confirm the opencode cache fix holds**
   Watch for any further "all models cooled" degraded windows for opencode with `rate_limit_count=0` — the #3409 fix should eliminate them going forward. If one recurs after the fix is live, it points to a different code path than the one just patched.

2. **Watch minimax:opus clear**
   ~5d11h remaining, expected clear around 2026-07-23. Watch for immediate re-exhaustion on clear, which would suggest the billing cycle itself (not just the cooldown math) needs a longer view.

3. **Keep an eye on the `new`-task count staying at 0**
   The global routing sweep (#3408) is new as of yesterday; a first quiet 24h with `new` at 0 is a good sign but one data point isn't a trend yet.

4. **Downstream blocked-task backlog (50) is stable, not growing**
   No orch action needed — this is CI/merge-side debt in downstream repos, already handled per-task at merge time. Continue monitoring only for growth, not investigating further unless the count moves.

---

*Prepared by Orch automation (internal:155153) at 2026-07-17 UTC.*
