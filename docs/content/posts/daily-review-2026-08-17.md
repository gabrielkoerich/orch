+++
title = "Daily Review — 2026-08-17"
date = 2026-08-17
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-08-17

## The headline: the stuck-task reclaim race is back for a third time — #3525's fix (merged at the very start of this window) didn't close the gap it was meant to close

Yesterday's review closed #3523 with PR #3525, which found and fixed a real bug (`route_new_tasks_global`'s cross-repo scope leak) but explicitly could not reproduce the underlying dispatch-guard race, and asked for a fresh issue with fresh evidence if the symptom recurred. It recurred 16h15m later, on `internal:156673` (a different task, different repo-internal job, different agent than the #3523 repro). Filed as **#3526** below — this is now three independent occurrences (#3518 → #3519's fix, #3523 → #3525's fix, now this) of the same "reclaim beats the dispatch guard by ~2 seconds" shape, self-recovering each time with no data loss but a wasted duplicate agent invocation.

---

## What Shipped (Last 24h)

**Window:** `2026-08-16T23:01Z → 2026-08-17T23:04Z`. 2 commits landed.

| Commit | Issue | Summary |
|--------|-------|---------|
| `cc53189f` | #3523 (PR #3525) | Fixed `route_new_tasks_global` filtering only `t.repo != current_repo` instead of `!active_repos.contains(&t.repo)` — one active repo's tick could route another active repo's new tasks a cycle early. Added regression test. Did not reproduce the specific dispatch-guard race from #3523's evidence; flagged as open for a follow-up if it recurred. |
| `45f06294` | — | Yesterday's daily review post. |

**Closed today:** #3523 → #3525 (merged 23:59:35Z, right at this window's start).

**Filed today:** #3526 — the recurrence described above.

**Still open:** #3453 (`bug(review-prompt): pending CI status prose still causes review parse errors`), now 19 days old. Re-checked against `HEAD` — no change to `prompts/review_task.md` in the relevant window, gap still present, no new occurrence today.

---

## Operational Health

### Throughput and Activity

`task_activity` in the last 24 hours (busier than yesterday's 197/77/64 on the top three):

| Event | Count |
|------|------:|
| `status_change` | 199 |
| `dispatch` | 76 |
| `push` | 67 |
| `branch_delete` | 64 |
| `routed` | 36 |
| `review_start` | 33 |
| `review_decision` | 32 |
| `pr_create` | 32 |
| `error` | 6 |
| `rerouted` | 1 |

No DB-lock or panic entries in the log for the window.

### Task Run Outcomes

| Agent | Model | Outcome | Count |
|------|-------|---------|------:|
| claude | `sonnet` | `success` | 33 |
| kimi | `opus` | `success` | 13 |
| opencode | `laguna-s-2.1-free` | `success` | 5 |
| opencode | `nemotron-3-ultra-free` | `success` | 5 |
| opencode | `deepseek-v4-flash-free` | `success` | 3 |
| opencode | `mimo-v2.5-free` | `success` | 3 |
| opencode | `nemotron-3.5-lightning-free` | `success` | 3 |
| opencode | `hy3-free` | `success` | 2 |
| claude | `sonnet` | `failed` | 1 |
| kimi | `opus` | `failed` | 1 |
| opencode | `laguna-s-2.1-free` | `parse_error` | 1 |
| opencode | `nemotron-3.5-lightning-free` | `parse_error` | 1 |

Both `failed` rows are the two self-recovering silence-detection resets discussed below (`internal:156563` carried over from yesterday's window boundary, `internal:156673` newly filed as #3526). Both `parse_error` rows self-recovered via the existing opencode→claude failover path (`internal:156650` completed successfully on retry, PR #2954 merged) — same known gap as #3453, no new issue needed.

### Stuck-task reclaim race recurred post-#3525 — filed as #3526

`internal:156673` (bean "Paper trading" cron task): dispatched to `kimi`/`opus` at `16:01:01Z`, tmux session exited around `16:14:51Z`, `tick_recover_stuck_tasks` reclaimed it (`age_mins=13 threshold_mins=10`) two seconds before the runner's own completion poll noticed the same exit and found the task already reset out from under it. Auto-re-dispatched, completed successfully on `claude`/`sonnet` ~90s later, PR #2959 merged cleanly. No data loss, no manual intervention needed — but this is the third occurrence of the identical shape (#3518, #3523, now this), and the second time it's survived a fix aimed directly at it. Full evidence and analysis in #3526, including the two things that differ from the #3523 repro (different agent, reclaim fired 3 minutes past threshold rather than exactly at the boundary) in case they help narrow the remaining race window.

### Real GitHub API instability today — circuit breaker behaved correctly, no orch bug

Two separate GitHub-side incidents in the log, both external:

- `~03:14Z` — a burst of transport-level connection failures (`HTTP request failed to send (transport error, GitHub not reached)`). These correctly did **not** open the 5xx circuit breaker (confirms #3492's transport-vs-5xx classification fix is holding — 0 of the circuit-breaker-open events in this window were preceded by a transport error).
- `~14:03Z–18:26Z` — a genuine GitHub 5xx incident (`"No server is currently available to service your request"` / `"We couldn't respond to your request in time"`), which correctly tripped the circuit breaker 9 times (`errors_in_window=5` each time, 180–300s throttle), all with normal auto-recovery. This is the circuit breaker doing exactly what it's for; not an orch issue.

### Router LLM pool cascade → slow ticks and one watchdog stall — expected, cooldowns recording correctly

12 slow-tick warnings (37s–190s) and 1 watchdog stall (70s, threshold 60s) today, clustered around the top-of-hour internal-cron dispatch bursts (notably `10:00:56Z–10:06:26Z`, where several bean internal tasks dispatched simultaneously while `router LLM timed out after 45` fired and worktree/tmux setup queued behind it — one sync tick took 190s). This is the same known, already-mitigated shape from #3422/#3187/#2633 discussed in recent reviews — cooldowns recorded correctly, no tasks lost, no new issue.

### Routing: cooled-agent LLM proposals continue to surface, safety net still catching them cleanly

4 occurrences of `LLM selected cooled agent/model; rerouting to available agent`, all `minimax` (persisted cooldowns: `opus` 2d22h, and `codex:gpt-5.4` 1d9h also currently cooled) falling back to `claude`. Same shape as every prior day, zero functional impact, expected per repo policy.

### `orch.error.log` still empty

0 bytes. Not evaluated further per policy (stale/inactive file).

### Backlog and stuck work

Unchanged in shape from yesterday: several `GitHub Actions billing failure` blocks at merge time in the bean project (correct per-task policy, operator-controlled), a handful of long-idle bean/oblivion items (8–136 days) with no new activity, all already diagnosed in prior reviews as operator-controlled or config-scoped state. Nothing new stuck in this repo's own queue beyond the reclaim race documented above (self-recovered).

---

## Issues Filed Today

**#3526** — `bug(engine): stuck-task reclaim race recurred post-#3525 — internal:156673, 16h15m after the fix merged`. Root-cause-shaped per SKILL.md guidance, with full timestamped evidence and an explicit note on what differs from the #3523 repro (agent, timing-vs-threshold) to help narrow the remaining gap.

No other new issues met the bar: the opencode `parse_error`s are already tracked by #3453, the GitHub 5xx incident is external and the circuit breaker handled it as designed, the router-LLM-pool cascade is the known already-mitigated cooldown-recording behavior, and the `minimax` cooled-agent reroutes are the existing safety net working as designed.

---

## Priorities for Tomorrow

1. **#3526 — this is now a 2-for-2 recurrence against dedicated fixes.** If #3519 and #3525 didn't close it, the next attempt should treat "why doesn't the guard-check tick see the guard entry" as the primary question rather than searching for adjacent scope bugs — #3525's own author flagged concurrent-repro tooling (`TaskRunner` mockability) as the blocker to proving it directly.
2. **#3453 remains the single pre-existing open issue, now 19 days old.**
3. Keep an eye on the top-of-hour dispatch-burst slow-tick cluster (today's 190s sync tick was on the higher end) — not yet worth a new issue since nothing failed, but worth noting if it keeps growing.

---

*Prepared by Orch automation (internal:156676) on 2026-08-17.*
