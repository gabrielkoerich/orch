+++
title = "Morning Review — 2026-04-15"
date = 2026-04-15
description = "Daily operational check-in: 12 commits merged in 24h, claude/opus at 27% (unchanged), kimi cooldown extended unexpectedly, version mismatch present."
+++

# Morning Review — 2026-04-15

## Recent Commits (last 24h)

12 commits merged — steady day with emphasis on bug fixes and push semantics:

| Commit | Issue | Description |
|--------|-------|-------------|
| `353287ac` | #2657 | **set_fields duplicate ALLOWED_FIELDS check** — dead code removed |
| `86a990de` | #2652 | **Fallback message for empty task_runs.error** — improves diagnostics |
| `f87bc031` | #2647 | **Hoist batch_session_active() to tick()** — eliminates duplicate tmux subprocess per cycle |
| `50dc043e` | #2646 | **PushResult::NoCommits variant** — semantic fix for no-commits case |
| `fedaf9f5` | #2645 | **Security: has_leaks only on high-confidence patterns** — reduces false positives |
| `190c086a` | — | **set_model/set_agent_cooldown now async** — guarantees KV persistence |
| `f96a1cc9` | #2641 | **RouterConfig::from_config() per review cycle** — not called repeatedly |
| `976ef4f1` | #2610 | **files_modified falls back to empty** — retry agents retain file context |
| `ddf5636e` | #2623 | **Ollama local model routing** — new feature |
| `b26b7ab2` | #2636 | **GitHub API fire-and-forget** — reduces tick latency |
| `d33f4463` | #2635 | **Corrupted worktree index recovery** |
| `f4d48adb` | #2633 | **LLM routing budget cap** — prevents tick loop stalls |

---

## Operational Health

### Service

- **Version mismatch**: CLI 0.69.0, Service 0.69.2 — `brew upgrade orch && brew services restart orch` needed
- Logs: clean tick cycle (~1.5s), no persistent errors, one HTTP retry noted (rate limit)
- Jobs executed: morning-review, morning-briefing, twitter-trending-watch

### Agent Health (24h)

| Agent | Model | Success | Failed | Rate |
|-------|-------|---------|--------|------|
| claude | sonnet | 39 | 27 | 59% |
| claude | opus | 3 | 8 | **27%** |
| minimax | opus | 28 | 4 + 4 rl | 70% |
| opencode | gpt-5-mini | 17 | 0 | **100%** |
| opencode | minimax-m2.5-free | 19 | 1 + 1 empty | 90% |
| opencode | gemini-3.1-pro-preview | 2 | 8 | 20% |
| opencode | gpt-5.4 | 1 | 7 | 13% |
| opencode | nemotron-3-super-free | 10 | 5 | 67% |
| glm | opus | 17 | 6 + 2 rl | 68% |

**Notable:**
- **claude/opus at 27%** — unchanged from yesterday (3/8 → 3/8). Issue #2653 is open.
- **opencode/gpt-5-mini 100%** — continues best free model performance.
- **kimi cooldown extended unexpectedly** — was expected to expire ~03:00 UTC but still shows 6d23h (billing cycle from Apr 14, not model-level). Still in extended outage.
- **opencode:github-copilot failures** — gpt-5.4 (7 failed), gemini-3.1-pro-preview (8 failed), claude models struggling.

### Active Cooldowns

| Key | Remaining | Reason |
|-----|-----------|--------|
| codex | 6d | Billing cycle exhausted |
| kimi | 6d23h | Billing cycle (still extended unexpectedly) |
| kimi:haiku | 3h4m | Model cooldown |
| opencode:github-copilot/gpt-5.4 | 2h59m | Persistence |

---

## Stuck / Blocked Tasks

- **internal:145307** — previous morning review, blocked 19h
- **2639, 2640** — external blocked tasks (maintenance / cleanup)
- No CI failure escalation observed in this window.

---

## Retro Follow-ups

| Priority from Apr 14 | Status |
|----------------------|--------|
| Fix CLI version mismatch | **Still pending** — CLI 0.69.0 vs Service 0.69.2 |
| Investigate claude/opus 27% rate | **Unchanged** — #2653 open |
| Monitor kimi recovery (Apr 15) | **Extended unexpectedly** — still 6d23h cooldown |
| Investigate "no PR/code changes" | Not queried in this review |
| Audit 47 blocked tasks | Not yet conducted |

---

## Task Activity (12h)

| Event | Count |
|-------|-------|
| status_change | 956 |
| dispatch | 310 |
| routed | 154 |
| branch_delete | 148 |
| push | 142 |
| error | 80 |
| review_start | 70 |
| review_decision | 65 |
| pr_create | 62 |

---

## Priorities Today

1. **Fix version mismatch** — `brew upgrade orch && brew services restart orch`. Was pending from Apr 14.

2. **Investigate kimi extended cooldown** — kimi was expected to recover ~03:00 UTC but shows 6d23h remaining. Either billing cycle did not reset or cooldown was re-applied. Check KV store or logs for re-trigger.

3. **Continue monitoring claude/opus** — issue #2653 is open. Track whether error patterns are now visible after #2652 fix.

4. **Audit blocked tasks** — 2-3 blocked from current list. Categorize for review.

---

## Notes

- kimi's unexpected billing cycle extension is unusual — was supposed to expire overnight. Needs investigation.
- No new GitHub issues created this morning beyond existing #2653.
- Service is otherwise healthy with clean tick cycles and steady throughput.

---

Prepared by Orch automation (internal task internal:145498).