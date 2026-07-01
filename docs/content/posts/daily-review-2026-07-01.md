+++
title = "Daily Review — 2026-07-01"
date = 2026-07-01
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-07-01

## What Shipped (Last 24h)

**2 commits** landed in the last 24 hours.

| Commit | PR | Description |
|--------|----|-------------|
| `f615b203` | #3369 | bug(review): record_review_agent_failure catch-all records no cooldown for AgentFailed — model re-selected immediately after failure |
| `73f742ba` | #3366 | docs(posts): daily review 2026-06-30 |

**Notable fix:** PR #3369 corrects a gap in the failure recording path. The `record_review_agent_failure` catch-all branch (which handles all `AgentFailed` errors not matched by more specific arms) was not calling `record_agent_failure()`, so the model was immediately re-selected after an `AgentFailed` error instead of entering exponential backoff. The fix ensures the generic backoff system applies uniformly — no special-casing.

---

## Operational Health

### Throughput (Last 24h)

| Metric | Count |
|--------|-------|
| Status changes | 284 |
| Pushes | 85 |
| Dispatches | 83 |
| Review starts | 47 |
| Branch deletes | 44 |
| Review decisions | 41 |
| PRs created | 39 |
| Routed | 31 |
| Errors | 14 |

Throughput is up solidly from yesterday (83 dispatches vs 66, 39 PRs vs 31, 41 review decisions vs 32). Errors also rose from 8 → 14, driven primarily by claude/sonnet failures.

### Agent / Model Outcomes (Last 24h)

| Agent | Model | Outcome | Count |
|-------|-------|---------|-------|
| claude | sonnet | success | 33 |
| kimi | opus | success | 10 |
| codex | gpt-5.4 | success | 8 |
| opencode | deepseek-v4-flash-free | success | 6 |
| claude | sonnet | failed | 5 |
| opencode | mimo-v2.5-free | success | 3 |
| opencode | nemotron-3-ultra-free | failed | 2 |
| claude | sonnet | push_failed | 1 |
| claude | sonnet | rate_limit | 1 |
| claude | sonnet | *(null)* | 1 |
| codex | gpt-5.4 | rate_limit | 1 |
| kimi | opus | rate_limit | 1 |
| opencode | nemotron-3-ultra-free | parse_error | 1 |
| opencode | north-mini-code-free | parse_error | 1 |

**Notable trend:** `claude/sonnet` failures are escalating — 0 on 6/28, 3 on 6/29, 5 today. Also hit rate_limit once and push_failed once. Still no cooldown triggered (failure count below threshold), but this is approaching the threshold. The PR #3369 fix means future `AgentFailed` errors through the catch-all will now correctly accumulate failure counts and trigger cooldown.

`codex/gpt-5.4` recovered well: 8 successes today (vs 1 yesterday). `kimi/opus` maintained 10 successes despite a rate_limit hit.

### What Went Well

1. **High throughput day.** 83 dispatches, 39 PRs created, 41 review decisions — best numbers in several days.
2. **codex/gpt-5.4 recovered.** 8 successes after near-zero yesterday. Weight decay had not fully suppressed it.
3. **Routing fallback worked.** This review task (internal:154569) had the LLM router select cooled minimax → fallback to claude:sonnet executed cleanly in the same tick.
4. **PR #3369 landed.** The catch-all gap in `record_review_agent_failure` is fixed. Failure backoff is now complete for all AgentFailed error variants.
5. **Sync tick performance.** Ticks stable at 1.8–3.9ms. No circuit breaker events today.

### What Failed

#### 1. `claude/sonnet` — 5 failures + 1 rate_limit + 1 push_failed

Third consecutive day of escalating claude/sonnet failures (0 → 3 → 5). The rate_limit hit suggests billing or quota pressure rather than model-level issues. Push_failed is a separate Git/GitHub failure (not model-related).

The PR #3369 fix closes the gap: going forward, `AgentFailed` errors through the catch-all will accumulate failure counts. If failures continue tomorrow, a cooldown will be triggered and the router will shift load to kimi or codex.

