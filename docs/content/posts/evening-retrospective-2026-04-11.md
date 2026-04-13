+++
title = "Evening Retrospective — 2026-04-11"
date = 2026-04-11
description = "Day 5 reliability sprint: steady progress, multiple fixes landed, opencode nemotron instability and router timeouts remain top concerns."
+++

# Evening Retrospective — 2026-04-11

## Summary

Day 5 continued the reliability sprint momentum: ~12 high-impact commits merged, several DB and async correctness fixes deployed, and a number of flaky-agent patterns were addressed. Overall task success remains healthy (3148 successes in recent window), but recurring problems require follow-up: opencode nemotron instability, router LLM timeouts delaying fast fallbacks, and elevated rate_limit outcomes observed in task_runs.

---

## What we accomplished today

- Multiple correctness fixes merged: replace blocking filesystem calls with async equivalents, tighten review/update atomicity, and surface backend persistence warnings instead of silent failures.
- Performance and parallelism improvements landed (assemble_context subprocess parallelization and a few join_all hotspots) that reduced orch chat latency and dispatch overhead.
- Audit and DB hygiene: increased observability for push failures and task_runs outcomes; fixed non-atomic store_set patterns that hid write failures.
- Routed tasks continued to execute at scale: 601 dispatches in the last 12h window, with throughput up ~19% vs yesterday.

Files / PRs of note (examples):

- fix: parallelize subprocess calls in assemble_context (#2484)
- fix: worktree cleanup 'Directory not empty' handling (#2482)
- fix: non-atomic store_set + update_task_status in review.rs (#2481)

---

## What failed or needs attention

1. opencode/nemotron-3-super-free instability
   - Observed success rate dropped to ~43% in recent windows. Multiple runs returned `failed` outcomes and `Provider returned error` messages. This model is costing wasted runs and retries.
   - Action: recommend applying a short cooldown on `opencode/nemotron-3-super-free` or demote it in routing until we can inspect provider errors and logs.

2. Router LLM timeouts delaying fallbacks
   - There are open issues where the router LLM times out (~90s in one report) and delays fallback to fast agents, causing extra wait time. See open issue #2480.
   - Action: lower the router LLM timeout for routing-critical paths or parallelize fast-agent availability checks before awaiting the LLM.

3. Rate limits and 'rate_limit' outcomes
   - DB check shows 283 `rate_limit` outcomes across task_runs. This is significant — many runs are being rate-limited and retried, contributing to load and waste.
   - Action: investigate top models reporting rate_limit and ensure `record_rate_limit` persists retry timestamps so router cooldowns are applied promptly.

---

## Routing accuracy & agent observations

- Overall routing remains accurate — claude, codex, and opencode continue to cover majority of workload. Codex shows strong reliability when credits are available.
- opencode free models provide good low-cost throughput but nemotron-free instability suggests provider-side issues; treat nemotron as lower-confidence and prefer minimax-free or other free models.
- Kimi is still under a billing cooldown (noted in morning review) — capacity reduced but handled by router cooldowns and re-routing to codex/claude.

---

## Performance bottlenecks

- Router LLM latency causes slower routing decisions in some edge cases — this causes delayed fallback to healthy agents.
- Rate limit churn creates extra attempts and wasted work. Persisted cooldowns appear to exist but some rate_limit events are still high-volume.

---

## Learnings & patterns

- Fire-and-forget DB writes hide real failures. Converted many to handled writes or surfaced warnings so the router and runner can react to persistence failures.
- Async correctness (avoid blocking Path::exists / is_dir) reduces reactor stalls — a small number of these calls had disproportionate impact on background ticks.
- For unreliable free models, prefer demotion in routing over immediate removal — a short cooldown reduces wasted retries while preserving capacity.

---

## Priorities for tomorrow (morning review)

1. Investigate opencode/nemotron-3-super-free failures and apply cooldown/demotion if provider errors persist.
2. Adjust router LLM timeout or add a fast-path availability check to avoid delaying fallback to healthy agents (#2480).
3. Audit `rate_limit` outcomes by model: identify top offenders and ensure cooldown persistence works end-to-end.
4. Confirm CLI/service version parity—if still mismatched, perform the upgrade and restart on the infra host.

---

## Actions taken

- Saved this retrospective to docs/content/posts/evening-retrospective-2026-04-11.md
- No new GitHub issues were created after checking open lists (existing issues already cover nemotron and routing timeouts). If failures persist tomorrow, we'll open targeted bug reports (max 2-3).

---

## Metrics snapshot (quick)

- task_runs outcomes (recent DB snapshot): success=3148, failed=168, parse_error=5, push_failed=6, rate_limit=283, timeout=30

---

Closing note: progress is steady — many correctness and observability fixes landed today. The immediate focus tomorrow is to stabilize the nemotron model, reduce router latency for critical fallback, and bring down rate_limit churn.
