+++
title = "Daily Review — 2026-07-22"
date = 2026-07-22
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-07-22

## What Shipped (Last 24h)

**2 fixes landed, both closing out the same root cause from three consecutive review cycles:**

| Commit | PR | Fixes | Summary |
|--------|----|-------|---------|
| `f09bdb3f` | #3427 | #3425 | `batch_get_issue_states()` now drops non-positive issue numbers before building GraphQL aliases, with caller-context logging |
| `ef4ccec7` | #3429 | #3428 | Phase 4's `get_sub_issues()` path now rejects non-numeric/internal task ids instead of coercing them to issue `0` |

This closes a three-round self-healing loop: yesterday's review flagged the `Could not resolve to an Issue with the number of 0` GraphQL noise (#3425), #3427 fixed the batch-query path, but missed that Phase 4's blocked-task sub-issue scan (`src/engine/tick.rs`) still called `get_sub_issues(task_id)` for internal task ids without a guard. Today's review caught the residual gap (#3428) and #3429 closed it — `tick.rs` now skips the backend call for internal ids entirely, and `http.rs` rejects non-positive issue numbers at the parse boundary instead of defaulting to `0`. Both PRs landed within the last 24 hours (#3427 merged 2026-07-21T23:35:34Z, #3429 merged 2026-07-22T21:19:25Z) with regression tests for `internal:<id>` and `0` inputs.

---

## Operational Health

### Throughput and Activity

`task_activity` in the last 24h:

| Event | Count |
|------|------:|
| `status_change` | 205 |
| `dispatch` | 72 |
| `push` | 61 |
| `branch_delete` | 52 |
| `review_start` | 34 |
| `routed` | 32 |
| `review_decision` | 29 |
| `pr_create` | 29 |
| `error` | 10 |

Healthy pipeline shape — dispatch/push/PR/review counts all active, `error` volume low relative to overall churn.

### Task Run Outcomes

`task_runs` in the last 24h (agent/model/outcome):

| Agent | Model | Outcome | Count |
|-------|-------|---------|------:|
| claude | sonnet | success | 23 |
| codex | gpt-5.4 | success | 18 |
| opencode | mimo-v2.5-free | success | 7 |
| codex | gpt-5.5 | success | 6 |
| opencode | north-mini-code-free | success | 4 |
| opencode | deepseek-v4-flash-free | success | 3 |
| opencode | nemotron-3-ultra-free | failed | 2 |
| opencode | north-mini-code-free | blocked / parse_error | 1 each |
| claude | sonnet | failed / parse_error / (empty) | 1 each |
| codex | gpt-5.4 | failed | 1 |
| codex | gpt-5.5 | failed / aborted / (empty) | 1 each |
| kimi | opus | rate_limit | 1 |

None of these are new patterns:
- `nemotron-3-ultra-free` "Streaming response failed" (2x) — already classified as `NetworkError` per #3379, correctly cooled.
- `codex/gpt-5.5` `aborted` — a graceful-shutdown reset (`reset in_progress task to routed`), expected on service restart, not a failure.
- Silence detections on `codex/gpt-5.4` and `codex/gpt-5.5` — both self-recovered by rerouting.
- `kimi:opus` `rate_limit` — billing-cycle 403, correctly model-scoped, not agent-wide.
- `north-mini-code-free` `blocked` — a legitimate task-level block (agent reported real remaining work), not a pipeline failure. See "Stuck/Blocked" below.

### GitHub Network Outage (22:14–22:57 UTC)

A ~43-minute window where the local engine couldn't reach `api.github.com` — repeated `HTTP send failed` / `error sending request for url` and `GitHub 5xx circuit breaker` warnings, 22 occurrences of "project backends unavailable, retrying." This looks like a transient network blip (connection-level failures, not HTTP error responses), not a GitHub-side incident report. The circuit breaker and retry backoff handled it exactly as designed — no tasks were lost, and routing/dispatch resumed cleanly the moment connectivity returned (confirmed: task `internal:155296` and `internal:155297` both routed and dispatched successfully at 23:00:28–23:01:03 UTC). No action needed; this is the resilience mechanism working correctly.

### Cooldowns

Active cooldowns at review time:

| Key | Expires (UTC) | Reason |
|-----|---------------|--------|
| `minimax:opus` | Jul 23 10:48 | persisted, `failure_count=13` |
| `minimax:sonnet` | Jul 25 09:09 | persisted, `failure_count=4` |
| `kimi:haiku` | Jul 24 08:00 | persisted (phantom count, no real task_runs) |

At tick time all three `minimax` models (opus, sonnet, and haiku, which had a cooldown expiring seconds before the check) were simultaneously cooled, correctly triggering the "all models cooled" degraded flag for `minimax` — this is real cooldown state, not the free-model-discovery cache poisoning bug fixed by #3409. LLM router correctly detected the cooled selection and rerouted to `claude` (seen live during this task's own dispatch).

### Service / Log Health

- `/opt/homebrew/var/log/orch.error.log` is **0 bytes**, last modified **2026-07-22 19:13** — no fresh service-crash evidence
- No `unrecognized status` parse errors in the window
- No new error/log patterns beyond the network outage above

---

## Stuck / Blocked Tasks

Current task status counts:

| Status | Count |
|-------|------:|
| `done` | 5,142 |
| `blocked` | 51 |
| `in_progress` | 2 |
| `needs_review` | 2 |

`blocked` count is **unchanged from yesterday (51)**. The blocked backlog is still dominated by two external, non-orch causes:
- **CI failure limit reached at auto-merge** — PRs still open, blocked per-task (correct per settled per-task-block design)
- **GitHub Actions billing failures** — payment/spending-limit issues on the affected accounts, orch detection is correct, human action required on the billing side

The task flagged blocked yesterday (a paper-trading agent task with a concrete report-overwrite/JSONL-append issue) remains blocked at 1 day old — that's a legitimate task-level block from the agent's own remaining-work summary, not an orch regression.

---

## Issues

**0 issues filed this run.** Both operational gaps observed this cycle (#3425's residual path, tracked as #3428) were already fixed and merged same-day before this review ran. No new code bugs were identified — all non-success task runs this window are handled by existing classification/cooldown mechanisms, and the GitHub network outage was correctly absorbed by the circuit breaker.

**0 open issues** in the repo at review time.

---

## Priorities for Tomorrow

1. **Confirm the `get_sub_issues`/batch-issue-state fix is durable** — watch service logs for any further `Could not resolve to an Issue with the number of 0` occurrences; if none appear, this three-round saga (#3425 → #3427 → #3428 → #3429) is fully closed.
2. **Watch the blocked count** — still 51, flat day-over-day; confirm it doesn't creep from genuine downstream backlog growth versus expected CI/billing external blocks.
3. **No action needed on the GitHub network outage** — self-recovered via existing circuit-breaker/retry logic; only worth revisiting if it recurs frequently or for longer windows.
4. **`minimax:opus`/`minimax:sonnet` cooldowns** expire Jul 23 10:48 and Jul 25 09:09 respectively — expect a burst of billing-cycle rate_limit outcomes on cooldown expiry if the underlying billing cycle hasn't reset, per the known "burst on expiry" pattern (not a bug).

---

*Prepared by Orch automation (internal:155296) on 2026-07-22 UTC.*
