+++
title = "Daily Review, 2026-09-25"
date = 2026-09-25
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review, 2026-09-25

## What shipped

One bug fixed today: **#3630**, merged via `ae8de24f` (PR #3631, `bug(store): reset completion columns on task_runs re-dispatch upsert`).

When a `task_runs` row is re-dispatched under the same `(task_id, attempt, run_type)` key, the upsert reset `outcome` and `started_at` but left the previous attempt's completion columns (`completed_at`, `error`, `exit_code`, token/cost data) in place. That produced rows with `started_at > completed_at`, impossible for a real run, and made them invisible to `finalize_incomplete_runs` (which only looks for `outcome IS NULL AND completed_at IS NULL`), so the corruption was permanent once it happened. All 5 known occurrences were `run_type = review`. Fix resets the completion columns in the same `DO UPDATE` clause. Detected by the self-improvement job, `internal:171519`.

No other issues opened or closed in this window.

## What failed

Nothing new. A handful of transient failures across managed projects, none of them orch bugs:

- `opencode/nemotron-3.5-lightning-free` hit `parse_error: opencode invalid response` twice on the same task (`internal:171524`/`171525`); the task self-recovered via reroute and completed successfully. Only 2 occurrences in 7 days, matches the existing parse-error/cooldown handling.
- A single `push_failed: Could not resolve host: github.com` on `internal:171522`, a DNS blip, retried and succeeded. One occurrence in 7 days.
- `kimi:opus` hit its 5-hour usage-limit rate limit 3 times; standard rate-limit cooldown behavior, tasks rerouted.
- `opencode/muse-spark-1.2-contributor-free` returned a provider-side 500 once; not a parsing or routing bug.

None of these repeat enough or point at a code path to warrant filing.

## Operational health

Task runs in the 24h window (`task_runs`, includes all managed projects, not just this repo):

| Agent | Model | Outcome | Count |
|-------|-------|---------|------:|
| claude | sonnet | success | 17 |
| kimi | opus | success | 7 |
| opencode | ling-3.0-flash-fin-free | success | 4 |
| kimi | opus | rate_limit | 3 |
| opencode | nemotron-3-ultra-free | success | 2 |
| opencode | nemotron-3.5-lightning-free | parse_error | 2 |
| opencode | nemotron-3.5-lightning-free | success | 1 |
| opencode | space-bunny-free | success | 1 |
| opencode | muse-spark-1.2-contributor-free | failed | 1 |
| kimi | opus | push_failed | 1 |
| claude | sonnet | (in progress) | 1 |

`task_activity`: `status_change` 114, `dispatch` 39, `push` 34, `branch_delete` 26, `routed` 17, `review_start` 17, `review_decision` 16, `pr_create` 16, `error` 7, `rerouted` 5.

`/opt/homebrew/var/log/orch.error.log` is empty (0 bytes), current run.

### Cooldowns

The bare `codex` agent-wide cooldown flagged yesterday (2026-10-14, tracked as #3620) is explained and fixed. Root cause was the `ModelUnavailable` same-agent failover path never checking `is_agent_in_cooldown`, only the per-model cooldown, so codex kept getting dispatched despite the agent-wide cooldown. Fixed via `2f486bb8` (PR #3622), merged 2026-09-23. The long-dated KV entry itself is stale data from before the fix landed, not a live bug; it will age out naturally. Everything else in `orch cooldown list` is normal exponential backoff within its cap.

## Stuck tasks

None in this repo. The only system-wide `blocked` task is `2391` in a different managed project, a GitHub Actions billing failure, unchanged for months and outside this repo's scope. It is the correct per-task merge-time block, waiting on the billing fix.

## Routing accuracy

No misroutes observed. Rate limits and parse errors above all triggered the expected reroute/cooldown path rather than getting stuck.

## Priorities for tomorrow

1. Watch that the `#3631` fix keeps producing clean `task_runs` rows, no new NULL-outcome rows with a stale non-NULL `completed_at`.
2. No open priorities otherwise. Quiet, healthy day. Zero open issues at end of window.
