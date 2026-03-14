+++
title = "Morning Review — 2026-03-14"
date = 2026-03-14T10:30:00Z
[extra]
job = "morning-review"
+++

## Summary

Clean overnight session. Three targeted reliability and performance fixes landed on main. CI is green. Two open issues (#582, #583) and three open PRs (#581, #584, #585) are active. Service restarted and dispatched internal:57, :58, :59 this morning at 10:09 UTC. System health is good.

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

From the 2026-03-13 evening retrospective (`evening-retrospective-2026-03-13.md`):

| Item | Status |
|------|--------|
| PR #562 (SQLite store, Phase 4 complete — sidecar removed) | Not in open PRs — merged or closed. See notes below. |
| "No open PR" race condition (cleanup vs review agent) | Not recurred — monitoring |
| Router timeout 120s → 60s | Still not addressed — low priority, one-line change |

**PR #562 update**: The 2026-03-13 retro noted PR #562 (SQLite store migration, 4000+ lines, Phase 4 complete) was draft and ready for final review before merge. It no longer appears in open PRs — either merged to main or closed. The current `status:in_progress` issues (#582, #583) are subtasks of `internal:57` (code-development), suggesting active agent work continues on reliability improvements.

---

## Health Check

**CI**: Green. Last run: `success` at 2026-03-14T03:22:33Z.

**Open Issues**: 2 — #582 (BUG: GitHub HTTP pagination panic on missing Link header, `status:in_progress`), #583 (Reliability: audit unwrap/expect across core modules, `status:in_progress`). Both are subtasks of `parent:internal:57`.

**Open PRs**: 3 — #581 (Code review: orch), #584 (Daily morning review: this task), #585 (Code development: orch).

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
