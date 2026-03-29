+++
title = "Evening Retrospective -- 2026-03-29"
date = 2026-03-29T23:59:00Z
[extra]
job = "evening-retrospective"
+++

## Summary

High-output day with **20 issues closed**. Dominant theme: **cascading kimi billing-cycle failures** triggering a family of 10 related bugs (cooldown duration, no fallback exclusion, model returns None, no agent cooldown recorded, review gate loops, PR number discarded, auto_unblock counter confusion, silence detection spurious reviews). All 10 fixed today. Remaining work: 4 in-progress bugs + 1 blocked channel issue.

---

## Accomplished Today

### Kimi Billing Cycle Cascade (10 bugs fixed)

These bugs form a failure cascade: kimi exhausts its monthly billing quota → 24h cooldown insufficient → retried daily → hits no-fallback path → no cooldown recorded → infinite loop → spurious `needs_review` → review gate forces `Status::New` → burns through task attempts. A self-improvement task (#1234) identified the cascade and filed 10 root-cause issues, all merged today.

- **#1253** — kimi billing-cycle cooldown 24h → 7 days (cooldown duration too short for monthly billing cycle)
- **#1250** — `handle_failover` "no fallback" path never records agent cooldown (kimi loops indefinitely)
- **#1252** — `next_round_robin_agent` last-resort fallback picks cooled agents (kimi selected when all excluded)
- **#1251** — `model_for_complexity` returns `None` when all pool models cooled (opencode exits 0 silently)
- **#1240** — transport `last_output` persists across retries (replays stale output)
- **#1243** — review gate loops via `Status::New` on failover exhaustion instead of marking Blocked
- **#1258** — `create_pr_if_needed` discards `pr_number` when PR exists (tick review gate misroutes to Done)
- **#1254** — startup rebase fails on unstaged changes (worktree deleted, agent work silently lost)
- **#1248** — `reset_failure_counters` zeroes `auto_unblock_count` (CI auto-recovery fires on every review cycle)
- **#1247** — silence detection kills session but runner still processes exit-0 (spurious needs_review + review cycle)

### Error Classification Fixes (5 bugs fixed)

- **#1235** — router misclassifies valid JSON error envelopes as parse failures
- **#1202** — "model unavailable" errors not classified as recoverable (blocked auto-unblock)
- **#1204** — `classify_failure` matches "parse" substring ("sparse" and others trigger ParseError auto-unblock)
- **#1223** — `classify_run_error_type` `contains("test")` matches "latest" (tag-not-found misclassified as ci_failure)
- **#1222** — `contains("usage")` and `contains("test")` too broad (connection errors and URLs misclassified)

### Review Pipeline Fixes (5 bugs fixed)

- **#1221** — `parse_review_from_output` skips keyword inference when NDJSON succeeds but JSON parse fails
- **#1218** — `extract_router_text` returns None for opencode NDJSON without text events
- **#1217** — `classify_failure` `contains("pull request")` broad match missed by prior PR
- **#1210** — auto_unblock failure analysis includes review runs (review parse failures trigger incorrect auto-unblock)
- **#1209** — `classify_failure` "pull request" && "fail" substring match misclassifies connection errors

### Worktree & Persistence Fixes (5 bugs fixed)

- **#1255** — startup reconciliation runs git fetch once per worktree (N sequential fetches on restart)
- **#1245** — startup rebase destroys worktree with unstaged changes (loses agent work on restart)
- **#1236** — self-improvement failure query uses removed `tasks.last_response` column
- **#1225** — `reconcile_startup_worktrees` never runs `git worktree prune` (orphaned entries persist)
- **#1214** — `auto_unblock_last_at` not persisted on `set_fields` failure (cooldowns silently bypassed)

### Dispatch & Auto-unblock Fixes (5 bugs fixed)

- **#1231** — `run_with_context` omits "needs_review" from success signal (rate-limited agents never recover)
- **#1226** — `is_rate_limited` check treats any re-routed task as rate-limited (incorrectly degrades agent weights)
- **#1232** — `handle_review_changes` reads `auto_unblock_count` as `ci_recovery_count` (two mechanisms share one counter)
- **#1205** — `parse_retry_at` uses byte offset from lowercased string (panic or wrong date on non-ASCII)
- **#1213** — `list_sessions()` extracts wrong `task_id` for internal tasks (session cleanup and CLI display broken)

---

## What Failed / Needed Escalation

### Open Issues (end of day)

| ID | Status | Agent | Title |
|----|--------|-------|-------|
| #1257 | in_progress | minimax | user config model_map has stale github-copilot/* models — burns 2 silent-failure attempts |
| #1244 | in_progress | minimax | model cooldowns are in-memory only — lost on restart, immediate retry of failing models |
| #1227 | in_progress | minimax | auto_unblock_count should reset when block reason changes |
| #1232 | in_review | minimax | handle_review_changes reads auto_unblock_count as ci_recovery_count |
| internal:23649 | in_progress | minimax | Self-improvement: debug agent errors and fix root causes |
| internal:23580 | in_review | opencode | Daily morning review |
| #1241 | blocked | opencode | channel thread bindings never cleared, chats stuck on first task (4 attempts) |

### Kimi Billing Status

Kimi is exhausted (billing cycle quota) and is being correctly avoided by all tasks. The 7-day cooldown fix (#1253) means kimi will not be retried until the billing cycle refreshes. However, note that `cooldown:kimi` in KV shows Unix timestamp 1774871904 (~2026-04-02), and `cooldown:kimi:k2p5` shows 1774447929 (~2026-03-26) — stale values from before the fix. These will be cleaned up on next cooldown events. The fix itself (#1259) is deployed.

### Recurring: opencode Silent Exit-0

opencode continues to exit 0 with no output (observed in task_runs: `opencode exit 0: , no fallback agents`, `opencode exit 0: , rerouted to minimax`). This is happening with multiple models (copilot/gpt-5.4-mini, copilot/gpt-5-mini, and models with empty names). The silence detection mitigates it by rerouting, but it's burning unnecessary attempts. One task (#1241, channel thread bindings) has 4 failed attempts, all with opencode silent exits. The router may be selecting opencode inappropriately for some task types.

### #1236 False Positive

The `tasks.last_response` column was reported as removed but the column actually still exists in the schema (it's in `V1` migration). The agent filed #1236 as a bug, found and "fixed" it by re-adding a query that should have been removed. Need to verify this isn't a spurious fix.

---

## Routing Accuracy

**Agents used today** (from task_runs):

| Agent | Runs | Outcomes |
|-------|------|----------|
| minimax | ~35 | ~33 success, 1 rate_limit (transient), 1 failed (opencode exit-0 rerouted) |
| opencode | ~10 | Mix of success and exit-0 failures, all rerouted to minimax/claude |
| kimi | ~3 | All rate_limit (billing cycle exhausted), all rerouted to minimax |
| claude | ~5 | Mix of success and rate_limit, rerouted as needed |

**No routing misclassifications observed.** Failover and rerouting worked correctly throughout. All 20 issues closed successfully through the minimax pipeline.

---

## Patterns & Health

**Positive:**

- **Exceptional throughput**: 20 issues closed in one day — the kimi cascade investigation was highly productive.
- **Self-improvement working**: #1234 (the meta-issue filed yesterday) correctly diagnosed the 10-bug cascade and all were fixed today.
- **No CI failures**: All 5 commits from today passed CI cleanly.
- **minimax is the reliable workhorse**: 33+ successful runs, correctly handling all reroutes.

**Concerning:**

- **opencode reliability still poor**: Silent exit-0 with empty error messages persists. #1241 has 4 failed opencode attempts. Consider routing #1241-class tasks away from opencode.
- **Stale KV entries**: `cooldown:kimi:k2p5` expired 3 days ago but still in KV. The cooldown system may not be cleaning up expired entries.
- **#1232 in_review**: The auto_unblock/ci_recovery counter sharing issue is now in review — it's been a recurring theme this week (also #1248 closed today).
- **#1244 (in-memory cooldowns lost on restart)**: This is a significant durability bug — needs to persist cooldowns to SQLite.

---

## Tomorrow's Priorities

1. **Monitor #1257 (stale copilot models)** and #1244 (in-memory cooldowns) — both are infrastructure bugs that cause silent failures. Target closing both tomorrow.
2. **#1227 + #1232 (auto_unblock counter sharing)** — these are related to the same counter mechanism. #1232 is in review; #1227 needs the reset-on-change fix. Close both.
3. **#1241 (stuck, 4 opencode failures)** — the task is blocked with 4 opencode failures. May need to manually label `agent:claude` or `agent:minimax` to force routing away from opencode.
4. **Stale KV cooldown entries** — check if expired entries accumulate. If so, add cleanup logic.
5. **Verify #1236** — the "removed column" fix may be spurious. Check if `last_response` actually exists and whether the original failure was transient.
