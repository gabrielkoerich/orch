+++
title = "Morning Review — 2026-04-11"
date = 2026-04-11
description = "Day 5: reliability sprint holds strong with 30+ commits overnight. kimi billing cycle exhausted (19h cooldown). CLI/service gap now 2 minor versions (0.61.20 vs 0.63.0). All timeouts are legitimate 1800s hard timeouts. Zero open issues."
+++

# Morning Review — 2026-04-11

## Recent Commits & Progress

Strong overnight batch — grouped by theme:

### Async/runtime correctness

- **`fd1b3c3f`** (closes #2436) bug: opencode.rs spawns nested tokio runtimes via std::thread — wastes resources and risks panics. Fixes blocking `thread::spawn` calls that created competing runtimes.
- **`39f700bf`** (closes #2434) bug: blocking `Path::exists()` calls in async cleanup functions stall tokio worker threads. Replaced with `tokio::fs::metadata`.
- **`abec9ab3`** (closes #2422) Blocking `std::thread::sleep` in async contexts blocks Tokio worker threads.

### Engine correctness

- **`227b61a7`** (closes #2437) fix(engine): return early when model store write fails in `handle_review_changes` — prevents dispatching with stale model.
- **`f9dd32c9`** (closes #2429) fix(router): hold lock across `spawn_blocking` in `load_skills_catalog` — double-checked locking race condition.
- **`842b6209`** (closes #2428) bug: `next_round_robin_agent` fallback returns `None` when `review_rr_index` is out of bounds after agent list shrinks.
- **`886f9fb4`** (closes #2424) fix(runner): retry free opencode models on silent exit-0 before failing.

### Performance

- **`1cd9e2cf`** (closes #2435) perf: add `(repo, pr_number)` composite index to avoid full table scan — critical for `review_poll` at scale.

### CLI/UX

- **`a503ca8b`** (closes #2422) feat(cli): add `--formatted` toggle to `orch stream` output for human-readable NDJSON.
- **`a7a3f900`** (closes #2430) fix(runner): quote script path in tmux `send-keys` command — broke when `ORCH_WORKTREES` had spaces.

### Refactor

- **`c7018f28`** (closes #2423) refactor: streamline review parsing via per-agent extractor — reduces duplication across agent parsers.

---

## Operational Health

**Overall: good, with one significant blocker.**

### CRITICAL: kimi billing cycle exhausted

Kimi is on a **~19-hour cooldown** (expires approximately 05:35 UTC on Apr 12). The root cause from the routing logs:

```
router LLM returned error: auth error: Failed to authenticate. API Error: 403
{"error":{"type":"permission_error","message":"You've reached your usage limit for this billing cycle."}}
```

Impact on 24h stats:
- **kimi/opus: 77 successes, 17 failures** — the 17 failures cluster before the cooldown was applied.
- **kimi:haiku failure_count: 14** — the router was repeatedly trying kimi/haiku as the routing LLM before the billing exhaustion cooldown was set.
- The exponential backoff system handled this correctly: kimi is now cooled until the next billing cycle refreshes.

**No action needed** — the cooldown system is doing exactly what it should. But capacity is reduced until kimi comes back online.

### CLI/service version gap: now 2 minor versions

```
CLI:     0.61.20
Service: 0.63.0  ✗ mismatch — 2 minor versions behind
```

This is now in its **4th consecutive day** as an unresolved issue. The service is at 0.63.x, the CLI at 0.61.x. Two minor version boundaries crossed. Every day this persists, risk of CLI/API incompatibility grows.

**Action required — highest priority:**
```bash
brew upgrade orch && brew services restart orch && orch version
```

### Timeout investigation resolved

The Apr 10 retro flagged claude/sonnet timeouts as unknown. Today's data confirms: **all timeouts are hard 1800s (30 min) timeouts**, not silence detection or soft timeouts:

```
claude|sonnet|timeout|1800.0|claude timed out after 1800s, clearing agent/model for re-route
kimi|opus|timeout|1804.0|kimi timed out after 1804s, clearing agent/model for re-route
minimax|opus|timeout|1803.0|minimax timed out after 1803s, clearing agent/model for re-route
```

These are tasks that legitimately ran for 30 minutes. Not a bug, not silence detection misfires. The per-complexity timeout feature (#2357, fixed in `03974275`) should help differentiate simple vs complex task time limits going forward.

### Error log

`/opt/homebrew/var/log/orch.error.log` is 0 bytes. No service-level errors.

---

## 24h Agent Health

| Agent | Model | Success | Failed | Timeout | Rate Limit | Total | Rate |
|-------|-------|---------|--------|---------|------------|-------|------|
| claude | sonnet | 87 | 10 | 1 | 1 | 99 | 88% |
| kimi | opus | 77 | 17 | 2 | 4 | 100 | 77% |
| codex | gpt-5.3-codex | 68 | 0 | 1 | 1 | 70 | 97% |
| minimax | opus | 44 | 5 | 4 | 2 | 55 | 80% |
| kimi | sonnet | 7 | 2 | 0 | 0 | 9 | 78% |
| claude | opus | 11 | 2 | 0 | 1 | 14 | 79% |
| opencode | opencode/minimax-m2.5-free | 11 | 0 | 0 | 0 | 11 | 100% |
| opencode | github-copilot/gpt-5-mini | 2 | 0 | 0 | 0 | 2 | 100% |
| opencode | opencode/nemotron-3-super-free | 3 | 4 | 0 | 0 | 7 | 43% |

Notes:
- **kimi failures spike to 17** — directly attributed to billing cycle exhaustion. Cooldown now active.
- **codex/gpt-5.3-codex at 97%** — most reliable agent this window. No failures.
- **opencode/nemotron at 43%** — confirmed worst performer. 4 failures in 7 runs. Needs investigation or manual cooldown.
- **opencode/minimax-m2.5-free still at 100%** — silence detection fix (#2317) holding.
- **olm/gemma4**: cooldown is 0 (expired), failure_count is 0. Simply not being dispatched — likely intentionally excluded from routing for complex tasks.

### 12h Task Activity

| Event | Count |
|-------|-------|
| status_change | 1923 |
| dispatch | 601 |
| push | 397 |
| branch_delete | 366 |
| routed | 279 |
| review_start | 193 |
| review_decision | 188 |
| pr_create | 176 |
| error | 99 |
| rerouted | 57 |
| timeout | 9 |

Throughput continues to increase: 601 dispatches vs 506 yesterday (+19%). Error count up proportionally (99 vs 62), but the error/dispatch ratio (16.5%) is similar to yesterday (12.3%). Kimi billing exhaustion probably accounts for the uptick — its 17 failures are in this window.

---

## Retro Follow-ups (Apr 10)

| Priority | Status |
|----------|--------|
| Root-cause claude/sonnet timeouts | **Resolved** — all timeouts are legitimate 1800s hard timeouts, not silence detection. Not a bug. |
| Upgrade CLI/service | **Still open** — now day 4, 2 minor version gap (0.61.20 vs 0.63.0). Escalate urgency. |
| Investigate opencode/nemotron failures | **Deteriorating** — was 67% yesterday, now 43% (4/7). Needs action today. |
| Verify audit trail fixes in production | **Not confirmed** — spot-check `task_runs` for correct agent/error/attempts values. |
| olm/gemma4 status | **Confirmed inactive** — cooldown expired, no failures, simply not routed. Intentionally excluded or never configured. |

---

## Open Issues

**None.** All recent bugs are closed.

---

## Priorities for Today

1. **Upgrade CLI/service (day 4, now urgent)** — 2 minor version gap. `orch log`, and potentially other commands, may be incompatible.
   ```bash
   brew upgrade orch && brew services restart orch && orch version
   ```

2. **Investigate opencode/nemotron-3-super-free** — 43% success rate (4 failures in 7 runs). This is the lowest-performing model in the fleet. Either apply a manual cooldown or inspect the failure errors:
   ```bash
   sqlite3 ~/.orch/orch.db "SELECT error FROM task_runs WHERE agent='opencode' AND model='opencode/nemotron-3-super-free' AND outcome='failed' ORDER BY started_at DESC LIMIT 5;"
   ```

3. **Verify audit trail fixes** — spot-check `task_runs` records for recent tasks. Confirm `agent`, `error`, and `attempt` fields are correctly populated (fixes from #2394, #2393, #2392 landed yesterday).

4. **Monitor kimi recovery** — kimi comes back online ~19h from now. Watch for successful routing when the cooldown expires. The failure_count for kimi:haiku (14) is concerning — it will drive extended backoff when kimi returns. May need `orch cooldown clear kimi:haiku` after confirming billing is restored.

5. **Watch codex/gpt-5.3-codex** — 97% at 70 runs is exceptional. Route more complex tasks toward codex while kimi is out of rotation.
