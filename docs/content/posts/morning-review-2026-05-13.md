+++
title = "Morning Review — 2026-05-13"
date = 2026-05-13
description = "Cleanup timeout WARNs returning despite #3112 fix; #3116 and #3117 filed for root cause investigation."
+++

# Morning Review — 2026-05-13

## Recent Commits (last 24h)

| Hash | Message |
|------|---------|
| `f9f03b28` | docs(posts): add evening retrospective 2026-05-12 (internal:149498) |
| `eef3867c` | fix(router): apply immediate 7-day cooldown for permanently-gone models (#3121) |
| `d938323d` | bug(runner): kimi/claude exit-1 with terminal_reason:completed misclassified as failure (#3120) |
| `02573f86` | docs(posts): add morning review for 2026-05-12 (internal:149464) |
| `a8d28a96` | docs(posts): evening retrospective 2026-05-11 (#3113) |

Five commits. Notable: permanent model cooldown fix (#3121) and kimi runner misclassification fix (#3120). Both address failure noise that was flagged in prior retros.

## Operational Summary

Service is running. Two new issues filed for cleanup reconciliation timeouts (#3116, #3117). Task activity healthy. LLM routing budget exceeded for this morning-review task (fell back to round-robin), consistent with overloaded router conditions.

## Health Checks

### Cleanup Timeout — Regression

Despite #3112 increasing the reconciliation timeout from 5s to 30s and deduplicating fallback logic, `timed out listing all tasks for closed-issue reconciliation` and `timed out listing fallback tasks for closed-issue reconciliation` WARNs are **back** in recent logs. Dozens of entries over the last ~2 hours.

This is a regression from the #3112 fix. #3116 and #3117 have been filed to investigate root cause (two separate timeout targets — `all tasks` and `fallback tasks`). The WARNs are occurring every tick, adding ~1.5s to each sync cycle. Not critical, but degrading tick performance.

### Stuck / Blocked Tasks

| ID | Status | Agent | Blocked On |
|----|--------|-------|------------|
| internal:149337 | blocked | minimax | SSH agent signing failure (git fetch) — owner action required |
| #3110 | blocked | opencode | Owner has not provided log context or task IDs needed to triage Claude 401 auth failures |
| #3116 | open | — | cleanup reconciliation `all tasks` timeout regression |
| #3117 | open | — | cleanup reconciliation `fallback tasks` timeout regression |

### task_runs Summary (last 24h)

```
opencode|github-copilot/gpt-5-mini|success|15
kimi|opus|success|10
claude|sonnet|success|8
codex|gpt-5.3-codex|success|6
glm|opus|success|6
minimax|opus|success|6
opencode|github-copilot/claude-sonnet-4.6|success|5
opencode|github-copilot/gpt-5.3|failed|2
opencode|github-copilot/gpt-5.4|success|2
kimi|opus|rate_limit|1
minimax|opus||1
opencode|github-copilot/claude-sonnet-4.6|failed|1
opencode|github-copilot/gpt-5-mini|blocked|1
```

Routing is healthy. Failures are low and single-count. opencode/gpt-5.3 shows 2 failures (persistent cooldown is working but model keeps appearing — likely still in project config). The persistent cooldown from #3121 should handle it; monitor for recurrence.

### Error Log

`/opt/homebrew/var/log/orch.error.log`: empty (0 bytes). No actionable errors in stderr.

## Retro Follow-ups

- **#3110 still blocked**: owner has not added log context. Blocked indefinitely until owner provides reproduction details.
- **internal:149337 SSH issue**: not resolved. Owner action required.
- **#3120 fix deployed**: kimi/claude misclassification fixed. Monitoring.
- **#3121 fix deployed**: permanent model cooldown deployed. Monitoring.
- **#3112 cleanup timeout fix**: regressed. #3116 and #3117 filed. Root cause investigation needed — likely the increased timeout isn't being applied to the right query path, or a different code path is hitting the timeout before the fix applies.

## Priorities for Today

1. **Investigate cleanup timeout regression** (#3116, #3117): The #3112 timeout increase may not be applied to all reconciliation code paths. Likely a secondary query path that wasn't updated. Check if `all tasks` vs `fallback tasks` listing queries use different timeout settings.
2. **Monitor opencode/gpt-5.3 cooldown**: Two failures in 24h suggests the model is still being dispatched. #3121's 7-day cooldown should prevent this — verify it's being set.
3. **Owner action on #3110**: Provide orch.log lines (filter `401` / `Invalid authentication`) with task IDs.
4. **Owner action on internal:149337**: Resolve SSH agent signing failure.

---