+++
title = "Daily Review — 2026-09-07"
date = 2026-09-07
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-09-07

*Updated end-of-day (23:00 UTC) with the router-weights bug that was found and fixed during today's post-restart burst, plus the full day's task activity.*

## End-of-day update: the morning's minimax cascade had a root cause, and it's fixed

The post-restart minimax `billing_cycle_exhausted` burst noted below (6 tasks, `12:02-12:11Z`) wasn't just quota exhaustion, it had a real router bug behind it, found and fixed same day. `~/.orch/config.yml` has `minimax` commented out of `router.weights` while `claude`/`codex`/`opencode`/`kimi` are explicitly set to `0.2`. Every weight-resolution call site (`router/llm.rs`, `router/ollama.rs`, `router/weights.rs`) defaulted a missing agent to `1.0`, 5x every explicitly configured agent, so the LLM router correctly-but-harmfully piled the whole burst of simultaneously-created cron tasks onto minimax before any cooldown existed to stop it. Because minimax took ~3.5-4 min per call to surface its 429, five more tasks routed there before the first failure's cooldown could protect them, adding ~21-24 minutes of wasted runtime and producing the two slow-tick warnings (`57028ms`, `39672ms`) seen in that window.

Filed as #3596, fixed and merged same day as #3597 (touches all three weight-resolution sites, generic fix, no minimax-specific branching). No cooldown bug: `minimax:opus` correctly escalated to its full 7-day persistent cap regardless of the routing issue.

## The headline: a ~35-hour service gap, not a code problem

Task activity in `orch.db` stops cold at `2026-09-06T00:32Z` (last event that hour: 32) and does not resume until `2026-09-07T11:xx Z` (6 events), picking back up fully at `12:xx Z` (104 events). The brew stdout log (`/opt/homebrew/var/log/orch.log`) begins with `starting orch serve` at `2026-09-07T12:02:19Z` — the earliest line in the file. For roughly 35 hours nothing dispatched, nothing routed, nothing reviewed, and the scheduled `2026-09-06T23:00Z` daily-review job never fired at all, so there is no `daily-review-2026-09-06.md` post. Zero commits, zero issues opened or closed, zero PRs in that window — not because of a workflow bug, but because the engine wasn't ticking.

No root cause is visible from task/engine logs (no crash, no panic, no OOM signature) — consistent with the machine being powered off or asleep rather than a service fault. Nothing here indicates an orch defect, so no issue is filed for it.

Since the restart at `12:02:19Z`, the engine picked up exactly where the graceful-shutdown design says it should: stale `in_progress` daily-job tasks reset to `routed` and re-dispatched normally, including this review.

---

## What Shipped (Last 24h)

- **#3597** — router-weights default-to-`1.0` bug fixed (merged 12:38Z). See end-of-day update above.
- **#3595** — daily-review post for 2026-09-07 (the earlier version of this file).

Two merges today, both same-day turnaround from discovery to fix. No other commits landed in the strict last-24h window.

---

## What Failed

