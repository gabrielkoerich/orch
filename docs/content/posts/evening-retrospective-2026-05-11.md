+++
title = "Evening Retrospective — 2026-05-11"
date = 2026-05-11
description = "Cleanup reconciliation timeout fixed; Claude 401 auth issue open; routing stable otherwise."
+++

# Evening Retrospective — 2026-05-11

## What Was Done Today

| Hash | Message |
|------|---------|
| `5d18541e` | fix(cleanup): increase reconciliation timeout from 5s to 30s and deduplicate fallback logic (#3112) |

One meaningful fix landed today. The closed-issue reconciliation timeout that was generating repeated `timed out listing fallback tasks` WARNs every few seconds has been resolved — timeout increased from 5s to 30s with deduplication of fallback logic. This was flagged as a priority in the morning review and was addressed same day.

### Issues Closed Today

| # | Title | Agent |
|---|-------|-------|
| #3111 | engine: closed-issue reconciliation listing times out | opencode/claude-sonnet-4.6 |
| #3109 | runner: 'Model not found' for opencode model github-copilot/gpt-5.3 — failures not recorded as persistent model failure | opencode/gpt-5-mini |

Both bugs from the prior day's batch were closed. Issue #3109 ensures that `ModelUnavailable` errors from opencode's copilot model are correctly recorded via `record_persistent_model_failure`, preventing that model from being retried unnecessarily.

## What Failed / Needs Attention

### Open: Claude 401 Auth Issue (#3110)

Issue #3110 (`auth: Claude 401 'Invalid authentication credentials'`) remains open and blocked. The agent dispatched to it (opencode/gpt-5-mini) responded asking for more context rather than investigating from available artifacts. The issue was filed with minimal reproduction info; the assigned agent stalled. This is a real operational concern — 401 errors during Claude routing would cascade into route failures. Owner should:

1. Pull relevant log lines from `~/.orch/state/orch.log` (grep for `401` or `Invalid authentication`)
2. Identify which task IDs triggered the failure
3. Add that context to issue #3110

### Cleanup Reconciliation — Fixed but Verify

The 5s→30s timeout fix addresses the symptom. It is worth verifying tomorrow that the `timed out listing fallback tasks` WARNs are gone from the log. If they persist even with 30s, the query itself may need indexing.

## Routing Accuracy

From morning review's task_runs snapshot (last 24h):
- minimax/opus: 40 successes
- codex/gpt-5.3-codex: 16 successes, 2 failures
- kimi/opus: 12 successes, 3 failures, 2 rate_limits
- opencode/gpt-5-mini: 11 successes
- claude/sonnet: 3 failures (auth-related 401s — matches #3110)

Routing remains largely accurate. The claude/sonnet failures are auth-related, not routing decisions. The kimi rate_limit events are expected and handled by cooldown backoff.

## Priorities for Tomorrow

1. **Verify #3111 fix**: confirm `timed out listing fallback tasks` WARNs are absent from logs after service restart with new binary.
2. **Unblock #3110**: owner needs to add log/task-ID context to the Claude 401 issue so an agent can investigate. Without that, the issue will sit blocked indefinitely.
3. **Monitor claude/sonnet auth**: if 401 failures recur, check token expiry in `~/.orch/config.yml` or rotate the GH_TOKEN/ANTHROPIC_API_KEY in use.
4. **Watch #3109 fix**: confirm opencode/gpt-5.3 model failures are now correctly hitting the persistent model cooldown path and not being retried.

---

Prepared by Orch automation (internal task internal:149445).
