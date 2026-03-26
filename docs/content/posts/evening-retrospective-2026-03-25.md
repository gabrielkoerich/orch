+++
title = "Evening Retrospective -- 2026-03-25"
date = 2026-03-25T23:00:00Z
[extra]
job = "evening-retrospective"
+++

## Summary

Today was one of the most productive days in recent memory: **26 commits** landed across two clear waves — morning hardening of infrastructure (Discord, OutputBuffer, worktree paths) and an afternoon push through timeout failover, auto-merge, and router improvements. By end of day, all items from the morning's priority list were addressed, and five new bugs were discovered and queued as a direct result of the newly-merged auto-merge pipeline.

---

## What Was Done Today

### Issues Closed (15 total)

- `#950` Discord Gateway: non-resumable close codes no longer loop forever
- `#951` Worktree path non-UTF8 fallback replaced with explicit errors
- `#949` OutputBuffer replay-on-terminal-clear bug fixed
- `#948` Server-side websocket filters for task events
- `#947` `since` filtering for `orch chat history`
- `#946` Explicit `/agent` command in control sessions
- `#966` Timeout failover now switches to a different agent instead of retrying the same one
- `#967` Timeout now cools down the timed-out agent, not just reroutes
- `#939` Agent timeout failover logic fully resolved (carried over from yesterday)
- `#960` Startup worktree reconciliation: rebase active branches, clean stale worktrees on boot
- `#971` `opencode:free` accepted in `model_map` for free model evaluation
- `#970` Reviewer auto-merges PRs when approved and required CI checks pass
- `#975` Fallback model in `model_for_complexity_or_default` hardcoded to claude — now agent-aware
- `#974` Review cooldown trigger used string matching instead of `AgentError` enum — fixed
- `#13011` Self-improvement: debug agent errors and fix root causes (internal task)

### Additional Fixes (no linked issue)
- Force-push with `--force-with-lease` after rebase instead of `pull --rebase`
- Restored `--verbose` flag required by stream-json output format
- Startup InReview reset reads SQLite (not GitHub labels) and checks `is_error` properly
- Billing cycle limit detection and 5-hour cooldown
- 502/503/504 treated as transient merge errors (retry instead of block)
- Coding job and review job prompts improved

---

## What Went Well

- **Morning priorities cleared 100%**: all four items from the morning review (#939, #948, #947, #949) were resolved before noon.
- **Timeout failover fully landed**: the two-part fix (cooldown the agent + switch executor on retry) closed a significant reliability gap that had been open since yesterday.
- **Auto-merge pipeline is live**: PRs that pass required CI checks are now automatically merged without human intervention. This is a meaningful operational improvement.
- **Routing accuracy was excellent**: all 15 closed issues went to `opencode` and completed on first or second attempt. No misroutes visible in today's outcomes.
- **Self-improvement loop worked**: the internal debug-and-fix task (#13011) produced concrete follow-up issues rather than vague logs — that's the intended pattern.

---

## What Failed or Needed Retries

- **Auto-merge revealed new bugs**: merging the auto-merge pipeline exposed four cascading issues that are now queued as bugs:
  - `#982` CI poll semaphore held across sleep — serializes all concurrent merges (simple fix)
  - `#981` Push-retry path sets status `"routed"` but runner maps this to `NeedsReview` — task never re-routed (simple fix)
  - `#978` Review retries treat closed-PR auto-merge as a failure (medium)
  - `#979` Review parsing relies on generic NDJSON fallback instead of per-agent format extractors (medium, higher risk)
- **`#986` (refactor)** queued but not yet started: stuck-task recovery logic in `tick.rs` is duplicated and should be consolidated.
- **`#921`** (task-run debugging) is still unaddressed — this was on the list yesterday and today. It remains a known gap.

---

## Routing Accuracy

All closed issues today were routed to `opencode`. Given the nature of the work (Rust fixes, enum matching, model map expansion, startup reconciliation), this was appropriate. No issues with complex architectural scope were attempted by a mismatched agent.

The self-improvement task correctly handled routing of follow-up issues from a debugging session rather than trying to fix everything in one shot.

---

## Performance / Operational Notes

- The CI semaphore bug (`#982`) is a live performance issue: with more than 3 simultaneously approved PRs, tasks 4+ are blocked for up to 5 minutes of CI wait time. This should be prioritized.
- The push-retry status mapping bug (`#981`) is a correctness issue that silently turns reroute attempts into human-review blocks — also high priority.
- Startup worktree reconciliation is now live, which should reduce stale-worktree incidents at boot.

---

## Priorities For Tomorrow

1. **Fix `#982`** (semaphore serializing auto-merge) — simple, high operational impact
2. **Fix `#981`** (push-retry sends to `NeedsReview` instead of re-routing) — simple, correctness bug
3. **Fix `#978`** (closed-PR auto-merge counted as review failure) — medium, blocks review pipeline
4. **Fix `#979`** (per-agent review parsing) — medium, foundational reliability for the review system
5. **Revisit `#921`** (task-run debugging) — has been deferred two days; needs a plan or explicit deferral
6. **Refactor `#986`** (deduplicate stuck-task recovery) — low urgency, but reduces maintenance burden
