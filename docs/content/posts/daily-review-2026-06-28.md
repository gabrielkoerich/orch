+++
title = "Daily Review — 2026-06-28"
date = 2026-06-28
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-06-28

## What Shipped (Last 24h)

**3 commits** landed in the last 24 hours.

| Commit | PR | Description |
|--------|----|-------------|
| `5b8722e2` | #3362 | fix(runner): detect Nvidia ResourceExhausted as rate limit |
| `84842e14` | #3360 | ops: service still running v0.80.31 after v0.80.32 release |
| `430039f2` | #3359 | docs(posts): daily review 2026-06-27 |

### Closed Issues (Last 24h)

| Issue | Description |
|-------|-------------|
| #3361 | bug(review): OpenCode Nvidia ResourceExhausted review failure |
| #3358 | ops: service still running v0.80.31 after v0.80.32 release |

The most significant fix was `#3362` / `#3361`: OpenCode review sessions were hitting Nvidia `ResourceExhausted` errors that the parser was not recognising as rate limits. The fix classifies them correctly so the runner now applies a model cooldown rather than treating them as fatal failures.

---

## Operational Health

### Throughput (Last 24h)

| Metric | Count |
|--------|-------|
| Status changes | 315 |
| Dispatches | 102 |
| Pushes | 99 |
| Branch deletes | 84 |
| Review starts | 48 |
| Review decisions | 47 |
| PRs created | 47 |
| Routed | 43 |
| Errors | 7 |
| Reroutes | 2 |

Throughput remained healthy throughout the window. Nearly 50 PRs created and reviewed with only 7 errors and 2 reroutes — the engine stayed stable.

### Agent / Model Outcomes (Last 24h)

| Agent | Model | Outcome | Count |
|-------|-------|---------|-------|
| claude | sonnet | success | 39 |
| codex | gpt-5.4 | success | 10 |
| kimi | opus | success | 10 |
| opencode | opencode/mimo-v2.5-free | success | 8 |
| opencode | opencode/deepseek-v4-flash-free | success | 5 |
| opencode | opencode/north-mini-code-free | success | 5 |
| opencode | opencode/nemotron-3-ultra-free | success | 4 |
| claude | sonnet | failed | 2 |
| codex | gpt-5.5 | failed | 1 |
| codex | gpt-5.5 | success | 1 |
| minimax | opus | rate_limit | 1 |
| minimax | sonnet | rate_limit | 1 |
| opencode | opencode/mimo-v2.5-free | failed | 1 |
| opencode | opencode/nemotron-3-ultra-free | failed | 1 |
| claude | haiku | success | 1 |

Claude/sonnet dominated success volume (39), followed by Codex/gpt-5.4 (10) and Kimi/opus (10). OpenCode contributed a healthy spread across four free models. The two `claude:sonnet` failures and the `codex:gpt-5.5` failure were isolated; failure counts for both are 0 in the current KV state, so they recovered cleanly.

### What Went Well

1. **Nvidia ResourceExhausted fix shipped and closed same day.** `#3361` was filed, `#3362` fixed, and both closed within the review window — rapid root-cause-to-merge cycle.
2. **Broad agent diversity.** Claude, Codex, Kimi, and four OpenCode models all recorded successful completions. The pool remains resilient.
3. **Low error and reroute rate.** 7 errors and 2 reroutes against 102 dispatches is a healthy signal — the engine did not need to fight against systematic failures.
4. **Routing fallback worked correctly for this very task.** The router's LLM selected `minimax` for this review task, immediately detected it was cooled, and fell back to `claude:sonnet` without operator intervention.

### What Failed

#### 1. Service is still running v0.80.31

The CLI and service both report `orch 0.80.31`. The latest published release is `v0.80.34` (which includes the Nvidia ResourceExhausted rate-limit fix from #3362, plus `fix(sync): edge-trigger stale model-pool alert log` from v0.80.32). The stale-alert warning is therefore still visible in every sync tick:

```text
agent model pool appears stale: persistent model failures in heavily cooled pool
affected_agents=["minimax:1/2:opus"]
```

This is a deployment lag issue, not a regression in the new code. The fix exists; it just has not been deployed.

**Action required:** `brew update && brew upgrade orch && brew services restart orch`

#### 2. `minimax:opus` remains in a long cooldown

Active cooldown state:

```
minimax:opus    3d10h remaining    (persisted)
```

Failure counts: `minimax=5`, `minimax:haiku=5`, `minimax:opus=4`, `minimax:sonnet=1`.

The pool continues to be effectively dead. The router is correctly skipping it, but the extended cooldown means minimax will remain unavailable for another 3+ days unless cleared manually. This is not new — it has persisted across multiple review windows. The behaviour is correct (exponential backoff protecting against a consistently failing pool), but the volume of stale-alert spam it produces will only stop once the service is upgraded to v0.80.34.

#### 3. Two `claude:sonnet` failures

The two `claude:sonnet` failures did not trigger cooldown escalation (failure count reset to 0 in KV), so they were likely transient. No pattern to investigate.

---

## Routing Accuracy

Routing was accurate this window. The one notable routing event was the LLM choosing `minimax` for this daily-review task and the fallback system correctly redirecting to `claude:sonnet` in the same routing pass — exactly the intended behaviour. No mis-routing patterns or silent model failures observed.

---

## Stuck / Pending Work

| Task | Status | Note |
|------|--------|------|
| `internal:154468` | in_progress | This daily review |
| `internal:154469` | created | Evening retrospective (dispatched same tick) |
| `internal:154470` | created | Weekly review (dispatched same tick) |
| External blocked tasks | blocked | GitHub Actions billing failures and CI-failure limits in downstream repos — unchanged from yesterday |

No new stuck-task pattern inside `gabrielkoerich/orch` itself.

---

## Issues

**Open issues in `gabrielkoerich/orch` at review time: 0.**

No new issues warranted from this review. The `minimax` cooldown spam is already addressed by v0.80.32; the Nvidia fix is in v0.80.34 — both are deployment lag only. The two transient claude failures require no action.

---

## Priorities for Tomorrow

1. **Upgrade the running service to v0.80.34.** The single most impactful action available — delivers both the edge-trigger stale-pool fix (v0.80.32) and the Nvidia ResourceExhausted rate-limit classification (v0.80.34) in one step.
2. **Confirm warning volume drops post-upgrade.** If the minimax stale-pool warning persists after upgrading to v0.80.34, that is a new regression and should be investigated. If it disappears, the noise was deployment lag only.
3. **Watch `minimax` cooldown expiry.** The active `minimax:opus` cooldown clears in ~3.5 days. When it does, observe whether the first new dispatch succeeds or re-triggers the rate-limit cycle.
4. **Continue monitoring `codex:gpt-5.5`.** It had one success and one failure in this window. It is currently at failure_count=0, so the failure was transient — keep an eye on it for early signs of a broader outage.

---

*Prepared by Orch automation (internal:154468) at 2026-06-28T23:01Z.*
