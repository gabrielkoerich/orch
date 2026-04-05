+++
title = "Evening Retrospective — 2026-04-05"
date = 2026-04-05
description = "High-velocity day: 30 commits, 174/188 runs successful (92.6%) — error-visibility sweep and auto_merge reliability dominated"
+++

# Evening Retrospective — 2026-04-05

## Summary

Another high-output day. **30 commits merged**, **174 successful runs out of 188** (92.6% success rate). The dominant theme was a systematic sweep of silent error swallowing — DB failures, append_activity calls, route result failures, and auto_merge blocking paths all gained proper logging or propagation. Secondary theme was auto_merge reliability: block reason persistence, git fetch/rebase misclassification, and stale model clearing.

Queue is clear at end of day. No open issues.

---

## Morning Priorities — Outcome

| Priority from morning review | Status |
|------------------------------|--------|
| Verify dispatch key leak fix stable | ✓ Confirmed — no unexpected needs_review accumulation observed |
| kv_increment silence fix follow-up | ✓ Cooldowns now escalating correctly — 3 kimi rate limit failures each received proper exponential backoff |
| Async blocking audit | Addressed indirectly via new issues; no dedicated `rg` pass landed as a commit |
| kimi recovery window | ✗ kimi billing cycle still exhausted — 4 failed attempts logged today (2:31, 12:22, 16:43, 21:05 UTC). Expected auto-recovery has not occurred. |
| Cron timing correctness | No issues reported post-fix — presumed stable |

---

## What Was Accomplished

### Error visibility sweep (largest cluster)

The self-improvement agent identified a broad pattern of swallowed errors and generated fixes across multiple subsystems:

