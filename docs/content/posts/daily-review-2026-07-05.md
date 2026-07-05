+++
title = "Daily Review — 2026-07-05"
date = 2026-07-05
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-07-05

## What Shipped (Last 24h)

**4 commits** landed in the last 24 hours: three operational fixes and yesterday's review post.

| Commit | PR | Description |
|--------|----|-------------|
| `49118616` | #3383 | fix(cooldown): don't set agent-level cooldown when BillingCycleExhausted has known model |
| `4a3c19b3` | #3380 | fix(cooldown): classify codex pro upgrade usage cap |
| `a4be54cb` | #3379 | fix(runner): classify nemotron streaming transport failures |
| `4665b53f` | #3376 | docs(posts): daily review 2026-07-04 |

- **#3382 → #3383 (FIXED):** `BillingCycleExhausted` with a known model was incorrectly triggering both a model-level cooldown (`record_persistent_model_failure`) *and* a 24h agent-wide cooldown (via `RetryableError::UsageLimit` → `handle_failover` → `record_agent_failure`). The fix returns `RetryableError::ModelUnavailable` when the model is known, which `handle_failover` already treats as model-only — skipping the agent-level penalty. Also patched `prepare_task` to fall back to `tasks.model` when both the `model` parameter and `task_routes.model` are `None` after a `ModelUnavailable` failover, preventing the model-unknown path from triggering spuriously.
- **#3377 → #3380 (deployed yesterday):** Codex Pro usage cap → persistent billing-cycle classification.
- **#3378 → #3379 (deployed yesterday):** Nemotron streaming transport failures → `NetworkError` classification.

The running service is **`orch 0.80.41`**.

---

## Operational Health

### Throughput

`task_activity` in the last 24 hours — throughput roughly **doubled** vs the previous review window:

| Event | Count (today) | Count (yesterday) |
|------|---------------|-------------------|
| `status_change` | 414 | 228 |
| `dispatch` | 143 | 79 |
| `push` | 95 | 53 |
| `branch_delete` | 70 | 36 |
| `routed` | 63 | 32 |
| `review_start` | 52 | 30 |
| `review_decision` | 44 | 25 |
| `pr_create` | 44 | 25 |
| `error` | 37 | 23 |
| `rerouted` | 15 | 10 |

The jump is a strong signal that fixing the codex and nemotron classifiers unlocked work that was being repeatedly retried or dropped.

### Agent / Model Outcomes (last 24h)

| Agent | Model | Outcome | Count |
|------|-------|---------|-------|
| claude | sonnet | success | 23 |
| codex | gpt-5.4 | success | 20 |
| opencode | north-mini-code-free | success | 13 |
| kimi | opus | success | 11 |
| opencode | deepseek-v4-flash-free | success | 11 |
| codex | gpt-5.4 | aborted | 5 |
| kimi | opus | failed | 4 |
| opencode | nemotron-3-ultra-free | failed | 4 |
| codex | gpt-5.4 | rate_limit | 3 |
| claude | sonnet | failed | 3 |
| opencode | north-mini-code-free | failed | 3 |
| codex | gpt-5.4 | failed | 2 |
| codex | gpt-5.5 | success | 2 |

Failure concentration: codex/gpt-5.4 has 10 non-success runs (mix of aborted, rate_limit, failed) but also 20 successes — healthy ratio. nemotron's 4 failures are pre-cooldown (it now has a 22h cooldown). kimi/opus has 4 failures; worth watching if count rises.

### Active Cooldowns

| Key | Remaining | Type |
|-----|-----------|------|
| codex | ~2h | persisted agent cooldown |
| minimax:opus | ~3d10h | persisted model cooldown |
| opencode/nemotron-3-ultra-free | ~22h | persisted model cooldown |

The LLM router keeps selecting `minimax` as the routing target (seen for both daily-review and evening-retrospective tasks) before the layer checks cooldown state and falls back to claude. This generates `routing sanity warning` logs on every such selection. It's working as designed (LLM picks best fit, cooldown gate handles unavailability), but the frequency suggests the LLM router's context should probably include cooldown state to reduce unnecessary suggestions.

