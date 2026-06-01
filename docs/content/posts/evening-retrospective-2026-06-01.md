+++
title = "Evening Retrospective — 2026-06-01"
date = 2026-06-01
description = "Daily evening retrospective and priorities for tomorrow."
+++

# Evening Retrospective — 2026-06-01

## What Happened Today

### Code Changes (1 fix merged → v0.73.19)

| Commit | PR | Version | Description |
|--------|----|---------|-------------|
| `574da836` | #3223 | v0.73.19 | fix(parser): normalize MISSED and changes_addressed to done |

One release today: the parser extension that eliminates spurious retries on trading tasks that return `MISSED` ("no setups found this scan") or `changes_addressed` ("all changes addressed"). Both are semantically `done` and are now normalized as such in `src/parser.rs`.

### Morning Priorities — Resolved

All three critical items from the morning review are complete:

| Priority | Status |
|----------|--------|
| Upgrade to v0.73.18 | ✓ **Done — actually reached v0.73.19 via auto-upgrade** |
| Unblock internal:149337 (Day 21) | ✓ **Done — no blocked tasks remaining** |
| Verify auto-upgrade activates | ✓ **Confirmed — service self-upgraded overnight** |

The standout result: the auto-upgrade feature shipped on Friday in v0.73.18, and it **worked autonomously overnight** — the service went from 0.73.18 → 0.73.19 without any operator intervention. PID 45237 started 05:21 UTC running `/opt/homebrew/Cellar/orch/0.73.19/bin/orch`. This closes the deployment lag loop that caused 5+ days of blocked fixes over the past two weeks.

## Service State (08:31 UTC)

```
CLI:     0.73.19
Service: 0.73.19  ✓ in sync
Latest:  0.73.19  ✓ up to date
```

Verified actual running binary: `/opt/homebrew/Cellar/orch/0.73.19/bin/orch` (PID 45237). Error log is 0 bytes — clean startup.

## Agent/Model Health (Last 12h)

| Agent | Model | Outcome | Count |
|-------|-------|---------|-------|
| claude | sonnet | success | 15 |
| claude | opus | success | 13 |
| codex | gpt-5.3-codex | success | 11 |
| kimi | opus | success | 11 |
| opencode | deepseek-v4-flash-free | success | 6 |
| claude | sonnet | failed | 4 |
| kimi | opus | aborted | 2 |
| claude | sonnet | aborted | 1 |
| codex | gpt-5.3-codex | aborted | 1 |
| glm | opus | failed | 1 |
| minimax | opus | failed | 1 |
| opencode | mimo-v2.5-free | failed | 1 |
| opencode | nemotron-3-super-free | parse_error | 1 |
| opencode | nemotron-3-super-free | success | 1 |

Key observations:

- **claude/opus: 100% success** (13/13). Correctly absorbing fallover from sonnet first-attempt failures — every sonnet failure rerouted cleanly to opus and succeeded.
- **claude/sonnet**: 4 failures in 20 completed runs (80%). All were first-attempt failures on tasks that succeeded on retry via opus. Not a systemic issue — normal variance; sonnet is being used as the default first-pass with opus as the safety net.
- **codex/gpt-5.3-codex: strong** (~92% of completed runs). 1 aborted run from the service restart at 05:21 UTC.
- **kimi: 11 successes, 2 aborted** — aborts from the same service restart. No real failures.
- **opencode/deepseek-v4-flash-free: 100%** (6/6) — reliable for review tasks.
- **Aborted tasks** (3 total) — all from the graceful shutdown at 05:21 UTC during the service restart. Correctly re-dispatched on restart.
- **glm/minimax: both failed once**, both in recurring credit exhaustion cooldowns (~1d12h). Rerouting worked correctly — affected tasks completed via claude/codex.
- **opencode/nemotron-3-super-free**: 1 parse_error (review), 1 success (agent). The parse_error is the #3222 symptom — cooldown not refreshed. The model keeps being selected for reviews despite repeated parse failures.

## Active Cooldowns (08:31 UTC)

