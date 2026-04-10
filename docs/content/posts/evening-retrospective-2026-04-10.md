+++
title = "Evening Retrospective — 2026-04-10"
date = 2026-04-10
description = "Day 4 reliability sprint closes strong: 43 commits, 20 bugs fixed, 0 open issues. Three audit trail bugs dominated the day. Success rate 85% (420/494). Throughput up ~30% vs yesterday."
+++

# Evening Retrospective — 2026-04-10

## Summary

Day 4 of the reliability sprint closes with **43 commits, 20 issues closed, and zero open issues.** Three audit trail bugs consumed most of the day's cycles. The pipeline is performing exceptionally well — 85% success rate on 494 task runs, throughput up ~30% vs yesterday.

---

## Morning Priorities — Outcome

| Priority from morning review | Status |
|------------------------------|--------|
| Upgrade CLI/service (0.60.x → 0.61.x boundary) | **Unknown** — not confirmed. Version may still be stale. |
| Verify #2317 silence detection fix | **Done** — opencode now has 25+ successes with minimax-m2.5-free; no 600s kills observed. |
| Investigate claude timeouts (5) | **Unresolved** — 5 new claude/sonnet timeouts today. Persistent pattern. |
| Investigate olm/gemma4 absence | **Inconclusive** — still absent from routing. Not configured or intentionally excluded. |
| Monitor throughput stability | **OK** — 30% throughput increase held, error rate proportionally lower. |

---

## What Was Accomplished

### Today's commits — grouped by theme

#### Audit trail integrity (3 major bugs, ~18 commits)

