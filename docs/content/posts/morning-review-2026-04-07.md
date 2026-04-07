+++
title = "Morning Review — 2026-04-07"
date = 2026-04-07
description = "High-throughput reliability sweep: ~20 commits overnight, 4 tasks blocked on review-agent failure cascade, CLI/service mismatch persists — kimi recovery expected by noon"
+++

# Morning Review — 2026-04-07

## Recent Commits & Progress

Very active overnight batch — ~20 commits since last evening's retro. Theme: reliability and correctness across the review pipeline, task dispatch, and async safety.

**Review pipeline correctness:**
- `review_poll global last_review_ts watermark permanently silences reviews` (#2095 / `1db81916`) — global watermark was updated even when a newer review existed, permanently suppressing parallel reviews from earlier reviewers. Fixed to per-reviewer map.
- `task success path still bypasses per-agent result extractors` (#2086 / `400a7da6`) — success path fell back to text synthesis instead of using per-agent extractors. Now uses extractors consistently.
- `tmux final-output capture logs errors for normal session teardown` (`e685ed0c`) — normal teardown events were generating spurious error logs, inflating visible error counts.
- `router success logs still dump raw NDJSON preview` (`3f177f8a`) — success path was logging raw NDJSON instead of parsed output, making logs noisy and harder to scan.
- `skip NeedsReview refire when store read fails` (`1ff01f46`) — prevented infinite refire loop when store was transiently unavailable.

**Dispatch and routing correctness:**
- `tick_dispatch_tasks uses fragile error-string matching to detect missing agent` (#2093 / `e3c45fb0`) — missing-agent detection relied on matching the string "executable file not found", which is OS-specific and locale-dependent. Silent regression risk on non-English or non-Linux systems.
- `skip routed transition when route persistence fails` (#2094 / `22641d42`) — task could get stuck in limbo if persistence failed mid-transition.
- `row_to_task silently maps unknown/corrupt status to TaskStatus::New` (`af5b3799`) — corrupt DB rows caused tasks to reappear as new rather than erroring visibly.
- `mention dedup query failure silently creates duplicate tasks` (`b9fd3049`) — dedup query failure fell through to task creation, producing duplicates.

**Async safety:**
- `Synchronous file system operations stall tokio executor threads` (`b9fd3049`) — `std::fs::read_to_string` in async paths; dispatched via `spawn_blocking` now.
- `read current counter value on DB error in push_failures` (`d0809cbc`) — counter returned stale value on DB error, bypassing failure limits.

**Infrastructure:**
- `log git worktree prune errors and use output_with_context` (#2091 / `d06cb0c6`) — prune errors were silently swallowed.
- `batch-get failure logging in ingest_external_tasks` (`4fb80f4e`) — batch failures produced no log entry, making ingestion failures invisible.
- `cleanup worktree on task_init errors` (`f871c4bf`) — leaked worktrees on init failure.

---

## Operational Health

**Overall: degraded. CLI/service mismatch persists. 4 external tasks blocked. opencode/qwen3.6 failure rate high.**

### CLI/Service version mismatch — action still needed

```
CLI:     0.60.57
Service: 0.60.76  ✗ mismatch (19 versions behind)
```

This was flagged in yesterday's morning review (0.60.44 vs 0.60.50) and the evening retro confirmed it was not resolved. The gap has now grown to 19 versions. Run:

```bash
brew upgrade orch && brew services restart orch
```

### Agent success rates (last 24h)

| Agent | Model | Successes | Failures | Notes |
|-------|-------|-----------|---------|-------|
| claude | sonnet | 70 | 3 + 1 timeout | Primary workhorse, 95% |
| minimax | opus | 67 | 4 + 3 timeout | 90% — periodic cooldowns |
| opencode | minimax-m2.5-free | 21 | 1 | 95% |
| opencode | github-copilot/gpt-5-mini | 15 | 1 | 94% |
| opencode | qwen3.6-plus-free | 4 | 14 | **22% — still failing** |
| opencode | github-copilot/gpt-5.4 | 13 | 1 | 93% |
| opencode | github-copilot/claude-sonnet-4.6 | 12 | 4 | 75% |
| opencode | nemotron-3-super-free | 11 | 1 | 92% |
| claude | opus | 10 | 0 | 100% |
| claude | haiku | 9 | 0 | 100% |
| codex | gpt-5.3-codex | 0 | 8 | Cooled until Apr 9 18:22 UTC |
| kimi | opus | 0 | 7 | Cooled until ~12:35 UTC today |

**opencode/qwen3.6 (14 failures):** 22% success rate despite Alibaba rate-limit detection fix (`e454c61d`) deployed yesterday. Some failures may pre-date the fix, but 14 is high enough to warrant verification that cooldowns are being applied on new failures. Check `orch cooldown list` after next qwen3.6 failure.

**codex (8 failures):** All from credit exhaustion before cooldown was applied. Now cooled correctly until Apr 9. No action needed.

**kimi:** Both cooldowns expire before noon today (~12:20 and ~12:35 UTC). Recovery should be automatic.

### Active cooldowns

| Key | Remaining | Reason |
|-----|-----------|--------|
| codex | 2d 11h | credit exhaustion |
| kimi | 2h 34m | billing cycle (expires ~12:35 UTC) |
| kimi:haiku | 2h 18m | billing cycle (expires ~12:20 UTC) |

### Task activity (last 12h)

| Event | Count | vs. Yesterday |
|-------|-------|--------------|
| status_change | 1,254 | +257 |
| dispatch | 341 | +37 |
| push | 301 | +31 |
| branch_delete | 264 | +18 |
| review_start | 168 | +28 |
| review_decision | 139 | +19 |
| pr_create | 130 | +11 |
| error | 47 | **+21** — elevated |
| rerouted | 30 | **+20** — elevated |
| timeout | 4 | +1 |

**Errors elevated at 47 (up from 26 yesterday).** Reroutes also up sharply to 30 (from 10). Some of this is expected noise from the qwen3.6 failures and blocked task cascades. Watch over next 12h — if errors stay above 30 after qwen3.6 cooldown applies, investigate.

### Stuck/blocked tasks

| Task | Status | Age | Reason |
|------|--------|-----|--------|
| #2058 | blocked | 8h | Bug: Blocking I/O in async webhook server startup |
| #2045 | blocked | 11h | perf: async blocking audit (1 try) |
| #2043 | blocked | 11h | bug: parse error in review should re-route (2 tries) |
| #2001 | blocked | 12h | Collapse ingest status fan-out |
| internal:63857 | blocked | 11h | Code improvement discovery (review agent blocked) |

**All 4 external tasks are blocked.** The retro identified root cause: all blocked due to review agent parse failures before per-agent extractor fix (`8bb493d2`) was deployed. Issue #2043 (parse error should re-route instead of block) needs to land before these can be unblocked — but #2043 is itself blocked. This is a deadlock: fixing the review parse failure requires review to work, but review is broken for these tasks.

**Human action required:** Manually unblock these tasks with `orch task unblock all` once the queue is clear, or unblock #2043 specifically to get it re-dispatched.

---

## Retro Follow-Ups

| Priority from Apr 6 retro | Status |
|---------------------------|--------|
| CLI/service sync (was 0.60.44 vs 0.60.50) | ✗ Not resolved. Now 0.60.57 vs 0.60.76 — gap widened to 19 versions. |
| #2043 fix landing → unblock 4 tasks | ✗ #2043 still blocked (2 tries, review agent failure). Human unblock needed. |
| kimi recovery overnight | ✓ Cooldowns set correctly; expire ~12:20-12:35 UTC today. |
| opencode/qwen3.6 stability | ✗ 14 failures in 24h. Detection fix deployed but qwen3.6 failure rate still 22%. Verify cooldown applies on next failure. |
| Async blocking audit (#2045) | ✗ Deferred again — task is blocked. Needs unblock first. |
| #2030 GraphQL projects.rs | ✓ Merged (`787aa237` — `fix: use GraphQL variables for project queries`). |

---

## Priorities for Today

1. **Upgrade CLI and restart service** — `brew upgrade orch && brew services restart orch`. Service is 19 versions ahead. This has been flagged two days running. Must act today.

2. **Unblock stuck tasks** — `orch task unblock all`. All 4 external tasks are blocked due to pre-fix review parse failures. After unblocking, they should re-dispatch successfully with the new per-agent extractor logic. #2043 specifically needs attention — if it fails a 3rd time, investigate the review agent dispatch directly.

3. **Verify kimi recovery** — Cooldowns expire ~12:20-12:35 UTC. Confirm dispatches resume after that window by checking `orch task list` mid-morning.

4. **Watch qwen3.6 cooldown application** — 14 failures in 24h is above expected. Verify the Alibaba detection fix (`e454c61d`) is actually triggering cooldowns by checking `orch cooldown list` after the next failure. If qwen3.6 is still failing without a cooldown being applied, the fix may not be in the running service version.

5. **Monitor error rate** — Errors at 47 (up from 26), reroutes at 30 (up from 10). Both elevated. Should drop after unblocking tasks and qwen3.6 cooldown stabilizes. If still elevated by afternoon, check logs for new error patterns.
