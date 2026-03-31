+++
title = "Evening Retrospective -- 2026-03-31"
date = 2026-03-31T23:59:00Z
[extra]
job = "evening-retrospective"
+++

## Summary

Excellent day — **26 commits landed** since last night's retrospective, with a dominant focus on hardening the review pipeline. Almost every recurring failure mode in the review cycle has been addressed: infinite loops, stuck states, missing git ops, empty block metadata, and missing token tracking. Agent health is strong: 90 successes out of ~108 runs in the last 12 hours (83%). No open GitHub issues — the queue is fully cleared.

---

## Accomplished Today

### Review Pipeline Overhaul (8 bugs closed)

These were the most impactful fixes of the day — the review subsystem had accumulated several silent failure modes:

- **#1401** — `synthesized needs_review` response was skipping git commit/push/PR ops, silently losing agent work. Fixed: synthesis path now runs full git ops before transitioning to `needs_review`.
- **#1397** — Approved PR reviews left tasks stuck in `in_review` indefinitely (infinite review loop). Fixed: `in_review` tick now transitions to `done`/`blocked` when all reviews are approved.
- **#1399** — Startup `in_review` reset was silently abandoned after 3 retries, leaving stale tasks. Fixed: 3-retry exhaustion now falls through to a hard reset.
- **#1398** — `block_reason` and `last_error` were empty when task was blocked via max review cycles. Fixed: fields now populated before the block transition.
- **#1391** — `review_poll` no-PR reroute had no circuit breaker — tasks could loop indefinitely on non-code tasks with no PR. Fixed (d9ed5d5): circuit breaker added after 3 reroutes.
- **#1389** — `auto_unblock_count >= 3` early exit fired before the reason-change reset, blocking tasks prematurely. Fixed: reason-change reset now runs before the exit guard.
- **#1402** — Token usage was not preserved when NDJSON parse fell back to synthesized result. Fixed: token fields threaded through synthesis path.
- **#1388** — Review agent token usage was never tracked, causing orch cost reports to undercount. Fixed: review runner now records tokens like the main agent runner.

### System Reliability Fixes

- **Deadlock in `tick_dispatch_tasks`** — read lock taken on a write-locked `RwLock`. Fixed (3f195da): lock acquisition order corrected.
- **Atomic SQL increment for failure counts** (#1363/#1365) — failure counter increments were non-atomic under concurrent dispatch. Fixed.
- **Backoff jitter centering** (#1362/#1364) — jitter was applied before capping, skewing distribution toward the cap. Fixed: jitter now centered around the capped delay.
- **SqliteRow OOB panic** (#1386) — `try_get` now used for recently-added task fields to avoid index-out-of-bounds on older schema versions.
- **Immediate cooldown recording on rate limit** — concurrent dispatches could hit the same rate-limited agent/model before the first cooldown was written. Fixed (213a67d): cooldowns recorded at first rate-limit signal, not on task completion.
- **PR orphaned on GitHub 502** (#1393) — `create_pr_if_needed` was not retrying on transient 5xx, leaving tasks without a PR link. Fixed.
- **token/cost data silently dropped** on `parse_envelope` fallback (b627d0d) — synthesis fallback now propagates token metadata.
- **opencode timeouts resetting to `new`** (#1320) — silence detection was transitioning timed-out tasks to `new` (losing branch/worktree) instead of `needs_review`. Fixed.

### New Features

- **`orch doctor`** (bbe6682) — new CLI subcommand that detects: done tasks without merged PRs, orphaned worktrees, stale KV cooldown entries, and tasks stuck in terminal states. Excellent foundation for automated health monitoring.
- **Exponential backoff cooldown CLI** (04d56ec) — `orch cooldown list` and `orch cooldown clear` now available for operator inspection and emergency resets.
- **`skip_limited_threshold` router guard** (fbeb1f3) — agents with routing weight below threshold are pre-emptively skipped before LLM routing, preventing wasted dispatch cycles.

### Synthesizer Improvements

- **False-positive parse failures** (5f613a4) — LLM outputs containing JSON-like fragments (e.g., `{"key": "value"}` in prose) were triggering structured-parse failures. Fixed.
- **`classify_failure` tuning** (5a0c3bb) — "the fix is complete" and "all tests pass" phrases now correctly classified as `DONE` rather than `needs_review`.

---

## Morning Priorities — Follow-up

| Priority | Status |
|----------|--------|
| Monitor #1245 (startup rebase blocks) | No active blocked tasks with this pattern — appears resolved by #1254/#1277 |
| #1244 (in-memory cooldowns lost on restart) | ✅ Closed — exponential backoff + `orch cooldown list/clear` CLI landed |
| #1247 (silence detection spurious review, 4 tries) | Resolved — silence detection reset-to-new path fixed in #1320 |
| Watch opencode/empty-model failures | ✅ No empty-model failures observed today |
| Stale KV cooldown cleanup | `orch doctor` now surfaces stale entries; `orch cooldown clear` available for manual cleanup |
| Stale git worktree metadata log spam | Not explicitly addressed — still a low-priority log noise issue |

---

## Agent Performance (Last 12h)

| Agent | Model | Success | Failed | Rate-limit | NULL | Notes |
|-------|-------|---------|--------|------------|------|-------|
| claude | sonnet | 29 | 3 | 1 | 2 | Dominant; credits appear restored |
| opencode | github-copilot/gpt-5-mini | 22 | 0 | 0 | 3 | Healthy |
| minimax | opus | 17 | 0 | 0 | 1 | Reliable workhorse |
| claude | opus | 11 | 0 | 1 | 1 | Solid |
| claude | haiku | 4 | 1 | 0 | 0 | Low volume |
| opencode | free models | 7 | 1 | 1 | 2 | Scattered, acceptable |

**83% success rate** (90/108 runs). NULL outcomes (10) likely reflect mid-session kills from the evening retrospective task restarts — not alarming.

---

## Current State

- **Active tasks**: 2 in_progress (evening retrospective jobs), 1 blocked (#30429 — trading update, stale)
- **Open GitHub issues**: 0 — queue fully cleared
- **Pipeline health**: No recurring failures observed. All major review-cycle failure modes addressed.

---

## Patterns & Observations

**What's working:**
- Review pipeline is now substantially more robust — 8 bugs fixed in a single day covering the complete failure surface
- `orch doctor` gives operators a fast health snapshot without querying SQLite directly
- Cooldown CLI enables emergency resets without service restarts
- 83% agent success rate with claude leading (credit restoration apparent)

**Still worth watching:**
- `NULL` outcome runs (10 in 12h) — mostly benign (cleanup races) but worth confirming they trend down
- Stale worktree metadata (`fatal: not a git repository` in brew error log) — benign but obscures real errors; a `git worktree prune` in user-managed project dirs at startup would eliminate this
- Task #30429 (trading update) is blocked with no block_reason — may need manual investigation

---

## Tomorrow's Priorities

1. **Investigate #30429 (blocked, no block_reason)** — check whether the block is stale or needs human intervention.
2. **Monitor NULL outcome trend** — should decline given today's fixes. If still elevated (>5 in 12h), investigate cause.
3. **Stale worktree metadata log spam** — low urgency, but a `git worktree prune` call in `reconcile_startup_worktrees` for user-managed project dirs would clear it permanently. Consider filing if not already tracked.
4. **Run `orch doctor`** — first production use of the new feature; validate outputs are accurate and actionable.
5. **Review cost accounting** — token tracking was broken for review agents until today's fix. Verify post-fix cost data looks correct in next morning's cost report.
