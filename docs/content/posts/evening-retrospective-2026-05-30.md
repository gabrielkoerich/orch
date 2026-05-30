+++
title = "Evening Retrospective — 2026-05-30"
date = 2026-05-30
description = "Daily evening retrospective and priorities for tomorrow."
+++

# Evening Retrospective — 2026-05-30

## What Happened Today

### Code Changes (4 fixes merged → v0.73.17 and v0.73.18)

| Commit | PR | Version | Description |
|--------|----|---------|-------------|
| `38957922` | #3213 | v0.73.17 | fix(parser): add missing status aliases — changes_made, acknowledged, flat |
| `e045bcec` | #3214 | v0.73.17 | fix(engine): recover stuck in-progress tasks from inactive repos |
| `dcebd594` | #3217 | v0.73.18 | fix(runner): treat ModelUnavailable 'not supported' as permanently gone (7d cooldown) |
| `9f353ee4` | #3216 | v0.73.18 | feat(engine): auto-upgrade service via brew when newer release detected |

Two releases shipped back-to-back this evening. All four issues that were open this morning are now closed.

### Highlight: Auto-Upgrade Feature (#3216)

The most consequential fix today is not a bug fix — it's the auto-upgrade feature that eliminates the deployment lag that has been the recurring root cause for the past week. When `engine.auto_upgrade: true` (default), the hourly version check now runs `brew upgrade orch` and sends SIGTERM to self so launchd restarts onto the new binary automatically. No operator intervention required.

This closes the loop on a pattern that's caused 5+ days of critical bugs sitting undeployed: #3200 (false version reporting), #3205 (codex network blocked), #3204 (ThinkingBlockConflict unclassified), and #3197 (arg-parse masking) all spent 1–5 days deployed in code but not running. The auto-upgrade feature, once the operator upgrades to v0.73.18 and restarts, will prevent this class of problem permanently.

### Fix: Status Aliases (PR #3213, closes #3210)

`changes_made`, `acknowledged`, and `flat` are now normalized to `done`. Singular/common variants (`change_made`, `no_action`, `no_action_needed`) added preemptively. These three aliases caused 5 wasted runs in the past 7 days. The fix eliminates unnecessary retries for agents that use domain language in their status field.

### Fix: Zombie Tasks from Inactive Repos (PR #3214, closes #3212)

`tick_recover_stuck_tasks` was project-scoped — it only evaluated tasks for the repo currently being ticked. Tasks from projects removed from the active tick loop (e.g., `gabrielkoerich/oblivion`) were stuck in `in_progress` indefinitely with no recovery path. Tasks 140644 and 140647 from `oblivion` had been stuck since April 13, 2026 (47 days). The fix adds a global scan for orphaned in-progress tasks on every tick.

### Fix: ModelUnavailable 'not supported' → 7d Cooldown (PR #3217, closes #3215)

`is_permanently_gone` only matched `"not found"`. The string `"not supported"` (emitted when an account type mismatches a model, e.g., `gpt-5.2-codex` on a ChatGPT plan) fell through to the transient 5min→4h backoff. `gpt-5.2-codex` accumulated 6 failed runs over 40 days, retrying every ~4 hours. Now routes to `record_model_permanently_gone` (4h→7d) just like `"not found"`.

## Service State (23:01 UTC)

```
CLI:     0.73.13
Service: 3467   0.73.16  ✗ mismatch — service needs upgrade
Latest:  0.73.18  ⚠  upgrade available
```

The auto-upgrade feature is deployed in v0.73.18, but the service is still on v0.73.16. The operator needs to do one manual upgrade to get onto v0.73.18, after which the service will self-maintain:

```bash
brew update && brew upgrade orch && brew services restart orch
orch version  # expect: CLI and Service on 0.73.18, PID-bound
```

## Agent/Model Health (Last 12h)

| Agent | Model | Outcome | Count |
|-------|-------|---------|-------|
| claude | sonnet | success | 41 |
| claude | haiku | success | 28 |
| codex | gpt-5.3-codex | success | 18 |
| opencode | deepseek-v4-flash-free | success | 16 |
| claude | opus | success | 12 |
| opencode | mimo-v2.5-free | success | 6 |
| kimi | opus | success | 3 |
| claude | sonnet | **failed** | 5 |
| codex | gpt-5.3-codex | **failed** | 3 |
| claude | haiku | **failed** | 2 |
| codex | gpt-5.3-codex | blocked | 2 |
| glm | opus | **failed** | 1 |
| codex | gpt-5.2-codex | **failed** | 1 |

Key observations:

