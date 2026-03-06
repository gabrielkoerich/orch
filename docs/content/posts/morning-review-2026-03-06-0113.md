+++
title = "Morning Review — 2026-03-06 (fifth pass, 01:13 UTC)"
date = 2026-03-06
description = "Stable state: 6 open issues, 13 open PRs, colon bug and stale session bugs covered, service running v0.10.6"
+++

## Summary

No code changes since the fourth pass (01:11 UTC). Main branch has moved ahead of this working branch — PR #455 (docs alignment), PR #438 (bare-clone path fix), and PR #439 (orch task logs) are all in `origin/main`. This branch continues to carry the colon-sanitization fix (PR #434) and morning review docs only.

Service is running v0.10.6. All active failure modes have open PRs. No new issues needed.

---

## Recent Changes (last 24h, main branch)

| Commit | Description |
|--------|-------------|
| `1196c6c` | docs: align workflow, getting-started, and CLI with Rust v1 (#455) |
| `e3b0abe` | fix: resolve_repo_root uses correct bare-clone path format (#438) |
| `41fe745` | Add orch task logs <id> (#439) |
| `1dd6bd3` | fix: recover internal tasks stuck in in_progress after engine restart (#440) |

---

## Open Issues

| # | Title | Status |
|---|-------|--------|
| #452 | Blocked/delegated tasks trigger review agent — infinite loop | in_review |
| #448 | Engine health checks don't include internal tasks | in_review |
| #446 | orch task status excludes internal SQLite tasks | in_review |
| #443 | External task status not updated to NeedsReview on infra failure | needs_review |
| #441 | orch task unblock ignores internal tasks | in_review |
| #431 | Bidirectional channel interaction | in_progress |

---

## Open PRs

| PR | Fix |
|----|-----|
| #457 | fix: restore correct bare-clone path in resolve_repo_root |
| #456 | fix: WeightSignal::Blocked to prevent review agent on blocked tasks |
| #454 | bug: engine health checks don't include internal tasks |
| #453 | bug: orch task unblock ignores internal tasks |
| #451 | orch task status includes internal tasks |
| #447 | docs: fix stale PLAN.md |
| #445 | fix: external task status → NeedsReview on infra failure |
| #444 | feat: bidirectional channel interaction |
| #442 | fix: internal tasks stuck in InProgress recovery |
| #434 | fix: sanitize colons in tmux session names (this branch) |
| #432 | fix: sanitize colons in tmux session names (internal:9 branch) |
| #428 | fix: harden internal task dispatch |
| #427 | docs: late evening retrospective 2026-03-05 |

---

## Active Failure Modes

**1. Colon bug** — `internal:11`, `internal:13` session names `orch-orch-internal:13` parsed as `session:window` by tmux. The `set-environment` call fails "no such session", agent exits -1, falls back, resets to `new`. Loops.

- Fix: PR #434 (this branch), PR #432

**2. Duplicate session** — `internal:12` session `orch-orch-internal_12` exists from prior run; `tmux new-session` fails "duplicate session."

- Fix: PR #442 (stuck in_progress recovery)

**3. Max attempts** — `internal:14` at 10/10 attempts, not dispatched further.

- Action: `orch task unblock internal:14` once PR #453 (or #441) merges.

**4. Claude rate limit** — Weight 0.05, 47 hits. Tasks routing to opencode/kimi. Auto-recovers.

---

## Checklist

1. **Stuck/failing tasks** — Yes: internal:11, 12, 13, 14. Root causes tracked, PRs exist.
2. **Test gaps** — None new beyond existing issues.
3. **Log patterns** — No new patterns vs fourth pass.
4. **Script simplification** — Nothing to simplify.
5. **GitHub issues** — All 6 open issues have active PRs. No owner feedback requiring immediate attention.

---

## No New Issues Filed

All failure modes are covered by existing issues and PRs. Service will recover once PRs #434/#432 (colon fix) and #442 (stale session recovery) are merged and deployed.
