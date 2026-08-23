+++
title = "Daily Review — 2026-08-23"
date = 2026-08-23
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-08-23

## The headline: two watchdog/sync fixes shipped, and a real 2x over-count bug was found in this review's own monitoring queries

**Window:** `2026-08-22T23:02Z → 2026-08-23T23:05Z`. Two commits landed, both closing same-day-filed issues: `4ae538bc` (#3544, watchdog false-stall race) and `6a49923c` (#3545, startup issue-ingest rescan capped at 24h). While diagnosing routine task-run stats for this review, a genuine bug turned up in the SQL this repo's own daily-review runbook recommends: **`WHERE started_at > datetime('now', '-24 hours')`-style queries systematically over-count by up to ~2x** because stored RFC3339 timestamps (`T`-separated) don't string-compare correctly against SQLite's `datetime('now', ...)` output (space-separated). Filed as **#3548**. All numbers below use the corrected (`julianday()`-based) window.

---

## What Shipped (Last 24h)

| Commit | Issue | Summary |
|--------|-------|---------|
| `4ae538bc` | [#3544](https://github.com/gabrielkoerich/orch/issues/3544) | Give the tick watchdog its own `SuspendDetector` instead of racing the main loop's `checkpoint()` for the shared suspend-gap log — closes the window where 9 false `WATCHDOG: possible stall` ERRORs fired between `2026-08-22T03:08Z` and `08:21Z`, each immediately contradicted by an INFO suspend/resume line seconds later. |
| `6a49923c` | [#3545](https://github.com/gabrielkoerich/orch/issues/3545) | `clear_issues_last_ingested()` used to *delete* the ingest cursor on startup, which made the next rescan fall back to the same 24h window as routine incremental ticks — any open issue whose `updated_at` already lagged more than 24h (no comments/labels since filing) stayed permanently invisible across every restart. Now writes a sentinel that resolves to an unbounded fetch of all open issues on the next ingest. |

**Closed today:** #3544, #3545 (both filed and fixed same-window).

**Still open, unchanged from yesterday:**
- **#3535** — opencode `"not available in your country"` misclassification. Fix already committed (`ae8146bb`); PR #3536 still blocked on the same stale-branch CI issue as #3538, not on review.
- **#3453** — pending-CI-status prose causing review parse errors, 25 days old. Root cause of *why it has no task* was diagnosed and fixed today (#3545/`6a49923c`) — the fix requires the next full ingest rescan to pick it up; not evaluated further here per this repo's version/deployment policy.

---

## Operational Health

### Task Run Outcomes (last 24h, corrected window)

| Agent | Model | Outcome | Count |
|------|-------|---------|------:|
| claude | `sonnet` | `success` | 20 |
| opencode | `mimo-v2.5-free` | `success` | 4 |
| claude | `sonnet` | `failed` | 3 |
| kimi | `opus` | `success` | 3 |
| opencode | `x-preview-f-free` | `success` | 3 |
| claude | `sonnet` | (null / recovery) | 2 |
| kimi | `opus` | `rate_limit` | 2 |
| opencode | `nemotron-3-ultra-free` | `success` | 2 |
| opencode | `nemotron-3.5-lightning-free` | `success` | 2 |
| opencode | `muse-spark-1.2-contributor-free` | `success` | 1 |

All 9 `error`-type `task_activity` rows in the actual last-24h window trace to two already-understood, self-healing patterns — no new failure shape:

- **`claude sonnet failed` x3** (`internal:157183`, `157181`, `157178`) — `silence detection set task to new` following `stuck-task recovery: no session found`. Designed generic recovery path; all three tasks are `done`.
- **`kimi opus rate_limit` x2** (`internal:157181` review run, `internal:157174` agent run) — both messages are Kimi's `"You've reached your usage limit for this billing cycle"` text. The stored `outcome` column still says `rate_limit` (expected — see the regression test `classify_failure_kimi_billing_cycle_is_credit_exhausted_not_rate_limit` added for #3529), but `classify_failure()` correctly re-derives `CreditExhausted` from the message content at the point cooldown/retry decisions are made. Not a regression of #3529 — confirmed by reading the fix and its test, not just the raw outcome string.
- **`opencode x-preview-f-free`** — one `stuck-task recovery: internal in_review session killed` (`internal:157167`), self-healed, `done`.

### `task_activity` (last 24h, corrected window)

`status_change` 125, `dispatch` 48, `branch_delete` 40, `push` 37, `routed` 23, `review_decision` 18, `review_start` 17, `pr_create` 17, `error` 9, `rerouted` 3. All accounted for by the patterns above.

### `orch.error.log`

Not evaluated — empty (0 bytes), consistent with no crash since last restart.

### Routing and cooldowns

No repeated-cooldown or silent-model-failure patterns beyond the two accounted for above. No evidence of the stuck-task reclaim race (#3518 → #3523 → #3526) recurring. `rate_limits`-table-driven degraded-agent detection (`recent_rate_limit_counts`) is unaffected by the #3548 timestamp bug — that table's writes and reads both use SQLite-native `datetime('now')` formatting, so cooldown/routing decisions were never using the wrong window; only reporting (`orch stats`, chat-driven stats, `task_metrics`) was.

### Backlog and stuck work

- `internal:156996` (PR #3538) and `3535` (PR #3536) remain `blocked` on the same stale-branch CI issue flagged in the last several reviews — PR #3538 is confirmed 6 commits behind `main`, 1 ahead. Still needs an operator rebase-and-retry; unchanged.
- `bean` and `oblivion` backlogs unchanged in shape: `GitHub Actions billing failure` blocks at merge time (correct per-task policy, operator-controlled), review-rebroadcast-escalation and max-review-cycle blocks already diagnosed in prior reviews.
- Global queue: 3 `in_progress` (this review + two `bean` retrospective jobs), rest previously-diagnosed `blocked` or `done`.

### Policy alignment

Current operational behavior matches `prompts/skills/orch/SKILL.md` — no drift detected. One correction going forward: the SKILL.md's own example diagnostic query (`WHERE started_at > datetime('now', '-24 hours')`) inherits the #3548 over-count bug, since `task_runs.started_at`/`task_activity.timestamp` are written in the same `T`-separated ISO format. Until #3548 is fixed, treat raw counts from that exact query as up to ~2x inflated and prefer a `julianday()`-based comparison for accurate windowing (as used in this review).

---

## Issues Filed Today

- **[#3548](https://github.com/gabrielkoerich/orch/issues/3548)** — `task_metrics`/`task_runs`/`task_activity` "last N hours" window queries over-count by up to ~2x due to RFC3339-vs-`datetime()` string-comparison mismatch. Verified live against the running DB (e.g. task_runs 24h: 90 raw vs. 44 corrected). Affects `orch stats`, chat-driven repo stats, and this repo's own daily-review runbook query — does not affect cooldown/routing, which reads the unaffected `rate_limits` table.

---

## Priorities for Tomorrow

1. **PRs #3536 and #3538 still need an operator rebase-and-retry** — both remain `BEHIND` main; unchanged across multiple reviews now.
2. **#3548** — fix the timestamp-window comparison in `src/store/metrics.rs` (wrap stored columns in `datetime(...)` or switch to `julianday()`), then update `prompts/skills/orch/SKILL.md`'s example query to match so future daily reviews don't need a manual correction step.
3. **Confirm #3545's ingest-rescan fix recovers #3453** on the next full rescan — no action needed until then per deployment policy.
4. **Confirm #3544's watchdog fix eliminates false stall ERRORs** on the next host suspend/resume cycle.

---

*Prepared by Orch automation (internal:157233) on 2026-08-23.*
