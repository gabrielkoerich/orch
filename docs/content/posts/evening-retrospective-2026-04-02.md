+++
title = "Evening Retrospective -- 2026-04-02"
date = 2026-04-02T23:59:00Z
[extra]
job = "evening-retrospective"
+++

## Summary

Exceptionally high-throughput day: **34 commits landed** across a concentrated reliability blitz covering routing, session management, response parsing, and channels. The open issue queue is clear (0 open issues). All three priorities from the morning review were addressed directly by agents working in parallel.

The day exposed a new anti-pattern: **duplicate issue filing** — three bug pairs were filed with near-identical titles within seconds of each other, resulting in duplicate work and wasted cycles. This is the clearest signal yet that issue deduplication needs to happen at task intake.

---

## Accomplished Today

### Priority Fixes (from Morning Review)

**1. `opus/` model assignment bug** — Addressed via #1610 (arch: Model resolution fragility). Agent-specific aliases no longer leak across agents on failover. The `opencode:opus/` cooldown remains active (~2h45m left) but new assignments should resolve correctly.

**2. SQLite OOB panics** — #1577 enforced explicit column lists via a lint, addressing the root cause. The `orch.error.log` panics reported in prior retros are from **Mar 30** (stale). No new panics in today's service log.

**3. OpenCode free model blocking** — #1598 (1h cache), #1601 (deduplicate async vs sync discovery), #1603 (poisoned mutex recovery). The synchronous subprocess blocking the async executor is now fixed.

### Response Handling & Parsing (4 fixes)

- **#1614/#1613** — OpenCode responses parsing as plain text/NDJSON were silently failing and skipping PR creation + token metadata. Fixed: parser now handles both formats.
- **#1617** — Push failures were swallowed; `response_handler` now surfaces and persists push errors so they're visible in task history.
- **#1618** — Token budget overruns accepted by `response_handler`. Now enforces output size limits to prevent runaway agents.
- **#1599** — Token metadata was being extracted from `error.raw` instead of `raw_stdout`. Fixed extraction path.

### Stale Session Management (3 fixes)

- **#1562** — Stale tmux sessions surviving graceful shutdown blocked dispatch. Fixed: session cleanup now runs during shutdown.
- **#1615/#1616** — Existing session check was too permissive — sessions registered in DB but already dead prevented re-dispatch. Fixed: dead sessions are now detected and ignored.

### Routing & Free-Model Discovery (3 fixes)

- **#1583** — Router blocked task routing for hours when all agents/models are cooled (no fallback path). Fixed.
- **#1598** — Free OpenCode models re-fetched synchronously per-request. Fixed with 1h cache.
- **#1578** — Added routing observability (route decisions + retry chains tracked in store).

### Channels & Transport (5 fixes)

- **#1592** — Slack/Telegram/Discord HTTP clients had no request timeouts. Fixed.
- **#1589/#1585** — `std::sync::Mutex` in async Slack/Telegram contexts. Replaced with `tokio::sync::Mutex`.
- **#1591** — Discord `INTERACTION_CREATE` processed events with empty `channel_id`/`custom_id`. Fixed: validate before processing.
- **#1587/#1582** — Discord gateway processed malformed `MESSAGE_CREATE` events with empty IDs. Fixed: skip and log.
- **#1596** — Transport conversation key excluded `topic_id`, causing multi-topic message misrouting. Fixed.

### Review Pipeline (3 fixes)

- **#1595** — Batched PR review polling assumed GraphQL comments are time-ordered (they're not). Fixed.
- **#1572** — Blanket `review_cycles` reset on `NeedsReview` was circumventing max cycle enforcement. Fixed: preserved in stale-review bypass.
- **#1561** — Closed PR without merge triggered false review agent failures and blocked task. Fixed.

### Other Fixes

- **#1568** — `is_transient_github_error` missed several transient patterns (408, 429, connection reset). Fixed.
- **#1570/#1569** — PR URL parsing in `auto_merge` and `review.rs` lacked validation. Fixed.
- **#1576** — Worktree cleanup failed on broken git metadata. Fixed: validate before git operations.
- **#1594** — Channel task bindings ignored topic/thread identity, misrouting multi-topic messages. Fixed.
- **#1558** — `auto_merge` CI polling blocked `sync_tick`. Fixed: spawned in background with exponential backoff.
- **#1571** — Token metadata discarded when response fell back to text synthesis. Fixed.

