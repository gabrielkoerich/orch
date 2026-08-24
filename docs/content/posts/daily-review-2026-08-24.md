+++
title = "Daily Review — 2026-08-24"
date = 2026-08-24
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-08-24

## The headline: both bugs found in yesterday's review got fixed today, and nothing new turned up

**Window:** `2026-08-23T23:01Z → 2026-08-24T23:01Z`. Quiet, healthy day — two commits landed, both closing same-window-filed issues, and neither reopened any prior concern. No new operational problems surfaced during this review.

---

## What Shipped (Last 24h)

| Commit | Issue | Summary |
|--------|-------|---------|
| `381af449` | [#3550](https://github.com/gabrielkoerich/orch/pull/3550) → [#3548](https://github.com/gabrielkoerich/orch/issues/3548) | Fixed the RFC3339-vs-`datetime()` string-comparison over-count found in yesterday's own review. `task_metrics`/`task_runs`/`tasks.updated_at` columns are stored `T`-separated but were compared with raw `>=`/`<` against SQLite's space-separated `datetime('now', ?)` output — any row on the cutoff's calendar day counted as "in window" regardless of time-of-day, inflating "last 24h" counts up to ~2x. Fixed at every call site in `src/store/metrics.rs`, and the same fix applied to the runbook queries in `prompts/jobs/agent-debugger.md`, `prompts/jobs/daily-review.md`, and `prompts/skills/orch/SKILL.md` (confirmed: this review's own SKILL.md query now wraps the column in `datetime(...)`, matching the fix). |
| `490543fe` | [#3552](https://github.com/gabrielkoerich/orch/pull/3552) → [#3551](https://github.com/gabrielkoerich/orch/issues/3551) | The review module has two rate-limit/auth-error detection paths: the `AgentError`-based one that #3529/#3530 fixed to route through `detect_credit_exhaustion`, and a second inline text-scan path (used when a review process exits without a clean `AgentError`) that hardcoded `outcome="rate_limit"`/`"auth_error"` and applied only the generic 5min→4h agent cooldown. Kimi billing-cycle-exhaustion messages caught by this second path kept the model under-cooled, so it got re-selected for review every 12-19h and failed the same way each time (3x in the 24h window this was filed against). Both branches now route through the already-tested `outcome_for_agent_error()`/`record_review_agent_failure()` helpers instead of hand-rolling a second classification. |

**Closed today:** #3548, #3551 — both filed during yesterday's review, both fixed same-day.

**Still open, unchanged:**
- **#3535** — opencode `"not available in your country"` misclassification. Fix already committed (`ae8146bb`); PR #3536 still blocked on the same stale-branch CI issue as #3538 (see below), not on review.
- **#3453** — pending-CI-status prose causing review parse errors, 26 days old. Awaiting the next full issue-ingest rescan (per #3545's fix, already shipped) to pick it back up; nothing further to evaluate here per deployment policy.

---

## Operational Health

### Task Run Outcomes (last 24h, corrected `datetime()`-wrapped window)

| Agent | Model | Outcome | Count |
|------|-------|---------|------:|
| claude | `sonnet` | `success` | 17 |
| opencode | `muse-spark-1.2-contributor-free` | `success` | 4 |
| opencode | `x-preview-f-free` | `success` | 3 |
| claude | `sonnet` | `failed` | 2 |
| claude | `sonnet` | (null / recovery) | 2 |
| opencode | `hy3-free` | `success` | 2 |
| opencode | `mimo-v2.5-free` | `success` | 1 |
| opencode | `nemotron-3-ultra-free` | `success` | 1 |
| opencode | `nemotron-3.5-lightning-free` | `success` | 1 |
| kimi | `opus` | `rate_limit` | 1 |
| opencode | `x-preview-f-free` | `parse_error` | 1 |

Every non-`success` row traces to an already-understood, self-healing pattern — no new failure shape:

- **`claude sonnet failed` x2** (`internal:157237`, `157281`) — `silence detection set task to new`/`routed`. Designed generic recovery path; both self-healed.
- **`kimi opus rate_limit` x1** (`internal:157279`, review run at `16:12:17Z`) — the exact billing-cycle-exhaustion pattern #3551/`490543fe` fixed, occurring ~2.5h *before* that fix landed (`18:43:18Z`). Task self-healed by falling through to an `opencode` review that succeeded. This is the last instance of the now-fixed classification gap in the window; no way to evaluate whether it recurs post-fix within this review's scope, per deployment policy.
- **`opencode x-preview-f-free parse_error` x1** (`internal:157269`) — single occurrence, task retried on `claude/sonnet` and succeeded, task is `done`. Matches the long-running class of opencode-review parse issues (8+ closed issues in this area, most recently #3512), not a new pattern worth a fresh filing for one self-healed occurrence.

### `task_activity` (last 24h, corrected window)

`status_change` 101, `dispatch` 37, `push` 30, `branch_delete` 28, `routed` 18, `review_start` 16, `pr_create` 15, `review_decision` 14, `error` 6, `rerouted` 2. All accounted for by the patterns above.

### `orch.error.log`

0 bytes — no crash since last restart.

### `orch log 200`

No `WATCHDOG`, no unexpected `error` lines, no silence-detection spam beyond the two accounted-for above. `recent_rate_limit_counts` cooldown-health check running cleanly every tick.

### Routing and cooldowns

No repeated-cooldown or silent-model-failure patterns. No evidence of the stuck-task reclaim race (#3518 → #3523 → #3526) recurring. No new circuit-breaker trips.

### Backlog and stuck work

- **`internal:156996` (PR #3538) and `3535` (PR #3536) remain `blocked`** on the same stale-branch CI issue flagged across the last several reviews — both confirmed `mergeStateStatus: BEHIND` main (mergeable, not conflicted; CI is failing because it's testing against stale `main`, same root cause as before). Unchanged; still needs an operator rebase-and-retry.
- `bean`/`oblivion` backlogs unchanged in shape: `GitHub Actions billing failure` blocks at merge time (correct per-task policy, operator-controlled), review-rebroadcast-escalation and max-review-cycle blocks already diagnosed in prior reviews.
- Global queue: 2 `in_progress` (this review + a `bean` retrospective job), rest previously-diagnosed `blocked` or `done`.

### Policy alignment

Current operational behavior matches `prompts/skills/orch/SKILL.md` — no drift detected. The SKILL.md example query itself now reflects the #3548 fix (`datetime(started_at)` wrapping), so no manual correction step was needed for this review, unlike yesterday.

---

## Issues Filed Today

None. Both issues surfaced by yesterday's review (#3548, #3551) were fixed same-day; nothing new met the bar for filing (single self-healed occurrences of already-well-documented failure classes don't warrant a fresh issue).

---

## Priorities for Tomorrow

1. **PRs #3536 and #3538 still need an operator rebase-and-retry** — both remain `BEHIND` main; unchanged across 4+ consecutive reviews now.
2. **Confirm #3551's fix holds** — watch for any recurrence of kimi review runs stored with outcome `rate_limit` instead of a billing-cycle-specific outcome; none observed yet post-fix (fix landed with ~4h left in this review's window).
3. **Confirm #3545's ingest-rescan fix recovers #3453** on the next full rescan — no action needed until then per deployment policy.

---

*Prepared by Orch automation (internal:157328) on 2026-08-24.*
