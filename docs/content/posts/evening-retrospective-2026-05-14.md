+++
title = "Evening Retrospective — 2026-05-14"
date = 2026-05-14
description = "Daily evening retrospective and operational summary."
+++

# Evening Retrospective — 2026-05-14

## Summary

Today the service continued to operate with high throughput and low failure counts. Two small fixes merged that reduce false-positive classifications and improve stderr handling for runner exits. Work focused on diagnosing a recurrence of the closed-issue reconciliation timeout and confirming persistent cooldown behaviour for dead models.

Recent commits (local):

- b0f57701 — bug(router): detect_error_payload false positive — MCP tool names containing 'authentication' trigger spurious auth cooldown on router LLM
- e6102533 — fix(runner): consider stderr when detecting terminal_reason/completion (kimi/minimax)

## What Went Well

- Routing and task_runs remain healthy: the vast majority of agent/model runs succeeded in the last 24h.
- NDJSON/auth false-positive was fixed (#3129) which reduces spurious auth cooldowns for the router.
- Runner stderr handling improvement reduces misclassification of successful runs as failures (fewer false negatives).

## What Failed / Needs Attention

- Closed-issue reconciliation timeouts reappeared in logs (warnings during cleanup tick). Two open issues track this regression: #3116 and #3117. Root cause appears to be some reconciliation query paths still using the old 5s timeout constant instead of the updated RECONCILIATION_LIST_TIMEOUT.
- Several tasks remain blocked awaiting owner action:
  - #3110 (Claude 401 auth): needs orch.log excerpts showing `401` / `Invalid authentication` and task IDs for triage.
  - internal:149337 (SSH signing failure): owner must rotate/fix SSH agent or switch to HTTPS remote.

## Routing & Execution Quality

- task_runs snapshot shows mostly single-count failures; opencode/gpt-5.3 and a few opencode/claude-opus variants are the only recurring low-count failures. Persistent model cooldowns are in place (see #3121) but we should spot-check the `rate_limits`/KV entries to confirm long cooldowns are set for permanently-dead models.
- Router accuracy: LLM-based routing remains reliable. The recent detect_error_payload fix prevents false auth cooldowns caused by tool names; routing weight decay and cooldown system are behaving as intended.

## Performance & Bottlenecks

- The re-introduced cleanup timeouts add ~1–2s per tick and occasionally cause warning noise and slightly longer sync cycles. This is not critical but worth fixing to avoid mask-and-retry cycles in the long run.
- No widespread rate limits or org-level blocking observed today.

## Learnings

- Make sure all reconciliation listing call-sites use the single RECONCILIATION_LIST_TIMEOUT constant — partial updates lead to intermittent regressions.
- NDJSON envelopes in agent output continue to be a common source of misclassification; prefer explicit guards around `terminal_reason:completed` and sanitize session JSON tails when building error messages.

## Priorities For Tomorrow (Morning Review)

1. Investigate and fix the reconciliation timeout regression (#3116, #3117): audit `src/engine/cleanup.rs` and callers that list tasks to ensure the updated timeout constant is used everywhere.
2. Verify persistent cooldowns for dead models (#3121): confirm 7d (or configured) cooldown entries exist in `rate_limits`/KV and that router skips cooled models.
3. Request owners for blocked tasks (#3110, internal:149337) to provide logs or take the SSH/credentials action so tasks can proceed.

## Actions / Issues Created

No new issues created during this retrospective — current problems are already tracked (see #3116, #3117, #3110, internal:149337). If further root-cause work is needed after code inspection, file targeted issues limited to root causes (max 2).

---

Prepared by Orch automation (internal:149595).
