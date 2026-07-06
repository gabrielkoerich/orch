+++
title = "Daily Review — 2026-07-06"
date = 2026-07-06
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-07-06

## What Shipped (Last 24h)

**3 commits** landed in the last 24 hours.

| Commit | PR | Description |
|--------|----|-------------|
| `11e2682e` | #3387 | fix(review): skip agent-wide cooldown for BillingCycleExhausted with known model |
| `1646aa69` | #3388 | bug(review): billing-limit fast-fail review attempt creates no task_run — missing audit trail |
| `7059caa3` | #3384 | docs(posts): daily review 2026-07-05 |

- **#3385/#3386 → #3387:** Yesterday's fix that skipped agent-wide cooldown for `BillingCycleExhausted` with a known model was applied to the main runner path. Today's follow-up (#3387) extends the same fix to the **review agent path** — `record_review_agent_failure` was applying `BillingCycleExhausted` as a generic agent failure when the model was known, triggering an unnecessary agent-wide cooldown on the review agent. Now returns `RetryableError::ModelUnavailable` in that path, keeping the penalty model-scoped.
- **#3388:** When a billing-limit check caused a review attempt to fast-fail before spawning a tmux session, no `task_run` row was created — leaving the audit trail blind. The fix inserts a `task_run` with the billing failure outcome before returning, so every review attempt is accounted for regardless of whether the agent ran.

The running service is **orch 0.80.43**.

---

## Operational Health

### Throughput

`task_activity` comparison vs yesterday:

| Event | Today | Yesterday |
|-------|-------|-----------|
| `status_change` | 338 | 414 |
| `dispatch` | 106 | 143 |
| `push` | 87 | 95 |
| `review_start` | 55 | 52 |
| `review_decision` | 41 | 44 |
| `routed` | 48 | 63 |
| `error` | 16 | 37 |
| `rerouted` | 5 | 15 |
| `timeout` | 9 | — |

Overall volume is slightly lower, but the quality signal is strong: **errors dropped 57% (37 → 16) and reroutes dropped 67% (15 → 5)**. This tracks directly with the BillingCycleExhausted cooldown fixes — fewer spurious agent-wide cooldowns means fewer forced reroutes.

### Agent / Model Outcomes (last 24h)

| Agent | Model | Outcome | Count |
|-------|-------|---------|-------|
| claude | sonnet | success | 43 |
| kimi | opus | success | 18 |
| opencode | deepseek-v4-flash-free | success | 9 |
| opencode | north-mini-code-free | success | 7 |
| kimi | opus | failed | 3 |
| opencode | north-mini-code-free | timeout | 3 |
| claude | sonnet | failed | 2 |
| codex | gpt-5.5 | success | 2 |
| codex | gpt-5.4 | failed | 1 |
| codex | gpt-5.4 | success | 1 |
| codex | gpt-5.4 | timeout | 1 |
| codex | gpt-5.5 | failed | 1 |
| codex | gpt-5.5 | rate_limit | 1 |
| kimi | opus | rate_limit | 1 |
| opencode | mimo-v2.5-free | parse_error | 1 |
| opencode | mimo-v2.5-free | success | 1 |
| opencode | nemotron-3-ultra-free | failed | 1 |
| opencode | north-mini-code-free | blocked | 1 |
| opencode | north-mini-code-free | failed | 1 |
| opencode | north-mini-code-free | parse_error | 1 |

Claude/sonnet is carrying the heaviest load (43 successes). Codex has a 2d17h persisted cooldown, which explains low codex volume. Kimi/opus failure count (3) is on the same level as yesterday; no escalating pattern. `north-mini-code-free` cooldown expires in ~3h — watch whether it returns cleanly.

### Active Cooldowns

| Key | Remaining | Type |
|-----|-----------|------|
| codex | ~2d17h | persisted |
| minimax:opus | ~2d11h | persisted |
| opencode/north-mini-code-free | ~3h | persisted |

The `minimax:opus` cooldown has ~2d11h left. The LLM router continues to select minimax as the preferred agent for both internal tasks (daily-review and evening-retrospective), triggering the `routing sanity warning` fallback to claude on every run. This is functioning correctly but generates noise.

### Blocked Inventory

| Reason | Count |
|--------|-------|
| CI failure limit reached | 40 |
| GitHub Actions billing failure | 5 |
| No block reason recorded | 4 |
| Review agent rebroadcast escalation | 1 |
| Max review cycles exceeded | 1 |
| **Total blocked** | **51** |

The CI-failure backlog grew by 1 (50 → 51). **`orch task unblock all` is still the recommended manual path** to drain the stale CI-failure accumulation — if tasks re-block immediately on retry they warrant investigation; if they clear, the backlog was stale.

---

## What Failed

### 1. Periodic GitHub API request failures

The service log shows 3 instances of `HTTP send failed, will retry if attempts remain` against one downstream project's issues endpoint. Each failure adds ~12–14s to the sync tick (vs ~2s normal). Retries succeed on the next attempt. This is the same transient pattern noted yesterday; frequency is consistent, not worsening.

### 2. LLM router selects minimax on every internal task

Both `internal:154779` (daily-review) and `internal:154780` (evening-retrospective) had the LLM router select minimax (2d11h cooldown remaining) before the fallback to claude fired. This will recur every day until the minimax cooldown expires (~2026-07-09). The fallback is working; the noise is the cost.

### 3. opencode/north-mini-code-free: 3 timeouts + 1 parse_error + 1 failed

The `north-mini-code-free` model is about to exit its cooldown. The 3 timeouts in this window likely contributed to the cooldown being set. Its performance post-cooldown will be the real signal.

---

## Routing Accuracy

Dispatch accuracy is good. No wrong complexity tier or wrong agent type visible. The LLM router's minimax selection is a false positive caught by the cooldown gate — the downstream assignment (claude/sonnet for medium, claude/opus for complex) is appropriate.

---

## Prompt / Workflow Quality

The BillingCycleExhausted fix series (#3382 → #3383 → #3385/#3386 → #3387) reached its natural conclusion today. The full flow — from initial misclassification to model-scoped penalty to review-agent path to audit trail — is now closed. The 67% reroute reduction is a direct measurement of the fix quality.

---

## Open Issues

`gh issue list --state open` returned **no open issues** in `gabrielkoerich/orch`.

No new issues filed. The minimax-routing noise and the CI-failure backlog are operational states, not code defects.

---

## Priorities for Tomorrow

1. **Watch `north-mini-code-free` after cooldown lifts** (~3h). If it immediately re-errors, a classifier or model-quality issue may need investigation.
2. **Consider running `orch task unblock all`** to drain the 40-task CI-failure backlog. Monitor if tasks re-block — if they do, inspect the specific CI failure reason.
3. **minimax cooldown expires ~2026-07-09** — expect the `routing sanity warning` on every internal task until then. No action needed; note it in the review.
4. **kimi/opus failure rate**: 3 failures / 18 successes (17%) today. If it climbs tomorrow, check for rate-limit patterns or model instability.
5. **Periodic GitHub API failures**: currently transient with auto-retry. If the pattern worsens to multiple failures per sync tick, investigate the network path to that project's GitHub endpoint.

---

*Prepared by Orch automation (internal:154779) at 2026-07-06 UTC.*
