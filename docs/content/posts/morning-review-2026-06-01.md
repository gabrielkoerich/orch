+++
title = "Morning Review — 2026-06-01"
date = 2026-06-01
description = "Daily morning review and operational priorities."
+++

# Morning Review — 2026-06-01

## Recent Commits (Last 24h)

| Commit | Description |
|--------|-------------|
| `4c5be3f8` | fix(service): evict ghost orch serve processes and detect stale runtime pids (#3226) |
| `f387989a` | docs(posts): add evening retrospective for 2026-06-01 (#3224) (#3225) |
| `5c50fe6e` | fix(runner): record model cooldown on review parse_error outcomes (#3224) |
| `574da836` | fix(parser): normalize MISSED and changes_addressed to done (#3223) |

Four commits landed overnight. Both issues flagged in yesterday's evening retrospective were resolved:

- **#3222 fixed** (`5c50fe6e`): Review runner now calls `record_model_failure(agent, model)` on `parse_error` outcomes. `opencode/nemotron-3-super-free` can no longer be retried indefinitely after a broken response format.
- **#3220 fixed** (`4c5be3f8`): `orch serve` now evicts ghost processes and detects stale runtime PIDs on startup. The structural cause of the ghost PID problem is addressed.

## Operational Health

**Overall: Excellent. Service on v0.73.21, all recent issues resolved, throughput strong. Kimi entered a 22h cooldown from failures — normal variance handled correctly by the system.**

### Service Version

```
CLI:     0.73.21
Service: 0.73.21  ✓ in sync
Latest:  0.73.21  ✓ up to date
```

Auto-upgrade ran again overnight: service went from v0.73.19 → v0.73.21. The auto-upgrade feature continues to work perfectly — zero operator intervention for the second consecutive upgrade cycle.

### Agent/Model Health (Last 24h)

| Agent | Model | Outcome | Count |
|-------|-------|---------|-------|
| claude | sonnet | success | 41 |
| codex | gpt-5.3-codex | success | 27 |
| claude | opus | success | 16 |
| kimi | opus | success | 11 |
| opencode | deepseek-v4-flash-free | success | 9 |
| claude | sonnet | failed | 4 |
| kimi | opus | failed | 4 |
| claude | sonnet | aborted | 3 |
| opencode | minimax-m3-free | success | 4 |
| opencode | mimo-v2.5-free | success | 4 |
| kimi | opus | aborted | 2 |
| claude | opus | aborted | 1 |
| codex | gpt-5.3-codex | parse_error | 1 |
| codex | gpt-5.3-codex | aborted | 1 |
| glm | opus | failed | 1 |
| minimax | opus | failed | 1 |
| opencode | mimo-v2.5-free | failed | 1 |
| opencode | nemotron-3-super-free | parse_error | 1 |
| opencode | nemotron-3-super-free | success | 1 |
| opencode | nemotron-3-super-free | timeout | 1 |

Key observations:

- **Claude: strong throughout** — sonnet 87% (41/47 excluding aborts), opus near-perfect (16/17). Aborts from graceful shutdown events, not failures.
- **Codex: solid** — 27 successes vs 1 parse_error (likely nemotron-adjacent). 96%+ effective success rate.
- **Kimi: degraded** — 4 failures in 13 runs (69% success) triggered a 22h40m cooldown. Now correctly out of rotation. System handled it correctly — affected tasks rerouted to claude/codex.
- **opencode/nemotron-3-super-free**: 1 parse_error + 1 timeout + 1 success. The #3222 fix is now live in v0.73.21. Expect this to enter cooldown after its next parse_error rather than cycling.
- **glm/minimax**: each 1 failure, both in recurring daily billing cycle cooldowns.

### Active Cooldowns (10:02 UTC)

| Key | Remaining | Reason |
|-----|-----------|--------|
| kimi | 22h40m | persisted (failures) |
| kimi:opus | 55m | persisted |
| glm | 1d11h | persisted (credit exhaustion) |
| glm:opus | 11h12m | persisted |
| minimax | 1d11h | persisted (credit exhaustion) |
| opencode:github-copilot/gpt-5-mini | 3d11h | persisted |

Kimi entered cooldown from provider-side failures — not a code bug. Both glm and minimax remain in their recurring daily billing cycle pattern (5th+ occurrence for glm this month).

### Task Activity (Last 12h)

| Event | Count |
|-------|-------|
| status_change | 461 |
| dispatch | 144 |
| push | 129 |
| branch_delete | 98 |
| review_start | 69 |
| review_decision | 64 |
| pr_create | 63 |
| routed | 59 |
| error | 22 |
| rerouted | 10 |
| timeout | 1 |

Good throughput: 63 PRs and 144 dispatches in 12 hours. 10 reroutes = expected given kimi/glm/minimax cooldowns. Error rate (22) proportional and normal.

### Log Patterns

- **Clean**: No crash-level events. No startup errors.
- **Recurring WARN** (every tick): `multi-agent degradation detected — kimi, minimax, glm cooled`. This is the correct behavior while these agents are in cooldown — not a bug.
- **WATCHDOG stall at 10:01 UTC**: Two watchdog alerts (70s, 100s) during this task's own initialization. Tick loop was blocked by task dispatch setup, not a real stall. Expected.
- **Routing reroutes**: LLM router selected opencode (cooled) for this task and internal:151258 — both auto-rerouted to claude correctly.

## Stuck / Blocked Tasks

- **internal:149337** — blocked (Day 21). SSH agent signing failure on auto-merge push. Unchanged.
  ```bash
  ssh-add ~/.ssh/default_id_ed25519
  orch task unblock all
  ```

No other stuck or blocked tasks.

## Retro Follow-ups

| Item | Status |
|------|--------|
| Fix #3222 — review parse_error cooldown | ✓ **Done** — `5c50fe6e` merged |
| Fix #3220 — ghost PID structural fix | ✓ **Done** — `4c5be3f8` merged |
| Upgrade to v0.73.19 (was pending) | ✓ **Done** — auto-upgraded to v0.73.21 |
| Unblock internal:149337 (ssh-add) | **NOT DONE** (Day 21) |
| Prune dead opencode model entries | **NOT DONE** (recurring carry-over) |
| Monitor glm/minimax re-entry frequency | Ongoing — both in cooldown again |

## Priorities For Today

### Operator

1. **Unblock internal:149337** (Day 21):
   ```bash
   ssh-add ~/.ssh/default_id_ed25519
   orch task unblock all
   ```

2. **Prune dead opencode model entries** from `~/.orch/config.yml` (carry-over 3rd day):
   - `github-copilot/gpt-5.3` — dead, in 7d cooldown
   - `github-copilot/claude-opus-4.6` — dead
   These produce router WARN noise each tick. Remove the entries.

### Monitoring

3. **Watch kimi recovery** — 22h cooldown (expires ~08:40 UTC tomorrow). Confirm kimi returns cleanly with no immediate re-failures. If it fails again on first re-entry, investigate provider stability.

4. **Monitor nemotron parse_error behavior under #3222 fix** — after the fix is active (v0.73.21), the model should enter cooldown on its next parse_error rather than looping. Verify no more than 1-2 additional parse_errors before it's locked out.

5. **glm/minimax billing cycle pattern** — both have now entered credit exhaustion 5+ times in June. If the pattern continues tomorrow, consider deprioritizing these agents in routing configuration (operator decision).

---

Prepared by Orch automation (internal:151260)
