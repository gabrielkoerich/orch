+++
title = "Morning Review -- 2026-03-25"
date = 2026-03-25T10:15:00Z
[extra]
job = "morning-review"
+++

## Summary

Today's review shows a healthy, active project with steady cleanup and operational hardening. 
The system is successfully recovering from state-machine edge cases and auth failures. 
The review pipeline is busy but moving, with several PRs in active cycles.

---

## Recent Activity

### Key Commits (Last 24h)
- **Discord Gateway Hardening**: Handled non-resumable gateway close codes (#954).
- **Worktree Safety**: Replaced silent UTF8 fallbacks with explicit errors in worktree paths (#952).
- **Auth Error Handling**: Detected `is_error:true` and usage limits in review output (Fixes kimi/minimax silent failures).
- **Operational Fixes**: Resolved chat input ambiguity, startup InReview resets, and stale NeedsReview catch-ups.

### Active Pull Requests
- **#943 (Issue #939)**: Agent timeout failover (Approving reviews from Opus, Sonnet).
- **#956 (Issue #948)**: Server-side WS filtering (Approving review from Sonnet).
- **#957 (Issue #947)**: Since filtering for chat history (Approving review).
- **#955 (Issue #949)**: OutputBuffer replay bug fix.
- **#953 (Issue #946)**: Explicit `/agent` command for control session.

---

## Operational Health

### System Status
- **Sync Catch-ups**: Stale `NeedsReview` tasks are being successfully checked.
- **Review Pipeline**: Automated reviews (Claude, MiniMax, Kimi) are actively approving fixes, including clippy-motivated adjustments.
- **State Persistence**: Control session memory persistence and task-run tracking are operational.

### Stuck or Failing Tasks
- **#939 (Timeout Failover)**: This was flagged as a priority in the evening retrospective and is currently in the final stages of the PR review/merge cycle.
- **#948 (WS Filtering)**: Encountered a CI failure on a previous commit but has been fixed and is being re-tested.
- **#947 (Chat History Filtering)**: CI failure detected on latest push; review agent is likely re-routing to fix.

---

## Retrospective Follow-ups

- [x] **Router Pool Fix**: Router now uses `effective_pool()` and respects overrides.
- [x] **Discord Gateway**: Resumability fixes landed.
- [/] **Timeout Failover (#939)**: Implementation complete, in review pipeline.
- [ ] **Task-run Debugging (#921)**: Still open, requires follow-up.

---

## Today's Priorities

1. **Monitor CI/Review completions**: Ensure #943, #956, and #957 merge successfully once CI passes.
2. **Task-run Debugging (#921)**: Investigate why this investigation is blocked and advance it.
3. **Agent Timeout Failover (#939)**: Verify the failover logic in production once merged.
4. **OutputBuffer Bug (#949)**: Follow the progress of the replay bug fix to ensure terminal clears behave correctly.
