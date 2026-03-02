+++
title = "Evening Retrospective — 2026-03-02"
date = 2026-03-02
description = "19 commits, 6 tasks done, review workflow overhauled, agent prompt hardening — cleanest close yet"
+++

## Summary

Exceptionally productive day. 19 commits merged, 6 tasks closed, zero open issues at EOD (excluding this retro). The biggest structural change was the **status-driven review workflow** that eliminates the `review_started` sidecar flag. Agent prompts were hardened with a mandatory pre-output checklist. Service is stable and the backlog is empty.

---

## Morning Review Recap

| Priority | Outcome |
|----------|---------|
| No specific bugs (service stable) | Confirmed — no bugs opened |
| Monitor: status-based review refactor | Landed this morning, all tasks passed through it successfully |
| Consider: reduce stuck threshold 30→10-15 min | Filing as issue today |

All priorities acknowledged.

---

## Tasks Completed Today

| Issue | Title | Agent | Notes |
|-------|-------|-------|-------|
| #267 | Graceful restart — wait for running agents | claude | Clean first-attempt |
| #265 | branch_name() panics on non-ASCII titles | claude | Byte-offset panic fixed |
| #264 | Transient GitHub API error clears active_task_id | claude | active_task_id preserved on 502 |
| #263 | Code review: orch | claude | Routine scheduled review |
| #257 | Extract LLM routing into engine/llm_router.rs | claude | Clean architecture refactor |
| #271 | Morning review | claude | Completed, drove today's priorities |

---

## Key Changes This Cycle

### 1. Status-Driven Review Workflow (95a2113)
The `review_started` sidecar flag — spread across 11 sites — is gone. Review lifecycle now uses status transitions exclusively:
- Agent done + PR exists → `needs_review`
- Engine spawns review agent → transitions to `in_review` as the guard
- Review failure → reset to `needs_review` (was: stuck `in_review` forever)
- Stale `in_review` with no tmux session → reset at startup and sync tick

This is a large behavioral change. Today's tasks all passed through it successfully, but edge cases (concurrent review + sync tick, rapid retry loops) should be watched.

### 2. Sidecar State Scoped Per-Repo (1dc75cb)
Sidecar files now land in `~/.orch/state/{repo}/{id}.json` via `tokio::task_local! { REPO_CONTEXT }`. Brew service (cwd=/) previously caused all sidecars to write flat to `~/.orch/state/{id}.json`, breaking worktree cleanup.

### 3. Graceful Restart (bb0e4f3)
`orch restart` now waits for all running agents to finish before restarting the service. Previously, an in-progress agent could be killed mid-task.

### 4. Agent Prompt Hardening
Two improvements to prevent the most common agent failure (work done but not pushed):
- **Pre-output checklist** (`3cee8fd`): agents must verify `git status`, `git log`, `git push`, and `gh pr view` before writing output JSON
- **Route prompt cleanup** (`1f8245d`): removed hardcoded executor descriptions that caused routing bias toward specific agents

### 5. Review Prompt Rewritten (404b557)
Rewrote `review_task.md` to be more explicit about rebase-first, CI-must-run, and decision rules. Review agents were previously approving without running CI.

### 6. Auto-Close on Done (c934eeb)
Issues are now auto-closed when tasks complete. Labels that were stuck at `status:in_review` on closed issues are also corrected.

---

## What Didn't Go Well

### Stale Integration Test (036ddbf)
Codex autonomous mode changed from `--ask-for-approval never` to `--full-auto` but the integration test was never updated. The test was `#[ignore]`d in the repo, masking the drift. **Root cause**: `#[ignore]`d tests aren't run in CI and can silently become stale. Filed as part of commit #272 and fixed.

### Labels Not Updating on Close (historical, fixed today)
Several older closed issues showed `status:in_review` label — the label update code had a gap for the no-PR path. Fixed by `c934eeb`.

---

## Prompt Effectiveness

| Prompt | Assessment |
|--------|-----------|
| `agent_system.md` | Strong. Mandatory pre-output checklist is new and should eliminate push-forgetting. |
| `review_task.md` | Improved. Rebase-first + explicit CI requirement are clear. Watch for edge cases with large diffs. |
| `route.md` | Good. Hardcoded executor descriptions removed. Historical labels now explicitly ignored. |
| `agent_message.md` | Not reviewed today — no issues observed. |

No prompt changes needed beyond what's already landed.

---

## Routing Accuracy

All 6 tasks today were routed to `claude`. No mis-routes observed. The route prompt cleanup (`1f8245d`) removes historical bias — upcoming scheduled tasks will be the first real test of neutral routing.

---

## Performance

- **GitHub API**: Transient 502s at ~11:21 UTC (last reported in morning review) recovered automatically. `fix: preserve active_task_id on transient GitHub API errors` was merged today — this exact scenario is now handled.
- **No lock contention** observed in logs.
- **Review latency**: status-driven workflow eliminates the `review_started` flag polling delay. Expected improvement in review cycle time.

---

## New Issues Filed

| Issue | Title | Priority |
|-------|-------|---------|
| #274 | Reduce stuck-detection threshold for tasks with no active tmux session | Low |

---

## Tomorrow's Priorities

1. **Monitor the new review workflow** — `needs_review` → `in_review` status transitions are live but new. Watch for any stuck tasks in `in_review` without an active review session, especially under concurrency.
2. **Stuck-detection threshold** — the newly filed issue. Once deployed: tasks with no tmux session should be reclaimed in 10-15 min, not 30 min.
3. **No other known bugs.** Service is in the best shape it's been. The next scheduled jobs will reveal if the route prompt cleanup introduced any unexpected routing changes.
