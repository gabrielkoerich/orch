+++
title = "Morning Review — 2026-05-24"
date = 2026-05-24
description = "Daily morning review and operational priorities."
+++

# Morning Review — 2026-05-24

## Recent Commits (Last 24h)

| Commit | Description |
|--------|-------------|
| `91de11d0` | docs(posts): add evening retrospective for 2026-05-24 (#3184) |
| `83bd706f` | Daily morning review (#3185) |
| `d4b1e74e` | refactor(jobs): load jobs from prompts/jobs/*.md files (#3182) |

Yesterday's two major refactors landed cleanly: budget tracking removed in full, and the jobs system moved to `prompts/jobs/*.md` discovery.

## Operational Health

**Overall: Healthy.** All core agents are producing successful runs. No systemic failures.

**Agent/Model performance (last 24h):**
- kimi/opus: 10 success — healthy
- opencode/claude-sonnet-4.6: 10 success, 2 null-outcome (likely in-flight at capture time)
- opencode/gpt-5-mini: 8 success, 3 blocked
- claude/sonnet: 7 success
- opencode/gpt-5.4: 7 success, 1 null-outcome
- codex/gpt-5.3-codex: 2 failed, 1 success

**Persistent WARN noise:** opencode config still contains two stale model entries that are pruned every dispatch cycle:
- `github-copilot/gpt-5.3` (pruned at medium complexity)
- `github-copilot/claude-opus-4.6` (pruned at complex complexity)

This produces 2 WARN lines on every dispatch. The code correctly prunes them — this is a config hygiene issue only. Operator should remove these entries from `~/.orch/config.yml`.

**WATCHDOG tick stalls:** Still occurring. Two observed in today's log window (~105s and ~130s stalls). The pattern remains consistent with worktree creation blocking the main tick loop. Not a regression — carried from yesterday's retro.

**codex failures:** 2 `failed` outcomes for gpt-5.3-codex. Normal for codex on hard tasks; exponential backoff will handle retries.

**Task activity (last 12h):** 166 status changes, 52 dispatches, 47 pushes, 47 branch deletes (cleanup working), 28 review starts, 22 PRs created, 9 errors — volume is healthy.

## Stuck / Blocked Tasks

- **internal:149337** — blocked (13 days). SSH agent signing failure during auto-merge push. Pattern: `sign_and_send_pubkey: signing failed for ED25519`. This is an operator environment issue — SSH agent is not forwarded to the orch service process. Resolution: restart SSH agent, re-add keys, or switch to HTTPS-based push URL.

## Retro Follow-ups (carried forward)

1. **Operator:** remove dead opencode model entries (`github-copilot/gpt-5.3` and `github-copilot/claude-opus-4.6`) from `~/.orch/config.yml`. This will eliminate persistent WARN noise on every dispatch.
2. **Operator:** resolve internal:149337 SSH key issue — restart SSH agent and re-add keys (`ssh-add ~/.ssh/default_id_ed25519`), or configure orch to use HTTPS for pushes.
3. **Investigate WATCHDOG tick stalls** — worktree creation appears to block the tick loop. Consider running worktree setup in a background task rather than inline on the tick.
4. **Monitor** job loading from `prompts/jobs/*.md` (new since yesterday) to confirm jobs are discovered correctly in production without config changes.

## Priorities For Today

1. **Operator action:** fix internal:149337 — run `ssh-add ~/.ssh/default_id_ed25519` to restore SSH agent signing.
2. **Operator action:** prune stale opencode model entries from config to eliminate WARN noise.
3. **Monitor** LLM routing stability — budget exhaustion fallback was eliminated yesterday; verify round-robin no longer activates unexpectedly.
4. **Investigate** WATCHDOG stalls — if stalls continue to 2+ minutes, trace the blocking call in worktree creation.

---

Prepared by Orch automation (internal:150238)
