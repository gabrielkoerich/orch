+++
title = "Evening Retrospective — 2026-04-29"
date = 2026-04-29
description = "Daily evening retrospective: auto-merge edge case fixed, throughput stayed high, and failures remained concentrated in known retry/model-availability paths."
+++

# Evening Retrospective — 2026-04-29

Throughput stayed healthy today and one production reliability fix shipped (`#3027` / commit `e0a2fa34`) to stop auto-merge stalls when workflows are skipped by `paths-ignore`. Failures were concentrated in known edges rather than broad instability.

## What Was Accomplished

### Issues closed today

| Issue | Outcome | Why it mattered |
|------|---------|-----------------|
| #3030 | Closed | Fixed final-status normalization path where agent outputs could map to `unknown` instead of `done`. |
| #3029 | Closed | Hardened transient GitHub API error classification for `circuit-breaker` strings. |
| #3027 | Closed | Fixed auto-merge pending loop when CI workflows are filtered out (`total=0` with workflows present). |

### Commits in the last 12 hours

- `e0a2fa34` fix(auto_merge): trust `mergeable_state=clean` when no check runs match `paths-ignore` PRs

## Execution Quality (task_runs, last 24h)

Outcome totals:

- `success`: 88
- `failed`: 4
- `rate_limit`: 1
- `blocked`: 1
- `push_failed`: 1
- `aborted`: 1
- `NULL outcome` (in-flight/accounting): 3

Approximate success rate excluding NULL rows: **~92.6% (88/95)**.

### Failure and retry patterns

1. **Retry hotspot on `#3031`**
   - `#3031` ran 12 times in the last 24h (11 successes + 1 failure) and remains open/in_review.
   - Latest non-success record: `max attempts reached`.
   - This is now the primary churn source and should be stabilized first tomorrow.

2. **Model-availability miss still appears in fallback paths**
   - One failure still attempted `opencode/github-copilot/gpt-5.3` (`Model not found`).
   - Router alias hardening exists, but runtime fallbacks can still surface dead IDs in some branches.

3. **Rate limits are isolated**
   - Single `claude:sonnet` rate-limit event with no cascade.

4. **One blocked + one push_failed are task-specific**
   - Blocked run tied to a worktree lock/permission condition on an internal task.
   - `push_failed` appeared once under `minimax:opus`; no repeat pattern in this window.

## Routing Accuracy

Routing remained mostly accurate:

- High-volume lanes (`codex:gpt-5.3-codex`, `claude:sonnet`, `minimax:opus`, `kimi:opus`, `glm:opus`) produced most successes.
- Review pipeline throughput remained strong (`review/success`: 34).
- Misses were concentrated in known fallback/model-availability edges, not general misrouting.

## Morning Plan vs. Actual

From this morning’s priorities:

- Unblock/resolve `#2789`: **not completed** (still open/blocked).
- Clear `internal:148540`: **not closed in this snapshot**.
- Reduce review-loop churn: **partially completed** via `#3027` fix to pending-with-zero-checks behavior.
- Reconfirm dead-model hygiene: **improved but not fully eliminated** (one dead model hit still observed).

## Open / Pending

- `#3031` (open): in-review churn with repeated attempts; now the highest-priority reliability follow-up.
- `#2789` (open): long-lived blocked artifact-collection task.

No additional new root-cause bug was identified that is not already represented by open or just-closed issues.

## Prompt/Workflow Observations

- Prompt/response format quality is stable; parser regressions were not the dominant source of failures today.
- The majority of non-success outcomes came from operational edges (attempt exhaustion, dead model fallback, single push/worktree incidents).
- Current prompts are generally effective; reliability gains are now mostly in routing/fallback and retry policy behavior.

## Priorities for Tomorrow Morning Review

1. Stabilize `#3031`: inspect why it reached `max attempts` despite high per-run success and tighten reroute/review transition criteria.
2. Close or re-scope `#2789` with explicit artifact-capture acceptance criteria and owner handoff.
3. Verify dead `github-copilot/*` model IDs cannot re-enter runtime via fallback/model pool paths.
4. Validate that `#3027` eliminated repeated pending-with-zero-check checks in fresh `review_poll` logs.

---

Prepared by Orch automation (internal task internal:148753).
