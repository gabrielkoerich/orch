+++
title = "Morning Review — 2026-05-19"
date = 2026-05-19
description = "Daily operational morning review."
+++

# Morning Review — 2026-05-19

## Recent Commits (last 24h)

| Hash | Message |
|------|---------|
| `d657176a` | docs(posts): add evening retrospective for 2026-05-18 (#3157) |
| `162ce122` | docs(posts): add morning review for 2026-05-18 (#3156) |

Only docs/posts landed in the last 24h — no code changes. The most recent code merges are from earlier in the week (router filtering, GLM credit detection, api_retry summarization).

## Operational Health

**Runtime version mismatch detected:** CLI is `0.71.13`, service is running `0.71.15`, latest available is `0.71.19`. The service needs upgrading to pick up all recent fixes.

**Reconciliation timeout persists.** Every sync cycle logs:
```
timed out listing all tasks for closed-issue reconciliation timeout_secs=30
using fallback tasks for closed-issue reconciliation count=151
```
This has been flagged for multiple days. It is still not resolved, and is now 151 fallback tasks per cycle. The fix requires upgrading the runtime or investigating the query path; it will not resolve on its own.

**Multi-agent degradation** (`kimi`, `minimax`, `glm`) continues firing on almost every sync tick with `agent_error` cooldowns, though `kimi` does briefly recover between cycles. This is persistent background noise but delivery remains stable.

## task_runs Snapshot (last 24h)

| Agent/Model | Outcome | Count |
|-------------|---------|-------|
| claude/sonnet | success | 31 |
| codex/gpt-5.3-codex | success | 17 |
| kimi/opus | success | 12 |
| opencode/github-copilot/gpt-5-mini | success | 10 |
| opencode/github-copilot/claude-sonnet-4.6 | success | 8 |
| opencode/opencode/minimax-m2.5-free | success | 5 |
| kimi/opus | failed | 5 |
| codex/gpt-5.3-codex | failed | 3 |
| glm/opus | rate_limit | 3 |
| minimax/opus | rate_limit | 2 |
| opencode/github-copilot/gpt-5-mini | blocked | 2 |

Core delivery agents (claude/sonnet, codex) remain healthy. Failure volume in degraded pools is consistent with prior days.

## task_activity Snapshot (last 12h)

| Event | Count |
|-------|-------|
| status_change | 351 |
| dispatch | 109 |
| push | 97 |
| branch_delete | 74 |
| review_start | 59 |
| routed | 48 |
| review_decision | 48 |
| pr_create | 45 |
| error | 21 |

High dispatch and PR-creation volume — the pipeline is actively delivering work.

## Stuck / Blocked Tasks

- **`#3110`** (open, blocked): Claude 401 invalid authentication credentials. Still awaiting owner-side auth diagnostics.
- **`internal:149337`** (blocked): SSH agent signing failure during push (`sign_and_send_pubkey`). Still awaiting owner-side SSH key fix.

No new blocked tasks beyond the known long-standing ones.

## Retro Follow-ups From 2026-05-18

1. **Verify runtime is upgraded to include recent fixes** — **NOT DONE**: service is on `0.71.15`, latest is `0.71.19`. Action required: `brew upgrade orch && brew services restart orch`.
2. **Reconciliation timeout** — **NOT RESOLVED**: still fires every cycle. Will not clear until runtime is upgraded to a version that addresses it (or the query is fixed).
3. **Monitor degraded pools** — Ongoing, no new escalation needed.
4. **Owner follow-up on `#3110` and `internal:149337`** — Still pending.

## Priorities For Today

1. **Upgrade runtime**: `brew upgrade orch && brew services restart orch` to move service from `0.71.15` → `0.71.19` and pick up all recent merged fixes.
2. **Verify reconciliation timeout clears** after upgrade. If it persists on `0.71.19`, file a bug targeting the specific query path (`cleanup::list_all_tasks`).
3. **Owner action on `#3110`**: Claude auth is still broken; gather and post concrete diagnostics to unblock.
4. **Owner action on `internal:149337`**: SSH key agent health needed for push operations.

## Issues Created

None.

Observed problems are either:
- already tracked (`#3110`),
- owner-environment blockers (`internal:149337`),
- pending resolution via runtime upgrade (reconciliation timeout), or
- known transient provider degradation (kimi/minimax/glm cooldowns).

---

Prepared by Orch automation (internal:149903).
