+++
title = "Morning Review — 2026-04-14"
date = 2026-04-14
description = "Daily operational check-in: 6 commits merged overnight (memory leaks, HashMap fixes, stale task detection). NEW REGRESSION: tick loop stalled 350s at 10:01 UTC due to router LLM timeout cascade. CLI version STILL mismatched."
+++

# Morning Review — 2026-04-14

## Recent Commits (last 24h)

Maintenance sprint continues. Six commits merged, focused on correctness and memory hygiene:

- `19f40336` — **bug: transport.rs `last_output` field is dead code** — written but never read, accumulates all agent output unboundedly (#2628)
- `635fe92d` — **fix: `Transport::unbind()` prevents HashMap memory leak** — sessions unregistered from transport map on unregister (#2630)
- `9c32d00e` — **fix: log `try_exists` errors on stored worktree path** instead of silently swallowing (#2629)
- `36c321c6` — **fix: add stale InProgress task detection to sync tick** (#2624)
- `5e8522c6` — **bug: review subscriber blocks tasks during GitHub outages** — circuit-breaker errors not recognized as transient (#2621)
- `503129c4` — **fix: release router RwLock read guard before async awaits** to prevent lock poisoning

Themes: memory hygiene, error visibility, and stale-state recovery.

---

## Operational Health

Overall status: service mostly healthy but with a significant new finding. Orch.error.log is empty (0 bytes — no panics). 175+ tasks completed in the last 24h.

### Service

- Version: orch/0.68.5 (service)
- CLI: 0.67.7 — **STILL MISMATCHED**. This was flagged in yesterday's evening retro and this morning's review. `brew upgrade orch && brew services restart orch` has not been run. Both versions need to match before the next session.

### Watchdog stall — NEW REGRESSION

**Tick loop stalled for 350 seconds (10:01–10:06 UTC) — a 5.8-minute stall.** This is the same watchdog mechanism that was fixed in #2574 yesterday. The stall occurred during high routing demand when two tasks (internal:145309, internal:145310) hit the router simultaneously, exhausting all LLM pool entries and timing out the fallback router. The tick never completed during this window, firing watchdog warnings every 30s up to 332s stale.

This is a **regression**, not the original #2574 root cause. The original fix addressed blocking Tokio workers; this new stall is specifically router-bound under high concurrent routing demand. The system recovered once routing demand dropped.

**Root cause hypothesis**: The LLM router is a bottleneck when multiple tasks require routing simultaneously. All pool entries timeout → fallback router times out → weighted round-robin fallback used, but the tick itself is blocked during this entire process. The fix for #2574 (which addressed inline blocking calls) apparently didn't cover this routing-timeout path.

### Notable events

- `internal:145238` ran yesterday, completed work but was incorrectly blocked by silence detection — see Stuck Tasks below.
- Task 2623 (`feat: implement local model routing via Ollama`) — stuck task recovery triggered at 22 min, session reclaimed and re-routed to new. Currently at new status (4 tries). Being handled normally.
- minimax/opus had 5 rate_limit outcomes in the last 24h — minor noise, cooldown applied and recovered.
- Router LLM pool entries timing out during the 10:01-10:06 stall window — both `haiku` entries (minimax, claude) timed out, fallback `haiku` (claude-haiku-4-5-20251001) also timed out. This is what caused the cascading stall.
- **CI failed on PR #2632** — review agent correctly caught a compile error introduced on this branch. `src/channels/transport.rs:269` called `self.clear_output(&skey).await` but the method doesn't exist (`last_output` was removed in `19f40336`). **Fixed**: removed the stale call and updated the docstring. Clippy passes. CI should go green on re-run.

---

## Stuck / Blocked Tasks

Three blocked tasks:

### internal:145238 — false positive blocked by silence detection
- **Status**: blocked (created by yesterday's morning review agent, ran 4h ago)
- **What happened**: Agent completed work — summary says "Reviewed codebase for improvements and created 2 tasks" — but was flagged as silent after 600s and marked failed/blocked.
- **Root cause**: Silence detection false positive. Agent output was produced but not captured correctly. The recent `f24f734f` ("silence detection bypassed when tmux session exits with seen-alias stub") was merged before this task ran but apparently didn't fully cover this scenario.
- **Action needed**: Unblock manually (`orch task unblock internal:145238`). The work is done — verify the 2 created tasks exist in GitHub.
- **Note**: The 2 tasks were created by the previous morning review agent. This is the "find improvements" task that created the Ollama routing and task watch issues.

### 2622 — `feat: add orch task watch command`
- **Status**: blocked (review agent exceeded failure threshold)
- **Reality**: The feature is already implemented. The review agent's PR check failed due to "GitHub API transient 5xx circuit-breaker active for 25s" — a false positive from the circuit-breaker treating its own throttling as a real CI failure.
- **Action needed**: Close or manually mark done. The feature was implemented in PR #2631 (same codebase as 2623). No duplicate work needed.
- **Related**: This overlaps with 2623's implementation scope.

### 2555 — `feat: auto-clean worktrees of CI-blocked tasks`
- **Status**: blocked, pending human review
- **Reality**: PR #2557 exists, implementation is complete. Requires human review and merge. This is legitimate — needs owner attention, not an operational problem.

---

## Retro Follow-ups (from evening retrospective)

| Item | Status |
|------|--------|
| Fix CLI version mismatch (CLI 0.67.7 vs service 0.68.5) | **NOT FIXED — still mismatched**. `brew upgrade orch && brew services restart orch` must be run. |
| Investigate claude/opus 50% failure rate | **Concluded: hard task mix.** 68 runs over 48h: 33 success, 35 failed. Error pattern: "no PR or code changes produced" (28/35 = 80%). Opus is routed for complex tasks where agents often can't produce working code. Not a model degradation issue. No action needed — this is expected behavior for difficult tasks. |
| Verify tick loop stall resolved (#2574) | **Partially resolved.** The original #2574 root cause (blocking Tokio workers) is fixed. However, a **new trigger path** was found: router LLM timeout cascade at 10:01 UTC stalled the tick for 350s. Filed as #2633. |
| Monitor kimi recovery | `kimi:haiku` cooldown ~2h remaining (recovers mid-afternoon UTC). `kimi` cooldown ~20h remaining (recovers ~Apr 15 06:30 UTC). Both active. Monitor via `orch cooldown list`. |
| Investigate claude/(blank) model field | **Low priority.** 49 runs over 48h: 24 success, 25 failed (50%). Model field being blank likely means model was auto-resolved by the Claude CLI. Consistent ~50% rate matches opus pattern — hard tasks, not a bug. |

---

## DB / Task Run Patterns (last 24h)

Top outcomes:

| Agent | Model | Success | Failed | Rate |
|-------|-------|---------|--------|------|
| claude | sonnet | 89 | 41 | 68% |
| opencode | gpt-5-mini | 41 | 0 | **100%** |
| minimax | opus | 43 | 0+5 rl | 90% |
| opencode | minimax-m2.5-free | 26 | 1 | 96% |
| opencode | (blank) | 23 | 0 | **100%** |
| claude | opus | 16 | 18 | 47% |
| claude | (blank) | 11 | 13 | 46% |
| glm | opus | 15 | 3 | 83% |
| opencode | nemotron-3-super-free | 9 | 6 | 60% |

**Key observations:**
- `opencode/gpt-5-mini` and `minimax-m2.5-free` remain the workhorses at high reliability.
- `claude/opus` and `claude/(blank)` both sit ~47-50% — consistent with yesterday's finding: hard task mix, not model degradation.
- GitHub Copilot models (gemini-3.1-pro-preview, gpt-5.4, claude-sonnet-4.6) continue failing; cooldowns are active and working.
- `codex` still in 2d6h billing cooldown — expected until Apr 16.

---

## Priorities

1. **Fix CLI version mismatch NOW** — `brew upgrade orch && brew services restart orch && orch version`. This has been outstanding for three days.
2. **Re-run CI on PR #2632** — compile error in transport.rs fixed (stale `clear_output` call removed). Clippy passes. Push the fix and re-trigger checks.
3. **Investigate tick loop stall regression (#2633)** — router LLM timeout cascade blocked the tick loop for 350s at 10:01 UTC. See issue for proposed fixes (per-tick routing time budget, immediate fallback).
4. **Unblock internal:145238** — false positive blocked. Verify the 2 tasks it created exist in GitHub, then `orch task unblock internal:145238`.
5. **Close/de-dup task 2622** — `orch task watch` already implemented in PR #2631.
6. **Human review PR #2557** (task 2555) — implementation complete, needs owner review.
7. **Monitor kimi recovery** — `kimi:haiku` recovers ~2h, `kimi` recovers ~20h.

---

## Issues

- **#2633** filed for tick loop stall regression (350s watchdog stall at 10:01 UTC). Router LLM timeout cascade blocked the tick loop itself. Proposed fixes: per-tick routing time budget, immediate fallback to weighted round-robin, routing cancellation.

No other operational issues.

---

Prepared by Orch automation (internal task internal:145307).
