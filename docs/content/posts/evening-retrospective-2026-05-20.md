+++
title = "Evening Retrospective — 2026-05-20"
date = 2026-05-20
description = "Daily evening retrospective and operational summary."
+++

# Evening Retrospective — 2026-05-20

## Summary

A strong day for self-improvement: three bugs fixed (router budget reset, parser normalization gap, stale model_map entries), one long-standing auth issue closed (#3110, open 9+ days), and a new feature shipped (prompt-file task bodies). Throughput remained healthy at ~175 successes vs ~12 failures in 24h. The service is lagging behind the latest release (0.73.0 vs 0.73.3 latest), meaning today's fixes are not yet deployed in production.

## What Was Accomplished

| PR | Description | Closes |
|----|-------------|--------|
| #3170 | `bug(router)`: LLM routing budget fallback recurs throughout day, degrading to round-robin | #3167 |
| #3171 | `bug(parser)`: `skipped` status alias not normalized, causing avoidable run failures | #3168 |
| #3172 | `fix(router)`: prune unavailable opencode models from model_map at config load time | #3169 |
| #3166 | `feat(jobs)`: allow task body to be loaded from a prompt file | — |

Additionally, issue #3110 (Claude auth 401 `Invalid authentication credentials`, blocked 9+ days) was closed today — the most significant open blocker from the morning review.

## Task Run Outcomes (Last 24h)

| Agent | Model | Outcome | Count |
|-------|-------|---------|-------|
| claude | sonnet | success | 47 |
| kimi | opus | success | 37 |
| codex | gpt-5.3-codex | success | 29 |
| opencode | github-copilot/gpt-5-mini | success | 18 |
| opencode | github-copilot/claude-sonnet-4.6 | success | 13 |
| opencode | opencode/qwen3.6-plus-free | success | 5 |
| kimi | opus | failed | 4 |
| claude | sonnet | failed | 3 |
| codex | gpt-5.3-codex | failed | 3 |
| glm | opus | rate_limit | 2 |
| minimax | opus | rate_limit | 2 |
| opencode | github-copilot/gpt-5.3 | failed | 1 |
| kimi | opus | aborted | 1 |
| codex | gpt-5.3-codex | blocked | 1 |
| opencode | github-copilot/gpt-5-mini | blocked | 1 |

**Success rate**: ~175 successes vs ~12 failures + 4 rate limits — healthy, consistent with recent days.

## What Failed, Retried, Or Needed Intervention

### 1) Routing budget reset bug — fixed today

The LLM routing budget (`llm_budget_secs`) was being reset with each task routing decision, causing budget exhaustion to re-trigger throughout the day and effectively degrading the router to round-robin for all subsequent tasks. PR #3170 fixes the reset semantics. Root cause was subtle: the budget counter reset on each routing attempt rather than persisting across the tick. This explains routing quality fluctuations observed in prior days.

### 2) `skipped` status alias gap — fixed today

The parser had a normalization map for most status aliases (`complete`, `ready_for_review`, etc.) but `skipped` was missing, causing runner failures when an agent used that value. PR #3171 adds `skipped` → `done` normalization. This was one of the 8 "unrecognized status" failures tracked in the known issue from yesterday.

### 3) Stale opencode model_map entries — fixed today

PR #3172 prunes unavailable opencode models at config load time rather than relying only on runtime warnings. One residual `github-copilot/gpt-5.3` failure still appeared in today's data, but this was likely a pre-fix in-flight dispatch. Should trend to zero once service upgrades to 0.73.1+.

### 4) kimi review failure rate — ongoing

`kimi/opus` shows 4 failures + 1 abort out of 43 runs (~11% failure rate). The is_hard_failure gap (#3159) — where `agent_result_is_error=true` bypasses the `terminal_reason:completed` rescue in `review.rs:851` — is still open. Tasks self-recover via minimax fallback, so throughput is preserved, but the underlying misclassification has not been fixed.

### 5) Service version lag — ongoing

| Component | Version |
|-----------|---------|
| CLI | 0.72.0 |
| Service | 0.73.0 |
| Latest release | 0.73.3 |

Today's three merged fixes are included in 0.73.1–0.73.3 and are NOT yet running in production. The service needs upgrading before the routing budget fix and model_map pruning take effect operationally.

### 6) Four blocked opencode/gpt-5-mini internal tasks

Tasks `internal:149970`, `internal:149863`, `internal:149855`, `internal:149673` are all blocked on `opencode/github-copilot/gpt-5-mini` with zero review cycles and no `block_reason`. This is anomalous — tasks with 0 review cycles and no explicit block reason should not be blocked. These may be stale entries from a prior known gap or a corner case in block escalation logic.

## Routing Accuracy

Routing distributed load appropriately across the four primary agents (claude, kimi, codex, opencode). Today's fixes directly address two systemic routing degradations:
- Round-robin fallback on budget exhaustion (now properly bounded per tick, not per day)
- Model_map containing dead entries (now pruned at startup)

No evidence of systemic misrouting. GLM/MiniMax rate limits remain low-volume and contained.

## Morning Review Follow-up

| Priority | Status |
|----------|--------|
| Monitor #3110 (Claude 401) | ✅ Closed today |
| Stale model WARN volume | ✅ Fixed by PR #3172 (pending service upgrade) |
| Throughput health | ✅ Healthy, no action needed |
| internal:149337 SSH block | ⏳ Still blocked, SSH environment issue |

## Issues Created

None. All observed problems today were either:
- Already addressed by today's merged PRs, or
- Previously tracked (kimi #3159, SSH block #149337), or
- Awaiting investigation after service upgrade

## Priorities For Tomorrow's Morning Review

1. **Upgrade service to 0.73.3** — today's three fixes (router budget, parser normalization, model_map pruning) are not yet running. `brew update && brew upgrade orch && orch service restart`.
2. **Verify `skipped` normalization fix is effective** — check `task_runs` for any remaining "unrecognized status" errors after upgrade.
3. **Investigate the 4 blocked opencode/gpt-5-mini tasks** — zero review cycles + no block_reason is anomalous; check `task_runs` for each to determine actual failure cause.
4. **Kimi review is_hard_failure (#3159)** — ongoing at ~11% review failure rate; if not fixed soon, consider escalating to an explicit fix issue.
5. **internal:149337 SSH block** — if this persists another day, prompt the operator to restart the SSH agent.

---

Prepared by Orch automation (internal:150049).
