+++
title = "Evening Retrospective — 2026-05-10"
date = 2026-05-10
description = "Daily retrospective: #3087/#3088 in progress, kimi rate limits elevated, codex improving, multi-agent degradation event noted."
+++

# Evening Retrospective — 2026-05-10

## Summary

Today the two high-priority runner bugs (#3087 and #3088) were addressed and closed upstream; fixes for NDJSON success-envelope handling and auth-error extraction landed. Codex failure rates improved after the NDJSON-envelope fix. We observed a multi-agent degradation event during the afternoon sync where multiple agents briefly entered an `agent_error` cooldown and only minimax accepted dispatch. The event appears transient but merits monitoring.

## What Was Accomplished

- #3087 (kimi exit-1 false failures) fixed and closed: runner now checks NDJSON `terminal_reason:"completed"` before classifying an error, preventing false failures.
- #3088 (auth error garbling) fixed and closed: auth detection extracts the real error reason instead of returning NDJSON/session tail noise.
- NDJSON-envelope related fixes reduced codex failure noise; codex/gpt-5.3-codex failures dropped in the 7-day aggregate.
- Morning review (internal:149285) completed and sent to needs_review.

## What Failed / Still Pending

- Multi-agent degradation event: several agents (claude, opencode, kimi, glm, codex) briefly entered `agent_error` cooldowns during an afternoon sync. Cooldowns are expiring and services recovered; root cause is unclear (could be a short-lived infra/auth spike). This requires watching the next 24h for recurrence.
- Kimi rate limits persist at a concerning baseline. Although #3087 addressed false failures, separate rate_limit events still occur and should be monitored; consider extended cooldowns if rates remain elevated.
- opencode:gpt-5.3-codex / dead-model noise: model-level failures persist in the pool (see #3051). This is a configuration/cleanup item rather than a runtime bug; removing dead model IDs from opencode pool would stop repeated ModelUnavailable events.

## Execution Quality (task_runs — 7-day aggregate)

| Agent | Model | Outcome | Count |
|-------|-------|---------|-------|
| opencode | github-copilot/claude-sonnet-4.6 | success | 15 |
| claude | sonnet | success | 12 |
| opencode | github-copilot/gpt-5-mini | success | 8 |
| kimi | opus | success | 7 |
| codex | gpt-5.3-codex | success | 6 |
| glm | opus | success | 5 |
| minimax | opus | success | 5 |
| codex | gpt-5.3-codex | failed | 3 |
| kimi | opus | failed | 2 |
| kimi | opus | rate_limit | 2 |
| minimax | opus | failed | 2 |
| opencode | github-copilot/gpt-5-mini | failed | 2 |
| opencode | github-copilot/gpt-5.3 | failed | 2 |
| opencode | gpt-5.3-codex | failed | 2 |

**Notable improvements:**
- codex/gpt-5.3-codex: Down from 9 failures/day to 3 failures/7 days — the NDJSON envelope fix (`0c6a1f28`) is working
- opencode/success rates healthy overall

**Concerns:**
- kimi/opus rate_limits (2 in 7 days) — may be separate from the exit-1 issue
- opencode/gpt-5.3-codex failures persist (dead model in pool per #3051 note)

## Routing & Agents

 - Multi-agent degradation event during afternoon sync: claude, codex, opencode, kimi, glm all flagged `degraded` — only minimax accepted dispatch. `cooldown_reasons: agent_error` for all 5.
 - This pattern of simultaneous degradation across multiple agents suggests a short-lived systemic signal (network/auth or transient upstream issue). If this repeats, collect timestamps and kv cooldown keys for root-cause analysis.
 - Routing decisions remained stable; LLM budget and pre-emptive routability checks prevented watchdog stalls.

## Performance / Bottlenecks

- Sync tick elapsed: 2022ms (normal)
- No rate limit escalations beyond kimi baseline
- Service log clean

## Priorities for Tomorrow (Morning Review)

1. Confirm that the NDJSON/auth fixes eliminated false failures in recent task_runs (sample `task_runs.error` and outcomes).
2. Monitor cooldown expirations and run a 24h watch for the multi-agent degradation pattern; if it recurs, gather timestamps and `kv` cooldown keys and open a diagnosis issue.
3. Track kimi rate_limit frequency; if it remains elevated, consider increasing model/agent backoff or temporarily pruning high-failure models from the pool.
4. Propose removing dead opencode model identifiers (gpt-5.3-codex / github-copilot/claude-opus-4.6) from the opencode pool to avoid persistent ModelUnavailable events (file an issue if owner approval required).

---

Prepared by Orch automation (internal task internal:149298, attempt 3).
