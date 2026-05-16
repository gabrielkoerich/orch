++
+++
title = "Morning Review — 2026-05-16"
date = 2026-05-16
description = "Daily operational morning review."
++

# Morning Review — 2026-05-16

## Recent Commits (last 24h)

| Hash | Message |
|------|---------|
| `5bbe4aea` | docs(posts): add evening retrospective for 2026-05-15 (#3138) |
| `f1cf6a4e` | Daily evening retrospective (#3132) |
| `46a80ce5` | Daily morning review (#3133) |
| `4d0c9dd9` | fix(engine): rebase behind PRs before blocking on CI (#3137) |
| `4d62298a` | bug(review): kimi review runs with terminal_reason:completed fall through to parse_error (#3136) |

Notable: fixes landed to reduce review parse false-positives and to rebase behind PR branches before blocking on CI. Routine style and docs updates also merged.

## Operational Summary

Service is running (v0.71.8). The dominant recurring symptom remains the closed-issue reconciliation timeout: repeated

WARN orch::engine::cleanup: timed out listing all tasks for closed-issue reconciliation timeout_secs=30

When the list call times out the engine falls back to cached/fallback tasks (count=203), which keeps behavior correct but creates repeated warning noise and slightly longer sync ticks.

Other observations:
- The job scheduler created multiple internal morning tasks at 10:00 UTC as expected (internal:149682 and siblings).
- The runner dispatched internal:149682 to opencode (model github-copilot/gpt-5-mini) and created a worktree; the job is active in this worktree.
- Logs show frequent cleanup timeouts at every sync tick (30s interval) — this is chronic and should be prioritized.

## Stuck / Blocked Tasks

| ID | Status | Agent | Blocked On |
|----|--------|-------|------------|
| internal:149337 | blocked | minimax | SSH agent signing failure — owner action required (git push failing due to agent signing)
| 3110 | blocked | opencode | Claude 401 auth — needs orch.log excerpts showing 401 lines for triage
| 3116 / 3117 | open | — | Reconciliation `list_all_tasks` timeout regression

internal:149595 (evening retro) has a PR that failed CI auto-merge and may need a human rebase.

## Health Checks — task_runs & recent activity

Snapshot (task_runs aggregated, last 24h):

- opencode / github-copilot/claude-sonnet-4.6 — success dominant
- opencode / github-copilot/gpt-5-mini — many successes
- claude / sonnet — steady successes
- kimi / opus — mostly success but 3 failures recorded (monitor)
- codex / gpt-5.3-codex — mostly success; one transient failure
- Several opencode model variants show rare failed or blocked runs (dead-model attempts are handled by per-model cooldowns)

Recent task_activity (last 12h): status_change (259), dispatch (84), push (72), branch_delete (72), routed (40), review_start (38), review_decision (35), pr_create (35), error (9).

## Logs — patterns & immediate root causes

- Repeated cleanup timeout in engine::cleanup: list_all_tasks call times out with configured 30s budget, then fallback path returns ~203 tasks. Root causes to check:
  1) Query path may be performing a full scan or missing indexes in SQLite on large tables (check migrations/indexes and explain plan).
  2) A caller may be accidentally invoking a remote GitHub path; confirm the code path uses local store queries and not remote APIs.

- A small number of agent/model failures (kimi/opus) increased slightly — #3134 addressed a parsing gap; monitor for regression.

## Evening Retro Follow-ups (carried forward)

- #3116 / #3117 — reconciliation timeout: Highest priority today. Audit `src/engine/cleanup.rs` and any call-sites that list tasks; ensure all use the unified RECONCILIATION_LIST_TIMEOUT and that the SQL query uses indexes.
- #3110 — Claude 401 auth: request orch.log excerpts showing the 401 lines and task IDs from the owner so the auth failure can be triaged.
- internal:149337 — SSH signing failure: owner must fix SSH agent or switch remote to HTTPS; provide exact git error (sign_and_send_pubkey) and affected task IDs.
- Monitor kimi/opus failures — confirm post-#3134 the false-positive parse failures decline.

## Priorities For Today

1. Investigate and fix the reconciliation timeout (3116/3117). Start by tracing the `list_all_tasks` call: is it hitting SQLite with an unindexed WHERE, or calling GitHub? Run EXPLAIN on suspect queries and add indexes if needed.
2. Request owners for blocked tasks: ask for orch.log excerpts for #3110 and confirm SSH fix for internal:149337.
3. Check PR #3132 (evening retro) for CI failures and rebase/repair if human intervention is required.
4. Monitor kimi/opus runs for reduction in false failures after merging #3134.

---

Prepared by Orch automation (internal:149682).
