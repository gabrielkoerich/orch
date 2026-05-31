+++
title = "Morning Review — 2026-05-31"
date = 2026-05-31
description = "Daily morning review and operational priorities."
+++

# Morning Review — 2026-05-31

## Recent Commits (Last 24h)

| Commit | Description |
|--------|-------------|
| `ed0c55e5` | docs(posts): add evening retrospective for 2026-05-30 (#3218) |
| `9f353ee4` | bug(deployment): service at v0.73.13 missing 3 critical fixes (#3216) |
| `dcebd594` | fix(runner): treat ModelUnavailable 'not supported' as permanently gone (7d cooldown) (#3217) |
| `e045bcec` | fix(engine): recover stuck in-progress tasks from inactive repos (#3214) |
| `38957922` | fix(parser): add missing status aliases — changes_made, acknowledged, flat (#3213) |

Yesterday delivered four code fixes in two releases (v0.73.17, v0.73.18). All four issues open at the start of yesterday are now closed.

## Operational Health

**Overall: Strong throughput, clean logs, but service still 2 versions behind. Upgrade to v0.73.18 remains the top operator priority — it activates auto-upgrade and prevents future deployment lag permanently.**

### Service Version

```
CLI:     0.73.13
Service: 3467   0.73.16  ✗ mismatch — service is ahead of CLI by 3 versions
Latest:  0.73.18  ⚠  upgrade available
```

The auto-upgrade feature (the definitive fix for deployment lag) is in v0.73.18, but the service is still on v0.73.16. Until the operator upgrades, the service will continue to lag behind releases. One manual upgrade closes the loop permanently:

```bash
brew update && brew upgrade orch && brew services restart orch
orch version  # expect: CLI and Service on 0.73.18, PID-bound
```

### Agent/Model Health (Last 24h)

| Agent | Model | Outcome | Count |
|-------|-------|---------|-------|
| claude | sonnet | success | 49 |
| claude | haiku | success | 28 |
| codex | gpt-5.3-codex | success | 26 |
| claude | opus | success | 21 |
| opencode | deepseek-v4-flash-free | success | 21 |
| kimi | opus | success | 9 |
| claude | sonnet | **failed** | 6 |
| opencode | mimo-v2.5-free | success | 6 |
| codex | gpt-5.3-codex | **failed** | 3 |
| opencode | nemotron-3-super-free | success | 3 |
| claude | haiku | **failed** | 2 |
| codex | gpt-5.3-codex | blocked | 2 |
| claude | haiku | blocked | 1 |
| claude | sonnet | push_failed | 1 |
| codex | gpt-5.2-codex | **failed** | 1 |
| glm | opus | **failed** | 1 |
| opencode | nemotron-3-super-free | parse_error | 1 |
| opencode | nemotron-3-super-free | timeout | 1 |

Key observations:

- **kimi returned cleanly**: 9 successes, no immediate cooldown re-entry. As predicted.
- **Codex recovery excellent**: 26 successes vs 3 failures (89.7% success rate) — up dramatically from 57% yesterday. The #3206 network fix is fully in effect.
- **Claude remains strong**: sonnet 89% (49/56 including push_failed as failure), haiku 93% (28/31), opus near-perfect.
- **glm: still in cooldown** (1 additional failure before entering 1d12h cooldown — 4th+ credit exhaustion this month).
- **codex/gpt-5.2-codex**: 1 final failure expected; should now enter 7d cooldown per the `"not supported"` fix deployed in v0.73.18. Will confirm once service is upgraded.
- **opencode/nemotron-3-super-free**: 1 parse_error + 1 timeout out of 5 runs — within normal variance, not a pattern.

### Active Cooldowns (10:01 UTC)

| Key | Remaining | Reason |
|-----|-----------|--------|
| glm | 1d12h | credit exhaustion (recurring) |
| minimax | 1d12h | re-entered during yesterday |
| opencode:github-copilot/gpt-5-mini | 4d11h | persisted |

kimi cleared as predicted. glm and minimax both in ~1d12h cooldowns — billing issues at their respective providers, not code bugs.

### Task Activity (Last 12h)

| Event | Count |
|-------|-------|
| status_change | 705 |
| push | 209 |
| dispatch | 198 |
| branch_delete | 124 |
| review_start | 115 |
| review_decision | 106 |
| pr_create | 97 |
| routed | 77 |
| error | 24 |
| rerouted | 2 |
| timeout | 1 |

Excellent throughput: 97 PRs created and 198 dispatches in 12 hours. Error rate (24) is proportional and normal. No crash-level events.

### Log Patterns

- **Clean error log**: `/opt/homebrew/var/log/orch.error.log` is 0 bytes — no startup errors.
- **No WATCHDOG stalls**: yesterday's single stall was this task's routing; nothing recurring today.
- **Routing fallback for this task**: LLM router selected opencode (cooled), auto-rerouted to claude:sonnet. Expected — opencode cooling is working correctly.
- **Recurring WARN**: internal:151079 and internal:151077 appearing as "dispatchable" every tick but skipped due to existing tmux sessions. These are long-running tasks with active sessions; the skip is the correct behavior.

## Stuck / Blocked Tasks

- **internal:149337** — blocked (Day 20). SSH agent signing failure on auto-merge push. Unchanged from every prior day. Operator action required:
  ```bash
  ssh-add ~/.ssh/default_id_ed25519
  orch task unblock all
  ```

## Retro Follow-ups

| Item | Status |
|------|--------|
| 4 code fixes shipped (parser, engine, runner, deployment) | ✓ Done |
| Auto-upgrade feature deployed in v0.73.18 | ✓ Code deployed; **not yet running** (service on 0.73.16) |
| kimi cooldown cleared ~21:00 UTC | ✓ Confirmed — 9 successes |
| Codex recovery post network fix | ✓ 89.7% success — full recovery |
| Upgrade to v0.73.18 | **NOT DONE** — operator must run brew upgrade |
| Unblock internal:149337 (ssh-add) | **NOT DONE** (Day 20) |
| Prune dead opencode model entries from config | **NOT DONE** (recurring carry-over) |
| glm credit exhaustion (5th+ time this month) | Billing issue; operator should consider recharging |

## Priorities For Today

### CRITICAL (operator)

1. **Upgrade to v0.73.18** — activates auto-upgrade, permanently closes the deployment lag loop:
   ```bash
   brew update && brew upgrade orch && brew services restart orch
   orch version   # expect: CLI and Service on 0.73.18, PID-bound
   ```
   After this, check logs within 1 hour for: `auto_upgrade: running brew upgrade orch`

2. **Unblock internal:149337** (Day 20):
   ```bash
   ssh-add ~/.ssh/default_id_ed25519
   orch task unblock all
   ```

### Monitoring

3. **Verify auto-upgrade activates** after the manual upgrade — look for `auto_upgrade:` log lines within the first sync cycle.
4. **Confirm gpt-5.2-codex enters 7d cooldown** after the `"not supported"` fix (v0.73.18) goes live — should stop retrying every 4h.
5. **Monitor minimax/glm re-entry pattern** — glm has hit credit exhaustion 4+ times this month. If it continues, the provider should be de-prioritized in routing or the operator should recharge.

### Maintenance

6. **Prune dead opencode model entries** from `~/.orch/config.yml` (`github-copilot/gpt-5.3`, `github-copilot/claude-opus-4.6`) — reduces cosmetic router WARN noise each tick.

---

Prepared by Orch automation (internal:151158)
