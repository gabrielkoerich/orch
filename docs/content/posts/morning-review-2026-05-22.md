+++
title = "Morning Review — 2026-05-22"
date = 2026-05-22
description = "Daily morning review and operational priorities."
+++

# Morning Review — 2026-05-22

## Recent Commits (Last 24h)

| Commit | Description |
|--------|-------------|
| `3e38477b` | docs(posts): Daily evening retrospective (#3179)
| `7fb312fe` | fix(codex): replace deprecated --full-auto with --sandbox workspace-write --ask-for-approval never (#3177)
| `50409d06` | fix(runner): broaden retryable-blocked classifier to catch reordered 'worktree git lock' phrasing (#3176)
| `5da54917` | docs(posts): add evening retrospective for 2026-05-21
| `50409d06` | fix(runner): broaden retryable-blocked classifier (duplicate entry earlier)

A small set of fixes and docs updates landed in the last 24h. The service was restarted this morning and created internal morning jobs (internal:150141, 150142, ...).

## Operational Health

**Service state**:

- Service started today at 10:31 UTC; engine version running: 0.72.0 (warning: behind latest 0.73.5). The service logged a graceful shutdown earlier (SIGTERM) and then restarted.
- Worktree reconciliation and project initialization completed; backend (GitHub) connected for the two registered repos.

**Logs / notable warnings**:

- Repeated WARNs about opencode model alias from config: `github-copilot/gpt-5.3` — router warns it's not present in provider model list. This is a config-vs-provider mismatch and shows in multiple routing attempts.
- Multiple tmux "list-panes exited non-zero" warnings during startup (no tmux socket yet); this is expected when the service starts without active sessions.
- Many regular sync ticks completed successfully (typical elapsed_ms ~1.7–2.0s). A few slow ticks were observed (up to ~63s) but they were transient.

**Task run outcomes (last 24h sample)** — aggregated from local DB:

- Top success counts: claude:sonnet (25), kimi:opus (18), codex:gpt-5.3-codex (15), opencode:github-copilot/gpt-5-mini (12).
- Some failures observed for codex and kimi (small counts). Overall throughput remains healthy.

## Stuck / Blocked Tasks

- internal:149337 — blocked (SSH agent signing failure) — operator action required (restart SSH agent / re-add key).
- Several bean-project tasks blocked with 0 review_cycles and no block_reason (pattern previously observed). Recommend operator triage: inspect `orch task show <id>` and either unblock or close stale items.

## Retro Follow-ups (carried from last evening)

- Upgrade service to latest release (0.73.5) — still outstanding. Upgrading will activate several bug-fixes (model pruning, routing budget behavior).
- Confirm the broadened retryable-blocked classifier reduces the spurious blocks for worktree lock messages.
- Monitor opencode WARN noise after upgrade — if WARNs persist, they indicate config entries referencing models not present in the provider list and should be cleaned.

## Priorities For Today

1. Operator: upgrade orch service to latest (0.73.5). This is the highest-impact step — several fixes (routing budget, model pruning) are in recent releases.
2. After upgrade: verify that opencode stale model WARNs stop and that LLM routing budget exhaustion no longer triggers fallbacks for routine jobs.
3. Triage blocked bean tasks (long-running blocked tasks: inspect 148985, 149038, 149337). Close or reassign as appropriate.
4. If SSH agent blocking persists for internal:149337, request operator to restart SSH agent and re-add keys.

---

Prepared by Orch automation (internal:150141).
