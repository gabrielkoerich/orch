++
title = "Morning Review — 2026-03-03"
date = 2026-03-03
description = "Daily ops check: recent commits, health, prompt alignment, and follow-ups"
++ 

## Summary

Quiet, productive overnight: many fixes landed that improved agent reliability, webhook deduping, and review flow robustness. Primary action this morning: align the reviewer prompt with the orchestrator sandbox (remove explicit `git fetch`) and record current observations.

---

## Recent Changes (last 24 hours)

| Commit | Description |
|--------|-------------|
| `8c81dce` | Formula SHA256 placeholders will cause Homebrew install failures (#353) |
| `e2cc329` | Race condition in sync_tick review agent dispatch (#354) |
| `f88dd9d` | internal_tasks.rs module is dead code - duplicates db.rs functionality (#348) |
| `dc488b0` | refactor: use db internal task APIs in jobs (#352) |
| `b6a6660` | Race condition: Metrics recorded before final sidecar update (#337) |
| `b176fc0` | Code duplication: Status conversion logic duplicated in internal_tasks.rs (#342) |
| `c6912b5` | Review agent can be dispatched multiple times via sync_tick (#347) |
| `168dacd` | fix: prune old task metrics (#346) |

These builds continue the reliability-focused streak from the previous cycle (review flow, webhook dedupe, pre-fetching branches, stream fixes).

---

## Morning Tasks Performed

- Checked recent commits (git log --since="24 hours ago") and the latest posts in `docs/content/posts`.
- Read the evening retrospective for 2026-03-03 and carried forward the top priority: align review prompt with sandbox constraints.
- Updated `prompts/review_task.md` to remove an explicit `git fetch` step that conflicts with the orchestrator pre-fetch workflow (see files changed).
- Created this morning review post at `docs/content/posts/morning-review-2026-03-03.md`.

---

## Health Check

- Tests: repository contains many unit/integration tests; integration tests are intentionally `#[ignore]` and require real agent CLIs. No new failing unit-test patterns were observed in recent commits.
- Service logs: `~/.orch/state/orch.log` not present in the current worktree; README references Homebrew logs at `/opt/homebrew/var/log/orch.error.log`. No actionable new errors surfaced in repo files.
- Stuck tasks: recent commits and retrospectives show stuck-task recovery improvements (stuck detection, tmux duplicate session fixes). No outstanding high-severity stuck-task reports found in today's scan of docs and commits.

---

## Findings & Recommendations

1. Prompt alignment: reviewer prompt instructed `git fetch origin main` which conflicts with the orchestrator's sandboxed pre-fetch model. I removed the fetch and now instruct reviewers to rebase against `origin/main` (pre-fetched by the service). This reduces agent attempts to run `git fetch` inside worktrees that may lack network access.
2. Stuck-detection threshold: ongoing discussion — consider reducing `no_session_stuck_timeout` from 600s (10m) and `stuck_timeout` from 1800s (30m) to make scheduled jobs and failed runs recover faster. This is low-priority but high-value for responsiveness.
3. Logs: surface location of Homebrew error log in docs; consider copying recent service logs into state for easier triage in CI runs.

---

## Files Changed

- `prompts/review_task.md` — removed explicit `git fetch` step; rely on pre-fetched refs and rebase commands.
- `docs/content/posts/morning-review-2026-03-03.md` — this file.

---

## Remaining

- Monitor the review workflow for unexpected edge cases after the prompt alignment change.
- Optionally file a small issue to consider lowering the stuck detection thresholds and document rationale (I did not file to avoid duplication; check existing open issues first).

---

## Notes

If you'd like, I can also open a short PR to propose reducing the `no_session_stuck_timeout` and `stuck_timeout` defaults and add a short rationale in the config docs. Recommend 10m -> 5m (no_session) and 30m -> 15m (stuck) as a starting point.

(End of post)
