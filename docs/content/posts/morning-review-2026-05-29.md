+++
title = "Morning Review — 2026-05-29"
date = 2026-05-29
description = "Daily morning review and operational priorities."
+++

# Morning Review — 2026-05-29

## Recent Commits (Last 24h)

| Commit | Description |
|--------|-------------|
| `e1a571d2` | docs(posts): add evening retrospective for 2026-05-28 (#3201) |
| `ea2c4662` | docs(posts): add morning review for 2026-05-28 (#3199) |

No code changes landed in the last 24 hours. The last real code fix was `6ac8f851` (fix(runner): reject CLI parser diagnostics — part of v0.73.14), which remains undeployed because the service has not been restarted.

## Operational Health

**Overall: Degraded. The service is still running 0.73.8 (Day 5 of no restart). Codex failure rate has worsened to ~86%. Claude and opencode are carrying all productive work. Multi-agent degradation (kimi/minimax/glm) continues with ~1d10h remaining on cooldowns. Throughput is up from yesterday despite the degraded pool.**

### Service Version

```
CLI:      0.73.13
Service:  0.73.13  ✓ in sync  ← FALSE REPORT (issue #3200)
Latest:   0.73.14  ⚠  upgrade available

Running binary: /opt/homebrew/Cellar/orch/0.73.8/bin/orch (PID 84871)
```

**The engine is still running 0.73.8.** `orch version` falsely reports "in sync" — this is the known bug (#3200). The `lsof` confirms it: PID 84871 maps to `Cellar/orch/0.73.8/bin/orch`. Both #3190 (remove codex `--ask-for-approval`) and #3198 (reject CLI diagnostics in synthesize) are **not** running in production. A single `brew services restart orch` fixes everything.

### Agent/Model Health (Last 24h)

| Agent | Model | Outcome | Count |
|-------|-------|---------|-------|
| claude | opus | success | 27 |
| claude | sonnet | success | 22 |
| opencode | mimo-v2.5-free | success | 15 |
| opencode | deepseek-v4-flash-free | success | 13 |
| codex | gpt-5.3-codex | **failed** | **18** |
| codex | gpt-5.4 | success | 8 |
| opencode | nemotron-3-super-free | success | 4 |
| claude | opus | **failed** | **4** |
| opencode | github-copilot/gpt-5-mini | **failed** | 5 |
| glm | opus | rate_limit | 2 |
| codex | gpt-5.3-codex | success | 3 |
| codex | gpt-5.3-codex | rate_limit | 1 |
| minimax | opus | rate_limit | 1 |

Key observations:

- **Codex gpt-5.3-codex failure rate: ~86%** (18 failed / 21 total). Day 5 of degradation. Root cause remains the running 0.73.8 engine emitting `--ask-for-approval` — a flag codex 0.133.0+ rejects with a clap error. Fix is deployed in the installed binary but will only take effect on service restart.
- **Claude remains the primary workhorse**: opus 87% success (27/31), sonnet near-perfect. The 4 opus failures are new — monitor but not alarming at current rates.
- **opencode/deepseek-v4-flash-free recovered** — 13 successes overnight, exactly as the retro predicted (was in ~58m cooldown at 23:02 UTC).
- **opencode/mimo-v2.5-free** continues strong at 15 successes — now the second most active model.
- **opencode/github-copilot/gpt-5-mini** in a 6d11h cooldown — the 5 failed runs predate that cooldown.

### Active Cooldowns

| Key | Remaining | Reason |
|-----|-----------|--------|
| glm | 1d 10h | agent_error |
| kimi | 1d 10h | billing_cycle_exhausted |
| minimax | 1d 10h | agent_error |
| opencode:github-copilot/gpt-5-mini | 6d 11h | persisted |

The kimi/minimax/glm triple degradation continues. All three expire roughly 20:00 UTC tonight. opencode:deepseek-v4-flash-free has cleared (no longer listed) — routing pool is recovering.

### Task Activity (Last 12h)

| Event | Count |
|-------|-------|
| status_change | 445 |
| dispatch | 128 |
| branch_delete | 124 |
| push | 96 |
| review_start | 64 |
| routed | 62 |
| review_decision | 40 |
| pr_create | 39 |
| error | 33 |
| rerouted | 3 |

Throughput improved versus yesterday (445 vs 328 status_changes). 39 PRs created with 128 dispatches — productive day driven by claude and opencode. The error count (33) is slightly elevated but proportional to volume; no crash-level errors in logs. Error log is 0 bytes (clean service run).

### Log Patterns

- Recurring `WARN multi-agent degradation detected` every tick: kimi/minimax/glm — expected, benign, cosmetic noise.
- One transient `WARN HTTP send failed, will retry` from GitHub API — auto-recovered, not a pattern.
- No WATCHDOG fires, no panics, no critical errors.

## Stuck / Blocked Tasks

- **internal:149337** — blocked (18 days). SSH agent signing failure on auto-merge push: `sign_and_send_pubkey: signing failed for ED25519 "/Users/gb/.ssh/default_id_ed25519.pub"`. Operator action required:
  ```bash
  ssh-add ~/.ssh/default_id_ed25519
  orch task unblock all
  ```

## Retro Follow-ups

| Item | Status |
|------|--------|
| Service restart / upgrade to 0.73.13+ | **NOT DONE** (Day 5) — CRITICAL |
| Upgrade to 0.73.14 | **NOT DONE** |
| Unblock internal:149337 (ssh-add) | **NOT DONE** (Day 18) |
| Prune dead opencode model entries from config | **NOT DONE** |
| Verify WATCHDOG stalls don't recur | ✓ No stalls today |
| opencode/deepseek-v4-flash-free recovery | ✓ Recovered — 13 successes |
| Minimax investigation after recovery | kimi/minimax/glm all still cooled; re-check tonight |

## Priorities For Today

### CRITICAL (operator — blocks codex entirely)

1. **Restart the service and upgrade**:
   ```bash
   orch service restart                              # loads installed 0.73.13 (fixes #3190, #3198)
   brew update && brew upgrade orch                  # gets 0.73.14
   brew services restart orch                        # deploys 0.73.14
   lsof -p $(pgrep -f 'orch serve' | head -1) | grep -i 'txt.*Cellar/orch'  # verify real binary
   ```
   This is Day 5. Every codex task run is either silently faked or erroring. The fix is two commands.

2. **Unblock internal:149337** (Day 18):
   ```bash
   ssh-add ~/.ssh/default_id_ed25519
   orch task unblock all
   ```

### Monitoring (once service restarts)

3. **Verify codex recovery** — post-restart runs should show non-zero cost, runtime >30s, real work product.
4. **Watch claude/opus failures** — 4 failures today is new. If it persists post-restart, investigate failure mode.
5. **kimi/minimax/glm cooldown expiry tonight (~20:00 UTC)** — confirm they re-enter the routing pool cleanly without immediately re-entering cooldown.
6. **Prune dead opencode model entries** from `~/.orch/config.yml` (`github-copilot/gpt-5.3`, `github-copilot/claude-opus-4.6`) — reduces router WARN noise every tick.

### Open Code Issues

- **#3200** — `orch version` false "✓ in sync": fix should PID-bind the version file or query the live engine. Still open, not yet assigned.

---

Prepared by Orch automation (internal:150845)
