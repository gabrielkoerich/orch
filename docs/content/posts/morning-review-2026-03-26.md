+++
title = "Morning Review -- 2026-03-26"
date = 2026-03-26T10:15:00Z
[extra]
job = "morning-review"
+++

## Summary

High-throughput night: ~15 issues closed since yesterday, focused on auto-merge reliability, review parsing, and
operational hardening. The auto-merge rollout continues to surface edge cases; newer issues (#1030-#1032) are now in
the pipeline targeting websocket and review-retry behavior. Two critical correctness fixes landed this morning
(#1027, #1028). Service running on v0.56.36, healthy.

---

## Recent Activity (Last 24h)

### Key Commits

- **Stale worktree metadata auto-pruned** (4b7738e) — cleanup no longer leaves orphan metadata entries after a worktree is removed
- **`has_workflows` error propagated** (#1027) — `auto_merge.rs` was masking lookup errors with `unwrap_or(false)`, which could allow a PR to merge without CI running; now returns the error so the poll retries
- **Review poll timestamp fix** (#1028) — `last_review_ts` is now persisted only after `handle_review_changes()` succeeds, preventing silent watermark advances on transient failures
- **Deterministic WS port selection** (#1024) — eliminates port collision on ws server startup
- **Auto-merge CI skip fix** (#1016) — auto-merge was treating missing CI data as a success; now blocks
- **Review envelope parsing split** (#1014) — each agent format parsed independently, no more generic fallback reliance
- **Duplicate review dispatch removed** (#1012) — tick was dispatching reviews twice; deduplicated
- **Global task listing filters** (#1011) — `orch task list` now supports global filters
- **Null CI conclusion treated as pending** (#1013) — completed check runs with null conclusion no longer look like failures
- **Worktree cleanup idempotency** (#994) — cleanup returning `Err` instead of `Ok(false)` when no worktree/branch exists
- **Review cycle cap enforced in poll** (#1003) — review poll now enforces `max_review_cycles`
- **Auto-merge closed PR idempotent** (#997) — closed/merged PRs during auto-merge now return success instead of failing
- **CI semaphore scope fix** (#991) — semaphore no longer held across the entire auto-merge loop sleep/retry

### Active Pipeline (as of now)

| ID   | Status       | Title                                                                    |
|------|--------------|--------------------------------------------------------------------------|
| #1032 | in_progress | Event bus websocket lag is silent in the CLI stream                      |
| #1031 | routed      | Event bus websocket startup can race its port reservation                |
| #1030 | in_progress | Review agent retry after creating a missing PR counts as a failure       |
| #1021 | needs_review | Make worktree cleanup idempotent when git metadata is stale             |
| #1019 | needs_review | Validate /agent control commands before persisting selected agent        |
| #1018 | needs_review | Request-changes transition counted as review-agent failure               |

---

## Operational Health

### Service

- Running **v0.56.36** (confirmed via log). Previous stale entries referencing 0.56.35 are leftover from the upgrade
  restart — expected and benign.
- `orch` CLI reports **v0.36.1** — this is likely the brew-installed CLI being out of sync with the service binary.
  Worth checking with `brew upgrade orch` if commands behave unexpectedly.

### Patterns

- **Websocket reliability is the new focus**: #1031 (port race) and #1032 (silent lag) follow on from the deterministic
  port selection fix in #1024 — further edge cases in startup and observability.
- **Review retry correctness** (#1030): review agent creating a missing PR is valid recovery behavior, but was being
  counted as a failure and consuming retry budget. Needs to be treated as a successful retry, not a failure.
- **Correctness fixes for auto-merge and review poll** (#1027, #1028) landed this morning — these close two subtle
  data-loss/skip bugs that were present since the auto-merge feature shipped.

### No stuck or blocked tasks

All tasks are actively in progress, routed, or awaiting review agent pickup. No tasks are stuck or require human
intervention.

---

## Retrospective Follow-ups (from 2026-03-25 evening)

- [x] Fix auto-merge follow-ups (#991, #997, #1003, #1016, #1027 all landed)
- [x] Tighten retry/review parsing paths (#1014 split parsing by agent format)
- [x] Review poll timestamp correctness (#1028 landed)
- [x] Worktree recovery hardening (#1021 needs_review — worktree cleanup idempotency fix in review)
- [ ] Push-retry status mapping (#992 landed `routed→new` fix; #981 original — confirm fully resolved)

---

## Today's Priorities

1. **Watch #1031/#1032 (websocket)**: port-race and lag fixes are in flight; these affect CLI stream reliability.
2. **#1030 (review retry)**: review agent incorrectly penalized for creating a missing PR — watch for landing.
3. **#1021, #1019, #1018**: all in `needs_review`; confirm review agent picks them up and they clear cleanly.
4. **CLI version drift**: `orch version` returns 0.36.1 while service is on 0.56.36 — run `brew upgrade orch` to
   sync if this causes issues.