**No issue filed** — this is a known escalation to monitor. If failures continue past day 4, investigate the specific error variant being emitted.

#### 2. Service still running v0.80.31 — 5 versions behind (now 5+ days)

**Fifth consecutive day.** The service logs confirm: `orch upgrade available current_version=0.80.31 latest_version=0.80.36`. The ResourceExhausted fix for opencode/nemotron-3-ultra-free shipped in v0.80.34 and remains undeployed.

**Operator action required:**
```bash
brew update && brew upgrade orch
brew services restart orch
orch -V
```

#### 3. `opencode/nemotron-3-ultra-free` — 2 failures + 1 parse_error

Continues to fail as expected. The fix is in v0.80.34 (already shipped), waiting on operator to deploy.

#### 4. `opencode/north-mini-code-free` — 1 parse_error

One parse_error today (vs 2 yesterday). Trend unclear — could be noise. Third data point: 1 on 6/29, 2 on 6/30, 1 today. No pattern strong enough to file an issue.

#### 5. Multiple tasks blocked — GitHub Actions billing failure

5 tasks remain blocked at merge time due to GitHub Actions billing failure. These are at the correct granularity (per-task at merge time). Resolving requires fixing billing, then `orch task unblock all`.

#### 6. Multiple tasks blocked — CI failure limit

13+ tasks blocked with "CI failure limit (3) reached during auto-merge." Long-standing, not from today's work.

---

## Routing Accuracy

Routing was accurate. The LLM selected minimax for this review task — which is on cooldown — and the fallback system correctly rerouted to claude:sonnet within the same tick. No wasted dispatch.

`kimi` is on a long cooldown (2d15h remaining), which explains why `kimi:haiku` failed a pool-entry check for the evening-retrospective routing. The kimi:haiku sub-key cooldown (44m) is a short rate-limit cooldown layered on top of the agent-level cooldown — the generic system is handling it.

---

## Active Cooldowns

| Key | Remaining | Reason |
|-----|-----------|--------|
| kimi | 2d15h | persisted |
| minimax:opus | 10h2m | persisted |
| kimi:haiku | 44m | persisted |

Kimi agent-level cooldown is new since yesterday (only `minimax:opus` was listed then). This is a significant routing impact: kimi had been the second-highest-throughput agent (10 successes today) but will be fully unavailable for the next ~2.5 days.

---

## Stuck / Pending Tasks

- `internal:154570` (evening retrospective): in progress as of log time
- 5 tasks blocked by GitHub Actions billing failure
- 13+ tasks blocked by CI failure limit

---

## Open Issues

No open issues (confirmed: `gh issue list --state open` returned none).

Closed today:
- #3368: ops: service stuck at v0.80.31 for 7 days (closed)
- #3367: bug(review): record_review_agent_failure catch-all fix (closed — PR #3369 merged)
- #3364: ops: service v0.80.31 lags v0.80.35 (closed)
- #3361: bug(review): OpenCode Nvidia ResourceExhausted review failure (closed)

---

## Priorities for Tomorrow

1. **Upgrade the running service** to v0.80.36. Five consecutive days of deferrals. The ResourceExhausted fix for opencode/nemotron is waiting on this.
   ```bash
   brew update && brew upgrade orch
   brew services restart orch
   orch -V
   ```
2. **Monitor claude/sonnet failures.** Day 4 could trigger cooldown (depending on failure count threshold). If a cooldown fires, note whether codex/gpt-5.4 absorbs the load cleanly. If failures continue without cooldown triggering, extract the error variant and assess.
3. **Watch kimi cooldown.** With kimi out for 2.5+ days, load shifts to claude and codex. Confirm codex/gpt-5.4 sustains throughput tomorrow.
4. **Resolve GitHub Actions billing failure** so 5 blocked tasks can be unblocked.
5. **Monitor minimax cooldown** (~10h remaining). When it clears, first dispatch will verify recovery.

---

*Prepared by Orch automation (internal:154569) at 2026-07-01T23:00Z.*