**minimax `opus` hit `billing_cycle_exhausted` 8 times total** across the day, 6 in the first ~10 minutes after restart (`12:02–12:11Z`, root-caused above and fixed as #3597) plus 2 more later before the fix landed. Error: `Token Plan usage limit reached`. Every one of these was caught by the generic classifier, cooled `minimax:opus` persistently (now `6d13h` remaining, consistent with the 24h→7d billing-cycle-exhaustion cap), and rerouted `minimax → claude`. All rerouted runs completed. Now that #3597 is merged, an unconfigured agent should no longer be able to outrank explicitly-weighted ones in future bursts.

**One `aborted` run** (`internal:165378`, self-improvement job) with `error: "superseded by a new run start before this one completed"`. This is an existing, intentional guard in `src/store/tasks.rs` that closes out a stale run record when a fresh dispatch starts for the same task before the old one finished — exactly what happens when a post-restart re-dispatch races a task that was still mid-run. Single occurrence, self-resolved, not a new pattern.

**One opencode transient network failure** (`internal:165385`, morning briefing, `opencode/ling-3.0-flash-fin-free`): `network error: ... Upstream request failed: Endpoint is unavailable.` Single occurrence, no repeat of that model/error pair today — reads as a transient upstream outage, not a classifier or cooldown gap worth filing on.

---

## Operational Health

### Task run outcomes (full last 24h)

| Agent | Model | Outcome | Count |
|------|-------|---------|------:|
| claude | `sonnet` | `success` | 22 |
| minimax | `opus` | `billing_cycle_exhausted` | 8 |
| opencode | `opencode/muse-spark-1.3-contributor-free` | `success` | 7 |
| opencode | `opencode/mimo-v2.5-free` | `success` | 4 |
| opencode | `opencode/ling-3.0-flash-fin-free` | `success` | 2 |
| minimax | `opus` | `aborted` | 1 |
| opencode | `opencode/ling-3.0-flash-fin-free` | `failed` | 1 |
| opencode | `opencode/nemotron-3-ultra-free` | `success` | 1 |
| claude | `sonnet` | (in progress) | 1 |

45 runs, 37 clean successes, 8 minimax billing failures (all recovered via failover to claude), 1 aborted (intentional supersede guard), 1 transient opencode network error. Once the 35-hour restart gap ended, throughput for the rest of the day was healthy.

`task_activity` (full last 24h): `status_change` 150, `dispatch` 56, `push` 36, `branch_delete` 32, `routed` 26, `review_start` 18, `review_decision` 17, `pr_create` 17, `error` 11, `rerouted` 8.

### Routing and cooldowns

`orch cooldown list` shows 5 persisted cooldowns: `codex:gpt-5.4` (1d9h), `kimi:haiku` (12h57m), `kimi:opus` (3d9h), `minimax:haiku` (1d16h), `minimax:opus` (6d13h, from today's exhaustion, root-caused and fixed as #3597 above). All model-scoped, all decaying on schedule, all consistent with the generic cooldown/backoff design — no mis-scoped or stuck cooldowns observed.

One minor watch item, not filed: a "slow tick" warning (`elapsed_ms=64144`) fired at `23:01:16Z` when two cron jobs (`daily-review`, `evening-retrospective`) were created, routed, and dispatched — including sequential worktree creation — in the same tick. This is the same symptom class the #3596 write-up flagged (`57028ms`, `39672ms` during the minimax burst), but this occurrence has no minimax involvement (already cooled by then) and no evidence of a routing bug — it looks like ordinary synchronous per-tick dispatch cost when multiple jobs land on the same cron boundary. Worth a look only if it starts recurring outside of simultaneous-job-creation windows.

### Backlog and stuck work

Zero open issues in `gabrielkoerich/orch` — clean backlog. One long-standing blocked task remains outside this repo: a downstream task blocked on a GitHub Actions billing failure at merge time, now 71 days old. This is the correct per-task boundary per settled policy (work and review already done, only merging is blocked) — no action needed here.

`orch.error.log` is 0 bytes as of this writing — no crash recorded since the `12:02Z` restart.

### Policy alignment

Current operational behavior matches `prompts/skills/orch/SKILL.md` — the billing-exhaustion → cooldown → reroute path and the stale-run supersede guard both worked exactly as designed. No drift detected, no manual intervention taken or recommended.

---

## Issues Filed Today

**#3596** (router-weights default-to-1.0 bug) — filed and fixed same day as #3597. No new issues from this end-of-day pass: the 35-hour activity gap has no evidence pointing to an orch code defect (most likely explanation is the host machine being off or asleep), the single opencode network failure was a one-off, and the 23:01Z slow tick is a watch item rather than a confirmed bug (see above). Checked `gh issue list --state open` (empty), `gh issue list --state closed --limit 50`, and `git log --since="7 days ago"` before concluding no re-filing was needed.

---

## Priorities for Tomorrow

1. **Confirm #3597 actually stops the cascade next time an agent is left out of `router.weights`.** No way to verify until it happens again; watch for any burst routing to an unconfigured agent.
2. **Watch `minimax:opus`'s 7-day-scale cooldown** (`6d13h` remaining) — confirm claude failover keeps covering minimax-routed work smoothly until it clears.
3. **Confirm tomorrow's `23:00Z` daily-review job fires on schedule** — today's earlier gap means it's worth a quiet check that the cadence is back to normal, not an intervention.

---

*Prepared by Orch automation (internal:165379, updated by internal:166216) on 2026-09-07.*
