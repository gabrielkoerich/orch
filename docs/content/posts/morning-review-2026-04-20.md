+++
title = "Morning Review — 2026-04-20"
date = 2026-04-20
description = "Daily operational check-in: decode-fix streak merged, service healthy, GLM artifact collection still blocked, no new operational issues filed."
+++

# Morning Review — 2026-04-20

## Recent Commits (last 24h)

24h window was dominated by reliability fixes in store decode paths, routing/metrics correctness, and observability:

- `deba3cb7` bug: reroute agent-label update swallowed GitHub errors and logged false success (#2861)
- `d7ab4036` bug: repo-scoped rate-limit metrics undercount from cross-repo id collisions (#2859)
- `fb29fdb3` fix(store): propagate row decode errors in metrics APIs instead of masking as zeros (#2860)
- `eb82e372` fix(store): avoid panic decoding `source_id` rows (#2855)
- `6ed0506b` perf(sync): bound `list_source_ids_by_source` to a rolling 30-day window (#2851)
- `830b60db` perf(metrics): collapse metrics summary queries from 6 to 2 (#2849)
- `1d5f3c28` docs: evening retrospective 2026-04-19 (#2845)
- `8c1658c2` fix(cooldown): handle GLM monthly limit reset messages (#2844)

## Operational Health

### Service and logs

- Service appears healthy: sync ticks are steady (~1.5s-2.1s in recent logs), no crash/restart pattern observed.
- One routing warning observed at 2026-04-20 10:01:01 UTC for `internal:146522`: LLM routing budget exceeded (45s), task immediately fell back to round-robin and dispatched normally.
- `/opt/homebrew/var/log/orch.error.log` is 0 bytes and last modified on **2026-04-19 06:41** (stale, pre-current run), so no current-run brew stderr signal.

### Task/run health (last 24h)

`task_runs` outcomes:

- success: 150
- failed: 25
- rate_limit: 8
- push_failed: 4
- timeout: 4
- parse_error: 1
- empty outcome: 2

Top agent/model outcomes:

- `minimax/opus`: 44 success (best volume + reliability)
- `claude/sonnet`: 38 success, plus 7 non-success (4 timeout, 3 failed)
- `codex/gpt-5.3-codex`: 34 success, 3 push_failed
- `glm/opus`: 6 rate_limit, 0 success in the last 24h sample
- `opencode/nemotron-3-super-free`: 7 success, 1 parse_error, 1 rate_limit

`task_activity` (last 12h):

- `status_change` 744
- `dispatch` 237
- `push` 168
- `branch_delete` 148
- `error` 38
- `timeout` 4

No broad engine-level stall pattern is visible; instability remains concentrated in specific model lanes.

## Stuck / Blocked Work

- `#2789` (open, blocked): collect raw GLM failing artifacts for last 50 runs (parent: #2762).
- `#2831` (open): latest task metric duration still masks DB errors as missing metrics.
- `orch task list` currently shows only one blocked external task (`2789`) and this morning-review task in progress.

## Retro Follow-up Status (from 2026-04-19 evening)

1. Continue GLM investigation: **still pending** via #2789 (blocked).
2. Assign/fix cleanup timeout issue (#2746): **resolved** (closed 2026-04-18; timeout fix landed in commit `e312bd53`).
3. Capture nemotron parse samples and tighten parser: **partially pending** (only 1 parse error in last 24h, but not yet eliminated).
4. Confirm `orch stream --pipe` behavior live: **still pending explicit validation**.

## Tasks Waiting on Owner Feedback

- No open issues currently labeled `needs-feedback`.

## Priorities for Today

1. Unblock and close #2789 with concrete artifact analysis; decide whether GLM should remain deprioritized/excluded until rate-limit behavior stabilizes.
2. Close #2831 to finish the current decode-error/metrics correctness sweep.
3. Investigate `push_failed` outcomes (4 in 24h) to determine whether failures are transient GitHub/network events or a repeatable runner path.
4. Run a live validation of `orch stream --pipe` to close the remaining retro follow-up.

## Issue Creation

No new operational issues created in this review.

- Existing open issues already track the active operational problems (#2789, #2831).
- Recent closed issues and last 7 days of commits show ongoing fixes for routing, metrics, cooldown behavior, and decode-path reliability.

---

Prepared by Orch automation (internal task internal:146522).
