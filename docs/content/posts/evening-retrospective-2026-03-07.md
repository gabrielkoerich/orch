+++
title = "Evening Retrospective — 2026-03-07"
date = 2026-03-07
description = "Service outage recovery, two PRs merged, internal:29 loop diagnosis, code-development job body fix"
+++

## Summary

Today was disrupted by a ~4.5-hour service outage (SIGTERM at 12:37 UTC, restart at 17:06 UTC).
Despite this, two PRs merged, the review throttle and agent-output-in-logs features shipped.
The day's scheduled tasks (morning review, code review) were still in-flight at end of day after
being re-dispatched post-restart.

---

## What Landed Today

| Commit | PR | Description |
|--------|-----|------------|
| `1b8bd68` | #515 | fix: throttle review agents in sync tick — `max_concurrent_reviews` semaphore |
| `f0415d4` | #516 | feat(cli): show agent output in `orch task logs` with safe UTF-8 truncation |

Both PRs were reviewed by the automated review agent and passed CI (7/7 checks).

---

## What Was Planned (from last evening retro)

The 2026-03-06 close-out retrospective flagged:

1. **#441** — `orch task unblock` ignores internal task IDs
2. **#448** — Engine health checks blind to internal tasks
3. **#446** — `orch task status` excludes internal tasks
4. Reduce stuck detection thresholds (part of #448)
5. Morning review should self-dispatch reliably

**None of these were completed.** The service outage consumed the day. Both the morning review
(internal:34) and the code review (internal:33) tasks were killed mid-flight and re-dispatched
after restart — their outcomes are still pending as of this writing.

---

## Task Outcomes

| ID | Title | Status | Notes |
|----|-------|--------|-------|
| internal:31 | Throttle review agents | done | PR #515 merged |
| internal:32 | Show agent output in logs | done | PR #516 merged |
| internal:33 | Code review: orch | routed | Re-dispatched post-restart, in progress |
| internal:34 | Morning review | routed | Re-dispatched post-restart, in progress |
| internal:35 | Evening retrospective (this) | in_progress | — |
| internal:29 | Code development: orch | routed | 7 attempts, analysis-only loop |

---

## Failures and Retries

### Service Outage (SIGTERM at 12:37 UTC)

The orch service received SIGTERM with 4 active sessions. The graceful shutdown waited for
completion but the service was terminated externally (likely Homebrew service management or
a system event). Sessions for internal:29, 33, 34 were killed mid-run.

On restart at 17:06 UTC:
- 4 tasks recovered from stale InProgress/InReview state via the stuck-task detection
- internal:32 (InReview) reset to NeedsReview, review agent re-spawned, approved PR #516
- internal:33, 34, 35 re-routed and re-dispatched

The stuck-task recovery worked correctly. No manual intervention was needed.

### internal:29 — Code Development Analysis-Only Loop

After 7 attempts, the code-development task has never produced a PR. Each attempt ends with
the agent reading PLAN.md, suggesting improvements, but making no code changes. The review
agent correctly rejects with "no PR and no commits" and re-routes for retry.

**Root cause**: The code-development job body says "Review PLAN.md. Look for what still needs
to be implemented." Without a specific implementation target, agents default to analysis mode.

**Fix applied**: The job body has been rewritten to be directive — pick one specific PLAN item
and implement it, not analyze it. See `.orch.yml` change in this PR.

### internal:29 Max LLM Route Attempts (kimi rate limit)

At attempt 7, the LLM router hit 3 failed attempts (all falling to kimi which was rate-limited)
and fell back to round-robin. This is correct behavior — the rate-limit detection + cool-down
fix from earlier this week is working. The fallback to kimi was unlucky timing.

---

## Recurring Warning: corrupt updated_at timestamp

Every tick prints:
```
WARN orch::db: corrupt updated_at timestamp in internal_task error=premature end of input
```

This indicates one internal task row has a malformed `updated_at` field in SQLite. Not
blocking, but noisy. Root cause: likely a NUL byte or truncated write from a prior crash.

A repair migration should be added to `db_open()` in `src/db.rs` to sanitize or NULL out
corrupt timestamp fields on startup. This is a data integrity fix, not urgent.

---

## Routing Assessment

LLM routing performed well today:
- internal:33 (code review) → claude/complex — correct (deep codebase analysis)
- internal:34 (morning review) → claude/medium — correct (diagnostic + writing)
- internal:35 (evening retro) → claude/medium — correct
- internal:29 (code dev) — hit max LLM attempts (3), fell back to round-robin (kimi → claude)

Round-robin fallback is working. Rate-limit detection for kimi is working (from previous fix).

---

## Prompt Assessment

### code-development job body (fixed today)

The old body was too open-ended: "Review PLAN.md. Look for what still needs to be implemented."
This is an analysis prompt, not an implementation prompt. Agents that read PLAN.md and find
ideas but don't know *which one to implement* produce analysis-only output.

Fixed by: directive language requiring the agent to pick one specific TODO item from PLAN.md
and implement it in full, including tests and commit.

### Other prompts

- `prompts/agent_system.md` — no changes needed
- `prompts/review_task.md` — holding; PLAN.md + AGENTS.md pre-read still in effect
- `prompts/route.md` — working well; routing decisions are accurate

---

## Open GitHub Issues (last known state from 2026-03-06 retro)

| # | Title | Last Status |
|---|-------|-------------|
| #448 | Engine health checks blind to internal tasks | Routed |
| #446 | `orch task status` excludes internal tasks | In progress |
| #443 | Runner infra failure doesn't update external task to NeedsReview | In review |
| #441 | `orch task unblock` ignores internal task IDs | In progress |
| #435 | `resolve_repo_root` wrong path for bare-clone projects | In review |
| #431 | Bidirectional channel interaction | In review |

Note: `gh issue list` returns empty from this worktree context (auth issue). Verify from
project dir: `cd /Users/gb/Projects/orch && gh issue list --state open`.

---

## New Issues to File

### 1. SQLite corrupt timestamp repair migration

**Root cause**: A malformed `updated_at` value in the `internal_tasks` table causes a
deserialization error on every tick. The warning is harmless today but could mask real errors
and will print forever until fixed.

**Fix**: Add a startup migration in `src/db.rs` that runs `UPDATE internal_tasks SET
updated_at = NULL WHERE updated_at NOT LIKE '%T%'` (or similar) to sanitize broken timestamps.

This is a low-urgency data integrity fix. Filing as issue.

---

## Tomorrow's Priorities

1. **#441** — `orch task unblock` ignores internal task IDs. Most operationally urgent: stuck
   internal tasks can't be recovered without direct DB edits.
2. **#448** — Engine health checks for internal tasks. Stuck detection and NeedsReview catch-up
   are still blind to SQLite tasks. A second service interruption would leave more tasks stranded.
3. **SQLite corrupt timestamp** — file and fix. Low urgency but noisy every tick.
4. **Verify internal:33 and internal:34** — check their outcomes once they complete.
   If the code review (33) or morning review (34) found actionable issues, carry them forward.
5. **internal:29** — with the updated job body, the next code-development run should produce
   actual code. Watch attempt #8 closely.
