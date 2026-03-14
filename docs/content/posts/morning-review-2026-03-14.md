+++
title = "Morning Review — 2026-03-14"
date = 2026-03-14T10:30:00Z
[extra]
job = "morning-review"
+++

## Summary

Clean overnight session. Three targeted reliability and performance fixes landed on main. CI is green, no open issues, no open PRs. Service restarted and dispatched internal:57, :58, :59 this morning at 10:09 UTC. System health is good.

---

## Recent Changes (last 24 hours)

| Commit | Description |
|--------|-------------|
| `edb92f9` | fix: CI failure re-routes to New (not NeedsReview) and skip redundant cleanup API calls |
| `6496f32` | ci: switch to nextest and consolidate test steps (#580) |
| `9cadad9` | fix: skip API calls for closed issues already labeled status:done |

Three commits in 24 hours. All reliability and performance fixes — no new features.

**`edb92f9` detail**: When a CI check fails on a task's PR, the task now re-routes to `New` (not `NeedsReview`). Previously routing to `NeedsReview` caused human-attention noise for what is really an automated retry situation. Also skips redundant GitHub API calls during cleanup for tasks already in terminal states.

**`6496f32` detail**: CI now uses `cargo nextest` (faster, better output) and consolidates test steps to reduce total CI time. This matches the local dev workflow in CLAUDE.md.

**`9cadad9` detail**: Closed issues already labeled `status:done` no longer trigger GitHub API calls for label/comment updates. Pure optimization — reduces API quota usage for completed tasks.

---

## Evening Retro Priorities: Status

Previous evening retro context was not captured in a dedicated issue (last issue was from 2026-03-13 evening). Morning review 2026-03-13 noted:

| Item | Status |
|------|--------|
| PR #559 (counter-reset additional paths) | ✓ Merged (part of recent fix chain) |
| PR #562 (SQLite store, Phase 1–5) | Status unknown — not in open PRs list |
| Router timeout 120s → 60s | Not addressed — low priority |

---

## Health Check

**CI**: Green. Last run: `success` at 2026-03-14T03:22:33Z.

**Open Issues**: Zero.

**Open PRs**: Zero.

**Service**: Running. Engine dispatched 3 tasks at 10:09:53 UTC today:
- `internal:57` (opencode, fix task — existing worktree reused)
- `internal:58` (claude sonnet, code development)
- `internal:59` (claude sonnet, this morning review)

**Rebase skipped warning**: The engine logs show `skipping rebase: branch has too many commits to replay safely (commit_count=436, max=50)`. This is for the main branch history depth check — benign, expected as the repo grows. The 50-commit max is a safety guard to prevent long rebases on agent branches.

**Error log**: `orch.error.log` contains repeated `"no valid projects configured"` errors. These come from `orch` CLI invocations run outside the project directory (e.g., from worktrees or other directories). Benign — the brew service and engine are unaffected.

---

## Issues Filed

**None.** No new root-cause issues identified. System is healthy.

---

## Notes

**Router timeout**: Still at 120s default. The 2026-03-13 retro recommended 60s. Low-risk one-line change in `src/engine/router/config.rs:24`. Not worth a dedicated issue — any agent touching that file can reduce it inline.

**PR #562 (SQLite task_runs audit trail)**: Was a large draft (4442+ lines) in progress as of 2026-03-13. No longer appears in open PRs — either merged or closed. Worth verifying if the SQLite `task_runs` table is now live.

**Nextest migration**: CI now uses nextest (#580). Local dev should match: `cargo nextest run` is the correct command (documented in CLAUDE.md). `cargo test` still works as fallback.
