+++
title = "Evening Retrospective — 2026-03-23"
date = 2026-03-23T23:00:00Z
[extra]
job = "evening-retrospective"
+++

## Summary

An unexpectedly massive day — **20 issues closed, 31 commits** landed after the morning
review was written. The earlier draft of this post said "complete cool-down day" but that
was written during a brief idle window before the pipeline fired. Today ended up being one
of the highest-velocity days on record, matching or exceeding yesterday's 17-issue sweep.

Major themes: notify subscriber overhaul, dispatching guard key-leak fixes, streaming NDJSON
for real-time agent output, event-driven architecture additions, PR creation 422 race fixes,
and job scheduler reliability.

---

## What Was Done Today

### Issues Closed (20)

| Issue | Summary | Agent |
|-------|---------|-------|
| #851 | PR-creation 422 retry path counted as failure | opencode |
| #850 | `/block` command dropped the provided reason | opencode |
| #847 | feat: streaming NDJSON output for real-time visibility | claude |
| #844 | notify subscriber fired on ALL status transitions (noisy) | claude |
| #843 | notify subscriber + tick.rs both called `push_notification` — duplicate messages | claude |
| #841 | feat: interactive project picker for General channel | claude |
| #840 | notify subscriber hardcoded `duration_seconds=0.0` — all notifications showed "0s" | claude |
| #837 | `resolve_task_id` fallback query missing repo filter — wrong task in multi-repo | claude |
| #836 | dispatching key leaks when semaphore full or session exists — Routed tasks stuck | claude |
| #833 | dispatching HashSet key leaks if review task panics — task stuck in review loop | claude |
| #832 | `update_task_status` no-ops silently for unfound internal tasks, publishes phantom event | claude |
| #830 | worktree and branch not cleaned up for no-op tasks | claude |
| #827 | feat: event-driven parent unblocking — reduce latency from 10s tick to near-zero | claude |
| #826 | notify subscriber sent empty title/summary — channel notifications missing context | claude |
| #824 | Reviewer agent commented twice on same PR | claude |
| #820 | `create_pr` failed 422 when branch not yet pushed to remote | claude |
| #817 | `cron normalize_dow` mapped `0-5` to invalid range `7-5` — Sunday DOW ranges broken | claude |
| #816 | bash job `dir` resolved relative paths against process CWD (`/`) instead of project | claude |
| #815 | `load_jobs()` didn't validate cron schedule syntax — invalid schedules failed silently | claude |
| #814 | perf: add (repo, status) index — `list_by_status()` couldn't use existing compound index | claude |

### Key Commit Themes

