+++
title = "Morning Review — 2026-03-06 (second pass)"
date = 2026-03-06
description = "Tmux colon fix in PR #434, stuck task recovery fixes, and open PR queue overview"
+++

## Summary

Two critical fixes are queued in PRs and awaiting merge. The tmux colon bug (internal tasks failing instantly) is fixed in PR #434. Stuck-in-progress recovery (tasks never reset after restart) is in PR #442. The live service (v0.10.6) still shows both issues — deploy is the priority.

---

## Recent Changes (last 24h)

| Commit | Description |
|--------|-------------|
| `1dd6bd3` | fix: recover internal tasks stuck in in_progress after engine restart (#440) |
| `41fe745` | feat: orch task logs \<id\> — post-mortem for completed tasks |

---

## Carried-Forward Priorities — Status

| Priority | Status |
|----------|--------|
| Tmux colon bug in session_name() | ✅ Fixed in this branch, PR #434 open |
| Stuck in_progress recovery | ✅ Fixed in PR #442 (separate branch) |
| Stuck detection threshold reduction | ❌ Still pending — 1800/600s defaults, retro suggests 900/300s |
| Verify gitleaks active | ✅ Confirmed active since `909f663` |

---

## Live Service Status (v0.10.6)

From `/opt/homebrew/var/log/orch.log`:

- `internal:13` still failing with `orch-orch-internal:13` colon bug — fix not yet deployed
- `internal:10` hitting "duplicate session: orch-orch-internal_10" — orphaned session from previous run
- `task 441` recovered after 11 min (no_session_stuck_timeout=600s working)
- Claude agent weight at 0.05 (37 rate limit hits) — heavy usage today
- Rebase conflict in worktree for task 448 — runner logs show `could not apply 1dd6bd3`

---

## Open PRs

| PR | Title | Status |
|----|-------|--------|
| #451 | orch task status: add internal task overview | OPEN |
| #447 | docs: fix stale PLAN.md | OPEN |
| #445 | fix: external task status to NeedsReview on infra failure | OPEN |
| #444 | feat: bidirectional channel interaction | OPEN |
| #442 | fix: stuck in_progress recovery after restart | OPEN |
| **#434** | **fix: tmux colon sanitization (this branch)** | **OPEN** |
| #432 | fix: tmux colon sanitization (parallel branch) | OPEN |
| #428 | fix: harden internal task dispatch | OPEN |
| #427 | docs: late evening retrospective 2026-03-05 | OPEN |

Duplicate colon fixes in #434 and #432 — one of these needs to be closed.

---

## Open Issues

| # | Title | Priority |
|---|-------|----------|
| #449 | Docs: align workflow/getting-started/CLI with Rust v1 | low |
| #448 | Engine health checks blind to internal/SQLite tasks | medium — in_progress |
| #446 | orch task status excludes internal tasks | medium — in_progress |
| #443 | External task status not updated on infra failure | simple — in_review |
| #441 | orch task unblock ignores internal tasks | simple — new |
| #435 | resolve_repo_root wrong path for bare-clone projects | simple — in_review |
| #431 | Bidirectional channel interaction | complex — in_review |

---

## Action Items

1. **Merge PR #434** (or #432) — deploy tmux colon fix ASAP; orphaned sessions accumulate with each dispatch cycle
2. **Merge PR #442** — deploy stuck in_progress recovery
3. **Close duplicate** (#432 or #434) once the colon fix lands
4. **Stuck thresholds** — reduce `stuck_timeout` 1800→900, `no_session_stuck_timeout` 600→300 directly in config.yml (no code change needed)
5. **Kill orphaned session** `orch-orch-internal_10` — currently blocking re-dispatch

---

## Tomorrow's Priorities

1. Verify merged fixes are live after `brew upgrade orch && brew services restart orch`
2. Reduce stuck thresholds in config or code
3. Watch internal task dispatch success rate improve once tmux fix is deployed
