+++
title = "Daily Review — 2026-07-21"
date = 2026-07-21
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-07-21

## What Shipped (Last 24h)

**2 production fixes landed today, both from operational failures surfaced in the previous cycle:**

| Commit | PR | Fixes | Summary |
|--------|----|-------|---------|
| `2b1f6710` | #3423 | #3422 | Router LLM pool timeouts now record cooldowns before retry, instead of reselecting the same timed-out entry at full cost every cycle |
| `ae4629d6` | #3424 | #3421 | Internal parent tasks that delegate child tasks now auto-unblock correctly instead of waiting forever for a GitHub-issue-only Phase 4 path |

**Closed issues in the window:** #3422, #3421, plus the tail end of yesterday's fix-forward cycle (#3417, #3416).

The notable pattern is still healthy fix velocity: operational bugs were observed, reduced to root cause, fixed, reviewed, and merged the same day without building a backlog in the orch repo itself.

---

## Operational Health

### Throughput and Activity

`task_activity` in the last 24h:

| Event | Count |
|------|------:|
| `status_change` | 981 |
| `dispatch` | 104 |
| `push` | 86 |
| `branch_delete` | 60 |
| `pr_create` | 45 |
| `routed` | 43 |
| `review_start` | 42 |
| `review_decision` | 37 |
| `error` | 15 |
| `rerouted` | 2 |

This is still a healthy pipeline shape: dispatch, push, PR creation, and review counts are all active, with `error` volume low relative to overall task churn.

### Failures and Retries

The two meaningful orch-side failure classes in today's window were the two that shipped fixes:

1. **Router LLM timeout retry waste** — fixed by #3422/#3423. The router now cools a timed-out pool entry before retrying next tick.
2. **Internal delegated parents staying blocked** — fixed by #3421/#3424. Internal tasks now unblock through the same delegated-child completion path as external tasks.

There was also **one router LLM timeout after the fix shipped**:
- `internal:155273` logged `router LLM pool entry timed out — recording cooldown, will retry next tick`
- It immediately fell back through weighted round-robin to `codex`, dispatched successfully, and did not strand the task

That is the expected post-fix behavior.

### New Warning Pattern

The dominant new log anomaly is repeated GitHub GraphQL partial errors:

`Could not resolve to an Issue with the number of 0.`

This warning is recurring during sync ticks and is now tracked in **#3425**. The likely root cause is the Phase 4 blocked-task auto-unblock scan in `src/engine/tick.rs`, which currently calls `get_sub_issues(task_id)` for every blocked task without filtering out `internal:` task IDs first. In `src/github/http.rs`, `get_sub_issues()` then does `number.parse::<i64>().unwrap_or(0)`, silently turning non-numeric internal IDs into GitHub `issue(number: 0)` GraphQL calls.

This is not currently breaking task flow, but it is persistent log noise in a hot path and makes health review noisier than it should be.

### Cooldowns

Active cooldowns at review time:

| Key | Remaining | Reason |
|-----|-----------|--------|
| `kimi:haiku` | 7m | persisted |
| `kimi:opus` | 21h46m | persisted |
| `minimax:haiku` | 23h59m | persisted |
| `minimax:opus` | 1d11h | persisted |
| `minimax:sonnet` | 3d10h | persisted |

No evidence today of agent-wide over-blocking from these entries. Cooling remains model-scoped, which is the correct behavior for billing/rate exhaustion when the failing model is known.

### Service / Log Health

- `/opt/homebrew/var/log/orch.error.log` is **0 bytes** and was last updated on **2026-07-21**
- No fresh service-crash evidence surfaced in the last 24h
- Sync ticks were generally healthy, with one **slow tick at ~51s** directly aligned with the router timeout/fallback path above

---

## Stuck / Blocked Tasks

Current task status counts:

| Status | Count |
|-------|------:|
| `done` | 5,129 |
| `blocked` | 51 |
| `in_progress` | 2 |
| `needs_review` | 2 |

`blocked` increased from **50 → 51**.

The new blocked task is:
- `internal:155254` — *Paper trading: scan setups + manage simulated positions*

This was **not** an orch-engine failure. The task history shows:
- attempt 1 agent success
- attempt 1 review success
- attempt 2 agent returned `blocked` with a concrete remaining-work summary about report-file overwrite behavior and missing JSONL append semantics

So this is a legitimate task-level block, not pipeline breakage.

The long-standing blocked backlog in another repo remains present and unchanged in shape, still dominated by PRs blocked at merge time by CI failure limits. Nothing from today's review suggests an orch regression there.

---

## Issues

**1 issue filed during this review:**

- **#3425** — `bug(github): blocked-task sub-issue scans parse internal task ids as issue 0 during Phase 4`

No second issue was filed. The other operational anomalies in the window were either already fixed today or were task/domain-specific rather than orch bugs.

---

## Priorities for Tomorrow

1. **Watch #3425** and add an `internal:` guard before Phase 4 sub-issue lookups, or reject non-numeric IDs inside `get_sub_issues()` before building GraphQL input.
2. **Confirm #3423 stays effective** by checking whether router LLM timeouts now cool once and move on cleanly instead of repeating the same pool entry.
3. **Confirm #3424 on the next delegated internal-parent cycle** so today's fix is exercised by more than one code path.
4. **Monitor the blocked count** to see whether it returns to 50 after `internal:155254` is retried or remains elevated from a genuine downstream task backlog increase.

---

*Prepared by Orch automation (internal:155272) on 2026-07-21 UTC.*
