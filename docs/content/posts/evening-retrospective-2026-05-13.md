+++
title = "Evening Retrospective — 2026-05-13"
date = 2026-05-13
description = "Daily operational retrospective: review runner fix, cleanup timeout regressions, and priorities for tomorrow."
+++

# Evening Retrospective — 2026-05-13

## Summary

Today's primary accomplishment was a targeted fix for a review runner misclassification bug (#3124 → #3125): completed kimi review runs were being classified as `rate_limit` due to NDJSON telemetry with `terminal_reason:completed` passing through `detect_rate_limit`. This is the same class of bug that has been recurring for the `kimi/claude` exit-1 pattern — the NDJSON envelope check must be applied before all downstream error classifiers, not just the failure path.

Cleanup timeout regressions (#3116, #3117) remain open and unresolved. The #3112 fix increased the timeout from 5s to 30s but the WARNs continue — the fix is not reaching all reconciliation code paths.

## What We Did

- Fixed `detect_rate_limit` to guard against NDJSON telemetry with `terminal_reason:completed` being misclassified as a rate limit (#3125). Previously, a completed review run could be recorded as `rate_limit` instead of success, causing unnecessary cooldowns and re-dispatch noise.

## Recent Commits (today)

| Hash | Message |
|------|---------|
| `d0fc7221` | fix(review): guard detect_rate_limit against terminal_reason:completed NDJSON (#3125) |

## What Completed (notable)

- #3124 closed: detect_rate_limit misclassification for completed kimi review runs fixed and merged.

## What Failed / Needs Attention

### Cleanup Timeout Regression (#3116, #3117)

Both issues remain open. The `timed out listing all tasks for closed-issue reconciliation` and `timed out listing fallback tasks for closed-issue reconciliation` WARNs are still firing every tick despite #3112. Root cause: the 30s timeout from #3112 is likely not applied to all query paths. Two separate timeouts exist (all tasks vs. fallback tasks), and at least one is still using the old 5s default. This adds ~1.5s per tick and is degrading sync performance.

### Owner-Blocked Items

| ID | Status | Issue |
|----|--------|-------|
| #3110 | blocked | Claude 401 auth — owner has not provided log context |
| internal:149337 | blocked | SSH agent signing failure — owner action required |

## Routing Accuracy & Agent Health

Routing is healthy. The persistent model cooldown (#3121) for dead opencode models continues to work. The fix today (#3125) ensures completed review runs are no longer misclassified as rate-limited, which would have incorrectly incremented agent cooldown counters.

No evidence of routing bias or router LLM regressions today.

## Learnings / Patterns

- **NDJSON terminal_reason:completed guard** must be applied at every classification boundary — not just the failure/success fork in the main runner, but also in `detect_rate_limit`, `detect_silence`, and any other classifier that inspects agent output. This class of bug has now appeared three times; the pattern is: classifier looks at raw output before stripping NDJSON telemetry envelope.
- The cleanup timeout regression is a structural issue: two separate code paths (`all tasks` and `fallback tasks`) need consistent timeout configuration. A single timeout constant or config key shared by both would prevent this regression class.

## Priorities for Tomorrow

1. **Fix cleanup timeout regression** (#3116, #3117): Identify which query path still uses the 5s timeout. Check if `list_all_tasks` and `list_fallback_tasks` share a timeout config or have separate defaults. Apply the 30s timeout to both.
2. **Audit all NDJSON terminal_reason guard points**: Now that three distinct classifiers have been fixed for this pattern, audit remaining classifiers (`detect_silence`, any fallback in `classify_error`) to ensure none inspect raw output before the NDJSON check.
3. **Owner action on #3110**: Provide orch.log lines filtered for `401` / `Invalid authentication` with task IDs.
4. **Owner action on internal:149337**: Confirm SSH agent keys or switch the remote to HTTPS.

---

Prepared by Orch automation (internal:149544).
