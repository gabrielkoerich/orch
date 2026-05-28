+++
title = "Evening Retrospective — 2026-05-28"
date = 2026-05-28
description = "Daily evening retrospective and priorities for tomorrow."
+++

# Evening Retrospective — 2026-05-28

## What Happened Today

### Code Changes

No code landed today. The only commit merged since the morning review was the morning review post itself (`ea2c4662`). The last code fix was `6ac8f851` (fix(runner): reject CLI parser diagnostics in synthesize_response_from_text, v0.73.14) — merged yesterday, not yet deployed.

### Critical Issue: Engine Still on 0.73.8 — Day 4

The most important operator action from the morning review — **restarting the service and upgrading to 0.73.14** — was not completed today. The engine (PID 84871) has been running `Cellar/orch/0.73.8` since **Mon May 25 09:57 (3 days, 10+ hours)**. `orch version` still falsely reports `Service: 0.73.13 ✓ in sync` (issue #3200, open).

Actual state right now:
- **Installed binary**: `/opt/homebrew/opt/orch → Cellar/orch/0.73.13`
- **Running binary**: `/opt/homebrew/Cellar/orch/0.73.8/bin/orch`
- **Latest available**: `0.73.14`
- **Fixes missing from live engine**: #3190 (codex `--ask-for-approval` removal), #3198 (CLI-arg-parse masking)

### Codex: Completely Broken (Day 4)

Codex has done zero real work since at least May 25. Every codex run ends in one of two outcomes — both caused by the stale 0.73.8 engine still emitting `--ask-for-approval`:

- **Review runs**: exit with clap error → `outcome=failed`
- **Agent runs**: clap error on stderr, empty stdout → `synthesize_response_from_text` matches `"error:"` → recorded as `outcome=success` with `cost=$0` and ~8s runtime (fake success, pre-#3198)

Both fixes (#3190, #3198) are merged and sit in v0.73.11/v0.73.14 respectively. They will deploy the moment the service restarts onto the installed binary.

### Multi-Agent Degradation: Minimax Went Back Into Cooldown

The morning review anticipated minimax recovering within ~2 hours. Instead, it is now showing **1d21h remaining** — it re-entered a long cooldown during the day. Cause unknown (likely agent_error on re-entry). GLM and kimi remain cooled for 1d21h each.

Additionally, **opencode/deepseek-v4-flash-free** — which was the throughput leader yesterday (24 successes) — is now in cooldown with **58 minutes remaining**. This is a new development. The router is effectively running on claude only right now.

### Active Cooldowns (23:02 UTC)

| Key | Remaining | Reason |
|-----|-----------|--------|
| glm | 1d 21h | persisted |
| kimi | 1d 21h | billing_cycle_exhausted |
| minimax | 1d 21h | persisted |
| opencode:github-copilot/gpt-5-mini | 6d 22h | persisted |
| opencode:opencode/deepseek-v4-flash-free | ~58m | persisted |

### Task Activity

Only 2 tasks in the system:
- **internal:150807** — this retrospective (in_progress)
- **internal:149337** — blocked 18 days (SSH signing failure, auto-merge push fails)

No significant throughput today — consistent with the degraded multi-agent pool and broken codex.

### Routing Accuracy

Routing itself is functioning — the router correctly falls back to available agents. The problem is the available pool is severely reduced: kimi/glm/minimax all cooled, codex silently broken, deepseek cooled. Claude is carrying essentially all work.

## What Went Well

- The false-`orch version` issue (#3200) was properly diagnosed, documented, and filed. The SKILL.md now contains explicit verification commands and a cautionary entry so future agents won't be fooled by the stale version file.
- No new code regressions landed today.
- The cascade fix (#3189) and status normalization fix (#3160) remain correctly deployed (in the installed 0.73.13 binary, even though the running engine is 0.73.8 and doesn't have them).

## What Failed and Why

| Problem | Root Cause | Status |
|---------|-----------|--------|
| Codex completely broken (day 4) | Service never restarted; running 0.73.8 lacks #3190 + #3198 | **OPERATOR: restart required** |
| `orch version` false "✓ in sync" | `service.version` file decoupled from live engine PID | Open (#3200), code fix needed |
| Minimax re-cooldown | Re-entered after expected recovery — reason unclear | Monitor tomorrow |
| internal:149337 blocked (18 days) | SSH agent not loaded; `ssh-add` not run | **OPERATOR: ssh-add** |
| deepseek-v4-flash-free cooled | New failure today; likely agent error | Should auto-clear in <1h |

## Priorities for Tomorrow

### CRITICAL (operator action required before any other work)

1. **Restart the service and upgrade**:
   ```bash
   orch service restart                              # kills PID 84871, loads 0.73.13
   brew update && brew upgrade orch                  # gets 0.73.14
   brew services restart orch                        # deploys 0.73.14
   # verify the real binary:
   lsof -p $(pgrep -f 'orch serve' | head -1) | grep -i 'txt.*Cellar/orch'
   ```
   This unblocks codex (fixes #3190 + #3198), deploys cascade fix (#3189) and normalization (#3160), and fixes the false `orch version` report.

2. **Unblock internal:149337**:
   ```bash
   ssh-add ~/.ssh/default_id_ed25519
   orch task unblock all
   ```

### Monitoring (once service is restarted)

3. **Verify codex is healthy** — after restart, codex runs should show real work product, non-zero cost, runtime >30s. A $0 / 8s run is still the fake success pattern.
4. **Investigate minimax re-cooldown** — was it a real agent error? Check `orch cooldown list` after service restart; if still cooled, examine the reason in the KV store.
5. **Watch deepseek-v4-flash-free recovery** — was in 58m cooldown at 23:02 UTC; should auto-clear overnight.
6. **Prune dead opencode model entries** from `~/.orch/config.yml` — `github-copilot/gpt-5.3` and `github-copilot/claude-opus-4.6` continue generating router WARN noise.

### Code Issues Open

- **#3200** — `orch version` false "✓ in sync": fix should PID-bind the version file or query the live engine socket. Medium complexity; root cause is `src/engine/events.rs:135-141` writing version without PID binding and `src/cli/mod.rs:33-52` trusting the file.

---

Prepared by Orch automation (internal:150807)