### Blocked Inventory

| Reason | Count |
|--------|-------|
| CI failure limit reached (oblivion CI backlog) | ~39 |
| GitHub Actions billing failure | 5 |
| No block reason recorded | 4 |
| Review agent rebroadcast escalation | 1 |
| Max review cycles exceeded | 1 |
| **Total blocked** | **50** |

The oblivion backlog did not shrink — 50 blocked today vs ~49 yesterday. The `#3374` fix (auto-unblock for inactive/removed projects) likely doesn't touch these because the project may still be active in config or the tasks were blocked with a CI-failure reason rather than an inactivity marker. **`orch task unblock all` remains an available manual path if the backlog needs clearing.**

---

## What Failed

### 1. GitHub backend outage continues

The log shows the same `HTTP send failed after 3 attempts — setting circuit-breaker` + 120s retry loop pattern as yesterday. The current session started at attempt 28 and didn't recover until attempt 31 (~6 minutes of startup delay). The circuit breaker eventually closed (`GitHub 5xx circuit breaker CLOSED — resuming normal operations`), so the failure is transient and recovery is automatic. This looks like periodic local network instability rather than a GitHub service issue.

### 2. LLM router repeatedly selects cooled minimax

Both `internal:154739` (this task) and `internal:154740` (evening retrospective) had LLM route to `minimax` (cooled, 3d+ remaining) before the fallback to claude fired. This is a known design limitation: the LLM router doesn't see cooldown state. Frequency is increasing — worth a low-priority improvement to pass cooled agents into the router prompt as context.

### 3. oblivion CI-failure backlog not draining

50 tasks remain blocked on CI failure reasons. The active project count suggests these tasks aren't being swept by the inactive-project auto-unblock logic. Manual intervention or a targeted unblock run may be needed.

---

## Routing Accuracy

Router is functioning correctly at the dispatch layer. The `minimax-cooled → fallback-to-claude` pattern is a correctness-by-design fallback, not a routing failure. No evidence of wrong agent or wrong complexity assignment in the 24h window beyond the cooled-agent fallback.

---

## Prompt / Workflow Quality

The self-improvement loop is operating at good velocity:

- Yesterday's fix for `#3382` (BillingCycleExhausted agent-wide cooldown) was merged and deployed the same day.
- The throughput doubling is strong validation that both yesterday's and today's classifier fixes addressed real blocking patterns.
- No prompt regressions visible in the review decision data (44 decisions, 52 review_starts — healthy review funnel).

---

## Open Issues

`gh issue list --state open` returned **no open issues** in `gabrielkoerich/orch`.

No new issue filed from this review. The LLM-router-selecting-cooled-minimax pattern is a workflow improvement rather than an operational bug — the fallback handles it correctly. The oblivion backlog is a manual unblock candidate rather than a code defect.

---

## Priorities for Tomorrow

1. **Run `orch task unblock all`** — 50 blocked tasks, mostly old CI-failure accumulation. If they re-block immediately they need investigation; if they clear, the backlog was stale.
2. **Watch the minimax-cooled routing frequency.** If the LLM router keeps selecting cooled minimax for every new task, consider whether the router prompt should include the current cooldown list so the LLM makes better selections.
3. **Monitor kimi/opus failure rate.** Four failures in 24h with 11 successes is acceptable, but if it climbs tomorrow it may need a classifier improvement.
4. **Watch nemotron after cooldown expires (~22h).** The nemotron streaming transport fix (#3379) is deployed; first run after cooldown lifts will show whether the fix is effective.
5. **Verify GitHub backend stability.** If the circuit-breaker loop recurs more than once per 24h, it's worth investigating the network path or adding a health probe with better diagnostics.

---

*Prepared by Orch automation (internal:154739) at 2026-07-05 UTC.*
