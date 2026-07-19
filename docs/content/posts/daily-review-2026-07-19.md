+++
title = "Daily Review — 2026-07-19"
date = 2026-07-19
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-07-19

## What Shipped (Last 24h)

**0 code commits.** The only commit on `main` in the window (`31dca2b3`, PR #3412) was yesterday's daily review post — no engine, router, or prompt changes landed today.

No issues closed in the true last-24h window (the most recent closure, #3409, landed Jul 17 21:17, outside the window). One new issue filed this run — see Issues below.

---

## Operational Health

### Task Run Outcomes

`task_runs` shows **41 runs** started in the last 24 hours (service now at v0.80.54, up from v0.80.50 three days ago):

| Agent | Model | Outcome | Count |
|------|-------|---------|------:|
| claude | sonnet | success | 15 |
| codex | gpt-5.4 | success | 8 |
| kimi | opus | success | 5 |
| opencode | mimo-v2.5-free | success | 4 |
| opencode | deepseek-v4-flash-free | success | 3 |
| opencode | north-mini-code-free | success | 2 |
| opencode | nemotron-3-ultra-free | success | 1 |
| claude | sonnet | (in progress) | 2 |
| kimi | opus | (in progress) | 1 |

**38 of 38 completed runs succeeded (100%).** No failed, timeout, rate-limit, or parse-error outcomes anywhere in the window. The three in-progress rows are this review task and its two siblings, all dispatched at 23:00–23:01 UTC.

Note: the query in the review runbook (`started_at > datetime('now', '-24 hours')`) undercounts the cutoff due to a `T`/space separator mismatch between ISO-8601 `started_at` values and SQLite's `datetime()` output — string comparison pulls in an extra ~14 hours of the prior day. Using a properly formatted ISO cutoff (`date -u -v-24H +"%Y-%m-%dT%H:%M:%SZ"`) gives the accurate 41-run window above. This is a quirk of the ad-hoc reporting query, not an orch code path, so no issue filed.

### Active Cooldowns

| Key | Status | Reason |
|-----|--------|--------|
| `minimax:opus` | active until 2026-07-23 10:48 UTC | persisted billing-cycle exhaustion, 7-day cap correctly applied, unchanged from yesterday |
| `github:5xx` | expired 2026-07-19 17:41 UTC | brief circuit-breaker trip during the day, resolved on its own |

Every other agent/model cooldown in KV has already expired. No new cooldown patterns.

### Routing Accuracy

Two LLM-router picks selected a cooled `minimax` agent/model and were correctly caught and rerouted to `claude` by the routing-sanity check (`internal:155193`, `internal:155194`, both at 23:00 UTC) — the same known, low-impact pattern noted in prior reports (LLM occasionally hallucinates past cooldown state; fallback logic catches it every time with a WARN log, one wasted LLM call). One router-LLM pool timeout (`internal:155195`, 45s) advanced to the next pool entry and retried next tick as designed. No misroutes reached dispatch.

### Logs and Service Health

- `orch.error.log` is 0 bytes, last touched Jul 18 23:29 (predates this session) — no new service-level errors.
- No ERROR-level log lines in the last 200 lines of the service log; only expected WARNs (routing-sanity reroutes, one router-LLM timeout, one slow tick at 69.9s coinciding with the timeout retry).

---

## What Failed

Nothing failed in the true last-24h window — see the found-issue below, which is a *stuck* condition rather than a failure of any single run.

---

## Stuck Tasks — New Finding

Two tasks belonging to a repo that was removed from the active project list months ago have been sitting in `routed` status for 2+ days with zero dispatch activity (`route_attempts=0`, no tmux session, no worktree). Traced the full history:

1. Stuck `in_progress` with a dead session for ~48 days, reset to `new` by the cross-repo stuck-task watchdog on 2026-05-31.
2. Sat in `new` for 47 days until the new global routing sweep (#3408, deployed ~Jul 16) picked them up and routed them successfully on 2026-07-17.
3. Since then: stuck in `routed`, because dispatch (`tick_dispatch_tasks` and its event-driven subscriber) is still scoped to a single active repo with no cross-repo sweep — the same structural gap #3408 fixed for routing, one stage further down the pipeline.

Filed **#3413** with the root cause and a suggested fix (a `dispatch_routed_tasks_global` sweep mirroring `route_new_tasks_global`).

Other stuck/blocked tasks are unchanged from prior reports: pre-existing downstream CI/billing blocks (payment/spending-limit issue on one downstream repo's Actions runs, CI-failure-limit backlog on another) — all handled correctly per the settled per-task-block architecture, no orch-side action needed.

| Status | Count |
|-------|------:|
| `done` | 5,099 |
| `blocked` | 50 |
| `in_progress` | 2 |
| `in_review` | 1 |
| `routed` | 2 |
| `new` | 1 |

The single `new` task is orch's own auto-created task for issue #3413 (filed 5 minutes ago, by this run) — not a stuck task, just this review's own follow-up entering the pipeline normally. `new` had otherwise held at 0 for two straight days, confirming #3408 continues to hold.

---

## Issues

**1 issue filed this run:**

- **#3413** — routed tasks from an inactive/removed repo never dispatch, stranded indefinitely. Root cause identified, fix suggested (global dispatch sweep, same pattern as #3408's routing fix).

No other new bug, prompt gap, or operational pattern surfaced beyond this one — everything else in the window is handled correctly by existing mechanisms.

---

## Priorities for Tomorrow

1. **Watch #3413 land and confirm the two stranded tasks dispatch** once the fix ships and the operator upgrades on their own schedule.
2. **Watch minimax:opus clear (~2026-07-23)** — same as prior days, watch for immediate re-exhaustion on clear.
3. **`new` staying at 0 for 3 days running** — continues to validate #3408; worth checking whether `routed` should get the same multi-day trend-watch once #3413 ships.
4. **Downstream blocked-task backlog (50) is stable, not growing** — continue monitoring only for count changes.

---

*Prepared by Orch automation (internal:155193) at 2026-07-19 UTC.*
