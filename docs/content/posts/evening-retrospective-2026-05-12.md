++
title = "Evening Retrospective — 2026-05-12"
date = 2026-05-12
description = "Daily operational retrospective: fixes, remaining blockers, routing health, and priorities for tomorrow."
++ 

# Evening Retrospective — 2026-05-12

## Summary

Today focused on stabilizing cleanup/reconciliation and fixing several runner/router edge-cases that caused false failures and dead-model retries. The high-priority cleanup timeout fix deployed yesterday remains holding. Two owner-blocked items remain (#3110 and internal:149337). Routing and task dispatch remained healthy overall with low failure counts.

## What We Did

- Deployed fixes that addressed misclassified runner failures and persistent model cooldown handling (see commits today). These reduced false failure noise and ensured model-not-found events now trigger persistent backoff rather than retry loops.
- Verified the closed-issue reconciliation timeout increase and deduplication (the #3112 fix) — no more timeout WARNs during cleanup.
- Resolved a classification bug where kimi/claude exit-1 with terminal_reason:completed was treated as a failure; runs are now recognized as success when NDJSON indicates completion.

## Recent Commits (selected)

| Hash | Message |
|------|---------|
| `eef3867c` | fix(router): apply immediate 7-day cooldown for permanently-gone models (#3121) |
| `d938323d` | bug(runner): kimi/claude exit-1 with terminal_reason:completed misclassified as failure (garbled error messages) (#3120) |

These two commits address two of the most visible operational pain points: (1) opencode dispatching dead models and (2) false negative success classification for some agents.

## Operational Metrics (last 12h)

- Task activity: healthy (dozens of dispatches, pushes, and PRs created).
- Failures: very low — two single-event failures observed: one kimi/opus, one opencode:gpt-5.3 (the latter recorded and cooled as expected).
- Error logs: brew stderr and orch.error.log show no new, recent entries of concern.

## What Completed (notable)

- #3119 closed: misclassification causing false failures for kimi/claude fixed and merged.
- #3118 closed: router fixes to avoid dispatching dead Copilot model aliases merged.
- #3112 fix verified: increased reconciliation timeout and deduplication for closed-issue cleanup.

## What Failed / Needs Attention

- #3110 remains blocked: Claude 401 auth failures require owner-provided log lines and task IDs to triage. Agents cannot proceed without those artifacts.
- internal:149337 (SSH signing / git fetch) still blocked awaiting owner action (ssh-agent keys or switch to HTTPS remote).

Root causes:
- #3110: missing auth logs prevent reproducing the error; appears to be a credentials/session problem outside the agent's visibility.
- internal:149337: transient SSH agent problems causing push/fetch failures — operator action required.

## Routing Accuracy & Agent Health

- Routing remains accurate: most tasks distributed across claude, codex, opencode, kimi, and minimax as expected.
- Silent or dead models are being handled by persistent model cooldowns. The opencode:gpt-5.3 failure correctly triggered a persistent cooldown instead of repeated retries.
- No evidence of a routing bias or router LLM regressions in today's traces.

## Performance & Bottlenecks

- Cleanup listing timeouts were the primary bottleneck previously; #3112 increased the timeout and deduplicated fallback logic — the fix is holding.
- No new API rate-limit storms or watchdog stalls observed in the last 12 hours.

## Learnings / Patterns

- Persistent model cooldowns are working as intended: when a model is genuinely unavailable the system records long backoffs instead of retried dispatch loops.
- Classifying agent terminal cases (NDJSON envelope with terminal_reason) must be checked before falling back to error classification — this prevents false failures.
- When raising owner-blocked issues, include exact log lines (tail of orch.log and orch.error.log) and task_run IDs to enable agent triage.

## Priorities for Tomorrow (morning review)

1. Owner action on #3110 — provide orch.log / orch.error.log lines (filter `401` / `Invalid authentication`) and the task IDs that observed Claude 401s.
2. Owner action on internal:149337 — confirm SSH agent keys are loaded (`ssh-add -l`) or change the worktree remote to HTTPS for the affected project.
3. Monitor opencode model cooldowns — if the same model repeatedly reappears after cooldown expiry, consider removing it from per-project config.
4. Continue monitoring for misclassified runner failures (similar to the terminal_reason:completed case) and add regression tests around NDJSON terminal_reason handling.

---

Prepared by Orch automation (internal:149498).
