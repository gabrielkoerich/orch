+++
title = "Morning Review — 2026-04-17"
date = 2026-04-17
description = "Daily operational check-in: 5 commits merged, version mismatch recurred (CLI 0.69.25 / Service 0.69.27), github-copilot non-gpt-5-mini models still failing, kimi still in extended billing cooldown."
+++

# Morning Review — 2026-04-17

## Recent Commits (last 24h)

5 commits merged — focused on data integrity and concurrency safety:

| Commit | Issue | Description |
|--------|-------|-------------|
| `d4b36da2` | #2736 | **Stuck-task recovery** — stop swallowing `resolve_task_id` errors; fixes stale routing fields. |
| `354f05c4` | #2735 | **Review subscriber** — distinguish DB errors from stale status. |
| `c8d63bb2` | #2734 | **Worktree scan** — stay resilient when dir entry reads fail. |
| `d90c2854` | #2730 | **Router lock** — avoid holding read lock across dispatch awaits. |
| `567678b1` | #2728 | **Backend merge** — merge external tasks when store is internal-only. |

---

## Operational Health

### Service

- **Version mismatch recurred**: CLI `0.69.25`, Service `0.69.27`. This is the third consecutive day of mismatch despite yesterday's evening retro claiming the problem was "resolved."
  - Apr 15 morning: 0.69.15 vs 0.69.18
  - Apr 16 morning: 0.69.15 vs 0.69.18 (still pending at morning)
  - Apr 16 evening: claimed fixed at 0.69.25
  - Apr 17 morning: 0.69.25 vs 0.69.27
- Fix: `brew upgrade orch && brew services restart orch`
- Logs: clean tick cycle (~1.5–1.9s), no persistent errors
- One watchdog stall alert (70s > 60s threshold) during this morning's dispatch — caused by glm:haiku returning 403, LLM budget exceeded, falling back to round-robin. Expected behavior under routing failure, not a real stall.

### Agent Health (24h)

| Agent | Model | Success | Failed | Rate limit | Notes |
|-------|-------|---------|--------|------------|-------|
| claude | sonnet | 31 | 12 | 0 | Main workhorse; 12 failures vs 31 successes = 72% |
| minimax | opus | 24 | 1 | 5 | Strong; rate limits on review runs |
| glm | opus | 20 | 2 | 1 | Solid performance after runner-registry fix |
| opencode | minimax-m2.5-free | 18 | 0 | 0 | Clean run |
| codex | gpt-5.3-codex | 17 | 0 | 0 | Good medium-task results |
| opencode | gpt-5-mini | 9 | 0 | 0 | Best github-copilot model |
| claude | sonnet | 12 | 3 | 0 | Another claude/sonnet window (failed) |
| opencode | gpt-5.4 | 0 | 3 | 0 | github-copilot still broken |
| opencode | claude-sonnet-4.6 | 0 | 3 | 0 | github-copilot still broken |
| opencode | gemini-3.1-pro-preview | 0 | 3 | 0 | github-copilot still broken |
| opencode | nemotron-3-super-free | 7 | 1 | 1 | 6 parse errors |
| minimax | opus | 5 | 1 | 5 | Rate limits confirmed |
| codex | gpt-5.2-codex | 0 | 1 | 0 | Model unavailable |
| glm | opus | 2 | 2 | 1 | Mixed |

**Notable improvements vs Apr 16:**
- claude/sonnet at 72% (was 73% → consistent)
- glm/opus much improved: 20 successes, only 2 failures (was 12 successes / 5 failures)
- codex/gpt-5.3-codex at 100% (was good)
- opencode/gpt-5-mini at 100% (still best copilot)
- nemotron parse errors down to 6 (was 5 in 12h yesterday — rate consistent)
- github-copilot gpt-5.4, claude-sonnet-4.6, gemini: still 0% each

### Active Cooldowns

| Key | Remaining | Reason |
|-----|-----------|--------|
| kimi | 4d23h | Billing cycle exhausted |
| glm:haiku | 3h58m | Persisted |

---

## Stuck / Blocked Tasks

- No open GitHub issues.
- Only active tasks: this morning review (`internal:145911`), morning-briefing (`internal:145912`), twitter-trending-watch (`internal:145913`).
- No blocked or stuck external tasks.

---

## Retro Follow-ups

| Priority from Apr 16 Evening | Status |
|------------------------------|--------|
| Fix version mismatch | **Re-broken again** — CLI 0.69.25 vs Service 0.69.27. Pattern: every day a new push lands between evening and next morning. Needs `brew upgrade && restart`. |
| Post-fix health signals (#2720, #2723) | **Confirmed improved** — glm/opus went from 64% to 91%, claude/sonnet stable at 72%. Placeholder-error noise gone. |
| github-copilot investigation | **Still unresolved** — gpt-5-mini (100%) is fine; gpt-5.4, claude-sonnet-4.6, gemini (all 0%) still failing. |
| nemotron parse errors | **Still occurring** — 6 parse errors in 24h (vs 5 in 12h yesterday, rate similar). Not worsening. |
| Monitor stream changes (#2717, #2712) | **Not confirmed** — `orch stream --pipe` and same-length diffing were deployed but today's review didn't exercise streaming enough to verify. |

---

## Task Activity (12h)

| Event | Count |
|-------|-------|
| status_change | 631 |
| dispatch | 211 |
| branch_delete | 144 |
| push | 137 |
| routed | 101 |
| review_start | 72 |
| review_decision | 65 |
| pr_create | 61 |
| error | 41 |
| rerouted | 12 |

Throughput is healthy and consistent with yesterday. Error rate (41 / 211 = 19%) is slightly lower than yesterday's 23%, consistent with github-copilot models being handled by cooldowns.

---

## Priorities Today

1. **Fix version mismatch** — `brew upgrade orch && brew services restart orch`. This is the fourth consecutive day this has been flagged. Service is 2 patch versions behind.

2. **Verify stream changes** — tomorrow's review should confirm `orch stream --pipe` and same-length output diffing work correctly with no regressions.

3. **github-copilot routing** — gpt-5-mini is the only healthy copilot model. gpt-5.4, claude-sonnet-4.6, and gemini-3.1-pro-preview are all at 0% and should remain excluded via cooldown.

4. **nemotron parse errors** — 6 parse errors in 24h. If this rate persists or worsens, inspect raw outputs and file a root-cause issue.

---

## Notes

- The recurring version mismatch is the most actionable item. The service keeps falling behind because `brew upgrade` is not being run regularly enough after pushes to main.
- No new GitHub issues to file. All observable problems are either covered by existing retro priorities or handled by the generic cooldown/error-classification system.
- The 70s watchdog stall during dispatch was benign — it was caused by the router correctly falling through its pool (glm:haiku returned 403) and exhausting its LLM budget before falling back to round-robin. The stall threshold (60s) is too tight for slow model routing. This is not a bug.

---

Prepared by Orch automation (internal task internal:145911).
