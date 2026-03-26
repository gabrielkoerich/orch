+++
title = "Morning Review -- 2026-03-26"
date = 2026-03-26T10:15:00Z
[extra]
job = "morning-review"
+++

## Summary

High-throughput night: ~15 issues closed since yesterday, focused on auto-merge reliability, review parsing, and
operational hardening. The auto-merge rollout from yesterday continues to shake out edge cases; five new bug issues
were filed and are actively in the review pipeline. Service running on v0.56.36, healthy.

---

## Recent Activity (Last 24h)

### Key Commits

- **Deterministic WS port selection** (#1024) — eliminates port collision on ws server startup
- **Auto-merge CI skip fix** (#1016) — auto-merge was treating missing CI data as a success; now blocks
- **Review envelope parsing split** (#1014) — each agent format parsed independently, no more generic fallback reliance
- **Duplicate review dispatch removed** (#1012) — tick was dispatching reviews twice; deduplicated
- **Global task listing filters** (#1011) — `orch task list` now supports global filters
- **Null CI conclusion treated as pending** (#1013) — completed check runs with null conclusion no longer look like failures
- **Worktree cleanup idempotency** (#994) — cleanup returning `Err` instead of `Ok(false)` when no worktree/branch exists
- **Same-length tmux overwrite fix** (#1007) — output buffer correctly handles terminal clears that produce equal-length output
- **Review cycle cap enforced in poll** (#1003) — review poll now enforces `max_review_cycles`
- **Auto-merge closed PR idempotent** (#997) — closed/merged PRs during auto-merge now return success instead of failing
- **CI semaphore scope fix** (#991) — semaphore no longer held across the entire auto-merge loop sleep/retry

### Active Pipeline (as of now)

| ID   | Status      | Title                                                        |
|------|-------------|--------------------------------------------------------------|
| #1022 | in_review  | Auto-merge can skip CI when workflow lookup fails            |
| #1021 | in_review  | Make worktree cleanup idempotent when git metadata is stale  |
| #1017 | in_review  | Review poll can drop change requests after transient dispatch failure |
| #1019 | needs_review | Validate /agent control commands before persisting          |
| #1018 | needs_review | Request-changes transition counted as review-agent failure  |

---

## Operational Health

### Service

- Running **v0.56.36** (confirmed via log). Previous stale entries referencing 0.56.35 are leftover from the upgrade
  restart — expected and benign.
- `orch` CLI reports **v0.36.1** — this is likely the brew-installed CLI being out of sync with the service binary.
  Worth checking with `brew upgrade orch` if commands behave unexpectedly.

### Patterns

- **Auto-merge edge cases are the theme of the day**: since approved-PR auto-merge landed, a cluster of follow-up
  bugs has been filing in (#1022, #1021, #1017, #1018, #1019). This is expected churn from new functionality; all are
  in the active pipeline.
- **Review parsing is now split by agent format** (#1014) — this directly addresses the retro concern about over-relying
  on generic NDJSON fallback.

### No stuck or blocked tasks

All tasks in the list are either `in_review` or `needs_review` and moving. No tasks are stuck or require human
intervention.

---

## Retrospective Follow-ups (from 2026-03-25 evening)

- [x] Fix auto-merge follow-ups (#991, #997, #1003, #1016 all landed)
- [x] Tighten retry/review parsing paths (#1014 split parsing by agent format)
- [/] Worktree recovery hardening (#1021 in review — worktree cleanup idempotency)
- [ ] Push-retry status mapping (#981 — `routed` back to `NeedsReview` — not yet addressed)

---

## Today's Priorities

1. **Monitor active PRs** (#1022, #1021, #1017): these auto-merge fixes need to land cleanly; watch for review
   cycles or CI failures.
2. **#1019 and #1018**: both are in `needs_review` — verify the review agent picks them up quickly.
3. **Push-retry status mapping (#981)**: this one slipped through yesterday; should be filed or in progress.
4. **CLI version drift**: `orch version` returns 0.36.1 while service is on 0.56.36 — run `brew upgrade orch` to
   sync if this causes issues.
