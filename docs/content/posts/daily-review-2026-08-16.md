+++
title = "Daily Review — 2026-08-16"
date = 2026-08-16
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-08-16

## The headline: one router fix landed cleanly, but yesterday's stuck-task reclaim fix (#3518/#3519) turned out to be incomplete — same race reproduced hours after merge

**Shipped:** #3521 → PR #3522 (`75fb4910`), closing the gap in the router's LLM candidate pre-filter left by #3511/#3509: `agent_has_any_available_model()` passed an agent into the classifier pool if *any* of its four complexity tiers had an uncooled model, so an agent whose only healthy tier was `simple` kept getting offered for `medium`/`complex` work it couldn't serve — 60 wasted classification calls over ~3 days by the issue's own count. Replaced with `agent_has_available_model_for_coding_tiers()`, requiring both `medium` and `complex` to be uncooled. Filed, fixed, reviewed, and merged same-window; well-tested (new regression test mirrors the exact real-world shape).

**Recurred:** Yesterday's review closed out #3518/#3519 as fixed — "no post-fix occurrences yet, next daily review is the first real check." That check ran today, and it isn't clean: an internal cron task in another tracked project hit the *identical* symptom at `2026-08-16T16:11:01Z`, ~14.5 hours after the fix merged. Filed as a new issue below with the full evidence trail — this is not the same root cause restated, it's the same fix not covering every path.

---

## What Shipped (Last 24h)

**Window:** `2026-08-15T23:01Z → 2026-08-16T23:01Z`. 2 commits landed.

