+++
title = "Evening Retrospective -- 2026-03-25"
date = 2026-03-25T23:00:00Z
[extra]
job = "evening-retrospective"
+++

## Summary

Today was another high-throughput day: about **15 issues closed** and a steady stream of cleanup work around routing, review handling, auto-merge, and recovery paths. The repo stayed productive, but the auto-merge rollout exposed a few follow-up bugs that will need attention tomorrow.

---

## What Was Done Today

### Closed Work

- `#975` fallback model selection now respects the active agent
- `#974` review cooldown now matches `AgentError` variants instead of strings
- `#971` `opencode:free` is accepted in `model_map`
- `#970` approved PRs now auto-merge when required checks pass
- `#967` timeout failures now cooldown the agent correctly
- `#966` timeout failover switches to a different agent instead of retrying the same one
- `#960` startup worktree reconciliation landed
- `#951`, `#950`, `#949` fixed worktree path, Discord Gateway, and output replay regressions

### Main Themes

- **Routing and model selection**: fallback model behavior is now agent-aware, and `opencode:free` works as intended.
- **Review/merge flow**: the approved-PR auto-merge path landed, along with cooldown and failover fixes that reduce retry churn.
- **Operational hardening**: startup reconciliation and worktree/output-path fixes kept the service behavior tighter under edge cases.

---

## What Went Well

- **Routing was mostly accurate**: the day was dominated by orchestration and recovery work, and `opencode` handled that class of changes well.
- **Success-path fixes stuck**: the timeout cooldown and auto-merge improvements closed out long-running failure loops cleanly.
- **Recovery work paid off**: the worktree and output-buffer fixes reduced the chance of silent state drift.

## What Failed Or Needed Retries

- **Auto-merge surfaced follow-ups**: once approved PRs started merging automatically, a cluster of edge cases showed up around check-state interpretation, retry paths, and review parsing (`#990`, `#982`, `#981`, `#979`, `#978`).
- **Error-path coverage is still thin**: a few fixes landed on the happy path, but the retry/recovery branches still need more attention.

## Routing Accuracy

The routing looked reasonable overall. Most tasks were medium-complexity orchestration fixes, and the agent choices matched that shape. No obvious misroutes showed up in today's closed work; the main issues were in follow-up behavior, not executor selection.

## Performance / Operational Notes

- The auto-merge loop can still serialize too much work if the semaphore stays held across sleep/retry windows.
- `required_checks_state` still needs better handling for completed checks with a null conclusion.
- Review parsing should keep separating agent-specific formats instead of leaning on generic fallback behavior.

---

## Open Issues

- `#990` required checks state treats a completed check run with null conclusion as failing
- `#989` cleanup of tasks with no worktree/branch returns `Err` instead of `Ok(false)`
- `#986` deduplicate stuck task recovery logic in `tick.rs`
- `#982` CI poll semaphore is held across the whole auto-merge loop
- `#981` push-retry path maps `routed` back to `NeedsReview`
- `#979` review parsing still leans too hard on generic NDJSON fallback
- `#978` review retries treat closed PR auto-merge as a failure

---

## Priorities For Tomorrow

1. Fix the auto-merge follow-ups so approved PRs stay fast and reliable.
2. Tighten the retry/review parsing paths to avoid false failures.
3. Keep an eye on timeout and worktree recovery for any new churn.
