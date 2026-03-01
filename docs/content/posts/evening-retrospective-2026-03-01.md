+++
title = "Evening Retrospective — 2026-03-01"
date = 2026-03-01
description = "Daily evening retrospective: major bug-fix day, API failures, and tmux session collision root-cause"
+++

## Summary

Today was a significant bug-fix day. 15 commits landed in the first half of the day, resolving several critical issues that had been blocking reliable orchestration. The afternoon was dominated by API connection failures that caused all four scheduled jobs to get stuck, though the recovery mechanism handled them correctly.

---

## What Went Well

### 15 Commits, All Fixes (08:58–12:47 BRT)

| Commit | Fix |
|--------|-----|
| `35f8ea4` | Check for open PR before spawning review agent — prevents duplicate review agents |
| `24ae187` | Improved PR description prompt; removed 4 unused prompts |
| `653caf6` | Restored prompts for future use (kept clean) |
| `334e38a`, `6b6f6fc` | `agent_system.md` updated with rebase-first workflow and retry guidance |
| `c96acd6` | Filter PRs at source in GitHub HTTP API layer — fixes over-fetching |
| `e0ec115` | Updated jobs config |
| `cdaa658` | **Critical**: review/merge comments now post on PR, not issue |
| `d275713` | `cargo fmt` pass |
| `f11e4e7` | Dropped stale posts |
| `e83fae3`, `e55db7c` | `.orch.yml` updated and added to `.gitignore` |
| `ac0aa9d` | **Critical**: Jobs path resolution fixed for brew service (cwd=/ issue) |
| `5457345` | Deleted stale `.orchestrator.yml` |
| `e6f358e` | **Critical**: Skip empty `active_task_id` in job tick — prevents panic/corruption |

The three most impactful fixes:
1. **Jobs path resolve** (`ac0aa9d`): The brew service always runs with `cwd=/`, so it never found `.orch.yml` when scanning for jobs. Jobs now resolve from registered project paths in the global config.
2. **PR-vs-issue comments** (`cdaa658`): Review and merge workflow comments were being posted to the issue instead of the PR. The GitHub Actions workflow checking for approval was never seeing the comment.
3. **Empty active_task_id** (`e6f358e`): Job tick could emit an update with an empty task ID, corrupting state.

### Recovery Mechanism Worked

All four stuck tasks (#223, #224, #225, #226) were automatically recovered at ~20:14 UTC after being detected as `in_progress` with no active tmux session for 264–266 minutes. The mechanism correctly cleared the agent state and re-routed each task.

---

## What Failed

### API Connection Drops (All Tasks, Attempts 1)

Tasks #225 (morning-review) and #226 (evening-retrospective) attempt 1 both failed with:
```
API Error: Unable to connect to API (FailedToOpenSocket)
API Error: Unable to connect to API (ConnectionRefused)
```

These were after 544–555 seconds of agent runtime (25–34 turns). This is an external failure — the Claude API connection dropped mid-session. Not a code bug in orch, but worth noting: long-running agent sessions are vulnerable to transient network failures. The recovery mechanism is the current mitigation.

### Duplicate tmux Session (Task #224, Attempt 1)

Task #224 attempt 1 failed immediately with:
```
tmux new-session failed: duplicate session: orch-orch-224
```

**Root cause**: `TmuxManager::create_session` in `src/tmux.rs` does not check whether a session with that name already exists before calling `tmux new-session`. When the engine recovered task #224 from a previous stuck run (via the recovery mechanism or a restart), the old tmux session `orch-orch-224` was still alive. The new attempt immediately failed when trying to create a session with the same name.

The fix is simple: in `create_session`, check if the session exists and kill it before creating a new one.

### Morning Review (#225) — No Summary Produced

Task #225 (morning review) got stuck on attempt 1 (API failure), then attempt 2 was started at 17:18 BRT. The morning review never produced a summary post. The posts directory was empty at the start of today (old posts had been dropped). This means there was no prior context to carry forward.

---

## Prompt Effectiveness

- **`agent_system.md`**: Updated twice today with improved rebase-first workflow instructions and retry guidance. Looks solid.
- **`review_task.md`**: Clear, covers all required checks. No changes needed.
- **`route.md`**: Accurate executor descriptions. Complexity guidance is clear.
- **`agent_message.md`**: Not reviewed today — should be checked if routing accuracy issues emerge.

No prompt changes needed today.

---

## Routing Accuracy

All 4 tasks today were routed to `claude` with `complexity:medium` or `complexity:complex`. Given the tasks involve codebase analysis, this is appropriate. No routing mismatches observed.

---

## Performance / Bottlenecks

- **API session drops**: Long-running sessions (>500s) are the main fragility point. No orch-side fix available — this depends on API connection stability.
- **Recovery latency**: Stuck detection fires after ~264 minutes — this is a long time for scheduled jobs to be unresponsive. Consider reducing the stuck-detection threshold for `in_progress` tasks with no active tmux session.

---

## Issues Created

- **#227**: `TmuxManager::create_session` should kill stale sessions before creating new — prevents "duplicate session" failures on retry

---

## Tomorrow's Priorities

1. **Verify today's fix deployment**: The 15 commits from today need to be deployed (brew upgrade). Confirm the brew formula is updated and service is running the new binary.
2. **Check tasks #223, #224, #225**: All three are still `in_progress` (attempt 2/3 started at 17:18 BRT). Check if they completed successfully or need another retry.
3. **Review `#227`**: Simple fix — kill stale tmux session before creating new one. Should be a fast task for any agent.
4. **Consider reducing stuck-detection threshold**: 264 minutes is too long. Something like 60–90 minutes for tasks with no active session would improve responsiveness.
