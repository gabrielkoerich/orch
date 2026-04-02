+++
title = "Morning Review -- 2026-04-01"
date = 2026-04-01T11:30:00Z
[extra]
job = "morning-review"
+++

## Summary

Strong 24h with **22 commits landed**, focused on routing reliability (reroute skipping failed agents, GraphQL error surfacing, GitHub 5xx transient handling) and system hardening (panic fixes, session auto-cleanup). Zero open GitHub issues. The pipeline is healthy with ~82% agent success rate (134/163 runs) and no blocked tasks. Outstanding: the `opus/` model assignment bug flagged in yesterday's retro has not yet been resolved.

---

## Recent Activity (Last 24h)

### Routing & Dispatch Reliability (4 fixes)

- `fix: reroute should skip the previously failed agent` — #1493
- `fix: clear both agent and model on reroute` — #1492
- `fix: add error logging to store_reset_failure_counters` — #1475
- `fix: reset no_code_reroutes on auto-unblock and fix wrong field name` — #1475

### GraphQL & GitHub API (4 fixes)

- `fix: surface GraphQL partial errors in batch_is_pr_merged_by_branch` — #1485
- `fix: remove invalid escaping in batch GraphQL queries` — #1480
- `fix: skip transient GitHub 5xx errors in review subscriber` — #1484
- `fix: surface GraphQL errors and log response body` — #1483

### System Reliability (5 fixes)

- `fix: replace all unsafe .get() with .try_get()` in store layer — #1472
- `fix: prevent overflow in exponential backoff` — #1463
- `fix: mark_cleaned() failures now logged` — #1477
- `fix: avoid blocking Tokio worker in free_models() cache refresh` — #1479
- `feat: auto-cleanup sessions and worktrees on task close` — #1460

### Agent Transport & Sessions (3 fixes)

- `fix: double-push in TmuxChannel/CaptureService transport` — #1464
- `feat: one-shot --session-id for chat sessions` — #1464
- `fix: use --append-system-prompt-file for chat persistent sessions` — #1463

---

## Operational Health

### Agent Performance (last 24h, 163 total runs)

| Agent | Model | Success | Failed | NULL | Rate | Notes |
|-------|-------|---------|--------|------|------|-------|
| opencode | gpt-5-mini | 34 | 0 | 1 | 97% | Healthy primary |
| claude | sonnet | 33 | 2 | 1 | 92% | Workhorse |
| minimax | opus | 22 | 3 | 3 | 79% | Some NULLs (cleanup races) |
| claude | opus | 17 | 0 | 2 | 89% | Solid |
| kimi | opus | 13 | 0 | 0 | 100% | Excellent |
| claude | haiku | 7 | 0 | 0 | 100% | Simple tasks |
| opencode | opus | 0 | **6** | 0 | 0% | **Model assignment bug** |

**Overall: ~82% success** (134 success / 163 runs). Opencode/opus is the only notable cluster of failures (6 failures, all `failed` outcome) — tied to the `opus/` bug.

### Task Activity (last 12h)

- 872 status changes, 229 dispatches, 186 pushes, 162 branch deletes
- 103 review starts, 93 review decisions, 75 PR creates
- 40 errors (low relative to throughput)
- 1 auto-unblock

### Orch Doctor Warnings

- **#1500**: label mismatch — SQLite=`in_progress`, GitHub labels=`none`. Non-blocking, likely a sync lag.
- **Orphan worktree**: `/Users/gb/.orch/worktrees/bean/internal-31298-morning-briefing-daily-focus-plan` — no task owns it. Created by bean morning briefing job but task appears done/cleaned. Should be pruned.

### Log Errors

**Stale git worktree metadata** (brew error log, benign but noisy):
```
fatal: not a git repository: /Users/gb/Projects/orch/.git/worktrees/gh-issue-1005-...
fatal: not a git repository: /Users/gb/Projects/orch/.git/worktrees/gh-issue-1130-...
fatal: not a git repository: /Users/gb/Projects/orch/.git/worktrees/gh-issue-1251-...
```
These are old worktree metadata entries from the user's main orch project. The `git worktree prune` at startup runs in `~/.orch/worktrees` but not in the user's project directory. Low priority — a prune in the project dir would clear it.

---

## Stuck / Blocked Tasks

No blocked tasks. All current tasks are healthy:

| ID | Status | Agent | Age | Notes |
|----|--------|-------|-----|-------|
| #1500 | in_progress | opencode | 24s | Plain-text synthesis bug fix |
| internal:31297 | in_progress | minimax | 11s | Morning review (this task) |
| internal:31298 | in_progress | opencode | ~2h | Bean morning briefing |

---

## Retro Follow-ups (from 2026-03-31 Evening Retro)

| Item | Status |
|------|--------|
| `opus/` model assignment bug | **Not fixed** — 6 opencode/opus failures in last 24h |
| SQLite OOB panics (index 56) | **Still occurring** — 10 panics in error log |
| Morning review jobs blocked | ✅ Resolved — internal:31297 and internal:31298 are running |
| Minimax review parse failures | Monitor — 3 NULL outcomes, not confirmed parse-related |
| Stale worktree metadata | **Not fixed** — continuing noise |

---

## Today's Priorities

1. **Fix `opus/` model assignment bug** — 6 failures yesterday, root cause identified in retro. Most impactful fix remaining.
2. **Expand `.try_get()` coverage for SQLite OOB panics** — 10 panics in last 24h across sqlx workers. Despite #1472 landing, the panics continue.
3. **Prune orphan bean worktree** — `internal:31298` worktree at `~/.orch/worktrees/bean/` is orphaned. File issue or clean up.
4. **Monitor for new SQLite panics** — ongoing issue, needs root cause investigation beyond column count.
5. **Stale worktree metadata in main project** — `git worktree prune` in `~/Projects/orch/` would clear the brew error log noise.

---

## Open GitHub Issues

**0 open issues** — The queue has been clear since yesterday's sweep.
