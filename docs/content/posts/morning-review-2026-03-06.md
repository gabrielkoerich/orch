+++
title = "Morning Review — 2026-03-06"
date = 2026-03-06
description = "Internal tasks 1-7 orphaned in needs_review — issue #458 tracks root cause"
+++

## Summary

The orchestrator is running well overall. Recent fixes (colon in tmux session names, PTY runner removal) are working. However, there's a cleanup bug affecting internal tasks that leaves orphaned worktrees and stuck `needs_review` statuses.

---

## Recent Changes (last 24h)

| Commit | Description |
|--------|-------------|
| `6492b49` | fix: route status updates through TaskManager in review pipeline (#468) |
| `c7dd596` | bug: find_existing_worktree uses unsanitized task_id — fails to find internal task worktrees on retry (#467) |
| `3ea62d1` | fix: add minimum age threshold before treating InReview tasks as stale (#465) |
| `e2838ed` | feat: wire bidirectional channel interaction and output fanout (#444) |
| `9a4f51d` | bug: tmux session names for internal tasks contain colons — sessions created with wrong name (#461) |

---

## Issues Found

### 1. Internal tasks 1-7 orphaned in needs_review (Root cause: #458)

**Symptom**: Internal tasks 1-7 are stuck in `needs_review` status but have no worktrees:

```
sqlite> SELECT id, status FROM internal_tasks WHERE id <= 7;
1|needs_review
2|needs_review
3|needs_review
4|needs_review
5|needs_review
6|needs_review
7|needs_review
```

**Logs show**:
```
WARN no worktree found for review task_id="internal:7"
ERROR review agent failed — resetting to NeedsReview for retry reason="no worktree found"
```

**Root cause** (tracked in issue #458): `cleanup_done_worktrees` only queries the GitHub backend, not internal SQLite tasks. Worktrees for internal tasks are never cleaned up after completion, but the cleanup process also doesn't properly mark them as done. This leaves stale entries in the database.

**This is NOT a new issue to file** — already tracked in #458.

### 2. GitHub issue #458 is open

- **Title**: "bug: internal task worktrees never cleaned up — disk space accumulation"
- **Root cause**: `cleanup_done_worktrees` in `cleanup.rs` only queries `backend.list_by_status(Status::Done)` — GitHub tasks only. Internal SQLite tasks are never included.
- **Fix needed**: Pass `db` handle to cleanup function, add second loop for internal done tasks

---

## What's Working Well

- **CI checks pass**: `cargo fmt`, `cargo clippy`, `cargo test` all green
- **Routing**: Task 469 routed to claude (complex) for Git rebase issue
- **Colon fix working**: tmux sessions now use hyphens (`orch-orch-internal-13` not `orch-orch-internal:13`)
- **PTY runner removed**: Issue #416 resolved, legacy tmux runner is canonical
- **Review agents spawning**: internal:10 (minimax), internal:13 (opencode) review agents running
- **Auth resolution**: TokenResolver singleton working (issues #418, #421 closed)

---

## Evening Retro Priorities Status

From yesterday's evening retrospective (2026-03-05):

| Priority | Status |
|----------|--------|
| Remove PTY runner (#416) | ✅ Done |
| Reduce stuck detection thresholds | ❌ Not done — still pending |
| Verify gitleaks fix | ✅ Verified working |
| Fix internal task agent sessions | ✅ Fixed (colon in tmux session name) |

---

## Tomorrow's Priorities

1. **Fix #458 — internal task worktree cleanup** — This is the highest priority. The fix is well-scoped:
   - Pass `db: &Arc<Db>` to `cleanup_done_worktrees`
   - Add `db.list_internal_tasks_by_status(TaskStatus::Done)`
   - Read worktree path from sidecar, call `cleanup_task_worktree`

2. **Reduce stuck detection thresholds** — Still pending from earlier retro
   - `no_session_stuck_timeout`: 600s → 300s
   - `stuck_timeout`: 1800s → 900s

3. **Clean up orphaned internal tasks 1-7** — After #458 is fixed, verify these get properly cleaned up

---

## No New Issues Filed

Issue #458 already tracks the root cause. This review identified the symptom (orphaned tasks 1-7) but the fix is already in progress.
