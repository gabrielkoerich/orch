+++
title = "Morning Review — 2026-03-01 (22:16 UTC)"
date = 2026-03-01
description = "3rd attempt: fix cargo fmt on PR #233, add .opencode/ to .gitignore, diagnose stuck PRs"
+++

## Overview

Third morning review run for 2026-03-01. The system has been highly productive today — 30+ commits, 9 PRs merged, all major blocking bugs resolved. This run focuses on unblocking the remaining 5 open PRs and preventing rebase failures from `[branch ""]` config corruption.

---

## Evening Retrospective Carry-Forward

The evening retro (#249) identified these priorities:

| Priority | Status |
|----------|--------|
| Fix auto-merge retry logic | Partially addressed — `review_open_prs` simplified to mark-done only |
| Deploy latest fixes to brew | Pending — requires brew formula update and `brew upgrade orch` |
| Unblock PR #233 (cargo fmt) | **Fixed this run** — one-line `tracing::warn!` formatting |
| Merge PR #238 (approved, conflict) | Stuck — merge conflict, no rebase re-dispatch |

---

## Changes Applied This Run

### 1. Fix `cargo fmt` failure on PR #233

The CI check failed because `tracing::warn!` in `engine/mod.rs` used a multi-line macro invocation for two short arguments. `cargo fmt` wants it on one line.

**Before:**
```rust
tracing::warn!(
    delay_secs,
    "project engine init failed, retrying: {e}"
);
```

**After:**
```rust
tracing::warn!(delay_secs, "project engine init failed, retrying: {e}");
```

### 2. Add `.opencode/` to `.gitignore`

The engine's rebase-on-main step was failing with `"untracked working tree files would be overwritten by checkout"` because `.opencode/package.json` was left as an untracked file after the previous commit deleted it from tracking. This file is recreated by the opencode tool when it runs as an agent.

**Fix:** Added `.opencode/` to `.gitignore`. This prevents git from complaining about the file during rebase and keeps the directory out of commits.

### 3. Fixed `[branch ""]` git config corruption in main repo

The file `/Users/gb/Projects/orch/.git/config` had a stale `[branch ""] gh-merge-base = main` entry (line 364) that was blocking all `gh` CLI commands in the main repo. Removed the entry manually. This is a side effect of `gh issue develop` being called with an empty branch name — the root cause fix is in PR #240 (already merged), but stale entries can persist.

---

## PR Status

| PR | Task | Status | Issue |
|----|------|--------|-------|
| #233 | #225 | CI failing → **fixed** (cargo fmt) | `tracing::warn!` line length |
| #238 | #234 | CONFLICTING, auto-merge enabled | Merge conflict after main landed new commits |
| #246 | #228 | CI failing | `cargo fmt` drift — needs rebase on main |
| #252 | #231 | CI failing | `cargo fmt` in `src/cli/task.rs` |
| #253 | #244 | CI passing ✓ | Awaiting review merge |

**Root cause of #246 and #252**: Both PRs were created before recent formatting fixes landed on main. The agent re-dispatch mechanism (`a7f9174`) should trigger when CI fails and re-dispatch the agent to fix formatting. Check whether it's triggered for these tasks.

---

## System Health

**Tests**: 389 passing, 0 failed (unit). Integration tests skipped (require live agents).

**Engine**: Running. Tasks dispatching. Review agents failing on opencode NDJSON responses (see below).

**Error log highlights (last ~60 min)**:
- `auto-merge failed: no open PR found` for tasks #248, #241 — PRs were already merged, the old `auto_merge_pr` call in `review_open_prs` was a double-trigger. **Fixed** in this branch (simplified `review_open_prs` to mark-done only).
- `review agent error: invalid response: {"type":"step_start"...}` for task #244 — opencode agent used for review outputs NDJSON streaming format instead of plain JSON. Parser fails to extract the review decision.
- `review agent error: codex failed: WebSocket disconnected` for task #231 — codex review agent WebSocket error. Retry should reroute to claude.
- `rebase failed: untracked working tree files would be overwritten` for task #225 — caused by `.opencode/package.json` as untracked file. Fixed by adding `.opencode/` to `.gitignore`.

---

## Issue to File

**Root cause**: Approved PRs with merge conflicts are stuck indefinitely.

When a review agent approves a PR and `auto_merge_pr` enables GitHub auto-merge, subsequent commits to `main` can create a merge conflict. GitHub cancels auto-merge for conflicting PRs. Neither `review_open_prs` nor any other sync tick detects `mergeable: CONFLICTING` and re-dispatches the agent to rebase.

**Affected PR**: #238 (task #234) — approved, auto-merge enabled, but `CONFLICTING`.

**Fix**: In `review_open_prs`, when a PR is approved (`comment_approved || all_approved`) but `mergeable == CONFLICTING`, set task status back to `Routed` so the agent re-dispatches and resolves the conflict.

---

## Recommendations

1. **Brew deployment** — Commits `2fd899a` (codex fix), `a7f9174` (CI re-dispatch), `3c4b354` (rebase prompt), `23aea57` (crash-loop backoff) need to reach the brew binary. Update the Homebrew formula.
2. **File issue for CONFLICTING PR re-dispatch** — PR #238 is stuck; add `mergeable == CONFLICTING` check to `review_open_prs`.
3. **Opencode review NDJSON** — When opencode is the review agent, the NDJSON streaming output isn't parsed. Fix: extract last JSON object from NDJSON output, or switch review agent to claude exclusively.