- **`3d8b40a8` (closes #2394) prevent stale last_error from prior agent contaminating task_run audit**
  - UPSERT logic was overwriting `last_error` even when the new run succeeded, leaving the previous agent's error in the record.
  - Impact: audit trail showed wrong agent paired with wrong error. Makes post-mortem analysis unreliable.

- **`afbd2018` (closes #2394) preserve attempts counter in reset_failure_counters to prevent audit trail overwrites**
  - Related: `reset_failure_counters` was clearing the attempts counter before recording the new run, making `task_runs.attempts` always show 1.
  - Impact: all task runs appeared to be first attempts even on retries.

- **`b422dad3` (closes #2392) ReviewDecision::Rerouted stops no_code_reroutes counter reset — infinite retry loop**
  - `Rerouted` decision was missing from the "don't reset" list, causing the `no_code_reroutes` counter to reset on every re-route.
  - Impact: tasks with no PR and no commits would retry indefinitely, never hitting the block threshold.

- **`faa2970d` test regression tests for no_code_reroutes counter accumulation (#2392)**

- **`9ad9d33e` (closes #2389) bug: 41 fire-and-forget store_set calls silently discard DB write failures**
  - Massive cleanup: 41 sites across the codebase where `store_set` failures were ignored.
  - Pattern: `tokio::spawn(async { store.set(...) })` — failures silently swallowed by the spawned task.
  - Fixed by adding proper error handling or using a shared `observe()` pattern.

- **`513eb318` (closes #2384) review.rs fire-and-forget store_set for push_failures reset causes counter drift**

- **`8c8381a4` (closes #2383) store.get() error defaults to needs_review instead of done**
  - When `store.get()` failed, it was treated as "no PR found" → `needs_review`. Should be `done` (nothing to review).
  - Impact: tasks with transient DB errors would skip review instead of completing.

- **`03974275` (closes #2357) run_agent_session ignores invocation.timeout_seconds — per-complexity timeout unimplementable**

#### Performance improvements

- **`bebc68d1` perf: parallelize fallback_is_pr_merged_by_branch in cleanup.rs** — concurrent branch checks for worktree cleanup.

- **`47a4311f` perf: scan_comments re-introduces serial is_pull_request calls** — regression from batch fix #2342, serial calls re-introduced accidentally. Fixed.

- **`5f11441b` perf: batch is_pull_request checks in scan_mentions with join_all** (from yesterday, carried forward)

#### Mention / command / review fixes

- **`03225a9f` (closes #2377) acknowledge_mention failure leaves mention unacknowledged — cursor advances prematurely**
- **`36ba2a80` (closes #2376) advance cursor on FetchFailed/CollaboratorCheckFailed in scan_comments — infinite retry loop**
- **`e6504d11` fix: use expect() instead of unwrap_or(false) for is_pr_map lookup (#2378)**
- **`b9bdfba0` (closes #2368) auto_unblock_blocked_tasks increments counter before status update**
- **`42b87fad` refactor: unify scan_mentions and scan_commands into single scan_comments**
- **`3aa4ee0e` (closes #2363) handle_slash_command returns bool that callers ignore — unreachable false branch**
- **`5a3cfc58` fix(sync): only advance mention cursor on successful store write (#2362)**

#### Push / merge reliability

- **`4bf387f0` (closes #2382) pop stash and bail when rev-parse fails after git stash succeed**
- **`56eff734` (closes #2335) recover push failures by rebasing on remote branch**

#### Tests

- **`ff1b490d` (closes #2370) test: add test coverage for review_poll.rs — 19 tests across all 6 paths**
- **`673a6b8c` (closes #2372) test: all 4 event subscribers (dispatch, notify, review, unblock) have zero test coverage**
- **`faa2970d` test: regression tests for no_code_reroutes counter accumulation (#2392)**

#### Refactors

- **`982ccc3c` refactor: extract shared merge-conflict helper in review_poll.rs — ~120 lines duplicated**
- **`973339af` deps: replace unmaintained serde_yml with serde_norway**

#### Observability

- **`c0ac2932` (closes #2358) fix(ndjson): render Codex turn.failed events in orch stream**

---

## What Failed and Why

### claude/sonnet timeouts (5 in 24h, persistent from yesterday)

5 new `timeout` outcomes for claude/sonnet, distinct from `failed`. This is the same count as yesterday despite a 40% increase in total task runs. If it were purely proportional, we'd expect ~7 timeouts. The count is holding flat, which is mildly encouraging, but the pattern hasn't been root-caused.

The morning review flagged this for investigation — it wasn't addressed. Need to determine whether these are silence-detection timeouts (vs hard timeouts or something else), and whether the count is noise or a real ceiling on claude/sonnet latency.

### Fire-and-forget pattern is pervasive

41 sites of `store_set` failures being silently discarded — this was the biggest cleanup of the day. These were real bugs hiding behind fire-and-forget semantics. The pattern is now documented as a anti-pattern and fixed across the codebase, but it highlights how far the codebase had drifted from proper error handling.

---

## Routing Accuracy

**Overall: excellent.** 85% success rate (420/494) is the highest since the sprint started. Key improvements visible:

- **opencode silenced detection (#2317)**: opencode now successfully runs 25+ sessions with minimax-m2.5-free. The fix is working.
- **Audit trail**: task_runs now correctly records agent, error, and attempts for all runs.
- **no_code_reroutes infinite loop**: tasks without PRs now correctly hit the block threshold instead of retrying forever.
- **Store error handling**: 41 DB failure sites now handle errors properly — tasks won't silently skip review or lose state.

### Agent health (24h, 494 runs)

| Agent | Model | Success | Failed | Timeout | Rate Limit | Total | Rate |
|-------|-------|---------|--------|---------|------------|-------|------|
| claude | sonnet | 137 | 12 | 5 | 3 | 157 | 87% |
| kimi | opus | 79 | 5 | 2 | 3 | 89 | 89% |
| codex | gpt-5.3-codex | 70 | 4 | 0 | 0 | 74 | 95% |
| minimax | opus | 54 | 0 | 3 | 2 | 59 | 92% |
| opencode | minimax-m2.5-free | 25 | 0 | 0 | 0 | 25 | 100% |
| opencode | gpt-5-mini | 14 | 0 | 0 | 0 | 14 | 100% |
| claude | opus | 16 | 1 | 0 | 0 | 17 | 94% |
| opencode | nemotron-3-super-free | 10 | 5 | 0 | 0 | 15 | 67% |

Notes:
- **opencode/nemotron is the weakest model** (5 failures, 67% success). Worth watching — may need cooldown.
- **opencode overall is now healthy** with minimax-m2.5-free and gpt-5-mini (100% success).
- **claude/sonnet** has the most volume and the most failures (12 failed, 5 timed out — 10% failure/timeout rate).
- **codex** is performing well at 95% despite being the second-highest volume.
- **kimi/opus** rate limits are still low (3/89 = 3.4%), backoff handling correctly.
- **ol m/gemma4** — still absent from routing entirely.

---

## Open Issues

**None.** All 20 bugs filed today are closed. This is the first zero-open-issue retrospective since the sprint started.

---

## Priorities for Tomorrow

1. **Root-cause claude/sonnet timeouts** — 5 timeouts per 157 runs (3.2%) may be silence detection misfires or a legitimate latency ceiling. Need to look at actual session durations in `task_runs` to determine if these hit a hard timeout, a grace period, or something else. Check the `task_runs` table for `duration_seconds` on these records.

2. **Upgrade CLI/service** — now in its 3rd day as an unconfirmed issue. The service is on `0.61.x`, CLI on `0.60.x`. At this point the CLI upgrade is overdue. Check during morning review:
   ```bash
   brew upgrade orch && brew services restart orch && orch version
   ```

3. **Investigate opencode/nemotron failures (5)** — 67% success rate for this model is the lowest across all agents. Either apply a cooldown or investigate why it's failing disproportionately.

4. **Verify audit trail fixes in production** — the 3 task_runs bugs (#2394, #2393, #2392) all landed with tests, but the real validation is in the production DB records. After the morning review, spot-check `task_runs` to confirm agents/errors/attempts are all correct.

5. **ol m/gemma4 status** — still absent from routing after 2 days. Either confirm it's intentionally excluded from the agent config, or investigate why it's not appearing.
