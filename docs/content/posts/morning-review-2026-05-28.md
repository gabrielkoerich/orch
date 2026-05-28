+++
title = "Morning Review — 2026-05-28"
date = 2026-05-28
description = "Daily morning review and operational priorities."
+++

# Morning Review — 2026-05-28

## Recent Commits (Last 24h)

| Commit | Description |
|--------|-------------|
| `6ac8f851` | fix(runner): reject CLI parser diagnostics in synthesize_response_from_text (#3198) |
| `f99c15e9` | docs(posts): add morning review for 2026-05-27 (#3196) |

One code fix landed: the runner now rejects CLI parser diagnostic messages when synthesizing text responses, preventing spurious parse errors from being treated as real content.

## Operational Health

**Overall: Improved. The operator completed the long-pending service upgrade — CLI and Service are now both at 0.73.13. A new release 0.73.14 is already available. WATCHDOG stalls occurred this morning on the old 0.73.8 service during routing, but should not recur now that the cascade fix from #3189 is deployed. Multi-agent degradation persists (kimi/glm) but minimax is near expiry (~2h). Codex gpt-5.3-codex failure rate worsened to ~79%.**

### Service Version

```
CLI:     0.73.13
Service: 0.73.13  ✓ in sync
Latest:  0.73.14  ⚠  upgrade available
```

**The service upgrade that was carried for 3+ days has been completed.** CLI and Service are now aligned at 0.73.13. However, 0.73.14 is already available — operator should run the upgrade cycle again:

```bash
brew update && brew upgrade orch && brew services restart orch
```

### WATCHDOG Stalls (This Morning, Pre-Upgrade)

The router stalled for ~3.5 minutes while routing this task (internal:150765) on the old 0.73.8 service. The cascade attempted 4 failing pool entries before succeeding:

1. `opencode/nemotron-3-super-free` → "no text output" → cooldown recorded
2. `kimi/haiku` → LLM pool timeout (~50s)
3. `minimax/haiku` → LLM pool timeout (~50s)
4. `glm/haiku` → LLM pool timeout (~50s)
5. `claude/haiku` → LLM router succeeded, selected opencode → rerouted to claude (opencode cooled)

WATCHDOG fired at 80s, 110s, 140s, 170s, 200s, 230s. The cascade fix (#3189) was in 0.73.x but NOT in 0.73.8. Now that the service is on 0.73.13, stalls should not recur. **Monitor today to confirm.**

### Multi-Agent Degradation

| Agent | Reason | Cooldown Remaining |
|-------|--------|--------------------|
| minimax | `agent_error` | ~2 hours |
| glm | `agent_error` | ~10 hours |
| kimi | `billing_cycle_exhausted` | ~58 hours |

**Minimax is near recovery** — cooldown expires within ~2 hours. GLM expires tonight. Kimi remains out for ~2.4 more days. Routing continues to fall back correctly to claude, opencode, and codex.

### Agent/Model Health (Last 24h)

| Agent | Model | Outcome | Count |
|-------|-------|---------|-------|
| opencode | deepseek-v4-flash-free | success | 24 |
| claude | opus | success | 22 |
| claude | sonnet | success | 19 |
| codex | gpt-5.3-codex | **failed** | **15** |
| codex | gpt-5.4 | success | 6 |
| codex | gpt-5.3-codex | success | 4 |
| opencode | mimo-v2.5-free | success | 4 |
| claude | haiku | success | 3 |
| opencode | nemotron-3-super-free | success | 3 |
| opencode | github-copilot/gpt-5-mini | **failed** | **2** |
| codex | gpt-5.3-codex | rate_limit | 1 |

Key observations:
- **opencode/deepseek-v4-flash-free** is the new throughput leader (24 successes). New star performer.
- **Claude** remains healthy: opus 100%, sonnet ~95%. Haiku clean.
- **Codex gpt-5.3-codex** failure rate worsened to ~79% (15 failed / 19 total) — up from ~47% yesterday. Generic cooldown active. **Third consecutive day of degradation.**
- **codex gpt-5.4** continues healthy (6/6 successes).
- **opencode/mimo-v2.5-free** appeared with 4 clean successes.
- `cooldown:github:5xx` is active — GitHub returning 5xx errors to codex; contributing to failures.

### Task Activity (Last 12h)

| Event | Count |
|-------|-------|
| status_change | 328 |
| branch_delete | 130 |
| dispatch | 107 |
| push | 86 |
| review_start | 53 |
| routed | 52 |
| review_decision | 34 |
| pr_create | 34 |
| error | 21 |
| rerouted | 1 |

Throughput is reduced compared to yesterday (328 vs 1,476 status_changes). The WATCHDOG stall this morning likely consumed a significant tick window. Error count (21) is low — a healthy signal for the volume processed.

## Stuck / Blocked Tasks

- **internal:149337** — blocked (17d). SSH agent signing failure during auto-merge push: `sign_and_send_pubkey: signing failed for ED25519 "/Users/gb/.ssh/default_id_ed25519.pub"`. Requires operator: `ssh-add ~/.ssh/default_id_ed25519`.

## Retro Follow-ups

1. **RESOLVED**: Service upgrade to 0.73.13 — completed! CLI and Service now in sync.
2. **NEW**: Upgrade to 0.73.14 — `brew update && brew upgrade orch && brew services restart orch`.
3. **Operator (persistent)**: Resolve internal:149337 SSH signing failure — `ssh-add ~/.ssh/default_id_ed25519`.
4. **Operator (persistent)**: Prune stale opencode model entries (`github-copilot/gpt-5.3`, `github-copilot/claude-opus-4.6`) from `~/.orch/config.yml` to reduce router WARN noise.
5. **Monitor**: WATCHDOG stalls were pre-upgrade. Verify they don't recur on 0.73.13 today.
6. **Monitor**: Codex gpt-5.3-codex failure rate at ~79% for 3 days. Generic cooldown active. If rate persists through today, consider whether there's an underlying API issue distinct from the 0.133.0 `approval_policy` fix (#3190).

## Priorities For Today

1. **Operator (new)**: `brew update && brew upgrade orch && brew services restart orch` — 0.73.14 available.
2. **Operator**: `ssh-add ~/.ssh/default_id_ed25519` — unblock internal:149337 (17 days stale).
3. **Operator**: Prune dead opencode model entries from config.
4. **Monitor**: Confirm WATCHDOG stalls don't recur on 0.73.13 with cascade fix in effect.
5. **Monitor**: Codex gpt-5.3-codex. Day 3 of elevated failures. If still above 60% today, worth checking codex CLI changelog or API status.
6. **Watch**: Minimax recovers in ~2h — verify it re-enters the routing pool cleanly.

---

Prepared by Orch automation (internal:150765)
