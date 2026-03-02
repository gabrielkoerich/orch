+++
title = "Morning Review — 2026-03-02"
date = 2026-03-02
description = "Record 24h commit velocity, healthy service, and one stale ignored test fixed"
+++

## Summary

Productive overnight: 13+ commits landed, service is healthy, and only 1 open issue (this task).
One stale `#[ignore]`d test was found and fixed. No new issues needed.

---

## Recent Changes (last 24 hours)

| Commit | Description |
|--------|-------------|
| `c934eeb` | fix: auto-close issues on done and correct no-PR status logic |
| `1dc75cb` | fix: scope sidecar state per-repo via task-local, cleanup remote branches |
| `1f8245d` | fix: remove hardcoded executor descriptions from route prompt |
| `3cee8fd` | fix: add mandatory pre-output checklist to prevent agents forgetting to push |
| `16d3903` | docs: update PLAN.md task lifecycle for status-driven review workflow |
| `4d1b31a` | chore: delete unused pr_review.md, fix stale status lifecycle in chat.md |
| `404b557` | refactor: rewrite review prompt for retry robustness |
| `95a2113` | refactor: status-driven review workflow — eliminate review_started flag |
| `4a41d9a` | refactor: extract LLM routing from router.rs into engine/llm_router.rs (#269) |
| `bb0e4f3` | feat: graceful restart — wait for running agents before restarting (#270) |
| `4c1f275` | fix: review agent handles merge conflicts instead of re-dispatching original agent |
| `5fb82de` | fix: preserve active_task_id on transient GitHub API errors in job tick (#266) |
| `2d801fb` | bug: branch_name() panics on non-ASCII task titles due to byte-offset truncation (#268) |

---

## Yesterday's Priorities (from retro)

| Priority | Status |
|----------|--------|
| Fix #248 — auto-merge gap | Done (merged 2026-03-01T22:07Z) |
| Deploy to brew — codex + CI fixes | Done (brew 0.5.2 built 09:20 today) |
| Unblock PR #233 — crash-loop backoff | Done (fix merged) |
| Merge PR #238 — approved and green | Done |
| Kill stale tmux sessions before creating new (#227) | Done (closed) |
| Consider reducing stuck-detection threshold | Not done — still 30 min, acceptable for now |

All retro priorities resolved.

---

## Health Check

### Tests
- **399 passing, 2 ignored** (integration tests requiring live API keys)
- Fixed: `integration_supervised_config_translates_correctly` had a stale assertion (`--ask-for-approval never`) — codex autonomous mode now uses `--full-auto`. Updated assertion to match current implementation.

### Service
- Brew service last restarted at 12:20 UTC. Running correctly.
- Launched task #271 (this task) at 12:26 UTC without issues.
- `orch.error.log`: 651 lines — all from the Feb 27 crash loop (gh not in PATH). Historical noise, not current issues.
- `orch.log`: Recent transient GitHub API failures at 11:21 UTC (502/connection errors) recovered automatically within ~17 minutes. Expected behavior.

### Open Issues
- Only #271 (this morning review task, in_progress). No backlog.

### Recent Failures Observed (log scan)
- 10:39 UTC: review agent for task #267 got invalid response (step_start JSON). Task #267 already closed — recovered.
- 10:41 UTC: task #257 stuck (30 min) → auto-recovered to `new`. PR for #257 merged successfully.
- 11:21 UTC: GitHub API blip (502/connection). Engine ticked through errors, recovered without manual intervention.

---

## Key Architectural Changes (this cycle)

### Status-Driven Review Workflow (95a2113)
The `review_started` sidecar flag (11 sites) has been replaced by status transitions:
- Agent done + PR exists → `needs_review` (was `in_review`)
- Engine spawns review on `NeedsReview`, transitions to `InReview` as guard
- On review failure → reset to `NeedsReview` (not stuck)
- Stale `InReview` with no tmux session → reset to `NeedsReview` at startup and sync tick

### Sidecar State Scoping (1dc75cb)
Sidecar files now land in per-repo paths via `tokio::task_local! { REPO_CONTEXT }`. Previously, brew service (cwd=/) caused all sidecars to land flat in `~/.orch/state/{id}.json`, breaking worktree cleanup.

---

## Tomorrow's Priorities

1. No specific bugs identified. Service is stable.
2. Monitor: the `review_started` → status-based review refactor is new and complex. Watch for edge cases in the review flow.
3. Consider filing issue: reduce stuck-detection threshold from 30 min to 10-15 min for tasks with no active tmux session. Currently not urgent but worth tracking.
