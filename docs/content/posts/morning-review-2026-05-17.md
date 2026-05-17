+++
title = "Morning Review — 2026-05-17"
date = 2026-05-17
description = "Daily operational morning review."
+++

# Morning Review — 2026-05-17

## Recent Commits (last 24h)

| Hash | Message |
|------|---------|
| `45ca9c9b` | docs(posts): add evening retrospective for 2026-05-16 (#3147) |
| `bec81470` | fix(engine): use list_reconciliation_candidates() directly for closed-issue reconciliation (#3146) |
| `e437f712` | bug: opencode periodically dispatched to cooled/dead github-copilot models (#3145) |
| `ea8d421e` | fix(runner): auto-retry transient pass-store GPG decryption blockers (#3144) |
| `ea671dab` | Daily morning review (#3139) |


## Operational Summary

Service running; recurring cleanup timeout (list_all_tasks) still appears in logs and engine is using fallback tasks (count=188). Two carryover blocked tasks: #3110 Claude 401 and internal:149337 SSH agent signing failure. Recent fixes merged (#3144/#3145/#3146) but running service still on older deployed binary; deploy v0.71.15 to pick up reconciliation fix.


## Stuck / Blocked Tasks

- internal:149337 — blocked — SSH agent signing failure when pushing (owner action required)
- #3110 — blocked — Claude 401 Invalid authentication credentials; awaiting orch.log excerpts for triage

## Health Checks — task_runs & recent activity

task_runs (24h): opencode|gpt-5-mini: success heavy; claude|sonnet success; kimi|opus mostly success with a few failures; codex|gpt-5.3-codex mostly success. Aggregated outcomes show opencode/gpt-5-mini success=18, codex/gpt-5.3-codex success=18, etc.
Recent activity (12h): status_change=393, dispatch=130, push=104, branch_delete=90, routed=61, review_start=53, review_decision=51, pr_create=49, error=17.


## Logs — patterns & immediate root causes

- Repeated WARN: orch::engine::cleanup: timed out listing all tasks for closed-issue reconciliation (timeout_secs=30), engine falls back to cached tasks (count=188). This is the main operational noise and is resolved by deploying v0.71.15 which contains the reconciliation query fix.
- No watchdog stalls observed. Auto-merge pipeline is working; a recent PR was auto-approved and merged.

## Retro Follow-ups (carried forward)

- Deploy v0.71.15 to eliminate reconciliation timeouts in running service.
- Gather orch.log 401 traces for #3110 and ask owner for details.
- Owner action: internal:149337 SSH agent fix (or switch remote to HTTPS).

## Priorities For Today

1. Deploy v0.71.15 and restart service to apply reconciliation fix; verify cleanup timeout warnings stop.
2. Request orch.log excerpts for #3110 (Claude 401) and request owner fix for internal:149337 SSH agent signing failure.
3. Monitor task_runs for any new opencode dead-model dispatches and ensure per-model cooldowns prevent recurrence.
4. Watch kimi/opus outcomes for regression; confirm #3134 reduced false parse errors.


---

Prepared by Orch automation (internal:149773).
