+++
title = "Morning Review — 2026-05-15"
date = 2026-05-15
description = "Daily operational morning review."
+++

# Morning Review — 2026-05-15

## Recent Commits (last 24h)

| Hash | Message |
|------|---------|
| `b0f57701` | bug(router): detect_error_payload false positive — MCP tool names containing 'authentication' trigger spurious auth cooldown (#3131) |
| `e6102533` | fix(runner): consider stderr when detecting terminal_reason/completion (kimi/minimax) (#3130) |
| `6e2d44b0` | Daily morning review (#3127) |

Notable: Two fixes landed overnight. #3130 improves terminal_reason detection by checking stderr (kimi/minimax agents sometimes emit NDJSON telemetry to stderr rather than stdout). #3131 addresses a false-positive auth cooldown where MCP tool names containing the word `authentication` were incorrectly triggering the auth-error detection pathway in the router.

## Operational Summary

Service is running on v0.71.8. The dominant recurring issue is the closed-issue reconciliation timeout (#3116, #3117) — `timed out listing all tasks for closed-issue reconciliation (timeout_secs=30)` fires every sync tick. Despite the 30s timeout from #3112, the first query path still times out and falls back to 221 cached tasks. This adds ~30s of async wait per sync cycle, which combined with fallback processing adds ~1.5s to sync tick elapsed time. Not critical but chronic.

A slow tick warning also fired this morning: `slow tick elapsed_ms=46710` (~46s). This coincided with two tasks being dispatched simultaneously (internal:149614 and internal:149615), which is expected load — not a pathological stall.

## Health Checks

### Reconciliation Timeout — Persistent

Every sync tick (30s interval):
```
WARN orch::engine::cleanup: timed out listing all tasks for closed-issue reconciliation timeout_secs=30
INFO orch::engine::cleanup: using fallback tasks for closed-issue reconciliation count=221
```

The fallback path works correctly (221 tasks found), but the primary list query always times out. This has been ongoing since before #3112. The fix must target the query itself (e.g., missing index, full scan on a large table, or a query that is simply too slow for the GitHub API path if it's remote). Issues #3116 and #3117 track this.

### Stuck / Blocked Tasks

| ID | Status | Agent | Blocked On |
|----|--------|-------|------------|
| internal:149595 | blocked | opencode | CI failure limit reached during auto-merge (PR #3132 open) |
| internal:149337 | blocked | minimax | SSH agent signing failure (git push) — owner action required |
| #3110 | blocked | opencode | Claude 401 auth — owner has not provided log context |
| #3117 | open | — | Reconciliation `all tasks` timeout regression |
| #3116 | open | — | Reconciliation `all tasks` timeout regression (duplicate) |

internal:149595 (Evening retrospective) has an open PR #3132 that failed CI auto-merge after hitting the retry limit. This is worth checking — the PR may need a human push or rebase.

### task_runs Summary (last 24h)

```
opencode/gpt-5-mini        success  16
kimi/opus                  success   8   (3 failed)
claude/sonnet              success   7
minimax/opus               success   7
opencode/claude-sonnet-4.6 success   7
codex/gpt-5.3-codex        success   6
glm/opus                   success   5
opencode/gpt-5.4           success   5   (2 blank outcome)
opencode/gpt-5.3           failed    1
```

kimi/opus had 3 failures in 24h — elevated vs. yesterday (1 failed). Monitor for cooldown activation. opencode/gpt-5.3 had 1 failure — persistent cooldown from #3121 should handle it.

### New Fixes (since yesterday)

- **#3130** `fix(runner): consider stderr when detecting terminal_reason/completion` — fixes kimi/minimax cases where NDJSON telemetry ends up in stderr. This should reduce blank-outcome records for those agents.
- **#3131** `bug(router): detect_error_payload false positive` — MCP tool names with `authentication` in them were triggering auth cooldowns on the router LLM. This was causing spurious cooldowns on healthy agents. Fix is deployed.

## Retro Follow-ups

- **#3116 / #3117 reconciliation timeout**: still unresolved. Priority 1 today.
- **internal:149595 (evening retro PR #3132)**: CI-blocked. May need human push or rebase.
- **#3110 Claude 401**: still awaiting owner log context.
- **internal:149337 SSH**: still awaiting owner action.
- **kimi/opus failures (3 in 24h)**: monitor — if it persists, check whether cooldown is activated.

## Priorities for Today

1. **Fix reconciliation timeout** (#3116, #3117): the primary `list_all_tasks` query times out every tick even with 30s budget. Likely a slow GitHub API call or full table scan. Investigate whether the query is going remote or hitting SQLite — if remote, consider caching or reducing call frequency. If SQLite, check for missing indexes on the tasks table.
2. **Check PR #3132 (evening retro)**: blocked by CI failures. Human may need to review/rebase.
3. **Monitor kimi/opus failures**: 3 in 24h is higher than baseline. If failures continue, check if rate limits or auth issues are the root cause (`orch log 200 | rg kimi`).
4. **Verify #3131 fix scope**: ensure `detect_error_payload` false-positive fix covers all cases where MCP tool names could embed keywords that trigger auth/rate-limit detection.
5. **Owner actions**: #3110 (provide orch.log `401` lines), internal:149337 (fix SSH agent or HTTPS remote).

---

Prepared by Orch automation (internal:149614).
