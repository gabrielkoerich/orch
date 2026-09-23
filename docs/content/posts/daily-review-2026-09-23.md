+++
title = "Daily Review, 2026-09-23"
date = 2026-09-23
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review, 2026-09-23

## What Shipped

Nothing landed on `main` in the 24h window. The last merge was **#3614** on 2026-09-11 (`fix(engine): internal-task stuck-review recovery uses 30min in_review override`), and there were no commits after it. Three new bugs opened 2026-09-12 (**#3615**, **#3616**, **#3617**) sat untouched until today, when dispatch resumed and all three picked up active work again within the last hour.

## What's In Flight

| ID | Status | Agent | Title |
|----|--------|-------|-------|
| #3615 | `needs_review` | codex | watchdog tick loop stalls at 756-940s exceeding 60s threshold |
| #3616 | `in_progress` | kimi | router LLM times out after 45s preventing task routing |
| #3617 | `in_progress` | opencode | opencode model discovery times out after 30s returning empty model list |

PR #3618 (for #3615) is open, with `mergeStateStatus: CLEAN`. #3616 and #3617 don't have PRs yet, both have burned several re-route attempts today (4 and 5), mostly from codex/gpt-5.4 dispatch hitting the "model unavailable" failure described below.

`internal:169170` (a self-improvement job) has been sitting in `routed` with no activity since 2026-09-14T01:42:59Z. No corresponding tmux session exists. Given the long gap in orchestrator activity this window, this is most plausibly host-suspend time being correctly discounted from the stuck-task age check, not a live bug. Flagging for awareness, not filing, since there is not enough signal here to distinguish it from expected sleep and wake behavior.

## Bug found: agent-level cooldown bypassed by same-agent model failover

While investigating today's codex failures, found `cooldown:codex` (the bare agent-wide key, not a specific model) persisted at a timestamp about 20 days out, 2026-10-14, versus every other active cooldown in the table topping out at about 4.5 days. `failure_count:codex` and `credit_failure_count:codex` both read 0, so whatever escalated this is not visible in the current counters.

More importantly, this cooldown had zero effect today. `task_runs` shows codex dispatched and ran gpt-5.5 successfully at 16:06:00Z, immediately after gpt-5.4 failed with "model unavailable" at the same timestamp, while the bare `codex` agent cooldown was, and still is, active. Traced it to `src/engine/runner/fallback.rs:388-411`, the `ModelUnavailable` handler's "try next model before switching agent" path. It only checks `is_model_in_cooldown(agent_name, m)` for the candidate model, never `is_agent_in_cooldown(agent_name)`. So once any model under an agent throws `ModelUnavailable`, the in-process failover keeps retrying other models on that same agent even if the agent itself is under an active agent-wide cooldown for an unrelated reason. This is a different gap from #3599, which covers routing to dispatch staleness, not this same-attempt in-process model substitution. Filed as **#3620**.

## Operational Health

11 task runs in the last 24h, all in today's post-resume burst:

| Agent | Model | Outcome | Count |
|-------|-------|---------|------:|
| kimi | opus | success | 2 |
| claude | sonnet | success | 1 |
| claude | sonnet | (in progress) | 1 |
| codex | gpt-5.4 | failed | 1 |
| codex | gpt-5.5 | rate_limit | 1 |
| codex | gpt-5.5 | success | 1 |
| kimi | opus | (in progress) | 1 |
| minimax | opus | (in progress) | 1 |
| opencode | mimo-v2.5-free | failed | 1 |
| opencode | nemotron-3.5-lightning-free | (in progress) | 1 |

`task_activity`: `status_change` 51, `dispatch` 21, `routed` 12, `error` 6, `rerouted` 4, `push` 4, `branch_delete` 4, `pr_create` 2, `review_start` 1, `review_decision` 1.

### Cooldowns

```
codex                20d9h   anomalous, see bug above, unexplained by counters
codex:gpt-5.4        4d11h   persistent-model backoff, within cap
codex:gpt-5.5        2h3m    fresh rate-limit cooldown from today's billing hit
minimax:haiku        1d23h   extended-tier backoff, within cap
opencode             23h47m  within cap
```

Everything except the bare `codex` entry is normal exponential-backoff behavior. codex/gpt-5.4 continues to fail with "model not supported for this account" style errors, which is an account or model-access condition, not a detection bug. The classifier is correctly recording persistent-model cooldowns for it.

`/opt/homebrew/var/log/orch.error.log` is empty (0 bytes), current run.

## Priorities for Tomorrow

1. Fix the agent-cooldown bypass in the `ModelUnavailable` same-agent failover path (#3620).
2. Watch #3615, #3616, and #3617 through to merge. All three were actively progressing as of this review.
3. No action needed on `internal:169170` unless it is still un-dispatched after the host has clearly been awake for a while.
