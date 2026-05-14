++
title = "Morning Review — 2026-05-14"
date = 2026-05-14
description = "Daily operational morning review."
+++

# Morning Review — 2026-05-14

## Recent Commits (last 24h)

| Hash | Message |
|------|---------|
| `ca412fe2` | docs(posts): add morning review 2026-05-14 (internal:149564) |
| `bdaf59ac` | docs(posts): add evening retrospective 2026-05-13 (internal:149544) (#3126) |
| `d0fc7221` | fix(review): guard detect_rate_limit against terminal_reason:completed NDJSON (#3125) |

Notable: #3125 fixes NDJSON misclassification (reduces false rate-limit markings). Recent docs updates added the evening retro and this morning post.

## Operational Summary

Service is running. Key items:

- Cleanup/reconciliation timeout WARNs have returned despite earlier fixes (#3112). Two issues track this: #3116 and #3117.
- Two external issues remain blocked and awaiting owner action: #3110 (Claude 401 auth) and internal:149337 (SSH agent signing failure blocking fetchs).
- Routing and task_runs look healthy overall; most agent/model combinations are succeeding. A small number of single-count failures remain (opencode/gpt-5.3, opencode/claude-opus variants).

## Health Checks

### Cleanup Timeout — Regression

Despite #3112 increasing the reconciliation timeout from 5s to 30s and deduplicating fallback logic, `timed out listing all tasks for closed-issue reconciliation` and `timed out listing fallback tasks for closed-issue reconciliation` WARNs are firing again in recent logs. The code location is `src/engine/cleanup.rs` where `RECONCILIATION_LIST_TIMEOUT` is defined as 30s, but evidence suggests not all reconciliation query paths are using it.

Observed behavior: WARNs recur every tick and add ~1.5s to sync cycles (non-critical but degrading performance). Issues #3116 and #3117 were filed to track the two affected query paths (`all tasks` and `fallback tasks`).

### Stuck / Blocked Tasks

| ID | Status | Agent | Blocked On |
|----|--------|-------|------------|
| internal:149337 | blocked | minimax | SSH agent signing failure (git fetch) — owner action required |
| #3110 | blocked | opencode | Owner action required: provide orch.log lines (filter `401` / `Invalid authentication`) and task IDs for triage |
| #3116 | open | — | cleanup reconciliation `all tasks` timeout regression |
| #3117 | open | — | cleanup reconciliation `fallback tasks` timeout regression |

### task_runs Summary (last 24h)

```
opencode|github-copilot/gpt-5-mini|success|13
kimi|opus|success|9
opencode|github-copilot/claude-sonnet-4.6|success|9
claude|sonnet|success|8
minimax|opus|success|7
glm|opus|success|6
codex|gpt-5.3-codex|success|5
opencode|github-copilot/gpt-5.4|success|5
opencode|github-copilot/gpt-5-mini||2
claude|sonnet||1
kimi|opus|failed|1
kimi|opus|rate_limit|1
minimax|opus||1
opencode|github-copilot/claude-opus-4.6|failed|1
opencode|github-copilot/gpt-5.3|failed|1
opencode|github-copilot/gpt-5.4||1
```

Routing is healthy. Failures are low and single-count. opencode/gpt-5.3 shows failures — persistent cooldown (#3121) should prevent repeat dispatches; verify the cooldown is present in rate_limits KV.

### Error Log

`/opt/homebrew/var/log/orch.error.log`: empty (0 bytes). No actionable errors in stderr.

## Retro Follow-ups

- **#3110 still blocked**: owner has not added log context. Blocked until owner provides reproduction details and `orch.log` excerpts containing `401` lines.
- **internal:149337 SSH issue**: not resolved. Owner action required (fix SSH agent or use HTTPS remote).
- **#3125 fix deployed**: NDJSON/`terminal_reason:completed` guard applied to `detect_rate_limit`.
- **#3121 fix deployed**: persistent model cooldown for permanently-gone models deployed; monitor for recurrence.
- **#3112 cleanup timeout fix**: regression observed. #3116 and #3117 filed. Likely cause: not all reconciliation query paths updated to use the longer timeout constant (`RECONCILIATION_LIST_TIMEOUT`). The code location is `src/engine/cleanup.rs`; verify all calls that list backend tasks use the same timeout.

## Priorities for Today

1. **Investigate cleanup timeout regression** (#3116, #3117): determine which query path still uses the old 5s timeout (compare code paths that call `backend.list_all_tasks()` vs `backend.list_reconciliation_candidates()` and confirm both are guarded by `RECONCILIATION_LIST_TIMEOUT`). Also check for any separate timeout constants elsewhere.
2. **Verify persistent cooldowns** (#3121): confirm `record_persistent_model_failure` set the 7-day cooldown for bad models and that router skipping logic respects it (inspect `rate_limits`/KV and recent `task_runs`).
3. **Owner actions**: request orch.log excerpts for #3110 and SSH/key rotation for internal:149337.
4. **Audit NDJSON guards**: ensure classifiers (`detect_rate_limit`, `detect_silence`, `classify_error`) all strip/inspect NDJSON envelopes consistently.

---

Prepared by Orch automation (internal:149564).
