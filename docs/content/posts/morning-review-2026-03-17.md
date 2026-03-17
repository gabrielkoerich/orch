+++
title = "Morning Review — 2026-03-17"
date = 2026-03-17T10:00:00Z
[extra]
job = "morning-review"
+++

## Summary

Strong day yesterday — 13 commits, 12 PRs merged — with the delegation dedup
flood fully contained. Today opens with one active bug: `internal:1993` was
spinning in an infinite retry loop due to a 422 "No commits between" check gap
in the response handler. Fixed this morning. Three stale duplicate PRs (#657,
#663, #672) closed as cleanup. System otherwise healthy.

---

## Recent Changes (last 24 hours)

| Commit | Description |
|--------|-------------|
| `12ecb2b` | fix: auto_merge_pr returns Err when verify finds changes_requested (#682) |
| `2a68ed9` | fix: auto_close_task_on_approval and workflow.auto_merge inconsistency (#681) |
| `58220a8` | fix: reset route_attempts for stuck internal tasks and invalidate skills cache (#680) |
| `457c5bb` | docs: evening retrospective 2026-03-16 (#677) |
| `e67377a` | fix: dedup delegations to prevent duplicate GitHub issues |
| `bce527b` | fix: scrub GH_TOKEN from tmux global env after session cleanup |
| `249d40e` | Audit and replace panics/unwraps — runtime-critical modules (#669) |
| `698bf32` | Remove unnecessary `unsafe` env manipulation in tests (#659) |

---

## System Health

- **Running tasks**: `internal:3129` (this review), `internal:1993` (spinning — see below)
- **Open GitHub issues**: 0
- **Stuck tasks**: `internal:1993` — infinite retry loop (fixed this session)
- **Error log** (`orch.error.log`): stale March 1, no current errors
- **Tests**: 685 passed, 12 skipped
- **CI**: green

---

## Bug Fixed This Morning

### `internal:1993` — infinite review retry loop (no-commit 422)

**Root cause**: When an agent completes a task with no code changes (e.g.
responding to a GitHub mention, analysis-only work), the branch has no diff
from main. PR creation returns GitHub 422 "No commits between main and
`gh-task-internal-1993-...`".

The existing 422 guard only checked for `"head"` in the error string (covering
the already-merged case). The "No commits between" message doesn't contain
`"head"`, so `has_pushed` stayed `true` → status set to `needs_review` → review
engine saw no PR and no commits → re-routed to `New` → infinite retry.

**Fix** (`fed6511`): extend the 422 check to match `"No commits between"`,
clearing `has_pushed` so the task falls through to the `"done"` path directly.

```rust
// Before (only caught already-merged case):
if err_str.contains("422") && err_str.contains("head")

// After (also catches no-commits case):
if err_str.contains("422")
    && (err_str.contains("No commits between") || err_str.contains("head"))
```

**Impact**: Any task completed without code changes (mention responses, analysis
jobs, read-only investigations) was silently retrying forever instead of closing.

---

## Cleanup Done

Closed three stale duplicate PRs left over from the delegation flood:
- PR #657 — superseded by #659 (merged)
- PR #663 — superseded by #669 (merged)
- PR #672 — superseded by #659 (merged)

All three closed with explanatory comments.

---

## Yesterday's Priorities — Status

| Priority | Status |
|----------|--------|
| Close stale PRs (#657, #663, #672) | ✅ Done this session |
| Bean project end-to-end verification | Not done — no bean task dispatched |
| Dispatch_key race regression test | Not done — 4th carry-over |
| Code-development prompt gap (closed issues) | Not done |

---

## Log Observations

`orch.log` shows `internal:1993` caught in the spin:
```
WARN agent done, commits pushed, but PR creation failed — routing to review gate
WARN no PR and no commits — re-routing for retry
```
Repeated every ~20 seconds until this session. No other error patterns.

---

## Test Coverage Gap (still open)

The `a6d8b9a` dispatch_key race fix still has no regression test. Low urgency
but represents a "silent human feedback loss" scenario. One targeted test in
`sync.rs` would prevent re-introduction.

---

## Issues Filed

**None.** Root cause fix landed in-session (`fed6511`).

---

## Tomorrow's Priority

1. **Verify `internal:1993` clears** after the fix deploys — watch it transition
   to `done` on the next run rather than re-routing.
2. **Bean project dispatch** — Fourth carry-over. SSH push auth + per-project
   jobs both fixed; first bean task is the integration test.
3. **Dispatch_key regression test** — Low urgency, clean win to prevent
   silent review feedback loss from `a6d8b9a`.
4. **Code-development prompt gap** — Add `gh issue list --state closed --since 24h`
   next time the prompt is edited.
