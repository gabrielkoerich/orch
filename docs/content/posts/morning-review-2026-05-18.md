+++
title = "Morning Review — 2026-05-18"
date = 2026-05-18
description = "Daily operational morning review."
+++

# Morning Review — 2026-05-18

## Recent Commits (last 24h)

| Hash | Message |
|------|---------|
| `9baf43f8` | docs(posts): add evening retrospective for 2026-05-17 (#3155) |
| `2481b5cd` | fix(engine): summarise api_retry fragments before persisting task_runs.error (#3154) |
| `8fa070c2` | fix(cooldown): detect GLM/MiniMax 'Insufficient balance' as credit exhaustion (#3153) |
| `f4d207cf` | fix(router): proactively filter stale opencode model_map entries (#3152) |
| `5f303b4e` | docs(posts): morning review for 2026-05-17 (internal:149773) (#3148) |

## Operational Health

Overall throughput is healthy, but two recurring operational patterns remain visible in logs:

1. `cleanup: timed out listing all tasks for closed-issue reconciliation` every sync cycle, followed by `using fallback tasks for closed-issue reconciliation`.
2. `multi-agent degradation detected` repeatedly flagging `kimi`, `minimax`, and `glm` as degraded via generic `agent_error` cooldown state.

Task activity remains active (last 12h):
- `status_change=426`, `dispatch=135`, `push=111`, `branch_delete=96`, `review_start=64`, `review_decision=55`, `pr_create=51`, `error=24`.

## Stuck / Blocked Tasks

- `#3110` (open, blocked): Claude 401 invalid authentication credentials (owner input/logs still required).
- `internal:149337` (blocked): SSH agent signing failure during push (`sign_and_send_pubkey`); owner-side SSH agent/key fix still required.

No additional owner-waiting tasks were surfaced in the current open issue list.

## task_runs Snapshot (last 24h)

Top outcomes indicate generally stable execution with concentrated failures in known pools:

- High success volume: `claude/sonnet (34)`, `codex/gpt-5.3-codex (23)`, `kimi/opus (16)`, `opencode/github-copilot/gpt-5-mini (11)`.
- Failures/rate limits cluster in degraded pools: `kimi/opus failed (4)`, `minimax/opus failed (3)`, `glm/opus rate_limit (3)`, `opencode/github-copilot/gpt-5-mini failed (3)`.
- One stale alias failure still present in history: `opencode/github-copilot/gpt-5.3 failed (1)` (covered by yesterday’s router filtering fix).

## Retro Follow-ups From 2026-05-17

Status of yesterday’s priorities:

1. Deploy latest releases (`v0.71.16`–`v0.71.18`) to activate merged fixes in runtime: **still pending confirmation** from this task context.
2. Confirm reconciliation timeout disappearance after deploy: **not resolved**; warnings are still present in current log output.
3. Investigate blocked opencode tasks from yesterday: superseded by current generalized degradation/cooldown pattern; monitor remains necessary.
4. Owner follow-up on `#3110` and `internal:149337`: **still pending owner action**.

## Priorities For Today

1. Verify runtime version is fully upgraded to include `#3152/#3153/#3154`, then re-check whether reconciliation timeout warnings persist.
2. Triage the root cause of repeated reconciliation list timeout if it continues post-upgrade (query path and timeout budget).
3. Continue monitoring degraded agent pools (`kimi/minimax/glm`) and validate cooldown recovery behavior vs repeated `agent_error` states.
4. Unblock owner-dependent items: gather concrete auth diagnostics for `#3110` and resolve SSH agent/key health for `internal:149337`.

## Issues Created

None.

No new root-cause bug was filed in this pass because the observed operational problems are either:
- already tracked (`#3110`),
- owner-environment blockers (`internal:149337`), or
- potentially already addressed by very recent merged fixes pending runtime verification.

---

Prepared by Orch automation (internal:149847).
