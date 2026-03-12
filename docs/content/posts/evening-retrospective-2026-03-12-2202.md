+++
title = "Evening Retrospective — 2026-03-12 (late)"
date = 2026-03-12T22:02:00Z
description = "11 more fixes landed after the first retro: infinite-loop defenses complete, worktree cleanup extended, zero open issues"
+++

## Context

A second retrospective was needed because 11 more commits landed after the first one
(`19e06f9`, ~14:00 UTC). This entry covers only the delta since then.

The first retrospective correctly identified #536 (review gate infinite loop on PR creation
failure) as the top priority. **That fix landed within hours** — plus 10 additional fixes
addressing related infinite-loop patterns across the engine.

---

## What Landed Since the First Retro (14:00 → 22:00 UTC)

| PR / Commit | Description |
|-------------|-------------|
| #540 / `b934b53` | **fix: block task after MAX_PR_CREATE_FAILURES=3 consecutive PR creation failures** — closes #536 |
| `a59da11` | fix: return Ok from auto_merge_pr when task is intentionally Blocked |
| #544 / `c9f55d6` | fix: skip task on is_pr_merged error instead of silently re-dispatching |
| #545 / `29ca672` | fix(cleanup): clarify inline cleanup uses ttl_hours=0 for immediate removal |
| #546 / `c6ad4e4` | fix: cap output buffer delta on terminal clear in diff_and_update |
| #549 / `7ffc662` | fix: cap consecutive review agent failures to prevent infinite NeedsReview loop |
| #551 / `0662402` | fix: propagate is_pr_merged API errors instead of treating as false |
| #554 / `fd6126f` | fix: reset review_agent_failures counter on re-dispatch and stale-session recovery |
| #555 / `348e649` | fix: cap CI failures/timeouts in auto_merge_pr to prevent infinite loop |
| #557 / `e3211e2` | fix: reset merge_conflict_retries and pr_create_failures counters on re-route |
| #560 / `ce7fb05` | fix: reset all failure counters on /retry, /reroute, and orch task retry/unblock |
| `af3927a` | fix: cleanup worktrees for closed issues and blocked internal tasks |

**Total today: 28 commits merged. Zero open issues. CI green.**

---

## What Went Well

### 1. Systematic infinite-loop elimination

The first retro flagged one loop (#536 — PR creation failures). Agents traced the same
pattern across five more paths:

| Loop vector | Fix |
|-------------|-----|
| PR creation failure | `pr_create_failures` counter → block at 3 (#540) |
| Review agent failure | `review_agent_failures` counter → cap (#549) |
| CI failure / timeout | `ci_failures` counter → cap (#555) |
| is_pr_merged API error | propagate error, skip task (#551) |
| Blocked task in auto_merge_pr | return Ok (don't re-trigger) (#543) |

All counters now reset on `/retry`, `/reroute`, `orch task retry`, and `orch task unblock`
(#560). This closes the escape-hatch gap: operators can manually unblock a task and the
counter starts fresh.

### 2. Worktree hygiene extended

`af3927a` extends cleanup to closed GitHub issues and internally-blocked tasks. Previously,
worktrees only cleaned up on merge/done. Now the engine removes worktrees proactively when:
- The linked GitHub issue is closed
- The internal task is in `blocked` status

This directly addresses the stale blocked tasks noted in the first retro (`internal:10`, etc.).

### 3. Counter reset on re-route (#557)

When a task is re-routed (merge conflict, review rejection), `merge_conflict_retries` and
`pr_create_failures` both reset. This prevents a task from being immediately blocked on its
second attempt just because the first attempt had failures before the re-route.

### 4. Output buffer fix (#546)

Terminal clear sequences (e.g., `\x1b[2J`) were causing the diff-based capture to see a
large "negative delta" and overwrite the buffer incorrectly. Capped at the actual new
content length. This was causing garbled output in `orch task logs`.

---

## What Didn't Go Well

### 1. Volume of loop-fix PRs suggests missing test coverage

Eleven PRs, all fixing variants of the same pattern (unbounded retry counters), landed in
one day. This suggests the engine loop logic lacks systematic tests for the "failure
accumulation → block" path. Each fix required a human to notice a new loop variant in
production, not catch it in CI.

**Suggested follow-up**: property-based tests for the engine tick that verify all failure
counters are bounded. Not filing an issue today (no open issues remain, and the engine is
now correct), but worth noting for the next code review cycle.

### 2. is_pr_merged silent false → fix required two PRs (#544 + #551)

`is_pr_merged` errors were silently treated as `false` (#551), then the fallback was silently
re-dispatching the task (#544). These two bugs composed into a silent re-dispatch loop.
The fixes are correct, but the root cause is that API error handling was not consistently
propagated. A review of other `Result<bool>` → `unwrap_or(false)` patterns in the engine
would be prudent.

---

## Open Issues

**None.** All issues from today are closed or were fixed without filing issues.

---

## Routing & Prompt Assessment

No prompt changes needed. Routing was correct today:
- High-volume fix tasks routed to `claude/medium` — appropriate
- Review tasks routed consistently
- No router timeouts observed in the second half of the day (the first retro noted one
  timeout; none since)

The `router.timeout_seconds: 120` → `60` suggestion from the first retro still stands but
is non-urgent. No issue filed.

---

## Performance Notes

- **Worktree cleanup**: `af3927a` will reduce disk accumulation. No perf metrics yet (requires
  service restart to observe). Worth checking `~/.orch/worktrees/` disk usage tomorrow.
- **Webhook**: no fallback mode entries today. Webhook stable.
- **Review throttle**: no queue buildup with 2 concurrent max.

---

## Tomorrow's Priorities

1. **Monitor**: confirm no new infinite loops emerge from today's fixes. Check
   `/opt/homebrew/var/log/orch.log` for any `review_agent_failures` or `pr_create_failures`
   log lines that indicate the counters are being hit.

2. **Verify worktree cleanup**: check `ls ~/.orch/worktrees/orch/` to confirm stale blocked
   task worktrees were removed by `af3927a`.

3. **is_pr_merged error patterns**: scan `src/engine/` for other `unwrap_or(false)` or
   `unwrap_or(true)` on `Result` types where silent defaults could mask API errors. Low
   urgency — no new loops observed — but a worthwhile code review pass.

4. **Router timeout reduction**: if router timeouts recur, reduce `router.timeout_seconds`
   from 120 → 60 in config. Fast fallback is better than 2-minute wait.
