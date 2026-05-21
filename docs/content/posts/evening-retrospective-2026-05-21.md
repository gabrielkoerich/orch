++
title = "Evening Retrospective — 2026-05-21"
date = 2026-05-21
description = "Daily evening retrospective and operational summary."
++ 

# Evening Retrospective — 2026-05-21

## Summary

Today focused on reliability fixes and noise reduction. Two fixes landed that reduce spurious task blocks and update Codex runner flags. Overall throughput remained healthy and no new high-severity outages were introduced. Known environment blocks remain open and need operator attention.

## What Happened Today

- Commits (last 12h):
  - `7fb312fe` fix(codex): replace deprecated --full-auto with --sandbox workspace-write --ask-for-approval never (#3177)
  - `50409d06` fix(runner): broaden retryable-blocked classifier to catch reordered 'worktree git lock' phrasing (#3176)

- Closed issues / PRs observed today:
  - #3176 — broaden retryable-blocked classifier (closed)
  - #3175 — codex index.lock permission regression (closed)

## What Was Accomplished

- Reduced a class of blocked tasks by expanding the retryable-blocked classifier to match more variations of the "worktree git lock" message.
- Replaced a deprecated Codex runner flag (`--full-auto`) with the workspace-write sandbox and explicit approval policy to avoid future deprecation noise and make runner invocation explicit.
- Added this evening retrospective to the posts collection.

## Failures, Retries, and Ongoing Issues

- Environment / operator blockers still present:
  - `#3110` Claude 401 Invalid authentication credentials — owner action required (ongoing)
  - `internal:149337` SSH agent signing failure during pushes (`sign_and_send_pubkey`) — operator environment fix needed

- No new systemic routing regressions were observed. The router continues to fall back to round-robin only when LLM budget exhaustion is observed; that behavior remains bounded and expected.

## Routing & Agent Health

- Core agents (claude, codex, opencode) remain healthy in production metrics.
- Degraded pools (kimi, minimax, glm) continue to show low-volume transient failures and rate-limits; these are within expected behaviour and haven’t caused throughput regressions today.

## Priorities For Tomorrow's Morning Review

1. Confirm that the broadened retryable-blocked classifier stops the observed spurious blocks (check `task_runs` for blocked → retried patterns).
2. Operator triage for `internal:149337` (SSH agent) — if it persists, ask operator to restart SSH agent and re-add keys.
3. Monitor Claude auth (`#3110`) for any new diagnostics from the owner; escalate if no progress within 24h.
4. Watch opencode WARN noise for stale model aliases; if WARNs persist after recent pruning PRs, consider filing a short-lived PR to clean config entries (operator action).

---

Prepared by Orch automation (internal:150127).