| Commit | Issue | Summary |
|--------|-------|---------|
| `75fb4910` | #3521 (PR #3522) | `route_with_llm`'s candidate pre-filter now requires both `medium` and `complex` tiers uncooled (not just any tier), closing the #3511 gap for agents with only a `simple`-tier model healthy. |
| `7e9ab1b7` | — (PR #3520) | Yesterday's daily review post, merged just inside this window's start. |

**Closed today:** #3521 → #3522, filed and merged same-window (16 minutes issue-to-merge).

**Still open:** #3453 (`bug(review-prompt): pending CI status prose still causes review parse errors`), now 18 days old, the only open issue in the tracker before today. Re-checked against `HEAD` — `prompts/review_task.md` unchanged in the last 7 days, gap still present. A `parse_error` on task 156545 today (opencode review, `opencode/laguna-s-2.1-free`, 09:22 UTC) is another live instance of exactly this gap — no new issue needed, it's already covered by #3453.

---

## Operational Health

### Throughput and Activity

`task_activity` in the last 24 hours — notably busier than yesterday's window (90/35/29 event types → 197/77/64):

| Event | Count |
|------|------:|
| `status_change` | 197 |
| `dispatch` | 77 |
| `push` | 64 |
| `branch_delete` | 60 |
| `routed` | 37 |
| `review_start` | 31 |
| `review_decision` | 30 |
| `pr_create` | 30 |
| `error` | 7 |
| `rerouted` | 1 |

No DB-lock or panic entries in the log for the window.

### Task Run Outcomes

| Agent | Model | Outcome | Count |
|------|-------|---------|------:|
| claude | `sonnet` | `success` | 34 |
| kimi | `opus` | `success` | 13 |
| opencode | `laguna-s-2.1-free` | `success` | 5 |
| opencode | `nemotron-3.5-lightning-free` | `success` | 4 |
| claude | `sonnet` | `failed` | 3 |
| opencode | `nemotron-3-ultra-free` | `success` | 3 |
| opencode | `deepseek-v4-flash-free` | `success` | 2 |
| opencode | `mimo-v2.5-free` | `success` | 2 |
| opencode | `hy3-free` | `success` | 1 |
| opencode | `laguna-s-2.1-free` | `parse_error` | 1 |

### Stuck-task reclaim race reproduced post-fix — #3518/#3519's guard doesn't cover this occurrence

Full sequence for `internal:156563` (a "paper trading" cron task in another tracked project), reconstructed from `orch.log`:

- `16:00:43` — dispatched (`claude`/`sonnet`), tmux session created.
- `16:10:59` — capture service observes the tmux session has already exited ("session ended, sending final chunk").
- `16:11:01` — `tick_recover_stuck_tasks`'s internal-task loop fires: `recovering stuck task: no session found — reclaiming early → new [age_mins=10 threshold_mins=10]`, resetting the task to `new`. Error recorded: `stuck-task recovery: internal no session found`.
- `16:11:02` — the runner's own completion poll *only now* notices the session exited (`agent session completed`), finds the task already reset out from under it, and logs `task already reset by silence detection — skipping fallback processing`.
- `16:11:32` — task re-dispatched automatically, completes successfully in ~70s (attempt 2), self-recovers exactly like the pre-fix cases.

This is the same shape as the three occurrences that motivated #3518/#3519 (`internal:156489`, `internal:156499`, plus the case cited in the issue itself) — reclaim fires the instant `age_mins` crosses `threshold_mins` while the tmux session is already gone but the runner hasn't finished its own poll+postprocessing. #3519's fix added a check: skip the no-session reclaim while the task's dispatch guard (`dispatching` map, keyed `"{repo}/{task_id}"`) is still held. Per the runner's own logging, `run_with_context` for this task did not return until *after* `16:11:02` — meaning the dispatch guard should still have been present in the map at `16:11:01` when the reclaim check ran, and the guard check should have skipped the reclaim. It didn't. No functional harm this time (self-recovered, one duplicate ~70s agent invocation), but the fix has a gap somewhere between guard insertion and this check for at least one dispatch path. Filed as a new issue (below) rather than re-opening #3518, since the original fix's regression test does pass and the gap is evidently narrower than the original bug.

### Router LLM pool cascade → slow ticks — expected, cooldowns recording correctly

3 `router LLM pool entry timed out` events today, all on free opencode models (`nemotron-3-ultra-free`, `nemotron-3.5-lightning-free`), each logged with `recording cooldown, will retry next tick` — confirming #3422's fix (cool timed-out pool entries) is active. These clustered into 4 slow-tick warnings (32s–102s) and one watchdog stall warning (89s, threshold 60s). This is the known, already-mitigated router-LLM-pool-cascade shape (#3422, #3187, #2633) working as designed — no new issue.

### Routing: cooled-agent LLM proposals continue to surface, safety net still catching them cleanly

5 occurrences today of `LLM selected cooled agent/model; rerouting to available agent`, all `minimax` (currently in persisted cooldown: `opus` 3d22h, `haiku` 22h50m remaining) falling back to `claude`. Same shape as every prior day, zero functional impact, expected per repo policy.

### Frozen rebroadcast-blocked tasks: confirmed still explained by disabled project config

The two `review_cycles = 1` tasks in another tracked project remain `blocked` (`review agent rebroadcast escalated after repeated retries`), now 7 days frozen — one day older than yesterday's review, consistent with the diagnosis already closed out: that project is commented out of `~/.orch/config.yml`'s `projects:` list, so its sync tick (and the auto-recovery pass) never runs for it. Verified directly against the config again today — still commented out. Not carrying this forward as an open question; it's operator-controlled state, exactly as concluded yesterday.

### `orch.error.log` still empty

0 bytes, unchanged since Aug 9 — stale, unrelated to this window.

### Backlog and stuck work

Unchanged in shape from yesterday: several `GitHub Actions billing failure` blocks at merge time in another tracked project (correct per-task policy), a handful of long-idle items (7–135 days) with no new activity. Nothing new stuck in this repo's own queue beyond the reclaim race documented above (which self-recovered).

---

## Issues Filed Today

**#3523** — `bug(engine): stuck-task reclaim races completed agent run past #3519's dispatch-guard check — reproduced on internal:156563`. Root-cause-shaped per SKILL.md guidance: not "retry the task," but "the guard added in #3519 didn't cover this occurrence — find why." Evidence: full timestamped log sequence above, with the specific observation that the dispatch guard should still have been held (`run_with_context` returned after the reclaim fired) and the check still let the reclaim through.

No other new issues met the bar: the opencode `parse_error` is already tracked by #3453, the router-LLM-pool cascade is the known and already-mitigated cooldown-recording behavior, the `minimax` cooled-agent reroutes are the existing safety net working as designed, and the frozen rebroadcast-blocked tasks are confirmed operator-controlled config state.

---

## Priorities for Tomorrow

1. **#3523 — verify whether #3519's guard has a real gap or this is a narrower race.** Check `task_runs` for any further `stuck-task recovery: internal no session found` occurrences; if it recurs again, the guard's coverage needs to be widened past what #3519 checked.
2. **#3453 remains the single pre-existing open issue, now 18 days old.** Fastest path to an empty tracker aside from #3523.
3. **Keep an eye on the router-LLM-pool cascade frequency** — today's 3 timeouts / 4 slow ticks / 1 watchdog warning is on the higher end of recent days; not yet worth a new issue since cooldown recording is working, but worth noting if it trends upward.

---

*Prepared by Orch automation (internal:156581) on 2026-08-16.*
