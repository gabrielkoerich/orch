+++
title = "Morning Review — 2026-03-16"
date = 2026-03-16T10:00:00Z
[extra]
job = "morning-review"
+++

## Summary

Clean state going into today. Yesterday closed its issue backlog entirely —
the first time in recent history. Three more fixes landed overnight, all
addressing race conditions and correctness issues in the dispatch and cleanup
pipeline. System is healthy with no stuck tasks and no open issues.

---

## Recent Changes (last 24 hours)

| Commit | Description |
|--------|-------------|
| `a6d8b9a` | fix: add dispatch_key to dispatching set when sync spawns review agent (#646) |
| `610c5c4` | fix: only pull main when cleanup actually removes a worktree (#643) |
| `e86c75c` | fix: load jobs per project instead of from single global path |
| `1a62223` | docs: evening retrospective 2026-03-15 (#642) |

---

## System Health

- **Running tasks**: `internal:1139` (this review) — everything else idle
- **Open GitHub issues**: 0
- **Stuck tasks**: none
- **Error log** (`orch.error.log`): stale from March 1, no current errors
- **Tests**: all passing (`cargo test`)
- **CI**: green

---

## Key Fix Analysis

### `a6d8b9a` — dispatch_key missing when sync spawns review agent

This was the most significant fix of the batch. When `sync_tick` (step 5) spawned
a review agent for a `NeedsReview` task, it did not insert the task's `dispatch_key`
into the in-flight set. This created a window where `review_open_prs` (step 4)
could simultaneously re-dispatch the same task — re-routing it to `New` while the
review agent was still running. The consequence: human `CHANGES_REQUESTED` feedback
would be silently discarded when the review agent completed and marked the task Done.

Fix: mirror the insert-before/remove-after pattern already used in `tick.rs`.
The fix is correct and minimal (13 lines). No regression test exists for this
race condition — adding one would prevent re-introduction.

### `610c5c4` — spurious git pulls on every tick

`cleanup_task_worktree_with_opts` previously always returned `Ok(())`. The batch
janitor and inline cleanup paths then unconditionally triggered `git pull` after
calling it. This caused a git pull on every tick even when no worktree was actually
removed (TTL guard or active session guard blocked cleanup). Fix: return `Ok(true)`
only when real cleanup happened, `Ok(false)` otherwise. Pulls now only happen when
cleanup actually runs.

### `e86c75c` — jobs only loading for first project

`resolve_jobs_path()` returned the first `.orch.yml` found, so only the first
project's jobs ever fired. The `bean` project's jobs were silently ignored. Fix:
each `ProjectEngine` now resolves jobs from its own `.orch.yml` per tick.

---

## Yesterday's Priorities — Status

| Priority | Status |
|----------|--------|
| Verify SSH push auth end-to-end (bean project) | No bean tasks dispatched overnight to verify. Watch first bean dispatch. |
| Watch review gate stability (three-layer 422 fix) | `internal:1127` went through full review cycle cleanly — approved and merged. |
| Code-development prompt gap (`gh issue list --state closed`) | Not yet addressed. Engine deduplicates at dispatch level, so this is belt-and-suspenders. Low priority. |

---

## Log Observations

The `orch.error.log` contained 651 lines of "no valid projects configured"
errors — all from March 1 (stale, from an old version). No current errors.
The main `orch.log` shows healthy task lifecycle: route → dispatch → agent →
review → merge for all tasks today.

One `WARN` pattern appears repeatedly but is benign:
```
kill-session failed (may already be dead) session="orch-orch-<id>-review"
```
This fires when the review session exits before the engine tries to clean it up.
Harmless — the session is already gone. Not worth filing an issue.

---

## Test Coverage Gap

The `a6d8b9a` fix addresses a serious race condition (human review feedback
silently lost) but has no unit test. A targeted regression test of the
dispatching-set insert/remove lifecycle in `sync.rs` would prevent
re-introduction. Low urgency since the fix is in, but worth adding.

---

## Open Items

**None filed today.** The system is clean.

Items to monitor:
1. **bean project dispatch**: next bean task is the integration test for SSH push
   auth (`7d0b14f`) and per-project job loading (`e86c75c`). Watch for push errors.
2. **Review gate**: `a6d8b9a` landed today. Monitor next few review cycles for
   any timing edge cases.

---

## Issues Filed

None.

---

## Tomorrow's Priority

1. **Verify bean project end-to-end**: SSH push + per-project jobs are both fixed.
   The first bean task dispatch validates both fixes simultaneously.
2. **Consider regression test for dispatch_key race** — low urgency but clean win
   for preventing silent feedback loss in the future.
3. **Code-development prompt gap** — check and update if the prompt doesn't
   already mention `gh issue list --state closed --since 24h`.
