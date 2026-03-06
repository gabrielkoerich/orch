+++
title = "Morning Review — 2026-03-06"
date = 2026-03-06
description = "Critical bug fixed: empty repo in PR creation causes infinite retry loop for mention-response tasks"
+++

## What landed since last review (24h)

| Commit | Description |
|--------|-------------|
| `8709483` | docs: consolidate 'orchestrator' → 'orch' naming (#490) |
| `d5cb836` | fix: create child task for review changes instead of re-dispatching same task |
| `94a9e22` | fix: harden worktree janitor with TTL, tmux guard, dry-run (#488) |
| `b4fc571` | ci: add comments documenting each job in release.yml (#491) |
| `bd4c678` | Finalize channel bidirectional wiring and output fanout (Phase 3) (#486) |

Strong day yesterday. Phase 3 channel wiring landed, the PTY removal was completed, worktree janitor hardened.

---

## Findings

### Critical: Infinite retry loop for "respond to mention" tasks

**Root cause**: `get_current_repo()` returns `""` when called from the engine service process (CWD = `/`). The function walks up from CWD looking for `.orch.yml`, finds nothing, then falls back to `get("gh.repo")` which also fails because the global config (`~/.orch/config.yml`) only has `projects:` pointing to the project path — it doesn't embed `gh.repo` directly.

Result:
1. Agent completes (status=done), pushes commits to branch
2. `create_pr_if_needed` calls `get_pr_number("", branch)` → `GET https://api.github.com/repos//pulls` → 404
3. `has_pushed=true`, `has_pr=false` → response_handler routes to `needs_review`
4. Review gate: no PR found, no commits on branch vs main → re-routes to `new`
5. Loop repeats indefinitely, burning agent tokens

Affected tasks: internal:16, 17, 18, 19 (all "respond to mention" tasks that made no code changes).

**Fix applied** (`src/config/mod.rs`): Added final fallback in `get_current_repo()` to iterate `get_project_paths()` and return the first project's `gh.repo`. This resolves the empty-repo bug for the engine service context without requiring `.orch.yml` in CWD.

**Manual cleanup**: Marked tasks 1-7 (old stuck `needs_review` mention tasks with no worktrees) and tasks 16-19 (active retry loop) as `done` in SQLite.

### Issue #483: PLAN.md merge markers

Still open, assigned to codex which hit a rate limit. Will pick up this tick.

---

## Open Issues

| # | Title | Status |
|---|-------|--------|
| #483 | Resolve PLAN.md merge markers | needs_review (codex rate limited) |

All other issues from yesterday's retro (#441, #446, #448, #431, #435, #443) appear to be resolved or merged.

---

## Tomorrow's Priorities

1. Monitor that the `get_current_repo()` fix prevents the retry loop after deploy
2. Verify #483 gets through review after the rate limit clears
3. Reduce `no_session_stuck_timeout` from 600s → 300s (faster stuck detection)

---

## System Health

- **Tasks stuck in retry loop**: Fixed (tasks 1-19 mention tasks cleaned up)
- **Empty repo PR creation**: Fixed in code (deploy required)
- **Codex rate limit**: Transient, clears automatically
- **Service logs**: One recurring merge conflict on internal:13 (405 not mergeable) — stale PR, will auto-expire
