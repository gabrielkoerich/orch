+++
title = "Daily Review — 2026-08-31"
date = 2026-08-31
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-08-31

## The headline: two same-day root-cause-to-fix cycles on cooldown accuracy, PR #3536 still waiting on its next recovery window

**Window:** `2026-08-30T23:00Z → 2026-08-31T23:00Z`. Two commits landed, both closing issues filed and fixed within the same 24h window: `a64b7d88` (#3573, closes #3571) gives persistent model failures their own escalation counter so an intermittent success no longer resets a structurally unreliable model's backoff to zero, and `47d6b04e` (#3574, closes #3572) teaches the rate-limit path to parse relative usage windows (e.g. kimi's "5-hour usage limit") instead of falling back to the generic 5-minute-base backoff. Both fixes were triggered by real signal seen in `task_runs` this window — 4 kimi `rate_limit` outcomes citing the exact 5-hour-window message, and a documented pattern of an opencode review model's failure count getting wiped by isolated successes. `156854`/#3535/PR #3536 remains the sole blocked task in this repo (12 days); its auto-unblock counter advanced from 13 to 14 with an attempt recorded at `04:55 UTC` today, and per its 24h cooldown the next attempt lands around `2026-09-01T04:55Z`.

---

## What Shipped (Last 24h)

- **`a64b7d88` — give persistent model failures their own counter (#3573, closes #3571).** `record_persistent_model_failure()` and `record_model_failure()` previously shared the same `failure_count:{agent}:{model}` KV key, which `record_agent_success()` resets on every successful run. For a model with intermittent parse errors or truncated runs, a single success between failures reset the counter to zero, so the 4h→7d persistent backoff ladder never escalated past its base duration. Persistent failures now increment a separate `persistent_failure_count:{agent}:{model}` key; the generic counter still resets on success (preserving the #3478 fix for isolated transient failures), but the persistent counter now accumulates correctly across a real pattern of repeated structural failures.
- **`47d6b04e` — detect relative usage windows in rate-limit cooldown (#3574, closes #3572).** Kimi's "You've reached your 5-hour usage limit" message has no absolute retry timestamp, so `parse_retry_at()` returned `None` and the router fell back to generic 5-minute-base exponential backoff — retrying the same model well before its quota actually reset. A new `parse_relative_usage_window()` extracts "N-hour/minute/day", hourly, daily, weekly, and monthly windows from usage/quota/limit messages, and `record_rate_limit()` applies a model-specific cooldown for the advertised duration when a window is found, falling back to the existing generic path otherwise. Four kimi `rate_limit` runs in the 24h window before the fix (`2026-08-30T23:01` through `2026-08-31T10:01`) match exactly the pattern this fix addresses.

Both fixes follow the same shape: real observed failures in `task_runs` → root-caused same day → fixed same day, via the generic cooldown/classifier mechanisms rather than any per-model special-casing.

---

## Operational Health

### Task Run Outcomes (last 24h)

| Agent | Model | Outcome | Count |
|------|-------|---------|------:|
| claude | `sonnet` | `success` | 40 |
| opencode | `opencode/ling-3.0-flash-fin-free` | `success` | 10 |
| kimi | `opus` | `success` | 8 |
| opencode | `opencode/muse-spark-1.2-contributor-free` | `success` | 5 |
| kimi | `opus` | `rate_limit` | 4 |
| opencode | `opencode/nemotron-3.5-lightning-free` | `success` | 4 |
| opencode | `opencode/mimo-v2.5-free` | `success` | 3 |
| opencode | `opencode/nemotron-3-ultra-free` | `success` | 3 |
| opencode | `opencode/nemotron-3.5-lightning-free` | `parse_error` | 2 |
| claude | `sonnet` | (null / recovery) | 1 |
| claude | `sonnet` | `parse_error` | 1 |
| opencode | `opencode/muse-spark-1.2-contributor-free` | `timeout` | 1 |
| opencode | `opencode/nemotron-3.5-lightning-free` | `truncated` | 1 |

`task_activity` for the window: `status_change` 248, `dispatch` 81, `push` 73, `branch_delete` 64, `review_start` 40, `routed` 37, `review_decision` 34, `pr_create` 34, `error` 9, `rerouted` 3, `timeout` 1. The 4 kimi `rate_limit` outcomes are the exact signal that drove today's #3572/#3574 fix; the 2 opencode `parse_error` + 1 `truncated` on `nemotron-3.5-lightning-free` are the same pattern that drove #3571/#3573. Both classes of failure are now handled more precisely by today's fixes going forward — no further action needed on the historical instances, they already went through cooldown/reroute normally.

### `orch.error.log`

0 bytes — no crash since last restart.

### Routing and cooldowns

`orch cooldown list` shows four persisted, standard exponential-backoff cooldowns: `codex:gpt-5.4` (1d9h), `minimax:haiku` (9h59m), `minimax:opus` (2d23h), and `opencode:opencode/nemotron-3.5-lightning-free` (5h14m). Generic per-model cooldown system working as designed.

### Backlog and stuck work — PR #3536, next recovery cycle already fired, PR still unchanged

`156854`/#3535/PR #3536 remains the only blocked task in this repo (12 days). Its `auto_unblock_count` advanced from 13 to 14, with `auto_unblock_last_at = 2026-08-31T04:55:05Z` — confirming a recovery attempt fired on schedule this window. The PR's `headRefOid` and GitHub `updatedAt` are still unchanged since `2026-08-27T23:08:04Z`, and it remains `OPEN`. Required CI checks (`check`, `test`, `secrets`, `check-release`) are recorded as `success` on the current head commit. Per the 24h cooldown for `auto_unblock_count >= 4`, the next attempt is expected around `2026-09-01T04:55Z`.

### Policy alignment

Current operational behavior matches `prompts/skills/orch/SKILL.md` — no drift detected.

---

## Issues Filed Today

None. The two operational problems surfaced by this window's `task_runs` data (kimi relative-usage-window rate limits, opencode structural-failure counter reset) were both already root-caused and fixed same-day via #3571/#3573 and #3572/#3574 before this review ran. No other failure pattern in the window met the bar for a new issue.

---

## Priorities for Tomorrow

1. **Check whether `156854`/PR #3536 progressed after its `~04:55 UTC` recovery attempts.** Two cooldown-gated attempts (yesterday and today) have now fired against the poll-and-direct-merge recovery path; the PR head and GitHub state are still unchanged, which is worth a closer look if it remains unchanged after tomorrow's cycle too.
2. Confirm the two new cooldown-accuracy fixes (#3573, #3574) are holding up under real traffic — specifically that kimi 5-hour-window rate limits get a matching-duration cooldown instead of a 5-minute one, and that the opencode model's persistent failure count is no longer reset by isolated successes.
3. No other action items — routing, error rates, and task throughput all stayed within normal, already-handled patterns this window.

---

*Prepared by Orch automation (internal:162277) on 2026-08-31.*
