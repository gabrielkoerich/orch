+++
title = "Evening Retrospective — 2026-04-28"
date = 2026-04-28
description = "Daily evening retrospective: parser and codex sandbox fixes shipped, one dead opencode model failure persists, queue mostly healthy with a single long-lived blocked issue."
+++

# Evening Retrospective — 2026-04-28

Two production bug fixes shipped today and reduced real operational pain: parser status normalization and codex workspace-write commit compatibility. Throughput was healthy, with failures concentrated in known edge cases rather than broad instability.

## What Was Accomplished

### Issues closed today

| Issue | Outcome | Why it mattered |
|------|---------|-----------------|
| #3022 | Closed | Normalized `no_trades_no_positions` so runs are marked `done` instead of failing parse. |
| #3021 | Closed | Fixed codex `workspace-write` worktree commits when git common dir is outside the worktree. |
| #3017 | Closed | Router validation hardened against dead opencode Copilot model alias usage. |

### Commits in the last 12 hours

- `f50bb99e` fix(codex): grant write access to git common dir in workspace-write sandbox
- `76b7350a` fix(parser): normalize no_trades_no_positions to done
- `c97513bb` fix(router): canonicalize dead opencode copilot model alias
- `2c43845b` docs: add morning review 2026-04-28

## Execution Quality (task_runs, last 12h)

Outcome totals:

- `success`: 36
- `failed`: 2
- `rate_limit`: 1
- `blocked`: 1
- `aborted` (graceful shutdown): 1
- `NULL outcome`: 1

Approximate success rate excluding in-flight/NULL rows: **~88% (36/41)**.

### Failure and retry patterns

1. **Known parser edge case (now fixed)**
   - One GLM run failed with `unrecognized status: no_trades_no_positions` before #3022 landed.
   - Subsequent retry succeeded after normalization.

2. **Dead opencode model still appears at runtime**
   - One failure on `opencode/github-copilot/gpt-5.3` (`model not found`).
   - This indicates runtime pools/config still occasionally surface dead identifiers despite today’s alias hardening.

3. **Rate limit event is isolated**
   - One `claude:sonnet` rate-limit outcome; no cascade detected.

4. **One blocked and one aborted run are non-systemic**
   - Blocked run was task-specific.
   - Aborted run came from graceful shutdown reset behavior, then succeeded on retry.

## Routing Accuracy

Routing was mostly accurate today:

- Heavy lanes (`codex:gpt-5.3-codex`, `minimax:opus`, `claude:sonnet`, `glm:opus`, `kimi:opus`) delivered high success counts.
- Retries generally converged to success quickly.
- The main routing miss remains dispatch attempts toward dead opencode Copilot IDs.

## Morning Plan vs. Actual

From the morning review priorities:

- Dead model cleanup: **partially improved** (alias canonicalization shipped) but still not fully eliminated at runtime.
- Routing-budget stall reduction: **no structural change landed today**.
- Unblocking long-lived blocked work: **no closure** for #2789.

## Open / Pending

- `#2789` remains open and blocked (GLM artifact collection).
- No new recurring failure pattern discovered that warrants a new bug issue today.

## Prompt/Workflow Observations

- Prompting quality appears stable: most runs returned parseable structured output.
- The parser/codex fixes from today materially reduced avoidable infra-style failures.
- Remaining reliability drag is model-availability hygiene and operational follow-through, not prompt format drift.

## Priorities for Tomorrow Morning Review

1. Verify dead opencode model IDs are fully removed from all active routing/model pools (not only alias canonicalization paths).
2. Re-check routing budget pressure and watchdog timing in fresh logs (`LLM routing budget exceeded`, slow ticks).
3. Close or re-scope #2789 with concrete artifact-capture completion criteria.
4. Confirm no recurrence of `no_trades_no_positions` parse failures post-#3022.

---

Prepared by Orch automation (internal task internal:148678).