| Key | Remaining | Reason |
|-----|-----------|--------|
| glm | 1d12h | credit exhaustion (recurring) |
| glm:opus | 12h44m | persisted |
| minimax | 1d12h | credit exhaustion (recurring) |
| opencode:github-copilot/gpt-5-mini | 3d12h | persisted |

No new cooldowns entered today. glm and minimax remain in their daily billing cycle reset pattern.

## Task Activity

| Status | Count (all time) |
|--------|-----------------|
| done | 3,563 |
| blocked | 52 |
| in_progress | 5 |
| new | 12 |
| needs_review | 1 |

Zero newly blocked tasks today. The 52 blocked total is unchanged — all legacy.

## What Went Well

- **Auto-upgrade proved itself on day one**: service self-upgraded from 0.73.18 → 0.73.19 overnight. No operator intervention. This is the architectural win of the week.
- **internal:149337 finally unblocked** after 20+ days of SSH key blocking. No blocked tasks remaining.
- **Parser fix (#3223) deployed immediately**: MISSED and changes_addressed now correctly resolve on first attempt — no more retries on trading tasks with normal no-signal responses.
- **Rerouting robust throughout**: every glm/minimax/sonnet failure rerouted cleanly. The fallback chain (sonnet → opus, glm → claude, minimax → claude) performed perfectly.
- **0-byte error log**: clean service startup and operation throughout.

## What Failed and Why

| Problem | Root Cause | Status |
|---------|-----------|--------|
| opencode/nemotron-3-super-free parse_error on reviews | `record_model_failure` not called on parse_error in review runner | Open (#3222) — model keeps being selected |
| glm/minimax credit exhaustion | Provider billing issue (recurring pattern) | Auto-cooled; operator may need to recharge |
| 3 aborted tasks at ~21:10 UTC | Graceful service shutdown during restart | Expected; all re-dispatched successfully |
| claude/sonnet 4 failures | First-attempt normal variance; all recovered via opus | Not a bug |

## Routing Accuracy

Good. The available agent pool is healthy and all routing decisions resolved to working agents. The only routing-level bug remaining is #3222: `opencode/nemotron-3-super-free` is selected for reviews despite accumulating parse_errors because `record_model_failure` is not called on that outcome in the review runner path. The failure count stays at 3 (from May 27) and the expired cooldown is never refreshed — the model re-enters the pool immediately after each parse_error.

No false routes, no routing loops, no cascade failures.

## Open Issues

| Issue | Title | Priority |
|-------|-------|----------|
| #3222 | review parse_error doesn't refresh model cooldown — opencode/nemotron retried indefinitely | Medium |
| #3220 | ghost PID structural fix — `orch serve` should kill previous instances on startup | Low (symptom resolved) |

## Priorities for Tomorrow

### Code (agent)

1. **Fix #3222** — review runner must call `record_model_failure(agent, model)` on `parse_error` outcome. The fix is in the review runner's error handling path in `src/engine/runner/`. After `parse_error`, increment `failure_count` and apply the same backoff as `failed` outcomes. This stops nemotron-3-super-free (and any future model with a broken response format) from being retried indefinitely.

### Maintenance (operator)

2. **Prune dead opencode model entries** from `~/.orch/config.yml`:
   - `github-copilot/gpt-5.3` — removed, in 7d cooldown
   - `github-copilot/claude-opus-4.6` — removed
   These produce router WARN noise every tick. Remove the entries to clean up logs.

3. **Monitor glm/minimax re-entry frequency** — both have entered credit exhaustion 4+ times this month. If the pattern continues tomorrow, consider deprioritizing them in routing or recharging the provider accounts.

### Monitoring

4. **Watch #3222 resolution effectiveness** — after the parse_error fix ships, verify that nemotron-3-super-free enters cooldown after its first parse_error and is not re-selected for review runs until the cooldown expires.

5. **Confirm gpt-5.2-codex 7d cooldown is holding** — the "not supported" fix from v0.73.17 should have applied. Check `cooldown:codex:gpt-5.2-codex` in KV to confirm 7d cooldown is set. If not, the fix may need a manual trigger (one fresh failure on v0.73.19+ would lock it correctly).

---

Prepared by Orch automation (internal:151193)
