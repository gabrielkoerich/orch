+++
title = "Morning Review — 2026-05-27"
date = 2026-05-27
description = "Daily morning review and operational priorities."
+++

# Morning Review — 2026-05-27

## Recent Commits (Last 24h)

| Commit | Description |
|--------|-------------|
| `0402a728` | docs(posts): add morning review for 2026-05-26 (#150627) (#3195) |

A quiet commit day — only the daily review post landed. No code changes.

## Operational Health

**Overall: Stable but degraded. Core agents (claude, codex, opencode) are producing work. Three secondary agents (kimi, minimax, glm) are in cooldown and will remain unavailable for the next 10–34 hours. Service version is still one behind CLI. No WATCHDOG stalls observed today — cascade fix (#3189) confirmed working in production.**

### Service Version Mismatch

```
CLI:     0.73.13
Service: 0.73.12  ✗ mismatch
Latest:  0.73.13  ✓
```

Carried forward from yesterday. Operator action still required:

```bash
brew upgrade orch && brew services restart orch
```

### Multi-Agent Degradation (Persistent)

Every sync tick is emitting `multi-agent degradation detected` with 3 degraded agents:

| Agent | Reason | Cooldown Remaining |
|-------|--------|--------------------|
| kimi | `billing_cycle_exhausted` | ~10 hours |
| minimax | `agent_error` | ~27 hours |
| glm | `agent_error` | ~34 hours |

All three are in valid cooldown via the generic backoff system. Routing is correctly falling back to claude, opencode, and codex. No action required — cooldowns will expire automatically. The WARN noise is expected and correct.

### WATCHDOG Stalls

No stall alerts observed in today's logs. The router timeout cascade fix (#3189) is confirmed working in production. This was the primary concern from the 2026-05-24 retrospective.

### Agent/Model Health (Last 24h)

| Agent | Model | Outcome | Count |
|-------|-------|---------|-------|
| claude | sonnet | success | 31 |
| claude | opus | success | 22 |
| codex | gpt-5.3-codex | success | 19 |
| opencode | opencode/deepseek-v4-flash-free | success | 19 |
| codex | gpt-5.3-codex | failed | 17 |
| opencode | github-copilot/gpt-5-mini | success | 17 |
| kimi | opus | success | 16 |
| opencode | github-copilot/gpt-5-mini | failed | 9 |
| codex | gpt-5.4 | success | 5 |
| opencode | opencode/nemotron-3-super-free | success | 4 |
| claude | sonnet | failed | 4 |
| codex | gpt-5.3-codex | rate_limit | 3 |
| kimi | opus | rate_limit | 1 |
| minimax | opus | rate_limit | 1 |
| opencode | github-copilot/gpt-5-mini | parse_error | 1 |

Codex gpt-5.3-codex failure rate remains ~47% (19 success / 17 failed + 3 rate_limit). This is the second consecutive day at this level — not recovering as expected after the `approval_policy` fix (#3190). `codex:gpt-5.4` appeared with 5 successes (new pool entry). Opencode `gpt-5-mini` is at ~63% success. Claude remains healthy at ~88% (sonnet) and 100% (opus).

### Task Activity (Last 12h)

| Event | Count |
|-------|-------|
| status_change | 1,476 |
| push | 359 |
| dispatch | 303 |
| review_start | 301 |
| review_decision | 274 |
| error | 170 |
| branch_delete | 132 |
| routed | 74 |
| pr_create | 70 |
| rerouted | 7 |

Engine is operating at full throughput. Error count (170) is normal for this volume.

## Stuck / Blocked Tasks

- **internal:149337** — blocked (16d). SSH agent signing failure during auto-merge push: `sign_and_send_pubkey: signing failed for ED25519 "/Users/gb/.ssh/default_id_ed25519.pub"`. Requires operator: `ssh-add ~/.ssh/default_id_ed25519`.

## Retro Follow-ups (Carried Forward)

1. **Operator (immediate)**: Upgrade service to 0.73.13 — `brew upgrade orch && brew services restart orch`. Third day this has been carried.
2. **Operator**: Resolve internal:149337 SSH signing failure — `ssh-add ~/.ssh/default_id_ed25519`.
3. **Operator**: Prune stale opencode model entries (`github-copilot/gpt-5.3`, `github-copilot/claude-opus-4.6`) from `~/.orch/config.yml` to eliminate persistent WARN noise.
4. **Confirmed resolved**: WATCHDOG stalls are gone — cascade fix (#3189) is effective in production.
5. **Still monitoring**: Codex gpt-5.3-codex failure rate at ~47% for 2 days. Second-day persistence warrants investigation.

## Priorities For Today

1. **Operator (immediate)**: `brew upgrade orch && brew services restart orch` — 3rd day carrying this.
2. **Operator**: `ssh-add ~/.ssh/default_id_ed25519` — unblock internal:149337.
3. **Operator**: Prune dead opencode model entries from config to eliminate WARN noise.
4. **Engineering/Monitor**: Codex gpt-5.3-codex ~47% failure rate persisting. If it remains above 40% through today, investigate whether there is a new CLI or API issue distinct from the 0.133.0 `approval_policy` fix.
5. **No new issues filed**: multi-agent degradation is handled by cooldowns (no action needed); all other items are pre-existing.

---

Prepared by Orch automation (internal:150692)
