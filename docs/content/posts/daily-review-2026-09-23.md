+++
title = "Daily Review, 2026-09-23"
date = 2026-09-23
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review, 2026-09-23

## Update (later same day)

Dispatch caught up fast after this post was first written. All four bugs opened yesterday are now closed, three with fixes on `main`.

| ID | Status | Resolution |
|----|--------|------------|
| #3615 | closed | merged via `a35f60b9` (`fix(engine): cap routing quota across project ticks`, PR #3618) |
| #3620 | closed | merged via `2f486bb8` (`bug(runner): ModelUnavailable same-agent failover never checks agent-level cooldown`, PR #3622) |
| #3617 | closed | no PR, no fix, closed by a review-flow bug, see below |
| #3616 | closed | merged via `b0601f4c` (`bug(router): fallback router LLM timeout never records a cooldown`, PR #3619) |

`internal:169170` is still `routed` with no session, unchanged since the original write-up below. Read as discounted host-sleep time.

## New bug found: no-code review-skip closes external tasks that never produced a PR

`build_review_context` (`src/engine/review.rs:338-357`) marks a `needs_review` task `Done` outright whenever it has no worktree, no branch, and no PR number, with no distinction between internal and external tasks. That check (added in #2975, `668048e6`) was written for internal no-code jobs like daily retrospectives, where "no PR" genuinely means "nothing to review." It never checks `task_id.starts_with("internal:")`, so it fires identically for GitHub-issue tasks whose agent failed to produce any output.

That's exactly what happened to **#3617** today. opencode emitted 11 events with no text response, created no branch, so the review flow closed the issue as `Done` at 16:36:10Z with zero code changes and no re-route. The underlying bug the issue described (opencode model discovery timing out) is still unfixed, and now the issue is gone too.

This is the same class of failure the 2026-03-29 fix in `response_handler.rs` (`is_no_code_reroute`) already solved for the `done`/`completed` agent-status path, gated on `is_external`. That fix doesn't cover this path because this one triggers off `needs_review`, not `done`/`completed`, and lives in a separate function in `review.rs`. Filed as **#3623** rather than reopening #3617, since #2975 is correct for internal tasks and shouldn't be reverted.

## What's In Flight (original write-up)

| ID | Status | Agent | Title |
|----|--------|-------|-------|
| #3615 | `needs_review` | codex | watchdog tick loop stalls at 756-940s exceeding 60s threshold |
| #3616 | `in_progress` | kimi | router LLM times out after 45s preventing task routing |
| #3617 | `in_progress` | opencode | opencode model discovery times out after 30s returning empty model list |

PR #3618 (for #3615) is open, with `mergeStateStatus: CLEAN`. #3616 and #3617 don't have PRs yet, both have burned several re-route attempts today (4 and 5), mostly from codex/gpt-5.4 dispatch hitting the "model unavailable" failure described below.

## Bug found: agent-level cooldown bypassed by same-agent model failover

While investigating today's codex failures, found `cooldown:codex` (the bare agent-wide key, not a specific model) persisted at a timestamp about 20 days out, 2026-10-14, versus every other active cooldown in the table topping out at about 4.5 days. `failure_count:codex` and `credit_failure_count:codex` both read 0, so whatever escalated this is not visible in the current counters.

More importantly, this cooldown had zero effect today. `task_runs` shows codex dispatched and ran gpt-5.5 successfully at 16:06:00Z, immediately after gpt-5.4 failed with "model unavailable" at the same timestamp, while the bare `codex` agent cooldown was, and still is, active. Traced it to `src/engine/runner/fallback.rs:388-411`, the `ModelUnavailable` handler's "try next model before switching agent" path. It only checks `is_model_in_cooldown(agent_name, m)` for the candidate model, never `is_agent_in_cooldown(agent_name)`. So once any model under an agent throws `ModelUnavailable`, the in-process failover keeps retrying other models on that same agent even if the agent itself is under an active agent-wide cooldown for an unrelated reason. This is a different gap from #3599, which covers routing to dispatch staleness, not this same-attempt in-process model substitution. Filed as **#3620**.

## Operational Health (refreshed, end of day)

29 task runs in the 24h window, dispatch stayed busy through the whole burst:

| Agent | Model | Outcome | Count |
|-------|-------|---------|------:|
| claude | sonnet | success | 17 |
| kimi | opus | success | 3 |
| kimi | opus | rate_limit | 2 |
| claude | sonnet | failed | 1 |
| claude | sonnet | (in progress) | 1 |
| codex | gpt-5.4 | failed | 1 |
| codex | gpt-5.5 | rate_limit | 1 |
| codex | gpt-5.5 | success | 1 |
| minimax | opus | billing_cycle_exhausted | 1 |
| opencode | mimo-v2.5-free | failed | 1 |
| opencode | nemotron-3.5-lightning-free | success | 1 |

`claude/sonnet failed` (task `internal:169442`) errored with `unrecognized status: duplicate_skipped`, a status string the classifier doesn't have a mapping for. The task itself later reached `done`. This happened once this window, so it is not being filed as a separate issue.

`task_activity` (24h): `status_change` 126, `dispatch` 36, `push` 23, `branch_delete` 20, `routed` 17, `review_start` 11, `review_decision` 10, `pr_create` 10, `error` 10, `rerouted` 6, `push_recover_worktree` 4.

### Cooldowns

```
codex                20d8h   anomalous, see bug above, unexplained by counters
codex:gpt-5.4        4d11h   persistent-model backoff, within cap
codex:gpt-5.5        1h20m   fresh rate-limit cooldown
kimi:haiku           44m     fresh rate-limit cooldown
kimi:opus            4h25m   extended-tier backoff, within cap
minimax:haiku        1d23h   extended-tier backoff, within cap
minimax:opus         6d23h   billing_cycle_exhausted, model-level, within the 7-day cap
opencode              23h3m  within cap
```

Everything except the bare `codex` entry is normal exponential-backoff behavior. codex/gpt-5.4 continues to fail with "model not supported for this account" style errors, an account or model-access condition, not a detection bug. The bare `codex` cooldown's 20-day span with zero failure counters most likely comes from a vendor-supplied "retry at" timestamp on a rate-limit response, which the cooldown system treats as authoritative and exempts from the usual caps. That explains the zero counters, so this reads as a plausible explanation rather than a new finding, and isn't being filed separately from #3620.

`/opt/homebrew/var/log/orch.error.log` is empty (0 bytes), current run.

## Priorities for Tomorrow

1. Fix the no-code review-skip marking external tasks `Done` without a PR (#3623, filed today, already dispatched and in progress). This one silently deleted a real bug report.
2. No action needed on `internal:169170` unless it is still un-dispatched after the host has clearly been awake for a while.