---

## Agent Performance (Last 24h)

| Agent | Runs | Success | Success Rate | Notes |
|-------|------|---------|-------------|-------|
| claude | 76 | 60 | **78.9%** | Below average — model assignment artifacts |
| kimi | 56 | 32 | **57.1%** | Degraded — opus routing issues |
| opencode | 155 | 84 | **54.2%** | Degraded — parse failures + model issues |
| minimax | 7 | 0 | **0%** | Effectively offline today |

**Overall: ~58% success** (176 completed / 294 total runs) — lower than yesterday's 82%.

The degraded rates reflect a cascade: opencode parse failures and stale session blocking caused retry storms. The fixes landed late in the day, so the metrics don't yet reflect improvement.

### Issue Attribution (Issues Closed Today by Agent)

| Agent | Issues Closed |
|-------|--------------|
| opencode | 22 |
| kimi | 13 |
| claude | 2 |
| minimax | 1 |

Opencode dominated issue resolution despite its degraded success rate — indicating many tasks required multiple attempts before completion.

---

## Patterns & Issues

### 1. Duplicate Issue Filing — High Priority

Three pairs of near-identical issues were filed within minutes of each other:
- `#1606` + `#1611` — OpenCode parse-fail (filed at 21:53 and 21:54)
- `#1608` + `#1612` — Stale tmux session dispatch (filed at 21:55 and 22:14)
- `#1605` + `#1607` — Token budget overruns (filed at 22:07 and 22:10)

Each pair was resolved separately — wasting ~2 agent-task slots per duplicate. Root cause: the retrospective/analysis jobs that create GitHub issues don't check for already-open issues on the same topic before filing. The `gh issue list --state open` check is happening, but apparently misses recently-filed open issues or races with concurrent issue creation.

**Action needed**: Add deduplication at issue-filing time — check for open _and recently closed_ (< 24h) issues with similar titles before creating new ones.

### 2. Phantom Session Kill-Session Spam

The service emits `kill-session failed (may already be dead)` WARNs every tick for the same ~10 session IDs. These are numeric task IDs and `internal-33498` that are recorded in the DB but whose tmux sessions no longer exist. The stale session fix (#1615) addresses dispatch blocking, but the log spam from repeated kill attempts persists.

**Action needed**: Stale session entries should be pruned from the DB after a single confirmed kill failure, not retried every tick.

### 3. Minimax Completely Offline

7 runs, 0 successes. Minimax went from 12 successes yesterday to 0 today. No cooldown entries show for it. This needs investigation — either the agent binary is broken or all minimax models are failing silently.

### 4. Version Mismatch

CLI is at `0.57.4`, service at `0.57.7`. Indicates `brew upgrade orch` hasn't been run since today's releases. Non-blocking but should be cleaned up.

---

## Cooldown Status (Active)

| Agent/Model | Remaining | Reason |
|-------------|-----------|--------|
| kimi:haiku | 42m | Persisted |
| opencode:mimo-v2-omni-free | 1h38m | Persisted |
| opencode:nemotron-3-super-free | 3h55m | Persisted |
| opencode:opus/ | 2h47m | Model assignment bug (persisted) |

The `opencode:opus/` cooldown is from the model assignment bug. Once the fix from #1610 is deployed and the cooldown expires, this should resolve cleanly.

---

## Tomorrow's Priorities

1. **Fix phantom session log spam** — Sessions recorded in DB but tmux-dead are retried every tick. Should be pruned after first confirmed failure. Low severity but high noise.
2. **Investigate minimax failure (0/7)** — Determine if agent binary is broken, all models are failing silently, or something else. File issue if root cause found.
3. **Add issue deduplication at filing time** — Check recently-closed (< 24h) issues before creating new ones to prevent duplicate pairs.
4. **Run `brew upgrade orch` + service restart** — Sync CLI to 0.57.7.
5. **Clear `opencode:opus/` cooldown after expiry** — Confirm model assignment is resolved after fix deploys.

---

## Open GitHub Issues

**0 open issues** — Queue clear. All bugs discovered today resolved same-day.
