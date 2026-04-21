+++
title = "Morning Review — 2026-04-21"
date = 2026-04-21
description = "Daily operational check-in: 15 reliability fixes merged, watchdog stalls observed from LLM routing budget timeouts, service recovering, GLM and #2881 still pending."
+++

# Morning Review — 2026-04-21

## Recent Commits (last 24h)

15 commits focused on reliability and correctness:

- `ff93af99` fix(tasks): remove pre-emptive set_block_reason(None) before conditional update (#2892)
- `c1324329` fix(parser): remove dead best_status_known variable (#2891)
- `0be53d3c` bug: parser failures from opencode models (parse_error) and empty/invalid responses (#2887)
- `87dc514f` bug(sync): auto-merge CI failures can leave tasks blocked without reconciliation (#2886)
- `5a047047` fix(parser): tighten NDJSON candidate selection to reject bogus status values (#2885)
- `e0fde494` bug(sync): merged PRs can stay blocked indefinitely after CI-failure escalation (#2884)
- `c0005835` fix(discord_ws): wrap all websocket send operations with 10s timeout (#2878)
- `5a464178` bug(sync): NeedsReview refire escalation triggers after 4 refires instead of 5 (off-by-one) (#2872)
- `29dcf9a2` perf(router): Regex::new called in loop for static ASCII patterns (#2877)
- `cb6162b2` fix(store): propagate decode errors for created_at/updated_at (#2875)
- `29b22ff0` fix(patterns): map lowercased byte offsets back to original string (#2870)
- `945ce6a3` fix(router): use floor_char_boundary for safe UTF-8 slicing (#2869)
- `c7b88e0a` Review `.await` usage while holding Mutexes (#2868)
- `5aedd08a` fix(router): replace lowercased-index slicing with case-insensitive regex (#2866)

## Operational Health

### Service and logs

- **Watchdog stalls observed**: Two tick stalls detected (89s at 13:44 UTC, 79s at 13:45 UTC) — both triggered during LLM routing budget exhaustion (45s) + fallback to round-robin + task dispatch. The engine recovered automatically, but this pattern indicates LLM routing is timing out frequently.
- **LLM routing budget exceeded**: Multiple tasks fell back to round-robin in the last hour. This is causing tick delays that trigger the 60s watchdog threshold.
- **Kimi degraded**: Pre-emptive health check marked Kimi as degraded (agent in cooldown).
- `/opt/homebrew/var/log/orch.error.log` is stale (last modified 2026-04-19), no current-run errors.

### Task/run health (24h)

Outcomes (160 total):
- success: ~116
- failed: ~20
- rate_limit: ~5
- parse_error: ~3
- timeout: ~1
- empty outcome: ~3

Top agent/model outcomes:
- `minimax/opus`: 28 success (healthiest)
- `claude/sonnet`: 20 success, 3 failed, 1 timeout, 1 empty
- `opencode/minimax-m2.5-free`: 17 success, 3 failed, 1 empty
- `codex/gpt-5.3-codex`: 16 success, 2 failed, 2 empty
- `opencode/gpt-5-mini`: 15 success
- `glm/opus`: 8 success, 2 rate_limit

`task_activity` (12h): status_change (43), dispatch (18), branch_delete (14), routed (9), push (8), review_start (4), review_decision (4), pr_create (3), error (2), rerouted (1).

No broad engine stall — instability remains in routing timeouts and specific model lanes.

## Stuck / Blocked Work

- `#2789` (blocked): GLM artifact collection. Still waiting.
- `#2881` (new): task_runs.error stores raw api_retry JSON fragments, masking real error. Created 2026-04-20.

## Retro Follow-up Status (from 2026-04-20 evening)

1. Close #2831: **done** — decode-error/metrics sweep complete, issue should be closable.
2. File/fix #2881: **in progress** — issue was created yesterday, needs fix.
3. GLM investigation (#2789): **still blocked**, pending artifact collection.
4. Parse_error patterns: **still occurring** — 3 parse_errors in 24h from minimax/opus, nemotron, and minimax/free models.
5. `orch stream --pipe` validation: **still pending**.

## Tasks Waiting on Owner Feedback

- No open issues currently labeled `needs-feedback`.

## Priorities for Today

1. **Diagnose LLM routing budget timeouts**: Multiple watchdog stalls traced to 45s LLM routing budget exhaustion. Consider reducing budget or optimizing routing path.
2. **Fix #2881**: task_runs.error JSON fragment storage — quick sanitize needed.
3. **Investigate parse_error patterns**: 3 in 24h, need sample outputs to tighten parser further.
4. **Continue GLM investigation**: #2789 still blocked.

## Issue Creation

No new operational issues created in this review. #2881 was already created yesterday.

The service is functional but routing performance needs attention — the 45s LLM budget is causing cascading delays that trigger watchdog stalls.

---

Prepared by Orch automation (internal task internal:146650).