**Notify subscriber overhaul** (#826, #840, #843, #844): The notification system had four
separate bugs that together meant every channel notification was wrong — missing context,
zero duration, duplicate messages, and firing on non-terminal status transitions. All four
fixed in the same session. Notifications are now accurate and quiet.

**Dispatching guard key leaks** (#833, #836): The `dispatching` HashSet guards against
double-dispatch, but two separate paths leaked keys permanently — once when the semaphore
was full and once when a review task panicked. Both left tasks stuck in `Routed` forever.
The atomic check-and-insert (#838, `533a39b`) was the final hardening step.

**Streaming NDJSON** (#847, #849): Agents now stream stdout to the tmux pane in real-time
via `tee` instead of capturing to a variable. Two follow-up fixes (#848/#843: duplicate
notifications; #852/#851: 422 false-failure) were needed — both closed same day. This is
a significant UX improvement for `orch stream`.

**PR creation 422 race** (#820, #823, #851): The branch→push→create_pr sequence had a race
where the PR was created before the push completed. #820 fixed the core timing issue, #823
cleaned up edge cases, and #851 ensured the 422 recovery path didn't count as a failure in
task metrics.

**Job scheduler reliability** (#815, #816, #817): Three cron/job bugs landed together —
invalid schedule syntax went undetected at load time, relative `dir` paths resolved to `/`,
and DOW range normalization produced invalid ranges. All are correctness fixes that would
have caused jobs to silently not run.

**Event-driven architecture** (#827, #828, `df74029`, `3220b0a`): A task event bus
(`TaskEvent`, `EventBus`) was added with websocket server and `TaskManager` wiring.
Parent unblocking moved from the 10s tick loop to an event subscriber, reducing latency
to near-zero. New CLI commands `orch events` and `orch task watch` expose the event stream.

---

## Analysis

### What Went Well

- **Notify subscriber caught and fixed completely**: Four related bugs (#826, #840, #843,
  #844) were identified and fixed within a single session. No partial fix — the whole
  subsystem was addressed at once. Channel notifications are now correct.

- **Dispatching guard hardened**: The key-leak bugs (#833, #836) were subtle concurrency
  issues. Both were caught and fixed with an atomic check-and-insert as the final guard.
  Routed-stuck tasks should now be extremely rare.

- **Streaming visibility shipped end-to-end**: The NDJSON streaming feature (#847) was
  followed immediately by two fixup PRs (#843, #851) that closed same-day. Fast iteration.

- **Router reads SQLite before labels** (`59c64aa`): The router now checks the SQLite
  `agent` field before falling back to GitHub labels. This closes a gap where internal
  tasks (no labels) would always re-route instead of reusing the prior agent assignment.

### What Failed or Needed Retries

- **The morning review was stale before it was published**: The morning review was accurate
  at the time, but the pipeline went from empty to 20 issues in a single burst. The job
  timing is working correctly — the burst of work just happened to land after the review
  window.

- **PR creation 422 took three PRs to fully resolve** (#820, #823, #851): The original
  fix missed the retry path counting as a failure. This is a pattern: fixes that touch
  the success path miss the error path. Worth watching.

- **opencode only handled 2 of 20 issues** (#850, #851 — both medium-complexity bugs):
  Agent distribution is still heavily weighted toward claude. The routing is making
  reasonable choices for Rust bug fixes, but opencode's share is very low. If this
  continues, consider whether the routing prompt adequately profiles opencode's strengths.

### Routing Accuracy

18/20 issues routed to claude, 2 to opencode. All closed successfully — no re-routes or
retries visible in the issue list. Complexity was uniformly `medium`. For a day dominated
by Rust bug fixes, claude routing is defensible. The two opencode tasks (#850, #851) were
also Rust fixes and completed cleanly.

### Performance / Operational

- **Event bus + parent unblocking**: A new shared event bus is now live. This is new
  shared mutable state — monitor for any panics or contention in the first few days.
- **No open issues**: Pipeline ended the day empty again. Two high-output days in a row.
- **Streaming tee overhead**: The switch from variable capture to `tee` slightly increases
  I/O per agent invocation. No perf issues observed but worth tracking at scale.

---

## Open Issues

None.

---

## Priorities for Tomorrow (2026-03-24)

1. **Monitor event bus stability** — The `TaskEvent`/`EventBus` and websocket server are
   new infrastructure. Watch `orch.log` for panics, contention, or subscribers falling
   behind. The parent-unblocking subscriber is on the critical dispatch path.

2. **Streaming NDJSON smoke test** — Run `orch stream` against a live task to verify
   real-time output appears correctly. The `tee` approach is new — confirm no buffering
   issues on longer-running agents.

3. **Channel routing smoke test** — Now 7+ days pending. Multi-project channel_handler
   fixes are all merged (#780, #783, #785, #837). Create a deliberate cross-project test
   task to verify end-to-end dispatch works correctly.

4. **Webhook re-enable** — Still in polling fallback. With the pipeline stable, this is
   a good time to re-enable and verify instant delivery.

5. **Auth classifier shared test** — Three-step fix for the 401/403 pattern (#781, #803,
   #811) suggests a shared test vector across all three agent formats. Low urgency but
   would prevent partial fixes from landing in future.
