+++
title = "Evening Retrospective — 2026-05-17"
date = 2026-05-17
description = "Daily evening retrospective and operational summary."
+++

# Evening Retrospective — 2026-05-17

## Summary

Three reliability fixes landed today, all released by 21:45 UTC. Morning priority #1 (deploy v0.71.15 with the reconciliation fix) was completed — the running service is confirmed on v0.71.15 and the `cleanup: timed out listing all tasks` warnings visible in the pre-restart log are expected to be gone. However, three further releases (v0.71.16 → v0.71.18) cut today remain undeployed; the service needs one more upgrade to include today's work.

## What Was Accomplished

| Commit | PR | Description |
|--------|-----|-------------|
| `8fa070c2` | #3153 | `fix(cooldown)`: detect GLM/MiniMax `'Insufficient balance'` as `credit_exhaustion` — was previously falling through to generic failure, wasting exponential backoff cycles |
| `f4d207cf` | #3152 | `fix(router)`: proactively filter stale opencode `model_map` entries before routing — dead copilot model aliases no longer reach dispatch |
| `2481b5cd` | #3154 | `fix(engine)`: summarise `api_retry` fragments before persisting to `task_runs.error` — errors now contain human-readable text instead of raw JSON blobs |

All three merged and auto-released as v0.71.16, v0.71.17, v0.71.18 respectively. CI pipeline operated without issues.

Closed issues: #3149, #3150, #3151 (root causes for today's three fixes).

## What Failed, Retried, Or Needed Intervention

### 1) Deployment gap: service on v0.71.15, latest is v0.71.18

The service was restarted during the day to apply v0.71.15. Three subsequent releases (v0.71.16–v0.71.18) containing today's fixes are not yet running. The fixes are inert without deployment:

- GLM/MiniMax billing detour still takes the generic failure path (no `credit_exhaustion` cooldown) in the running binary.
- Stale copilot model aliases may still reach dispatch if they re-appear in the model pool before the per-model cooldown fires.
- `task_runs.error` may still contain raw `api_retry` JSON for new tasks until deployed.

Action required: `brew update && brew upgrade orch && orch service restart`.

### 2) Reconciliation timeout — expected to be resolved, unverifiable tonight

The v0.71.15 fix for `list_reconciliation_candidates()` should have eliminated the repeated 30-second cleanup timeouts. The current log (post-restart) is empty — the service restarted too recently to confirm. If the warning reappears in tomorrow's morning log, the fix did not take effect and requires investigation.

### 3) Carryover blocked tasks (no autonomous resolution path)

- `#3110` — Claude 401 "Invalid authentication credentials": still open, no owner-provided log excerpts. No automated action possible.
- `internal:149337` — SSH agent signing failure on `git push`: still blocked; owner fix required.
- `internal:149673`, `internal:149675`, `internal:149435` — recently blocked opencode tasks (model: `github-copilot/gpt-5-mini` and `github-copilot/claude-sonnet-4.6`). Worth checking `task_runs` for pattern.

### 4) LLM routing budget exceeded for this task

`internal:149810` (this retrospective) hit the 30s LLM budget and was routed via round-robin to `claude:sonnet` (index 0 of 4 agents). This is the correct fallback and the result is appropriate — no action needed.

## Routing Accuracy

Task-run outcomes for the last 24 hours:

| Agent | Model | Success | Failed | Other |
|-------|-------|---------|--------|-------|
| claude | sonnet | 24 | 2 | 1 |
| codex | gpt-5.3-codex | 23 | 0 | 2 |
| opencode | github-copilot/gpt-5-mini | 22 | 3 | 1 |
| kimi | opus | 20 | 3 | 0 |
| opencode | github-copilot/gpt-5.4 | 9 | 0 | 0 |
| glm | opus | 7 | 1 | 3 |
| opencode | github-copilot/claude-sonnet-4.6 | 6 | 0 | 1 |
| minimax | opus | 3 | 3 | 1 |
| opencode | github-copilot/gpt-5.3 | 0 | 1 | 0 |

Highlights:
- `kimi/opus` failures dropped sharply — 3 failed vs 69 failed yesterday. The `#3134` fix (false `parse_error` elimination) is holding.
- `opencode/github-copilot/gpt-5.3` shows only 1 failure and 0 successes — per-model cooldown is keeping this dead alias out of routing. Today's `#3152` fix will further harden this path.
- `glm/opus` shows 2 `parse_error` outcomes. The rate is low (2 out of 11 attempts) and within the generic failure/cooldown envelope. Not actionable yet.
- No dispatch storms, no watchdog stalls, no fallback-loop patterns.

## Performance / Bottlenecks

- Sync tick times remain healthy (1.4–1.7 s in the pre-restart log window).
- No `slow_tick` warnings in the last log before restart (contrast with 55s slow tick seen yesterday).
- Post-restart log is empty — baseline for tomorrow.

## Learnings Captured Today

- **GLM/MiniMax billing signals are vendor-specific** — `'Insufficient balance'` is the MiniMax string equivalent of OpenAI's `'credits'` exhaustion. The cooldown system is correctly generic; the gap was only in the detection layer (`parse_retry_at`). Pattern: when adding a new provider, audit its error strings against all `parse_*` functions before enabling it in routing.
- **Stale model aliases require proactive filtering, not just reactive cooldown** — the per-model cooldown prevents re-dispatch after a failure, but if a stale alias is still present in `model_map`, the first attempt still reaches the API. Proactive filtering at routing time (#3152) closes the window to zero.
- **`api_retry` fragment accumulation is a latent UI bug** — once a task enters retry loops, multiple raw JSON fragments accumulate in `task_runs.error`, making post-mortem diagnosis difficult. The fix (summarise before persist) is the right layer — the retry loop stays unchanged.

## Priorities For Tomorrow (Morning Review)

1. **Deploy v0.71.18** (`brew update && brew upgrade orch && orch service restart`) — apply the three fixes from today. Verify by checking `task_runs.error` for newly-clean error messages and confirming `glm`/`minimax` billing failures now log as `credit_exhaustion`.
2. **Confirm reconciliation timeout is gone** — check morning log for any `cleanup: timed out listing all tasks` entries. If present post-restart, the v0.71.15 fix did not apply and needs investigation.
3. **Investigate recently blocked opencode tasks** (`internal:149673`, `149675`, `149435`) — check `task_runs` for the blocking error and confirm whether per-model cooldowns triggered correctly or if these are novel failure modes.
4. **Push owners on carryovers** — #3110 (Claude 401 log excerpts), `internal:149337` (SSH agent fix). Neither has an autonomous resolution path; owner action is the only unblock.

## Issues Created

None tonight.

The three discovered problems from code review (#3149, #3150, #3151) were already filed and closed. The remaining open issue (#3110) is a carryover with no new information. No new systemic failures observed today.

---

Prepared by Orch automation (internal:149810).
