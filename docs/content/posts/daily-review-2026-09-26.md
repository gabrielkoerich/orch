+++
title = "Daily Review, 2026-09-26"
date = 2026-09-26
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review, 2026-09-26

## What shipped

One bug fixed today: **#3633**, merged via `14bf22f4` (PR #3634, `bug(cooldown): cooldown reason is never persisted to KV, collapses to 'persisted' on every restart`).

`set_cooldown_async` in `src/engine/cooldown.rs` persisted only the cooldown timestamp to KV, dropping the `reason` string. On the next `orch service restart`, `init_cooldown_store` reloaded cooldowns from KV and hardcoded the reason to the literal `"persisted"`, permanently losing whether a cooldown came from `agent_error`, `model_error`, `billing_cycle_exhausted`, a vendor rate-limit retry-at, etc. Since restarts are routine, most long-lived cooldowns showed `persisted` for the bulk of their life, which made `orch cooldown list` and the `multi-agent degradation detected` log line useless for diagnosing why an agent was cooled after the process that set the cooldown had exited. Found while investigating the codex agent-wide cooldown below. Fix serializes `{cooldown_until, reason}` into the KV value instead of the bare timestamp, with a fallback parse for old bare-integer entries already in KV.

No other issues opened or closed in this window; zero open issues at time of writing.

## What failed

Nothing new, and nothing that needed a fix:

- A review agent timed out once on a task in a different managed project (`internal:173353`); the engine reset it to `NeedsReview` for retry per the existing retry path, and the task finished successfully (`done`). Single occurrence, self-recovered.
- `opencode` model discovery returned an empty list once (`Error: Unexpected error / database is locked` from the opencode CLI's own local state), and the engine correctly fell back to the previous cached model list (7 models) instead of clearing it, the behavior the discovery-cache fix (#3617) was built for. Single occurrence, no task impact.
- One `timeout` on `opencode/mimo-v2.6-flash-free`, one `parse_error` on `opencode/nemotron-3.5-lightning-free`, one `kimi:opus` rate limit, all isolated, all went through the standard cooldown/reroute path.

## Operational health

Task runs in the 24h window (`task_runs`, includes all managed projects, not just this repo):

| Agent | Model | Outcome | Count |
|-------|-------|---------|------:|
| claude | sonnet | success | 24 |
| opencode | space-bunny-free | success | 4 |
| claude | sonnet | (in progress) | 2 |
| kimi | opus | success | 2 |
| opencode | ling-3.0-flash-fin-free | success | 2 |
| opencode | muse-spark-1.3-contributor-free | success | 2 |
| opencode | nemotron-3-ultra-free | success | 2 |
| kimi | opus | rate_limit | 1 |
| opencode | longcat-2.5-preview-free | success | 1 |
| opencode | mimo-v2.6-flash-free | success | 1 |
| opencode | mimo-v2.6-flash-free | timeout | 1 |
| opencode | nemotron-3.5-lightning-free | parse_error | 1 |

`task_activity`: `status_change` 123, `dispatch` 40, `push` 39, `branch_delete` 36, `review_start` 21, `review_decision` 19, `pr_create` 19, `routed` 18, `error` 2, `timeout` 1, `rerouted` 1.

`/opt/homebrew/var/log/orch.error.log` is empty (0 bytes), current run.

### Cooldowns

`orch cooldown list` shows 8 persisted entries: `codex` (17d2h), `codex:gpt-5.4` (1d5h), `kimi:haiku` (1h59m), `kimi:opus` (6d9h), `minimax:haiku` (22h46m), `minimax:opus` (3d17h), and two opencode model-level cooldowns. All still show reason `persisted` because they were set before today's fix landed, expected, not a regression. The 17-day `codex` entry is the one investigated in #3633: it's a legitimate vendor-specified rate-limit retry-at (`try again at Oct 13th, 2026`), correctly parsed and honored per the "vendor retry-at is authoritative" policy, not a bug. Cooldowns set from here on should carry their real reason instead of collapsing to `persisted` on the next restart.

## Stuck tasks

None in this repo. The only system-wide `blocked` task is `2391` in a different managed project, a GitHub Actions billing failure, unchanged for about 90 days and outside this repo's scope. It is the correct per-task merge-time block, waiting on the billing fix.

## Routing accuracy

No misroutes observed. The single rate limit, timeout, and parse error above all triggered the expected reroute/cooldown path rather than getting stuck.

## Priorities for tomorrow

1. Watch that new cooldown entries created after today's fix show a real reason (not `persisted`) after the next service restart.
2. No other open priorities. Quiet day, zero open issues at end of window.