- **kimi returned** (3 successes) — cleared from cooldown as predicted at ~21:00 UTC.
- **claude remains strong**: sonnet 89% (41/46), haiku 90% (28/31), opus near-perfect.
- **codex recovery continuing**: 18 successes vs. 3 failures. Remaining failures are network-dependent jobs (Hyperliquid sync, Twitter trending) that appear to be pre-fix dispatches — the `#3206` fix is deployed since v0.73.15. New dispatches should work.
- **glm:opus credit exhaustion** (1 failure, 429 `Insufficient balance`) — glm re-entered a 1d23h cooldown. This is a provider billing issue, not a code bug.
- **gpt-5.2-codex: 1 final failure** before the "not supported" fix kicks in — will now get 7d cooldown from next failure.
- **`funding_rate` unrecognized status** (1 haiku failure) — not a normalizable alias; agent is emitting domain analysis as status. Needs prompt-level fix, not a parser fix.

## Active Cooldowns (23:01 UTC)

| Key | Remaining | Reason |
|-----|-----------|--------|
| glm | 1d23h | credit exhaustion (recurring) |
| glm:opus | 3h3m | persisted |
| minimax | 1d23h | re-entered cooldown during day |
| opencode:github-copilot/gpt-5-mini | 4d22h | persisted |

The morning expected kimi/minimax/glm to clear at ~21:00 UTC. kimi cleared correctly. minimax and glm re-entered cooldown during the day — both are credit/billing issues with their respective providers. These are recurring patterns for glm (4th time this month) and minimax.

## What Went Well

- Four real fixes merged and shipped in two releases in a single day — all four morning open issues resolved.
- The auto-upgrade feature is the right architectural solution: it turns a recurring manual-intervention problem into a zero-touch operational baseline.
- kimi returned cleanly without immediately re-entering cooldown.
- Zombie task recovery from inactive repos closes a 47-day-old stuck state.
- The "not supported" cooldown fix stops gpt-5.2-codex from wasting dispatches indefinitely.
- Zero open GitHub issues entering tonight.

## What Failed and Why

| Problem | Root Cause | Status |
|---------|-----------|--------|
| minimax re-entered cooldown | Credit/billing issue at provider | Auto-clears; recurring — track frequency |
| glm re-entered cooldown | `Insufficient balance` (429) — 4th time this month | Recurring billing issue; operator may need to recharge |
| Codex network jobs still failing | Pre-fix dispatches + possibly some full-auto mode jobs | Monitor new dispatches post-0.73.16; may need codex sandbox config check |
| `funding_rate` unrecognized status | Agent (haiku) used domain term as status | Prompt tuning needed; not a parser issue |
| internal:149337 still blocked | SSH key not loaded — Day 20 | **OPERATOR: ssh-add required** |
| CLI at 0.73.13, service at 0.73.16 | Auto-upgrade not yet deployed | Operator: one manual upgrade |

## Routing Accuracy

Routing is functioning correctly. The available pool is healthy for claude and opencode. The auto-upgrade feature, once deployed, will keep routing working against new model fixes automatically. The `is_permanently_gone` extension means future "not supported" model errors will correctly cool for 7 days rather than retrying every 4h.

No routing-level bugs identified today.

## Priorities for Tomorrow

### CRITICAL (operator)

1. **Upgrade CLI and service to v0.73.18** — activates auto-upgrade, closes the deployment lag permanently:
   ```bash
   brew update && brew upgrade orch && brew services restart orch
   orch version   # expect: CLI and Service on 0.73.18, PID-bound
   ```

2. **Unblock internal:149337** (Day 20):
   ```bash
   ssh-add ~/.ssh/default_id_ed25519
   orch task unblock all
   ```

### Monitoring

3. **Verify codex network access for new dispatches** — confirm Hyperliquid sync and Twitter trending jobs succeed on new dispatches post-0.73.16. A continuing failure warrants checking the codex sandbox mode setting for these jobs.

4. **Watch minimax/glm cooldown re-entry pattern** — glm has entered credit exhaustion 4 times this month. If it continues daily, the provider should be deprioritized in routing or the operator should recharge the account.

5. **`funding_rate` status from haiku** — a single occurrence, but worth watching. If it recurs, the agent prompt for that task type needs to be explicit: "respond with status: done, no_changes_needed, etc." rather than letting the agent put domain output in the status field.

### Maintenance

6. **Prune dead opencode model entries** from `~/.orch/config.yml` (`github-copilot/gpt-5.3`, `github-copilot/claude-opus-4.6`) — reduces router WARN noise every tick.

7. **Verify auto-upgrade activates** after the manual upgrade to v0.73.18 — check logs for `auto_upgrade: running brew upgrade orch` message within the first hour of operation.

---

Prepared by Orch automation (internal:151123)
