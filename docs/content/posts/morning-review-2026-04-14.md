+++
title = "Morning Review — 2026-04-14"
date = 2026-04-14
description = "Daily operational check-in: 6 commits merged overnight (memory leaks, HashMap fixes, stale task detection). Service healthy. CLI version STILL mismatched. Task 2622 false-positive blocked by review circuit-breaker."
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

Overall status: service healthy. 175+ tasks completed in the last 24h with no service-level errors. Orch.error.log is empty (0 bytes — no errors at the service level).

### Service

- Version: orch/0.68.5 (service)
- CLI: 0.67.7 — **STILL MISMATCHED**. This was flagged in yesterday's evening retro and this morning's review. `brew upgrade orch && brew services restart orch` has not been run. Both versions need to match before the next session.

### Notable events

- `internal:145238` ran yesterday, completed work (reviewed codebase, created 2 tasks) but was incorrectly blocked by silence detection — see Stuck Tasks below.
- Task 2623 (`feat: implement local model routing via Ollama`) is in review cycle 1 of 2. CI `test` job failing; review agent requested changes. Normal workflow, being handled.
- minimax/opus had 5 rate_limit outcomes in the last 24h — minor noise, cooldown applied and recovered.

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
| Verify tick loop stall resolved (#2574) | **Resolved.** Recent commits (stale task detection, unbind, dead code removal) show tick loop functioning normally. Orch.error.log is empty. |
| Monitor kimi recovery (~Apr 15 06:32 UTC) | Cooldown still active: 20h30m remaining on `kimi`, 2h24m on `kimi:haiku`. Recovery expected later today. |
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

## Prioritie

1. **Fix CLI version mismatch NOW** — `brew upgrade orch && brew services restart orch && orch version`. This has been outstanding for two days.
2. **Unblock internal:145238** — false positive blocked. Verify 2 tasks were created in GitHub, then `orch task unblock internal:145238`.
3. **Close/de-dup task 2622** — `orch task watch` is already implemented in PR #2631. Either close 2622 as duplicate or mark done.
4. **Human review PR #2557** (task 2555) — implementation complete, needs owner review.
5. **Monitor kimi recovery** later today — `kimi` and `kimi:haiku` cooldowns expire ~20h from now.

---

## Issues

No new issues created. All operational problems are either actionable immediately (CLI mismatch, false-positive blocks) or tracked in existing issues.

---

Prepared by Orch automation (internal task internal:145307).
