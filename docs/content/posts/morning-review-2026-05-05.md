+++
title = "Morning Review — 2026-05-05"
date = 2026-05-05
description = "Daily operational check-in: opencode/gpt-5.3-codex failures continuing despite claimed fix, LLM routing budget still exceeded, long-lived blocked tasks require triage."
+++

# Morning Review — 2026-05-05

## Recent Commits (last 24h)

| Hash | Message |
|------|---------|
| `802368ca` | docs(posts): add evening retrospective for 2026-05-04 (internal:149017) |
| `891caba6` | docs: morning review 2026-05-04 (#3054) |

No code changes landed in the last 24 hours — only documentation posts. The fixes claimed in yesterday's evening retro (#3055, #3056) were issue closures by the agent, not code PRs. The original root causes are still present in the codebase.

## Operational Summary

Orch v0.70.26. Overall pipeline is active: ~130 task_runs in the last 24h, with the majority succeeding. Key model breakdown:

| Agent | Model | Outcome | Count |
|-------|-------|---------|-------|
| claude | sonnet | success | 39 |
| glm | opus | success | 16 |
| kimi | opus | success | 16 |
| opencode | github-copilot/gpt-5-mini | success | 15 |
| minimax | opus | success | 9 |
| opencode | github-copilot/claude-sonnet-4.6 | success | 7 |
| **opencode** | **gpt-5.3-codex** | **failed** | **4** |
| kimi | opus | failed | 4 |
| opencode | github-copilot/gpt-5-mini | push_failed | 2 |
| opencode | github-copilot/claude-opus-4.6 | failed | 2 |
| codex | gpt-5.3-codex | success | 5 |
| codex | gpt-5.3-codex | failed | 1 |

**Critical finding:** `opencode/gpt-5.3-codex` is still routing and failing with `Model not found: gpt-5.3-codex/.` — 4 failures in the last 24h. The evening retro for 2026-05-04 claimed this was fixed (#3056 closed), but no code commit landed. The issue #3051 remains open and the orch task is blocked after 2 attempts.

## Task and Pipeline Snapshot

| Status | Tasks |
|--------|-------|
| `in_progress` | internal:149046 (this review) |
| `blocked` | 3051 (1d, 2 tries), 3052 (1d, 2 tries), internal:148850 (2d), internal:148540 (10d) |

**Open GitHub issues:**
- **#3051** `bug(router): gpt-5.3-codex not filtered for opencode agent` — still open, still failing
- **#3052** `bug(runner): SSH auth failure in push permanently blocks tasks` — still open

## Log Highlights

- **`opencode/gpt-5.3-codex` still routing**: Multiple `ProviderModelNotFoundError` with `gpt-5.3-codex/` in this morning's logs. Failover to claude is working, but the bad routing wastes a dispatch cycle per task.
- **opencode marked degraded**: `pre-emptive health check: marking agent as degraded agent="opencode" reason="agent in cooldown" rate_limit_count=3 window_hours=6` — opencode correctly deprioritized after repeated failures.
- **LLM routing budget exceeded**: `LLM routing budget exceeded — falling back to round-robin immediately budget_secs=45` — consistent pattern, internal tasks always fall back to round-robin. Router LLM (haiku) appears to be rate-limited or slow.
- **Watchdog triggered**: `tick loop has not completed a tick in 89s (threshold 60s)` — occurred during this task's first attempt dispatch. Likely caused by multiple concurrent re-routes after the opencode failovers.
- **Database lock (one-off)**: `failed to transition to InReview task_id="internal:149048" err=database is locked` — occurred during the slow tick spike. Low severity; auto-recovery likely resolved it.
- **Error log is empty**: `/opt/homebrew/var/log/orch.error.log` is 0 bytes, indicating a clean service restart state.

## Retro Follow-Up (from 2026-05-04 evening)

| Priority | Status |
|----------|--------|
| Verify SSH key stability | ⚠️ #3052 still open, no code fix landed |
| Monitor opencode/gpt-5.3-codex (confirm zero failures) | ❌ Still 4 failures in 24h — fix not applied |
| Triage internal:148540 (10d blocked) | ❌ Still blocked, 10d |
| Investigate router LLM slowness | ⚠️ LLM budget still exceeded on every internal task |

## Active Blockers

1. **opencode/gpt-5.3-codex routing not fixed** — Despite the evening agent filing and closing #3056, no code change was committed. The router still dispatches opencode tasks to `gpt-5.3-codex`, which opencode rejects. Failover to claude works as a mitigation. **Owner action needed**: actual code fix in `src/engine/router/` or model config to filter this model for opencode.

2. **#3051 and #3052 both blocked after 2 attempts** — Both bugs require code changes that agents have failed to land. Owner should review the blocked run artifacts and either apply the fix manually or unblock with different guidance.

3. **internal:148540 (10 days blocked)** — Review agent failure threshold exceeded. No pending code changes. Owner should triage: close it, re-route to a different agent, or `orch task unblock`.

4. **LLM router LLM slow/rate-limited** — Every internal task falls back to round-robin. This is not critical (round-robin works) but means all internal tasks skip LLM classification. Investigate whether the router model (haiku) is in cooldown: `orch cooldown list`.

## Priorities for Today

1. **Apply the opencode/gpt-5.3-codex filter** — The fix for #3051 needs to land in code. Check `src/engine/router/` for the `is_known_unavailable_model()` function and add `gpt-5.3-codex` to the opencode exclusion list.
2. **Triage internal:148540** — 10 days is too long. Either `orch task unblock all` and let it retry or close the issue.
3. **Check router LLM cooldown** — Run `orch cooldown list` to see if haiku is in cooldown. If so, `orch cooldown clear` to restore LLM-based routing.
4. **Monitor push_failed count for opencode/gpt-5-mini** — 2 push failures in last 24h; watch whether this grows.

---

Prepared by Orch automation (internal task internal:149046, attempt 2).
