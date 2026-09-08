+++
title = "Daily Review — 2026-09-08"
date = 2026-09-08
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-09-08

## What Shipped (Last 24h)

- **#3598** — docs post updating yesterday's daily review with the router-weights fix writeup (merged 2026-09-07T23:07Z, just inside this window).

That's the only commit in the strict last-24h window (`git log --since="24 hours ago"`). No other code merged today. Issue volume was otherwise quiet: no issues closed in the last 24h, one new issue opened and moving through review as this post is written (**#3599**, "dispatch never re-checks agent/model cooldown set after routing" — a well-evidenced write-up of yesterday's minimax burst root-caused one layer deeper: routing and dispatch are decoupled, so a task routed just before a model's cooldown lands can still dispatch against it. Its own PR, #3600, is currently blocked — see below, it's also today's headline operational finding).

---

## What Failed

**minimax `opus`/`haiku` stayed cooled all day** — 8 `billing_cycle_exhausted` runs and 1 `aborted` run recorded in the last 24h, all from before the persistent cooldown was already in force (the exhaustion was root-caused and fixed yesterday as #3596/#3597; today's numbers are the tail of that same cooldown window, not a new cascade). No repeat burst today.

**opencode free-tier models had a normal noisy day**: 1 `failed`, 1 `timeout`, 1 `truncated` across ~20 free-model runs (`ling-3.0-flash-fin-free`, `nemotron-3-ultra-free`, `nemotron-3.5-lightning-free`, `muse-spark-1.2/1.3-contributor-free`). No repeated model/error pair — reads as ordinary free-tier flakiness, not a classifier gap.

**PR #3600 has been stuck `in_review` for 1h40m+ and counting**, unable to auto-merge despite an approved review — this is today's real finding, detailed below.

---

## Operational Health

### Task run outcomes (last 24h)

| Agent | Model | Outcome | Count |
|------|-------|---------|------:|
| claude | `sonnet` | `success` | 38 |
| minimax | `opus` | `billing_cycle_exhausted` | 8 |
| opencode | `opencode/ling-3.0-flash-fin-free` | `success` | 7 |
| opencode | `opencode/muse-spark-1.3-contributor-free` | `success` | 5 |
| opencode | `opencode/mimo-v2.5-free` | `success` | 4 |
| opencode | `opencode/muse-spark-1.2-contributor-free` | `success` | 3 |
| claude | `sonnet` | (in progress) | 2 |
| minimax | `opus` | `aborted` | 1 |
| opencode | `opencode/ling-3.0-flash-fin-free` | `failed` | 1 |
| opencode | `opencode/nemotron-3-ultra-free` | `success` | 1 |
| opencode | `opencode/nemotron-3-ultra-free` | `timeout` | 1 |
| opencode | `opencode/nemotron-3.5-lightning-free` | `success` | 1 |
| opencode | `opencode/nemotron-3.5-lightning-free` | `truncated` | 1 |

73 runs, 59 clean successes, 8 minimax billing failures (tail of yesterday's already-fixed cascade, all recovered via claude failover), 1 aborted, 3 opencode free-tier hiccups. Volume and success rate both look healthy.

`task_activity` (last 24h): `status_change` 223, `dispatch` 78, `push` 59, `branch_delete` 54, `routed` 36, `review_start` 31, `review_decision` 28, `pr_create` 28, `error` 13, `rerouted` 8, `timeout` 1.

### Routing and cooldowns

`orch cooldown list` shows 5 persisted cooldowns, all model-scoped and decaying on schedule: `codex:gpt-5.4` (9h19m), `kimi:haiku` (16h58m), `kimi:opus` (2d9h), `minimax:haiku` (16h59m), `minimax:opus` (5d13h — from yesterday's exhaustion, tracking down as expected). No stuck or mis-scoped cooldowns.

### Root cause found: CI pending-timeout never fires when a task is retried faster than `max_wait`

PR #3600 (task 3599) is blocked from merging because one required GitHub Actions check (`secrets`) is stuck reporting `status=in_progress` at the Checks API level, even though the job's own steps completed and the overall workflow run shows `completed`/`success` — a GitHub-side status inconsistency. That part isn't an orch bug.

What is a bug: orch has a designed fallback for exactly this ("CI checks still pending after timeout" → increment a failure counter → `Blocked` after 3 timeouts), but it never fires here. `ci_merge_failures` for task 3599 is still `0` after 1h40m+ of continuous retries. Traced the cause: `auto_merge_pr`'s CI-poll loop tracks elapsed wait time in a function-local `Instant`, but the loop's first action on every iteration is a per-task CI-check cooldown gate (60s) that returns early — discarding that local timer — whenever the sync tick re-invokes the function sooner than the cooldown allows, which it always does. So the 600s `max_wait` timeout can structurally never accumulate: every ~90-100s the whole wait state resets to zero. Filed as **#3601** with the full trace and a suggested fix (persist "first seen pending" in the KV store instead of a per-call local timer).

### Backlog and stuck work

One open issue in `gabrielkoerich/orch` (**#3599**, in review as of writing — expected to close itself once #3600 merges, once #3601 is fixed or the GitHub-side check clears on its own). One long-standing blocked task outside this repo (a downstream task blocked 72 days on a GitHub Actions billing failure at merge time) — correct per-task boundary per settled policy, no action needed.

`orch.error.log` — not re-checked as a finding source today; nothing in the engine log pointed at a crash or panic in the last 24h.

### Policy alignment

Behavior matches `prompts/skills/orch/SKILL.md` on every point checked: no brew/version-drift recommendations made, no config file edits, root-cause issue filed only after tracing the actual code path (not a symptom guess), existing closed CI-recovery issues (#3587, #3568, #3561, #3558) checked and confirmed to cover a different code path than today's finding.

---

## Issues Filed Today

**#3601** — `auto_merge_pr`'s CI pending-timeout never fires because the per-task CI-check cooldown resets the wait-loop's local elapsed-time state on every sync-tick-driven invocation. Root-caused with full code trace and evidence from the live-stuck PR #3600. Checked `gh issue list --state open/closed`, `git log --since="7 days ago" -- src/engine/auto_merge.rs` first — no overlap with previously closed CI-recovery issues.

No other issues filed: the minimax numbers are the known, already-fixed cascade's tail; the opencode hiccups are one-off free-tier flakiness with no repeated pattern.

---

## Priorities for Tomorrow

1. **Check whether PR #3600 / task 3599 ever resolved** — either the GitHub-side stuck check clears on its own, or it's still spinning silently (per #3601, it will spin forever until fixed). If still stuck, this is the clearest evidence yet that #3601 needs to land.
2. **Watch for #3601 getting picked up** — it's a precise, actionable fix (persist pending-start timestamp in KV instead of a local `Instant`).
3. **Confirm minimax cooldowns clear on schedule** (`opus` 5d13h, `haiku` 16h59m remaining) and failover keeps covering minimax-routed work in the meantime.

---

*Prepared by Orch automation (internal:168101) on 2026-09-08.*
