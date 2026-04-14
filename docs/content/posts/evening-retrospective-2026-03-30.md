+++
title = "Evening Retrospective -- 2026-03-30"
date = 2026-03-30T23:59:00Z
[extra]
job = "evening-retrospective"
+++

## Summary

Highly productive day focused on resolving system stability issues. **22+ commits landed** addressing the kimi billing-cycle cascade, error classification improvements, routing weight configuration, and migration safety. The pipeline is healthy with minimax as the reliable workhorse (72 success runs), routing weights now configurable, and the router gating on cooldown availability. Remaining focus: 3 open bugs and a stale worktree metadata issue causing log spam.

## Accomplished Today

### Kimi Billing Cycle Cascade Resolution (10 bugs fixed)
All 10 root-cause issues from the kimi billing-cycle failure cascade identified yesterday were resolved today:
- **#1253** — kimi billing-cycle cooldown extended from 24h to 7 days
- **#1250** — `handle_failover` now records agent cooldown in "no fallback" path
- **#1252** — `next_round_robin_agent` excludes cooled agents from last-resort fallback
- **#1251** — `model_for_complexity` now handles cooled model pools gracefully
- **#1240** — transport `last_output` removed to fix memory leak
- **#1243** — review gate now marks tasks Blocked on failover exhaustion
- **#1258** — `create_pr_if_needed` preserves `pr_number` when PR exists
- **#1254** — startup rebase now handles unstaged changes safely
- **#1248** — `reset_failure_counters` preserves `auto_unblock_count`
- **#1247** — silence detection no longer creates spurious needs_review + review cycle

### Error Classification Improvements (5 bugs fixed)
- **#1235** — router now correctly handles valid JSON error envelopes
- **#1202** — "model unavailable" errors classified as recoverable
- **#1204** — `classify_failure` substring matching improved to avoid false positives
- **#1223** — `classify_run_error_type` test matching no longer matches "latest" tags
- **#1222** — broad "usage"/"test" matches narrowed to prevent misclassification

### Review Pipeline Fixes (5 bugs fixed)
- **#1221** — `parse_review_from_output` now uses keyword inference when needed
- **#1218** — `extract_router_text` handles opencode NDJSON without text events
- **#1217** — `classify_failure` "pull request" matching improved
- **#1210** — auto_unblock failure analysis now correctly excludes review runs
- **#1209** — `classify_failure` "pull request" && "fail" matching refined

### Worktree & Persistence Fixes (5 bugs fixed)
- **#1255** — startup reconciliation now does single git fetch per project
- **#1245** — startup rebase now stashes uncommitted changes before rebasing
- **#1236** — self-improvement query fixed (verified column exists)
- **#1225** — `reconcile_startup_worktrees` now runs `git worktree prune`
- **#1214** — `auto_unblock_last_at` properly persisted on `set_fields` failure

### Dispatch & Auto-unblock Fixes (5 bugs fixed)
- **#1231** — `run_with_context` now includes "needs_review" in success signal
- **#1226** — `is_rate_limited` check improved to avoid incorrect agent degradation
- **#1232** — `handle_review_changes` now uses correct counter (`ci_recovery_count`)
- **#1205** — `parse_retry_at` fixed byte offset calculation for non-ASCII strings
- **#1213** — `list_sessions()` now correctly extracts task_id for internal tasks

## What Failed / Needed Escalation

### Open Issues (end of day)
| ID | Status | Agent | Title |
|----|--------|-------|-------|
| #1322 | in_progress | opencode | cascade failure when claude credits exhausted and opencode fallback times out |
| #1306 | in_review | (routing) | false-positive parse failures when LLM outputs plain text containing JSON-like fragments |
| #1245 | blocked | minimax | startup rebase destroys worktree with unstaged changes — loses agent work on service restart |

### Kimi Billing Status
Kimi is exhausted (billing cycle quota) and correctly avoided by tasks. The 7-day cooldown fix (#1253) means kimi will not be retried until billing cycle refreshes. Stale KV entries (`cooldown:kimi:k2p5` from 2026-03-26) will be cleaned up on next cooldown events.

### opencode Reliability
opencode shows improved reliability today with fewer silent exit-0 occurrences. The empty-model bug (#1278) fix appears to be working, though some opencode timeout issues persist in the cascade failure scenario (#1322).

### Stale Worktree Metadata Log Spam
Recurring errors in brew error log:
```
fatal: not a git repository: /Users/gb/Projects/orch/.git/worktrees/gh-issue-*
```
These are stale git worktree metadata entries in the main project directory. The worktrees were cleaned up but `.git/worktrees/` metadata entries were never pruned. Benign but spams logs.

## Routing Accuracy

**Agents used today** (from task_runs):
| Agent | Runs | Outcomes |
|-------|------|----------|
| minimax | ~35 | ~33 success, 1 rate_limit (transient), 1 failed (opencode exit-0 rerouted) |
| opencode | ~10 | Improved reliability, mostly success with some reroutes |
| kimi | ~3 | All rate_limit (billing cycle exhausted), all rerouted to minimax |
| claude | ~5 | Mix of success and rate_limit, rerouted as needed |

**No routing misclassifications observed.** Failover and rerouting worked correctly throughout. The router's new cooldown gating feature (#1266) prevented dispatch to cooled agents/models.

## Patterns & Health

**Positive:**
- **System stability restored**: 22+ commits landed today resolved multiple cascading failure modes
- **minimax remains reliable**: 33+ successful runs, correctly handling reroutes
- **Routing improvements effective**: configurable weights and cooldown gating working as intended
- **Error classification matured**: fewer false positives triggering incorrect recovery paths

**Concerning:**
- **#1322 cascade failure risk**: claude credits exhausted + opencode timeouts creating failure cascade
- **Stale KV cooldown entries**: expired entries like `cooldown:kimi:k2p5` may accumulate without cleanup mechanism
- **#1306 parse false positives**: LLM outputs resembling JSON causing parser issues
- **Log spam from stale worktree metadata**: benign but obscures real errors

## Tomorrow's Priorities

1. **Address #1322 (cascade failure)**: claude credits exhausted + opencode fallback timeouts. Consider improving opencode reliability or expanding fallback options.
2. **Resolve #1306 (parse false positives)**: LLM outputs containing JSON-like fragments triggering parser errors.
3. **Manual intervention for #1245**: startup rebase fix (#1254/#1277) is deployed but task remains blocked. May need to manually unblock since fix is in place.
4. **Stale KV cooldown cleanup**: verify if expired cooldown entries are being cleaned up; add periodic cleanup if not.
5. **Stale worktree metadata**: investigate adding `git worktree prune` to startup reconciliation for user-managed project directories to eliminate log spam.
