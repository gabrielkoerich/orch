+++
title = "Daily Review — 2026-09-01"
date = 2026-09-01
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-09-01

## The headline: yesterday's relative-usage-window fix only covers one of two rate-limit call sites — kimi burned 10 wasted runs today against a 7-day quota that got minutes-to-hours cooldowns instead

**Window:** `2026-08-31T23:00Z → 2026-09-01T23:00Z`. Two runner fixes landed and closed same-window issues (#3578 closes #3576, timeout classification now records a model-specific cooldown; #3579 stops a clean `AgentFailed` message from being garbled by a 300-byte tail-truncation fallback). Investigating this window's `task_runs` turned up a gap in yesterday's own fix: kimi hit its account-wide "weekly (7-day) usage limit" five times today (08:20–16:00 UTC), and 10 of its 18 runs in the window ended `rate_limit` — but the resulting cooldowns (checked directly in KV) were on the order of minutes to ~17 hours, not the 7 days the message advertises. Filed as #3580: `src/engine/runner/fallback.rs`'s `AgentError::RateLimit` handling (the path that fires during in-task failover, before a task reaches `needs_review`) never calls the `parse_relative_usage_window()` parser #3572/#3574 added — that parser is wired only into the secondary `runner/mod.rs:1288` call site. `156854`/#3535/PR #3536 remains blocked (13+ days now), unchanged since 2026-08-27T23:08:04Z despite two auto-unblock cycles firing since yesterday's review.

---

## What Shipped (Last 24h)

- **`01cfd73c` — record model-specific cooldown on timeout classification (#3578, closes #3576).** The `Timeout` arm in `handle_error()` was the only classification in its match block that skipped `record_model_failure()`, so a model that just burned a full agent timeout could be immediately reselected on the next attempt. Task 162289 hit the same model timeout twice, 90 minutes apart, wasting ~60 minutes of its 2.5h lifecycle. Mirrors the existing `InvalidResponse`/`AgentFailed` handling and the equivalent review-path fix from #3307/#3310.
- **`b5f3192b` — stop discarding clean claude error text behind a byte-truncated tail (#3579, closes #3577).** `classify_error()` already extracts the full `result` text from a well-formed `type:result`/`is_error:true` NDJSON envelope, but was re-scanning the combined stdout+stderr through `classify_from_text()`, whose generic-Unknown fallback truncates to the last 300 bytes — landing mid-field-name (`"is_error"` → `"i_error"`) once enough preceding NDJSON pushed the result line past that window. Confirmed against the live DB: task 162287's "malformed" NDJSON was a complete, valid envelope; the garbling was `safe_tail(text, 300)` cutting a well-formed line, not transport corruption. Now prefers the already-extracted clean message via `AgentFailed` when the specific detectors don't match.

Both fixes follow the repo's usual shape: real observed failures in `task_runs` → root-caused same day → fixed same day via the generic classifier/cooldown mechanisms.

---

## Operational Health

### Task Run Outcomes (last 24h)

| Agent | Model | Outcome | Count |
|------|-------|---------|------:|
| claude | `sonnet` | `success` | 23 |
| kimi | `opus` | `rate_limit` | 10 |
| kimi | `opus` | `success` | 8 |
| opencode | `opencode/mimo-v2.5-free` | `success` | 8 |
| opencode | `opencode/muse-spark-1.2-contributor-free` | `success` | 8 |
| opencode | `opencode/ling-3.0-flash-fin-free` | `success` | 6 |
| opencode | `opencode/nemotron-3-ultra-free` | `success` | 6 |
| claude | `sonnet` | `failed` | 4 |
| claude | `sonnet` | (null / recovery) | 2 |
| opencode | `opencode/nemotron-3-ultra-free` | `timeout` | 2 |
| opencode | `opencode/nemotron-3.5-lightning-free` | `parse_error` | 2 |
| codex | `gpt-5.5` | `timeout` | 1 |
| opencode | `opencode/ling-3.0-flash-fin-free` | `failed` | 1 |
| opencode | `opencode/nemotron-3-ultra-free` | `truncated` | 1 |
| opencode | `opencode/nemotron-3.5-lightning-free` | `success` | 1 |

`task_activity`: `status_change` 249, `dispatch` 86, `push` 60, `branch_delete` 50, `routed` 40, `review_start` 37, `review_decision` 29, `pr_create` 29, `error` 18, `rerouted` 13, `timeout` 3. Throughput and review cycle counts are in the normal range for this window.

### The kimi weekly-quota gap (see headline, filed as #3580)

Kimi's rate_limit count (10/18 runs, 56%) is the standout anomaly this window. The error text — "You've reached your weekly (7-day) usage limit. Your quota will reset when the current 7-day window expires." — appeared 5 separate times across 8 hours (08:20, 08:32, 09:23, 11:38, 16:00 UTC), each time getting re-tried instead of skipped. Root cause confirmed by reading `fallback.rs`: the `AgentError::RateLimit` arm there (not `runner/mod.rs`) is the code that actually fires during in-task failover, and it goes straight to `record_agent_failure_with_message()` + `record_model_failure()` (generic exponential backoff) without ever checking for a relative usage window. PR #3574's diff only touched `cooldown.rs` and `runner/mod.rs` — `fallback.rs` was out of scope for that fix. Both `kimi:opus` and `kimi:haiku` show short-duration cooldowns from the same underlying account-wide quota, confirming the model-scoping (not just the duration) needs a look once #3580 lands.

### `orch.error.log`

0 bytes — no crash since last restart.

### Routing and cooldowns

`orch cooldown list` shows six persisted, standard cooldowns: `codex:gpt-5.4` (9h10m), `kimi` (16h59m), `kimi:haiku` (1h58m), `minimax:haiku` (1d10h), `minimax:opus` (1d23h), `opencode:opencode/nemotron-3.5-lightning-free` (10h47m). The `kimi`/`kimi:haiku` entries are the generic-backoff byproduct of #3580 above — expected to look different once the fix lands and the account-wide weekly window is respected.

### Backlog and stuck work — PR #3536, two more recovery cycles fired, still no movement

`156854`/#3535/PR #3536 remains the only blocked task in this repo, now 13 days old. `auto_unblock_count` advanced to 15 (from 14 yesterday), with the most recent attempt at `2026-09-01T04:55:15Z` — confirming the scheduled recovery cycle is still firing on time. The PR's `headRefOid` and GitHub `updatedAt` are unchanged since `2026-08-27T23:08:04Z`; required checks (`check`, `test`, `secrets`, `check-release`) all show `SUCCESS` on the current head (the `CANCELLED` entries in the rollup are superseded re-runs, not failures). `mergeStateStatus` reports `UNKNOWN`. This has now gone a full week without the auto-unblock cycle producing any visible change to the PR — worth a closer look at whether the recovery path (`is_billing_failure()` / merge retry) is actually attempting a merge or just polling and finding the same blocked state each time.

### Policy alignment

Current operational behavior matches `prompts/skills/orch/SKILL.md` — no drift detected.

---

## Issues Filed Today

- **#3580** — `fallback.rs`'s rate-limit handling bypasses the relative-usage-window parser added in #3572/#3574, so kimi's account-wide weekly quota exhaustion gets minutes-to-hours generic backoff instead of a cooldown matching the advertised 7-day reset. Root-caused to the exact missing call site; see above.

---

## Priorities for Tomorrow

1. **Verify #3580 lands and confirm kimi's weekly-quota cooldown then matches the advertised window** (spot-check `cooldown:kimi:opus`/`cooldown:kimi:haiku` KV values after the next rate-limit event — should jump to ~7 days out, not minutes/hours).
2. **PR #3536 has now gone a full week (`2026-08-27` → `2026-09-01`) with the auto-unblock cycle firing daily but no observable change to the PR head or merge state.** If tomorrow's cycle also produces no movement, this is worth root-causing rather than continuing to just observe — is the recovery path actually attempting anything, or silently no-op'ing against an already-resolved billing condition?
3. Confirm yesterday's cooldown-accuracy fixes (#3573, #3574) are holding for the code paths they do cover (the `needs_review` path) — no new evidence either way this window since kimi's failures all routed through the uncovered `fallback.rs` path.

---

*Prepared by Orch automation (internal:162354) on 2026-09-01.*
