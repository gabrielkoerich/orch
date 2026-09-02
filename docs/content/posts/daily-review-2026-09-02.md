+++
title = "Daily Review — 2026-09-02"
date = 2026-09-02
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-09-02

## The headline: yesterday's three root-cause fixes all landed, but the long-blocked CI-failure PR is still stranded after 16 recovery cycles

**Window:** `2026-09-01T23:00Z → 2026-09-02T23:00Z`. All three issues opened yesterday and the day before closed with merges: #3582 wires the failover rate-limit path through `parse_relative_usage_window()`, #3585 stops successful agent runs from resetting review timeout/truncation cooldowns, and #3586 bails when no review agent has a configured review model. Meanwhile the only blocked task in this repo — `156854`/#3535/PR #3536 — advanced to `auto_unblock_count=16` without the PR actually merging. Its `mergeStateStatus` is back to `BEHIND` and its head has not moved since 2026-08-27. Filed as #3587.

---

## What Shipped (Last 24h)

- **`2ded4035` — apply relative-usage-window cooldown in in-task failover path (#3582, closes #3580).** The primary `AgentError::RateLimit` handling path in `src/engine/runner/fallback.rs` now calls `cooldown::parse_relative_usage_window()` and, when a window is parsed, applies `set_model_cooldown(agent, model, window_secs)` instead of the generic 5-minute-base exponential backoff. This closes the call-site gap left by #3572/#3574, which only wired the secondary `needs_review` path in `runner/mod.rs`. A regression test asserts the exact "weekly (7-day) usage limit" message produces a ~7-day model cooldown.
- **`a5cc2655` — use persistent cooldown for review timeout and truncation failures (#3585, closes #3583).** `src/engine/review.rs` now calls `record_persistent_model_failure()` instead of `record_model_failure()` on review timeout and `truncated_by_length`. The persistent counter is not reset by successful agent runs, so a model that is fit for coding but too slow/small for review can no longer be reselected after each successful coding run. This mirrors the existing parse-error path.
- **`43e48200` — bail when no review agent has a configured review model (#3586, closes #3584).** `select_review_agent()` now returns an error when the fallback agent has no review-tier model configured, instead of dispatching with `review_model = None` and letting the agent CLI default to an unsupported model (e.g., Codex defaulting to `gpt-5.4`). A defensive guard was also added in `build_review_context`. The missing-model case is classified as a deferred retry and does not count toward `MAX_REVIEW_AGENT_FAILURES`.
- **`45ef96da` — docs(posts): add daily review for 2026-09-01 (#3581).** Previous review post; it filed #3580, which #3582 closed today.

All three code fixes are minimal, follow the generic cooldown/routing design, and include tests.

---

## Operational Health

### Task Run Outcomes (last 24h)

| Agent | Model | Outcome | Count | Latest |
|------|-------|---------|------:|--------|
| claude | `sonnet` | `success` | 23 | 2026-09-02T21:54:04Z |
| opencode | `opencode/mimo-v2.5-free` | `success` | 8 | 2026-09-02T08:21:20Z |
| opencode | `opencode/ling-3.0-flash-fin-free` | `success` | 7 | 2026-09-02T10:07:42Z |
| kimi | `opus` | `rate_limit` | 6 | 2026-09-01T16:00:49Z |
| kimi | `opus` | `success` | 6 | 2026-09-02T21:49:12Z |
| opencode | `opencode/muse-spark-1.2-contributor-free` | `success` | 6 | 2026-09-02T10:05:08Z |
| claude | `sonnet` | `failed` | 4 | 2026-09-01T10:50:59Z |
| opencode | `opencode/nemotron-3-ultra-free` | `success` | 4 | 2026-09-02T21:43:10Z |
| opencode | `opencode/nemotron-3-ultra-free` | `timeout` | 2 | 2026-09-01T09:53:28Z |
| opencode | `opencode/nemotron-3.5-lightning-free` | `parse_error` | 2 | 2026-09-01T21:18:41Z |
| opencode | `opencode/nemotron-3.5-lightning-free` | `success` | 2 | 2026-09-02T16:10:58Z |
| codex | `gpt-5.4` | `failed` | 1 | 2026-09-02T08:21:02Z |
| codex | `gpt-5.5` | `timeout` | 1 | 2026-09-01T08:01:40Z |
| opencode | `opencode/ling-3.0-flash-fin-free` | `failed` | 1 | 2026-09-01T08:02:30Z |
| opencode | `opencode/nemotron-3-ultra-free` | `truncated` | 1 | 2026-09-01T09:10:54Z |

`task_activity`: `status_change` 233, `dispatch` 79, `push` 56, `branch_delete` 52, `routed` 38, `review_start` 35, `review_decision` 27, `pr_create` 26, `error` 15, `rerouted` 10, `timeout` 3. Activity volume is essentially flat versus yesterday.

### Notable failure patterns

- **kimi weekly-quota rate limits are pre-fix only.** All six `kimi|opUS|rate_limit` rows occurred before #3582 merged at 2026-09-01T23:25:23Z. The latest rate-limit event was 2026-09-01T16:00:49Z. No kimi rate-limit has occurred in the ~31 hours since the fix landed, so it is too early to confirm the cooldown now reaches 7 days, but the path is covered.
- **Claude OAuth expiry was transient.** Three `claude|sonnet|failed` runs between 08:01:01Z and 10:50:59Z failed with "OAuth session expired and could not be refreshed". Later claude runs succeeded, so the session was refreshed.
- **Codex review without review model was the last occurrence before #3586.** Task `internal:162386` review failed at 2026-09-02T08:21:02Z with `codex agent error: model unavailable (gpt-5.4)`. #3586 merged at 2026-09-02T21:57:33Z, so this class should stop.
- **opencode `nemotron-3.5-lightning-free` parse errors and `nemotron-3-ultra-free` timeout/truncation** are the same review-model-fit pattern #3585 now addresses.

### `orch.error.log`

0 bytes — no crash since last restart.

### Routing and cooldowns

`orch cooldown list` shows four persisted cooldowns: `codex:gpt-5.4` (6d9h), `kimi:haiku` (3h58m), `minimax:haiku` (10h16m), `minimax:opus` (23h2m). The `kimi:haiku` cooldown is residual from the pre-#3582 generic backoff; once a fresh kimi weekly-quota event fires, the new path should produce a ~7-day `kimi:{model}` cooldown instead.

### Backlog and stuck work — PR #3536 now at 16 auto-unblock cycles, still unmerged

`156854`/#3535/PR #3536 remains the only blocked task, now 14 days old. `auto_unblock_count` advanced to 16 (from 15 yesterday), with the most recent recovery attempt at `2026-09-02T04:55:16Z`. Live PR state:

- `state`: `OPEN`
- `mergeable`: `MERGEABLE`
- `mergeStateStatus`: `BEHIND`
- `headRefOid`: `7ac37b94fa6893ced845383d8501f02603e3b883`
- `updatedAt`: `2026-08-27T23:08:04Z` (unchanged since the last branch update)

The recovery code added by #3568/#3569 calls `update_pr_branch`, re-enables auto-merge, and spawns `poll_and_merge_recovered_pr` with a direct-merge fallback. Yet the PR head has not moved since 2026-08-27 and the status is `BEHIND` again. This means either `update_pr_branch` is silently failing for this PR, or it succeeds and then `main` moves again before the poll/merge completes, but the head timestamp should still advance in the latter case. Filed as #3587 for root-cause investigation.

### Slow tick

One `slow tick elapsed_ms=61277` warning at 2026-09-02T23:01:19Z, while dispatching `internal:162450` to kimi. This is a single event in the 24-hour window and does not yet indicate a pattern.

### Policy alignment

Current operational behavior matches `prompts/skills/orch/SKILL.md` — no drift detected.

---

## Issues Filed Today

- **#3587** — PR #3536 remains `BEHIND` and unmerged after 16 CI-failure recovery cycles, despite the recent `update_pr_branch` + direct-merge fallback improvements. The recovery sweep increments `auto_unblock_count` and appears to run, but the PR head has not moved since 2026-08-27. Root cause needs investigation (silent `update_pr_branch` failure, or poll/merge fallback not completing).

---

## Priorities for Tomorrow

1. **Watch for a fresh kimi weekly-quota rate-limit event and confirm #3582 produces a ~7-day model cooldown.** The fix is merged but not yet exercised in the post-merge window.
2. **Follow #3587 to root-cause why PR #3536 is not recovering.** Check whether `update_pr_branch` is returning errors that are not being surfaced, or whether the background poll/merge task is exiting early against `BEHIND` without retrying within the same cycle.
3. **Confirm #3586 eliminates Codex review runs with `review_model = None`.** Tomorrow's reviews should no longer show `codex|gpt-5.4|failed` with "model metadata not found".

---

*Prepared by Orch automation (internal:162449) on 2026-09-02.*
