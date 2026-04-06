+++
title = "Evening Retrospective — 2026-04-06"
date = 2026-04-06
description = "High-velocity security and correctness day: ~35 commits, 20 issues closed — token exposure, GraphQL injection, review pipeline reliability, and auto_merge correctness"
+++

# Evening Retrospective — 2026-04-06

## Summary

Extremely productive day. **~35 commits merged**, **20 issues closed**. The dominant theme was two high-severity security fixes that should have shipped sooner (GitHub token exposure, GraphQL injection), followed by a broad review pipeline correctness sweep and auto_merge reliability fixes. By end of day the review pipeline is substantially more reliable: per-agent extractors, Alibaba rate limit detection, review_agent_failures reset, and auto_merge merge-conflict bail correctly routed.

Queue is active: 4 tasks in_review (CI fix, NetworkError backoff, GraphQL projects.rs, NeedsReview backoff), 1 in_progress (#2048 pagination), and 4 blocked on review agent failures.

---

## Morning Priorities — Outcome

| Priority from morning review | Status |
|------------------------------|--------|
| Upgrade CLI/service (mismatch 0.60.44 vs 0.60.50) | No evidence of action — CLI/service mismatch may persist |
| Codex usage limit cooldown (until Apr 9) | ✓ Cooldown applied: `cooldown:codex` expires Apr 9 18:22 UTC. 8 dispatches today — all failed before cooldown locked in. |
| kimi recovery at ~13:00 UTC | ✗ Not yet recovered — cooldown expires ~23:33 local (Apr 7 02:33 UTC). failure_count:kimi=19, kimi:haiku=22. |
| Unblock internal:54549 | Unknown — not visible in current task list. Presumably handled or expired. |
| Async blocking audit (#2045) | ✗ Now blocked ("review agent blocked — exceeded failure threshold"). Fix for #2043 would unblock it. |
| Watch review pipeline stability | ✓ No unexpected accumulation. Five overnight fixes confirmed stable. |

---

## What Was Accomplished

### Security fixes (highest priority)

- **GitHub token in CLI args** (#2007 / `7e5dd799`) — token was passed directly via `git -c http.extraheader=...` making it visible in `ps` output to any local user. Fixed to use env var or credential helper.
- **GraphQL injection via `format!()`** (#2005 / `42de5506`) — string interpolation in GraphQL queries allowed injection attacks. Fixed to use variables in all queries. A companion issue (#2030) remains open for `projects.rs` specifically (currently in_review).

### Review pipeline correctness sweep

- **Per-agent extractors for review parse** (#2032 / `8bb493d2`) — replaced manual parse logic with per-agent extractors. Codex and opencode previously failed JSON extraction frequently; parse failures now give structured error with raw output captured (`8939bdcb`).
- **Alibaba "rate increased too quickly" detection** (#2023 / `e454c61d`) — opencode/qwen3.6 rate limit messages were not being detected, causing re-routing without cooldown. 7 out of 10 qwen3.6 runs failed today (pre-fix). Fix now in production.
- **review_agent_failures not reset across review cycles** (#2024 / `eb486c3b`) — transient failures from previous review cycles were accumulating and blocking tasks prematurely. Fixed: failures now reset when re-routing for a new cycle.
- **Review watermark persistence** (`a52e518c`) — watermark was persisted before `handle_review_changes` succeeded; on DB error, same review was re-processed every tick. Fixed to persist only on success.
- **Auth headers in branch deletion** (#2010 / `efd25fbe`) — cleanup.rs `delete_branches` was missing auth headers, causing silent 401 failures on remote branch cleanup.

### auto_merge reliability

- **Merge-conflict bail misrouted as review_agent_failure** (#2031 / `3294fc3e`) — `auto_merge_pr` merge-conflict errors were going through `classify_review_failure`, incorrectly incrementing `review_agent_failures` instead of `merge_conflict_retries`. This was the root cause of task #2001 blocking cascade. Fixed.
- **Null `mergeable` treated as merge-ready** (#2011 / `7106b2e7`) — GitHub returns `null` for `mergeable` while still computing it; was proceeding to merge and hitting 405. Now retried as transient error.
- **Stash drop by reference** (#2006 / `697e1d95`) — `git stash drop` was called with SHA hash instead of stash ref (`stash@{N}`), causing silent no-ops during rebase cleanup.

### Data integrity / correctness

- **store_set_result for attempts counter** (`21d04147`) — `max_attempts` safeguard used a non-atomic increment that returned 0 on DB failure, allowing bypass of the max attempt limit.
- **reconcile_closed_tasks worktree cleanup only on status update success** (`3f933608`) — worktrees were being deleted even when the DB status update failed, losing work on transient DB errors.
- **PR recovery check on non-transient final errors** (`aa9c4aa0`) — final non-transient errors skipped the PR recovery path, leaving stale PRs open.
- **skip GitHub API calls for internal task IDs** (`88428c3a`) — internal task IDs (e.g. `internal:65302`) were being sent to GitHub API, producing 404 errors on every tick.

### Performance

- **Store helpers redundant SQL** (`499bebf3`) — store helpers were re-resolving `task_id` on every call via separate SQL query; now passed directly.
- **Ingest fan-out collapse** (`f644fd4a` / #1998) — `ingest_external_tasks` was issuing separate `list_by_status` queries per status; collapsed into single `active-issues` query.

### Infrastructure

- **Router LLM fallback routing to same cooled agent** (`57b0a5b7`) — when only one available agent was cooled, the fallback still routed to it. Fixed.
- **Control session: restrict directory listing** (`3ade1051`) — control sessions were able to read/list the main project directory, bypassing the sandbox intent.

---

## What Failed and Why

### Agent failures (last 12h)

| Agent | Model | Dispatches | Successes | Failures | Root cause |
|-------|-------|-----------|-----------|---------|------------|
| codex | gpt-5.3-codex | 8 | 0 | 8 | Credit exhaustion (until Apr 9 18:22 UTC) |
| kimi | opus | 5 | 0 | 5 | Billing cycle exhausted |
| opencode | qwen3.6-plus-free | 10 | 3 | 7 | Alibaba rate limit (detection fix now deployed) |
| claude | sonnet | 50 | 47 | 3 | 2 cooldown-triggered, 1 unknown |
| minimax | opus | 50 | 45 | 5 | Periodic agent_error (failure_count:minimax:haiku=3) |

**codex (8 failures):** All failed due to credit exhaustion (Apr 9). Cooldown is now applied correctly. No dispatches expected until Apr 9.

**kimi (5 failures):** Billing cycle not reset yet. `failure_count:kimi=19, kimi:haiku=22`. Cooldowns: kimi expires ~02:33 UTC Apr 7, kimi:haiku ~00:00 UTC Apr 7. Both should clear overnight. No intervention needed.

**opencode/qwen3.6 (7 failures):** Alibaba rate limit pre-dates today's detection fix. With `e454c61d` deployed, next failures will apply cooldown and stop the retry loop.

### Review agent blocks (4 tasks)

Four tasks are blocked with "review agent blocked — exceeded failure threshold": #2045, #2043, #2001, `internal:63857`. All blocked due to review parse failures before the per-agent extractor fix (`8bb493d2`) was deployed. Issue #2043 (parse error should re-route instead of blocking) is currently in_review — once that fix lands, these tasks can be unblocked and re-reviewed successfully.

---

## Routing Accuracy

Routing is working correctly for degraded agents — codex and kimi are cooled and skipped. Load is spread across claude, minimax, and opencode variants.

| Agent | Successes | Notes |
|-------|-----------|-------|
| claude/sonnet | 47 | Primary workhorse. 94% success. |
| minimax/opus | 45 | 90% success. Minor periodic cooldowns. |
| opencode/minimax-m2.5-free | 13 | 100% |
| opencode/gh-copilot/gpt-5-mini | 12 | 100% |
| opencode/gh-copilot/claude-sonnet-4.6 | 10 | 100% |
| opencode/qwen3.6-plus-free | 3 | 30% — rate limited (fix deployed) |
| opencode/nemotron-3-super-free | 8 | 89% |
| claude/haiku | 6 | 100% |
| claude/opus | 5 | 100% |
| codex/gpt-5.3-codex | 0 | Cooled until Apr 9 |
| kimi/opus | 0 | Cooled until ~02:33 UTC Apr 7 |

The router LLM fallback fix (`57b0a5b7`) prevents the edge case where a single available cooled agent was still selected on fallback. Weight decay and cooldown signals are working correctly to avoid wasted dispatches.

---

## System Health

- **Queue:** 4 in_review (CI, NetworkError backoff, projects.rs GraphQL, NeedsReview backoff), 1 in_progress (#2048), 4 blocked (review agent failure — pending #2043 fix)
- **Errors (last 12h):** 5 task errors visible in activity (HTTP transient). Claude cooldown at 23:33 local suggests a rate limit event this evening.
- **Active cooldowns:** codex (until Apr 9), kimi (until ~02:33 UTC Apr 7), kimi:haiku (tonight UTC), minimax (short — already close to expiry), opencode (short)
- **CI:** #2047 (CI broken on main) currently in_review — pipeline may be temporarily unstable
- **Stale KV:** `failure_count:opencode:github-copilot/gemini-3.1-pro. Did you mean` (17) and `failure_count:opencode:opus/` (15) are stale corrupted keys from #1934. Harmless but persist.

---

## Priorities for Tomorrow

1. **Verify CLI/service sync** — Morning review noted 0.60.44 vs 0.60.50 mismatch. Check `orch version` and run `brew upgrade orch && brew services restart orch` if still mismatched.

2. **#2043 fix landing** — Parse error in review should re-route instead of block. Currently in_review. Once merged, manually unblock the 4 tasks stuck with "review agent blocked" (#2045, #2043, #2001, internal:63857) so they get re-reviewed with the new extractor logic.

3. **kimi recovery overnight** — Both kimi cooldowns expire before morning UTC. Verify at morning review that `failure_count:kimi` drops and dispatches resume.

4. **opencode/qwen3.6 stability** — With Alibaba rate limit detection now deployed, watch that qwen3.6 failures apply cooldown correctly on next rate limit. If 0% success continues without cooldown being applied, something in the detection path is still wrong.

5. **Async blocking audit (#2045)** — Deferred three days running. Once unblocked post-#2043 fix, this is a targeted `rg 'std::fs::' src/` pass across async functions. Should be simple/quick but keeps slipping.

6. **#2030 GraphQL projects.rs** — Currently in_review. Completes the injection prevention sweep from #2005. Should land tomorrow.
