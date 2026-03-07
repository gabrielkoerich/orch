+++
title = "Morning Review — 2026-03-07"
date = 2026-03-07
description = "Service 4.5h downtime, corrupt timestamp fix, PR #516 merged, 3 tasks dispatched"
+++

## Summary

Service recovered from a 4.5-hour downtime window (12:37–17:06 UTC). Four tasks were stuck and
recovered by the stale session detector at 17:06. PR #516 merged successfully at 17:08. The only
code fix applied today: a V4 DB migration to clean up corrupt `updated_at` timestamps.

---

## Recent Commits (last 24h)

| Commit | Description |
|--------|-------------|
| `bc6837d` | fix: skip cooled-down router LLM agent and detect rate limits |
| `407cd6d` | fix: use configured default branch in merge-conflict rebase |
| `c62ce12` | fix: rebase worktree on merge conflict |
| `3553c5f` | fix: handle codex NDJSON format in review response parser |
| `4c12134` | fix: block tasks at max attempts; fix Zola taxonomy |

---

## Log Analysis

### Service Downtime: 12:37–17:06 UTC (~4.5 hours)

At 12:37 UTC a SIGTERM was received (likely a `brew services restart` during a prior agent session).
The service initiated graceful shutdown with 4 active sessions (`internal:29`, `32`, `34`, `35`)
and a 600s wait. The error log then accumulated 651 "no valid projects configured" lines — these
are historical log entries from an earlier crash-loop period (pre-PR #233), not new errors.

At 17:06 the stuck-detection recovered all 4 tasks back to `new` status. By 17:07 all were
re-routed and dispatched. The pipeline resumed cleanly.

**No action needed** — stuck detection worked as intended.

### PR #516 Merged (17:08 UTC)

`internal:32` (fix: show agent output in `orch task logs`) completed its review cycle:
- kimi hit rate limit → put in cooldown
- minimax assigned → also cooled
- codex assigned → approved PR #516, CI green (7/7), merged

Cooldown rotation working correctly since `bc6837d`.

### Corrupt `updated_at` Timestamp (Fixed)

Every tick cycle logged:
```
WARN corrupt updated_at timestamp in internal_task error=premature end of input
```

**Root cause**: some `internal_tasks` rows have an empty string `""` for `updated_at`
(from early schema state or DB migration). The parse fails with `premature end of input`.

**Fix**: Added `SCHEMA_V4` migration in `src/db.rs` — backfills `updated_at = created_at`
for any row with NULL or empty timestamp. The warn fallback remains for any future corruption.

### internal:29 (Code Development) Round-Robin Fallback

The code-development task hits max LLM router attempts (3) and falls back to round-robin on
every re-dispatch. This is because the router LLM calls are all exhausting retries. Once the
task makes no commits and is re-reviewed → re-routed, the LLM route attempts reset. This is
expected behavior with rate-limited router agents; not a bug.

---

## Open Issues (5)

| # | Title |
|---|-------|
| #514 | Show agent output in `orch task logs` (PR open) |
| #513 | Throttle concurrent review agents (PR open) |
| #512 | docs: fix contradictions in morning-review-2026-03-06 |
| #510 | fix: hardcoded origin/main in merge-conflict rebase |
| #506 | docs: fix morning review 2026-03-06 inaccuracies |

Issues #441, #448, #446, #452 from yesterday's retro are no longer in the open list —
either resolved or closed. PRs #514 and #513 are pending review.

---

## Code Fix Applied

**`src/db.rs` — V4 migration** (this session):
- `SCHEMA_V4` added: `UPDATE internal_tasks SET updated_at = created_at WHERE updated_at = '' OR updated_at IS NULL`
- Migrate function updated to apply V4
- All CI checks pass (`fmt`, `clippy`, `cargo test`)

---

## Current Task State

| Task | Status | Agent |
|------|--------|-------|
| `internal:29` | dispatched (kimi, round-robin) | code development |
| `internal:33` | dispatched (claude/complex) | code review |
| `internal:34` | running (this task) | morning review |
| `internal:35` | dispatched (claude/medium) | evening retro |

---

## Tomorrow's Priorities

1. **#513** — Throttle concurrent review agents. PR is open; should be reviewed and merged.
2. **#510** — Fix hardcoded `origin/main` in merge-conflict rebase. PR open.
3. **Watch `internal:29`** — if it continues cycling with no commits, the task body may need
   tightening to actually produce code changes rather than analysis reports.
4. **Service uptime** — 4.5h gap is significant. Consider investigating whether the graceful
   shutdown timeout (600s) is too long and delays recovery when agents are stuck.
