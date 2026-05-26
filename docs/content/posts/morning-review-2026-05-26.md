+++
title = "Morning Review — 2026-05-26"
date = 2026-05-26
description = "Daily morning review and operational priorities."
+++

# Morning Review — 2026-05-26

## Recent Commits (Last 24h)

| Commit | Description |
|--------|-------------|
| `43fa292f` | fix(jobs): respect frontmatter `enabled: false` and fix toggle for prompt-based jobs (#3194) |
| `68dc473a` | Daily morning review (#3192) |

The `enabled: false` frontmatter fix (#3194) closes the last known jobs-system gap — scheduled jobs with `enabled: false` were being executed anyway.

## Operational Health

**Overall: Recovering. Core agents are producing successes; WATCHDOG stalls occurred this morning during routing on the pre-fix service version (0.73.8), but the service has since been upgraded to 0.73.12 which contains the cascade fix (#3189). One more upgrade step is required (see below).**

### Service Version Mismatch

```
CLI:     0.73.13
Service: 0.73.12  ✗ mismatch
Latest:  0.73.13  ✓
```

The service is one release behind the CLI. Operator action required:

```bash
brew upgrade orch && brew services restart orch
```

### WATCHDOG Stalls (This Morning — Pre-Fix Service)

Between 10:01–10:07 UTC, the service was still running 0.73.8 (pre-fix) when three scheduled morning jobs fired simultaneously (`morning-briefing`, `twitter-trending-watch`, `morning-review`). The router tried 4–5 pool entries sequentially before falling back to claude, each timing out at 60s — producing WATCHDOG alerts at 79s, 109s, 139s, 169s, 199s, 217s, and 247s.

The service was upgraded to 0.73.12 (containing the cascade fix from #3189) during or after this routing cycle. The same pattern should not recur once the service is updated to 0.73.13.

### Agent/Model Health (Last 24h)

| Agent | Model | Outcome | Count |
|-------|-------|---------|-------|
| kimi | opus | success | 35 |
| claude | sonnet | success | 30 |
| opencode | github-copilot/gpt-5-mini | success | 30 |
| codex | gpt-5.3-codex | success | 20 |
| codex | gpt-5.3-codex | failed | 18 |
| claude | opus | success | 11 |
| opencode | opencode/deepseek-v4-flash-free | success | 8 |
| opencode | github-copilot/gpt-5-mini | failed | 7 |
| claude | sonnet | failed | 4 |

Codex gpt-5.3-codex shows a ~47% failure rate. This may reflect residual dispatch failures from before the `approval_policy` fix (#3190) cleared the pipeline; worth monitoring through today to confirm recovery.

### Task Activity (Last 12h)

High-volume activity confirms the engine is running at full capacity:
- 1,577 status changes, 390 pushes, 328 dispatches, 318 review starts, 292 review decisions
- 172 errors (normal for high-volume operation; mostly backoff-driven retries)

## Stuck / Blocked Tasks

- **internal:149337** — blocked (15d). SSH agent signing failure during auto-merge push. Pattern: `sign_and_send_pubkey: signing failed for ED25519 "/Users/gb/.ssh/default_id_ed25519.pub"`. Requires operator intervention: `ssh-add ~/.ssh/default_id_ed25519`.

## Retro Follow-ups (Carried Forward)

1. **Operator**: `brew upgrade orch && brew services restart orch` — service is at 0.73.12, CLI at 0.73.13.
2. **Operator**: Resolve internal:149337 SSH signing failure — `ssh-add ~/.ssh/default_id_ed25519`.
3. **Operator**: Prune stale opencode model entries (`github-copilot/gpt-5.3`, `github-copilot/claude-opus-4.6`) from `~/.orch/config.yml` to eliminate persistent WARN noise.
4. **Monitoring**: Verify WATCHDOG stalls have ceased on 0.73.12/0.73.13 — collect metrics through today to confirm the cascade fix (#3189) is effective in production.
5. **Monitoring**: Watch codex gpt-5.3-codex failure rate through today — should trend toward recovery now that `approval_policy` fix (#3190) is deployed.

## Priorities For Today

1. **Operator (immediate)**: Upgrade service to 0.73.13 and restart — `brew upgrade orch && brew services restart orch`.
2. **Operator**: Fix internal:149337 SSH signing failure so the blocked task can clear.
3. **Operator**: Prune dead opencode model entries from config.
4. **Engineering**: Monitor codex dispatch success rate through today — if gpt-5.3-codex failures persist above 40%, investigate whether a new codex CLI issue has emerged.
5. **Engineering**: Confirm WATCHDOG stalls are absent on the upgraded service.

---

Prepared by Orch automation (internal:150627)
