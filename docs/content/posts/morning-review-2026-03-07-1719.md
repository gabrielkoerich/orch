+++
title = "Morning Review — 2026-03-07 (attempt 2)"
date = 2026-03-07
description = "PRs #515/#516 merged; rebase-on-unstaged fix; 2 issues in progress"
+++

## Summary

Service recovered from 4.5h downtime at 17:06 UTC. PRs #515 (throttle review agents) and #516
(show agent output in logs) both merged before this session. Two issues (#517, #518) are in progress
by `internal:33` (code review). This session fixes a recurring `rebase failed — unstaged changes`
warning by stashing before rebase.

---

## Recent Commits (last 24h)

| Commit | Description |
|--------|-------------|
| `f07f5f2` | fix: add V4 migration to clean up corrupt `updated_at` timestamps in SQLite |
| `bc6837d` | fix: skip cooled-down router LLM agent and detect rate limits |
| `407cd6d` | fix: use configured default branch in merge-conflict rebase |
| `c62ce12` | fix: rebase worktree on merge conflict instead of re-triggering review |
| `3553c5f` | fix: handle codex NDJSON format in review response parser |

---

## Evening Retro Carry-Forward (internal:35)

The evening retro identified:
1. **internal:29 analysis loop** — job body already rewritten to require real code commits
2. **SQLite corrupt `updated_at`** — fix committed (`f07f5f2`), needs brew upgrade to deploy
3. **Tomorrow priorities**: #441 (`orch task unblock` for internal IDs), #448 (engine health checks)

---

## Log Analysis

### Network interruptions (non-actionable)
Service hit GitHub-unreachable retries at 04:38, 06:16, 07:28, 10:24, 12:21 UTC — Mac sleeping
overnight. Backoff worked correctly; engine recovered at 12:34 UTC.

### Corrupt `updated_at` warning (fix pending deploy)
Still firing every tick. Fix committed in `f07f5f2` (V4 DB migration). Will resolve once the next
brew release is deployed.

### Rebase failed — unstaged changes (fixed this session)
```
WARN rebase failed, aborting and continuing with current state
err=error: cannot rebase: You have unstaged changes.
```
Fired at 17:13:14 (internal:33) and 17:14:46 (internal:35) — both were tasks restarted after the
service restart killed them mid-run, leaving leftover unstaged changes in the worktree.

**Root cause**: `rebase_on_default()` in `src/engine/runner/git_ops.rs` runs `git rebase` without
first stashing changes. Any worktree killed mid-run (or during service restart) has stale unstaged
changes that block the rebase.

**Fix**: Added `git stash --include-untracked` before rebasing, `git stash pop` after. Uses the
existing `has_changes()` helper — no new dependencies. Non-fatal path unchanged.

### internal:29 (code-development) — round-robin fallback
Still hitting max LLM router attempts (3) and falling back to round-robin on every dispatch.
Job body was updated to require real commits — next dispatch will test the fix.

---

## Open Issues

| # | Title | Status |
|---|-------|--------|
| #518 | `review_open_prs` ignores internal tasks in InReview | in_progress |
| #517 | `format_task_ref` not used in auto_commit/missing-PR body | in_progress |

Both handled by `internal:33` (code review task). No duplicates to file.

---

## Code Fix Applied

**`src/engine/runner/git_ops.rs` — stash before rebase** (this session):
- `rebase_on_default()`: stash unstaged changes before rebasing, pop after
- Prevents "cannot rebase: You have unstaged changes" on restarted tasks
- All CI checks pass (`fmt`, `clippy`, `cargo test`)

---

## Tomorrow's Priorities

1. **Deploy f07f5f2** — brew upgrade will clear the corrupt `updated_at` warning
2. **#441** — `orch task unblock` for internal task IDs (operationally urgent)
3. **Watch internal:29** — should now make code commits with updated job body
4. **#517/#518** — follow internal:33 to completion
