+++
title = "Morning Review — 2026-04-28"
date = 2026-04-28
description = "Daily operational check-in: recent fixes are in, routing-budget stalls continue, and blocked work remains concentrated in known buckets."
+++

# Morning Review — 2026-04-28

## Recent Commits (last 24h)

No new commits in this worktree in the last 24 hours (`git log --since="24 hours ago" --oneline` returned no entries).

Most recent landed changes in the past week remain:

- `99f99a3e` docs: morning review 2026-04-25
- `d0218c08` bug(runner): preserve blocked reason in task state
- `ff612d53` fix(runner): blocked runs recorded as blocked in audit
- `07f6817f` docs: evening retrospective 2026-04-24
- `0a7bf86a` fix: route merge conflicts to task agent
- `c4c96688` fix: task completion only when PR merges
- `dc88e160` bug(cooldown): stale retry_at no longer skips exponential backoff

## Context Carry-Forward

From the latest evening retrospective (`2026-04-24`), carry-forward priorities were:

1. Remove dead opencode Copilot model identifiers from live config (operator action)
2. Continue cleanup of blocked-outcome/audit correctness (already fixed by `#3011` and `#3012`)
3. Watch routing budget and slow tick behavior
4. Unblock stuck self-improvement/internal tasks after model config is corrected

As of this review:

- `#3011` and `#3012` are closed with fixes landed.
- `#3010` is closed, but opencode dead-model failures are still observed in current runs (`github-copilot/gpt-5.3` model unavailable).
- Slow tick / routing-budget pressure is still present.

## Pipeline Snapshot

### Open GitHub issues

- `#2789` (blocked): collect GLM failing run artifacts.

### Orch task queue snapshot

- In progress now: `internal:148631` (this review), `internal:148632`, `internal:148633`
- Blocked: `internal:148540` and external `#2789`

No broad queue jam is visible from `orch task list`, but there is persistent blocked carry-over.

## Operational Health

### Logs (`orch log 200`)

Observed patterns:

- Repeated `LLM routing budget exceeded — falling back to round-robin`
- Watchdog event: `tick loop has not completed a tick in 80s (threshold 60s)`
- Slow tick warning around ~90s elapsed
- One reroute trigger for this task after opencode model-not-found (`github-copilot/gpt-5.3`)

Interpretation:

- Router fallback is working as designed, but budget overruns still produce periodic long ticks.
- Dead model identifiers are still showing up in active routing paths.

### task_runs (24h)

From:

`SELECT agent, model, outcome, COUNT(*) ...`

- `codex / gpt-5.3-codex / outcome NULL / 2`
- `claude / opus / outcome NULL / 1`
- `claude / sonnet / outcome NULL / 1`
- `glm / opus / outcome NULL / 1`
- `kimi / opus / outcome NULL / 1`
- `opencode / github-copilot/gpt-5.3 / failed / 1`

Most NULL outcomes correspond to currently-running tasks; the concrete failure visible in this slice is the opencode dead-model miss.

### task_activity (12h)

- `status_change`: 24
- `dispatch`: 14
- `routed`: 7
- `branch_delete`: 4
- `rerouted`: 1
- `error`: 1

Activity level is healthy; errors are low but non-zero.

## Stuck Tasks / Owner Feedback

- `internal:148540` remains blocked (review-agent failure threshold history).
- `#2789` remains blocked and still needs artifact collection completion.
- No explicit `needs-feedback` labels in open GitHub issue list.

## Issue Creation Check

No new GitHub issues created in this pass.

Reason:

- Current operational pain points are already tracked or were recently closed (`#3010`, `#3011`, `#3012`, `#2789`).
- No brand-new root cause was identified that is both untracked and actionable as a separate bug issue.

## Priorities For Today

1. Apply operator-side config cleanup for dead opencode Copilot model identifiers still appearing at runtime.
2. Reduce routing-budget stall impact (config-level tuning and/or routing pool health checks).
3. Unblock and re-run `internal:148540` after model-route health is stabilized.
4. Progress `#2789` artifact collection to close the long-lived blocked item.

---
Prepared by Orch automation (internal task internal:148631).
