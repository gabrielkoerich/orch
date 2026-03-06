+++
title = "Morning Review — 2026-03-06 (third pass)"
date = 2026-03-06
description = "Colon bug still live, new infinite review loop bug #452, Claude heavily rate-limited"
+++

## Summary

Two critical fixes (tmux colon sanitization in PR #434, stuck in_progress recovery in PR #442) are still unmerged and undeployed. The live service (v0.10.6) continues to fail all internal task dispatches due to the colon bug. A new bug (#452) was filed overnight: blocked/delegated tasks incorrectly trigger the review agent in an infinite loop.

---

## Recent Changes (last 24h)

| Commit | Description |
|--------|-------------|
| `31a54d4` | docs: add morning review 2026-03-06 second pass |
| `1dd6bd3` | fix: recover internal tasks stuck in in_progress after engine restart (#440) |

No new code landed since the second-pass review — PRs are queued but not merged.

---

## Live Service Status (v0.10.6)

From `/opt/homebrew/var/log/orch.log` (01:06 UTC):

- **Colon bug still active**: `internal:11`, `internal:13` still failing with `orch-orch-internal:11` / `orch-orch-internal:13` session names — "no such session" on token set → exit -1 → failover to opencode → reset to new → repeat
- **Claude rate-limited**: weight at 0.05 (44 hits). Most tasks routing to opencode/kimi.
- **Review agent for #431 failing**: exit 1, resetting to NeedsReview for retry — likely Claude API errors due to rate limiting
- **Task #452 in_progress**: engine routing it as complexity:medium to claude, but claude is rate-limited

---

## New Issue: #452 — Infinite Review Loop for Delegated Tasks

Filed overnight with detailed root cause. When a task returns delegations:
1. `handle_success` sets `final_status = "blocked"`
2. `run_with_context` returns `WeightSignal::None`
3. `tick.rs` maps `WeightSignal::None → needs_review` and spawns review agent
4. Review agent finds no PR → `Skipped` → task stays in `InReview`
5. Stale `InReview` detector resets to `NeedsReview` → back to step 3

Children complete but parent is never unblocked. Fix requires adding a `WeightSignal::Blocked` variant or passing final status through the signal.

---

## Open PRs Needing Merge

| PR | Fix | Priority |
|----|-----|----------|
| #434 | Tmux colon sanitization (this branch) | CRITICAL |
| #442 | Stuck in_progress recovery | HIGH |
| #453 | orch task unblock for internal tasks | MEDIUM |
| #451 | orch task status includes internal tasks | MEDIUM |
| #454 | Engine health checks include internal tasks | MEDIUM |
| #445 | External task NeedsReview on infra failure | MEDIUM |
| #455 | Docs: align with Rust v1 | LOW |

---

## Observations & Priorities

1. **Deploy PRs #434 and #442** — the service is broken for internal tasks until the colon fix lands
2. **Fix #452 (infinite review loop)** — actively burning review agent cycles on blocked tasks
3. **Claude rate limit recovery** — no action needed, weight auto-recovers over time; other agents filling in
4. **Stuck thresholds** — still 1800/600s; reduce to 900/300s once above fixes land

---

## No New Issues Filed

All active problems have existing issues. #452 (infinite review loop) was already filed before this review.
