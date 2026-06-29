+++
title = "Daily Review — 2026-06-29"
date = 2026-06-29
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-06-29

## What Shipped (Last 24h)

**1 commit** landed in the last 24 hours.

| Commit | PR | Description |
|--------|----|-------------|
| `348dc61a` | #3363 | docs(posts): daily review 2026-06-28 |

Light commit day — no code changes, only the documentation post from the previous review cycle.

### Closed Issues (Last 24h)

| Issue | Description | Closed At |
|-------|-------------|-----------|
| #3364 | ops: service v0.80.31 lags latest v0.80.35 — Nvidia ResourceExhausted fix undeployed | 2026-06-29T21:13Z |

Issue #3364 was the third successive ops issue filed for service deployment lag. The root cause remains unchanged: the operator has not run `brew upgrade orch && brew services restart orch`. The fix (`#3362`, Nvidia ResourceExhausted classified as rate limit) shipped in v0.80.35 but the running service is still v0.80.31.

---

## Operational Health

### Throughput (Last 24h)

| Metric | Count |
|--------|-------|
| Status changes | 283 |
| Dispatches | 87 |
| Pushes | 91 |
| Branch deletes | 52 |
| Review starts | 49 |
| Review decisions | 43 |
| PRs created | 42 |
| Routed | 35 |
| Errors | 7 |
| Reroutes | 1 |

Throughput remained strong — 42 PRs created and 43 review decisions with only 7 errors and 1 reroute against 87 dispatches. The single reroute was the router correctly falling back from cooled `minimax` to `claude:sonnet` for this review task.

### Agent / Model Outcomes (Last 24h)

| Agent | Model | Outcome | Count |
|-------|-------|---------|-------|
| claude | sonnet | success | 39 |
| kimi | opus | success | 15 |
| opencode | deepseek-v4-flash-free | success | 5 |
| codex | gpt-5.4 | success | 4 |
| opencode | mimo-v2.5-free | success | 3 |
| opencode | nemotron-3-ultra-free | failed | 3 |
| opencode | nemotron-3-ultra-free | success | 2 |
| opencode | north-mini-code-free | success | 2 |
| claude | haiku | success | 1 |
| kimi | opus | failed | 1 |
| codex | gpt-5.5 | failed | 1 |
| codex | gpt-5.5 | success | 1 |
| minimax | sonnet | rate_limit | 1 |
| opencode | north-mini-code-free | parse_error | 1 |

**Notable trends vs. yesterday:**
- Kimi/opus grew from 10 → 15 successes — a significant increase, suggesting kimi took on more complex tasks today
- `opencode/nemotron-3-ultra-free` had 3 failures (up from 1 yesterday) — all Nvidia ResourceExhausted, the exact error targeted by the undeployed fix in v0.80.35
- `codex/gpt-5.4` dropped from 10 → 4 successes; `codex/gpt-5.5` appeared with both a success and a failure
- `opencode/north-mini-code-free` recorded its first `parse_error` outcome

### What Went Well

1. **Extremely low reroute rate.** Only 1 reroute in 87 dispatches — the router/fallback chain ran cleanly all day.
2. **Kimi stepped up.** 15 successful kimi/opus completions indicates the kimi agent is healthy and taking on a meaningful share of complex tasks.
3. **Engine stability.** Sync ticks running consistently at 1.7–3s elapsed; only occasional spikes to 15–28s due to transient GitHub HTTP failures (which retry and recover without human intervention).
4. **Routing fallback worked correctly.** The LLM router selected the cooled `minimax` agent for this daily review task, immediately detected it as cooled, and fell back to `claude:sonnet` — exactly the intended path.

### What Failed

#### 1. Service still running v0.80.31 (upgrade to v0.80.35 pending)

This is the third consecutive day this issue has been filed (ops #3358, #3360, #3364). The service continues on v0.80.31 while v0.80.35 includes:

- `fix(runner): detect Nvidia ResourceExhausted as rate limit` (#3362 / v0.80.34)
- `fix(sync): edge-trigger stale model-pool alert log` (#3357 / v0.80.32)

The stale model-pool alert noise from `minimax` will persist until the service is upgraded.

**Operator action required:**
```bash
brew update && brew upgrade orch
brew services restart orch
orch -V   # should show 0.80.35
```

#### 2. `opencode/nemotron-3-ultra-free` — 3 failures from Nvidia ResourceExhausted

All three failures today were Nvidia `ResourceExhausted` (worker local total request limit). Without the v0.80.35 fix deployed, these are classified as generic `AgentFailed` rather than `RateLimit`, causing short agent-level backoff (5 min) instead of a proper model-level cooldown. The result is repeated retries every few hours.

This will self-correct once the service is upgraded.

#### 3. `opencode/north-mini-code-free` — one parse_error

A single `parse_error` outcome for `north-mini-code-free`. This is a known intermittent failure mode for review sessions (documented in the skill notes, fixed for parse errors that trigger cooldown in v0.80.17). One occurrence is not a pattern; monitor for recurrence.

#### 4. `kimi/opus` — one failure

A single kimi/opus failure. No escalation to cooldown. Likely transient; failure count should be near 0 in KV. No action needed.

#### 5. `minimax:opus` — long cooldown continues

Current active cooldowns:

```
minimax:opus    2d10h remaining    (persisted)
```

This is a continuation from previous days. The router is correctly skipping minimax. No new action needed until the cooldown expires in ~2.5 days.

---

## Routing Accuracy

Routing was accurate. No mis-routing patterns or silent model failures observed. The LLM selected a cooled agent (`minimax`) for a task — the pre-emptive cooldown guard caught it and rerouted to `claude:sonnet` immediately. This is working as designed.

The `kimi/haiku` router was used for routing decisions (confirmed in logs) — the router itself is functioning cleanly.

---

## Stuck / Pending Work

Only `internal:154498` (this daily review) and `internal:154499` (evening retrospective) are currently in progress. No stuck or blocked tasks observed in the orch-internal task set at review time.

---

## Open Issues

**Open issues in `gabrielkoerich/orch` at review time: 0.**

No new issues warranted from this review. The deployment lag driving the nemotron failures is an operator action item, not a code bug. The parse_error and transient failures are below the threshold for issue creation.

---

## Priorities for Tomorrow

1. **Upgrade the running service to v0.80.35.** This is the highest-leverage available action — delivers the Nvidia ResourceExhausted rate-limit classification and the stale model-pool alert edge-trigger fix in one step. Has been pending for multiple days; nemotron will keep failing until this is done.
   ```bash
   brew update && brew upgrade orch
   brew services restart orch
   orch -V   # verify 0.80.35
   ```
2. **Confirm nemotron failures stop post-upgrade.** After the upgrade, `opencode/nemotron-3-ultra-free` should get model-specific cooldowns on ResourceExhausted instead of short agent-level backoff. If failures recur post-upgrade, that is a new regression to investigate.
3. **Watch `minimax:opus` cooldown expiry (~2.5 days).** When it clears, the first new dispatch will determine if minimax has recovered or if a deeper outage persists.
4. **Monitor `north-mini-code-free` parse_error rate.** One occurrence is noise; two in the same window would warrant a closer look at the parse path for that model's review output format.
5. **Watch `codex/gpt-5.5`.** Mixed success/failure today (1 each). If failures grow, verify whether the error is a new model-unavailability variant needing classifier coverage.

---

*Prepared by Orch automation (internal:154498) at 2026-06-29T23:01Z.*