- `store write helpers silently swallow DB errors` (#1919) — `Err` from `resolve_task_id` was never logged; callers received silent no-ops
- `assemble_context swallows DB failures` (#1913) — AI context assembly was silently returning partial/empty context on DB failure
- `get_route_result clears stale model with let _ ` (#1912) — DB failure during stale-model clearing was invisible
- `router: log DB failures when clearing stale model` (`6d3b01cd`) — additional logging at the router level
- `auto_unblock silently drops append_activity failure` (#1924) — activity logging in the unblock path was silently discarded
- `auto_unblock_blocked_tasks silently skips all work when list_by_status fails` (#1923) — entire unblock scan was aborting silently
- `log warning when list_by_status fails in auto_unblock_blocked_tasks` (`019f4386`) — defensive warning added
- `log warning when auto_unblock append_activity fails` (#1928) — parallel fix for the unblock path
- `log errors in ingest_external_tasks list_routable()` (`94ffac6d`) — ingestion errors now surfaced
- `add warning log to store_reset_counters on failure` (#1894) — counter reset failures now visible
- `log sentinel creation failures in scan_mentions command-execute path` (#1925) — sentinel drops now logged

### auto_merge reliability fixes

- `auto_merge blocks task with wrong reason when force-push fails` (#1921) — `block_reason` was not being set on the force-push failure path, leaving `CHANGES_REQUESTED` as the visible reason; misleading for operators
- `persist auto-merge block reasons` (#1938) — comprehensive fix ensuring all blocking paths on auto_merge correctly set `block_reason`
- `distinguish git fetch failures from rebase conflicts in auto_merge` (#1911) — git fetch errors were being misclassified as rebase conflicts, causing tasks to be immediately blocked rather than retried
- `bug: rebase conflict on merge → task immediately blocked` (#1908) — rebase conflicts were blocking rather than triggering a retry cycle

### Routing and model correctness

- `clear stale model from db in get_route_result` (#1907 / `05e2e0f7`) — stale model wasn't being cleared from the DB when discarded during routing
- `router LLM auth error logged with trailing raw NDJSON Claude stats blob` (#1936) — auth error logs were unreadable due to unparsed NDJSON appended to the message
- `stop parsing opencode "Did you mean" suggestion as model name` (#1934) — opencode's CLI suggestion string was being stored as the model identifier

### Agent prompt / behavior

- `agent prompt allows plan-only 'done' status for code-change tasks` (#1930) — agents were allowed to declare tasks done without code changes; prompt tightened
- `prevent agents from falsely closing issues without code changes` (`0d923959`) — complementary guard in engine
- `PendingPick entries never pruned from memory — unbounded HashMap growth` (#1894 / `39bcf57d`) — memory leak in the pending pick tracking structure

### Performance

- `cache authenticated username in GhHttp` (`229fb895`) — repeated `GET /user` calls on every authenticated request eliminated
- `list_internal_by_source fetches 57-column Task rows for mention filtering` (`160ebfce`) — overfetching reduced for the hot scan_mentions path

### Infrastructure

- `Telegram messages should not be truncated` (`5699a285`) — long messages now split and sent in full
- `orch task retry fails with 404 when no project context` (#1904) — retry CLI command now resolves project from task context
- `control session timeout increased to 30 minutes` (#1899) — previous 30-minute timeout was too short for complex control sessions; now 1800s
- `reset control session on timeout errors` (#1903) — timeout errors now clear the stored session UUID, forcing a fresh start
- `cleanup.rs logs branch_delete activity before mark_cleaned` (`6c93a417`) — activity ordering fixed (was logging completion before the DB write)
- `refactor: deduplicate store-first task listing across sync, cleanup, and review_poll` (#1929) — three copies of the same store-first pattern unified
- `non-rate-limited reroutes return WeightSignal::None — triggers unwarranted weight decay` (`2fb8b3f6`) — reroutes due to non-rate-limit reasons were incorrectly penalizing agent weights

---

## What Failed and Why

### Run-level failures (14 total)

| Root cause | Count | Details |
|------------|-------|---------|
| kimi billing cycle exhausted | 4 | Tasks 50300, 53412, 55239, 56720 — same billing cycle limit as yesterday |
| Silence detection | 2 | Task 54734 (opencode/copilot-sonnet-4.6), task 56488 (claude/sonnet) — agents started but produced no parseable output |
| Parse error in review response | 2 | Tasks 51548 (codex), 52047 (opencode/nemotron) — review agents returned non-JSON output |
| Claude timeout | 1 | Task 53852 — timed out after 1801s, re-routed |
| This task (attempt 1) | 1 | opencode/qwen3.6-plus-free rate limited by Alibaba upstream |
| Blank outcome (in-flight) | 4 | Active runs, not failures |

**kimi billing cycle:** Four failures across the day confirm the billing cycle has not reset. The generic backoff system is correctly sidetracking kimi — each failure was followed by successful routing to other agents. No intervention needed unless the billing cycle doesn't reset within the expected window.

**Silence detection (2 cases):** Both were correctly handled — silence detected, task reset to `new`, re-routed to a different agent, and completed successfully. The opencode/copilot-sonnet silence suggests that model combination may have intermittent silent failure. Worth watching.

**Review parse errors (2 cases):** codex and opencode/nemotron both returned non-JSON review responses on these tasks. These are one-off parse failures; tasks were re-reviewed successfully. The `router LLM auth error logged with trailing raw NDJSON` fix (#1936) may reduce some of these by improving error message clarity.

---

## Routing Accuracy

Routing was healthy and well-diversified:

| Agent | Model | Successes |
|-------|-------|-----------|
| claude | sonnet | 46 |
| minimax | opus | 38 |
| claude | haiku | 19 |
| codex | gpt-5.3-codex | 19 |
| opencode | nemotron-3-super-free | 9 |
| opencode | qwen3.6-plus-free | 9 |
| opencode | gpt-5-mini | 7 |
| opencode | minimax-m2.5-free | 7 |
| claude | opus | 6 |
| opencode | gpt-5.4 | 6 |

kimi correctly absent — 4 failures, all handled with escalating backoff. Router continues to spread load effectively across claude, minimax, codex, and opencode variants. No single model dominates to an unhealthy degree.

The weight decay fix (`2fb8b3f6`) should improve routing accuracy going forward — previously, non-rate-limit reroutes were incorrectly penalizing working agents.

---

## System Health

- **Queue:** 0 open issues. Backlog clear.
- **Active tasks:** 2 internal tasks in_progress (this retrospective + weekly review), 1 in_review.
- **Blocked tasks:** Only unrelated Solana/oblivion project tasks remain blocked (#161, #164, #165, #175, #205).
- **Error log:** `orch.error.log` not present (no service crashes since last restart).
- **kimi:** Billing cycle still not reset after >24h. Auto-recovery expected — no manual action unless cycle doesn't clear by morning.
- **Memory:** PendingPick HashMap unbounded growth fixed (#1894) — long-running services no longer leak memory through this path.

---

## Priorities for Tomorrow

1. **Monitor kimi recovery** — Billing cycle should reset. If kimi is still cooled at tomorrow's morning review, check `orch cooldown list` for the precise reset timestamp and whether the billing window has actually passed.

2. **opencode/copilot-sonnet silence watch** — Task 54734 silently failed. If this combination silences again in the next 24h, consider adding it to the monitoring watchlist or adjusting its routing weight threshold.

3. **Review parse failure pattern** — Two review parse failures in one day (codex, opencode/nemotron). Check if the NDJSON logging fix (#1936) reduces these; if parse errors continue, the review response parser may need more lenient fallback handling.

4. **Async blocking audit** — The morning review flagged this as a priority and it was not addressed directly today. A targeted `rg 'std::fs::' src/` pass across async functions is warranted.

5. **`verify_summary_matches_diff` stability** — Three-pass fix landed yesterday (#1836). Today saw no pre-dispatch validation failures, suggesting stability. Continue monitoring for one more cycle before declaring it resolved.
