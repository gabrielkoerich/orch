+++
title = "Morning Review — 2026-04-15"
date = 2026-04-15
description = "PR #2632 merged. GitHub 5xx circuit breaker active. kimi billing exhausted (6d11h cooldown — did not recover as expected). CLI still mismatched (0.69.8 vs 0.69.10). Multi-agent degradation: codex + opencode + kimi all cooled."
+++

# Morning Review — 2026-04-15

## Recent Commits (last 48h)

- `b0c37e60` — **fix: remove stale `clear_output` call in `Transport::unbind`** — fixed CI-blocking compile error on PR #2632 (stale callsite left after `last_output` removal in #2628)
- `b005ac35` — docs: update morning review — kimi recovery imminent, service healthy
- `3717e774` — docs: update morning review 2026-04-14 — add tick loop stall regression
- `ff4d77b5` — docs: add morning review 2026-04-14
- `19f40336` — bug: transport.rs `last_output` dead code accumulates unbounded output (#2628)
- `635fe92d` — fix: `Transport::unbind()` prevents HashMap memory leak (#2630)

PR #2632 (morning review 2026-04-14) is **merged**.

---

## Operational Health

### Service version

- CLI: `0.69.8`
- Service: `0.69.10` — **MISMATCHED**

`brew upgrade orch && brew services restart orch && orch version` is still outstanding. This mismatch has been flagged for 3+ days. Do this first before any debugging session.

### GitHub 5xx circuit breaker — active at review time

The `github:5xx` circuit breaker was open at review time (~2m remaining). This means routing is paused — no new tasks are being dispatched. The breaker opened in response to repeated GitHub API 5xx responses. The engine handles this gracefully, but it's worth monitoring: if the breaker keeps reopening frequently, it signals an upstream GitHub degradation.

### Multi-agent degradation

Three agents simultaneously in cooldown — the worst simultaneous degradation to date:

| Agent | Cooldown | Reason |
|-------|----------|--------|
| `codex` | 18h51m | Billing cycle exhausted (persisted) |
| `kimi` | **6d11h** | Billing cycle exhausted — was expected to recover today; did not |
| `opencode` | ~2m (expiring) | Short agent-level cooldown |
| `opencode:github-copilot/claude-sonnet-4.6` | 3h46m | Silence detection |
| `opencode:github-copilot/gemini-3.1-pro-preview` | 3h16m | Silence detection |
| `opencode:github-copilot/gpt-5.4` | 57m | Failure |
| `glm:haiku` | 3h5m | Model cooldown |

The engine correctly logged `multi-agent degradation detected` (degraded_count=3). Active agents: claude, glm, minimax. These three are carrying all load.

### kimi — did NOT recover as expected

Yesterday's retro said `kimi` cooldown was ~7h remaining and expected to recover by ~03:00 UTC Apr 15. It did not recover — the cooldown is now **6d11h** (billing cycle exhausted, updated to the extended backoff tier). This is a billing event, not a transient failure. kimi is unavailable for the rest of the week.

---

## Agent Performance (last 24h)

| Agent | Model | Success | Failed | Rate |
|-------|-------|---------|--------|------|
| claude | sonnet | 55 | 26 | **68%** |
| minimax | opus | 43 | 4 | **91%** |
| opencode | github-copilot/gpt-5-mini | 33 | 1 | **97%** |
| opencode | opencode/minimax-m2.5-free | 27 | 1 | **96%** |
| glm | opus | 25 | 10 | 71% |
| opencode | opencode/nemotron-3-super-free | 14 | 7+2 parse | 61% |
| claude | opus | 3 | 8 | **27%** |
| opencode | github-copilot/gemini-3.1-pro-preview | 1 | 11 | 8% |
| opencode | github-copilot/gpt-5.4 | 1 | 11 | 8% |
| opencode | github-copilot/claude-sonnet-4.6 | 0 | 6 | 0% |
| opencode | github-copilot/claude-opus-4.6 | 0 | 4+1 | 0% |
| claude | haiku | 1 | 2 | 33% |

**Key observations:**
- claude/sonnet, minimax/opus, and opencode/gpt-5-mini are the reliable workhorses (68–97%).
- claude/opus at 27% (3 success, 8 failed) — consistent with prior finding: complex task mix, not model degradation.
- All GitHub Copilot models continue failing. Cooldowns correctly applied. gemini-3.1-pro-preview and gpt-5.4 both at <10% — these should be deprioritized or disabled in routing weights.
- glm/opus at 71% — reliable secondary carrier.

---

## Task Activity (last 12h)

| Event | Count |
|-------|-------|
| status_change | 498 |
| dispatch | 167 |
| branch_delete | 116 |
| push | 99 |
| routed | 83 |
| review_start | 51 |
| review_decision | 45 |
| pr_create | 43 |
| error | 33 |
| rerouted | 6 |

High dispatch and review volume — the pipeline is working. 33 errors (tracked in task_activity) is moderate; with 167 dispatches that's a ~20% error event rate, which is expected given hard task mix.

---

## Stuck / Blocked Tasks

- **44 tasks blocked** — unaudited. Carried from yesterday.
- **4 tasks in progress** — normal.
- **0 tasks routed** — GitHub 5xx breaker paused routing.

Blocked task audit is still outstanding from yesterday. Until this is done, it's unknown how many blocked tasks are actionable vs permanent blocks.

---

## Retro Follow-ups

| Item | Status |
|------|--------|
| Fix CLI version mismatch | **STILL OUTSTANDING**. Now 0.69.8 vs 0.69.10. Run `brew upgrade orch && brew services restart orch`. |
| claude/opus failure rate | Concluded: hard task mix. Not model degradation. No action needed. |
| Tick loop stall regression (#2633) | In progress. No new stalls observed overnight. Fix likely landed in cap-routing-to-1 commit (`03690ce6`). Monitor. |
| kimi recovery (expected Apr 15) | **DID NOT RECOVER** — billing cycle exhausted. 6d11h cooldown now. kimi unavailable until ~Apr 22. |
| Audit 47 blocked tasks | **Still unaudited.** |
| Unblock internal:145238 (false positive) | Unknown — not verified this session. |
| Close/de-dup task 2622 | Unknown. |
| Human review PR #2557 (task 2555) | **Requires owner action.** |

---

## Priorities

1. **Fix CLI version mismatch** — `brew upgrade orch && brew services restart orch && orch version`. Outstanding for 4+ days.
2. **Monitor GitHub 5xx breaker** — was active at review time, should clear soon. If it reopens repeatedly today, investigate upstream GitHub status.
3. **Audit blocked tasks** — 44 blocked. Categorize by block reason. Identify any false positives that can be unblocked.
4. **Disable GitHub Copilot gemini/gpt-5.4 models in routing** — both at <10% success rate and consuming routing budget with near-certain failures. These should have their routing weights set to 0 or be added to a model denylist.
5. **Human review PR #2557** — task 2555, implementation complete. Needs owner review.
6. **Verify internal:145238 unblocked** — false-positive block from silence detection. Run `orch task unblock internal:145238` if still blocked.

---

## Issues

No new operational issues filed this session. Existing tracked issues:
- **#2633** — tick loop stall regression (router LLM timeout cascade). In progress.

The GitHub Copilot model failure pattern (gemini, gpt-5.4 at <10%) warrants a routing config change rather than a new issue — it's a known degradation the human operator should address via routing weights.

---

Prepared by Orch automation (internal task internal:145307).
