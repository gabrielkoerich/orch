+++
title = "Daily Review — 2026-09-11"
date = 2026-09-11
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-09-11

## What Shipped (Last 24h)

- **#3611** (merged 2026-09-10T23:47Z, closing **#3609**) — added persistence for `ci_pending_since` in `src/engine/auto_merge.rs`, using `kv_insert_if_absent` to survive across `auto_merge_pr`'s many short-lived invocations, plus a TOCTOU-safe guard for overlapping invocations and 2 new regression tests.

A quiet day otherwise — only 2 commits landed in the window, both continuing yesterday's `ci_pending_since` investigation.

---

## What Failed / Investigated

**PR #3600 (task 3599) is still stuck `in_review` — now 73h40m+.** Same PR flagged in the last two daily reviews. Live investigation this session found the `#3609` fix has not yet resolved it:

- The KV store has **no `ci_pending_since:3599` key** right now, even immediately after a freshly-logged `state=pending` CI check at 23:01:46Z that should have written it (the sibling `ci_check_ts:3599` key updated correctly at the same instant, confirming the write path itself works).
- Reading `src/engine/auto_merge.rs` on current `HEAD` confirms the fix is real and correctly placed: the `_ => ` (pending) arm calls `ci_pending_elapsed`, which calls `kv_insert_if_absent`. Per the "fix exists on `HEAD`, evaluate future runs against it" policy — noting this and stopping here rather than treating it as a live bug or a deployment question.
- The GitHub-side root cause from the last two reviews is still present: the `secrets` required check-run from run `34279902464` has been wedged `IN_PROGRESS` since 2026-09-08T21:20:09Z. `gh pr view` now also reports `mergeStateStatus: BEHIND` — the branch is behind `main` — though that's moot until the pending check resolves.
- No new issue filed for this — it's the exact scenario #3609 already covers.

**Root-caused a new bug: internal-task review sessions almost always get killed by stuck-task recovery.** Traced every internal task that reached `review_start` in the last 7 days (6 tasks, including yesterday's own daily-review task, 168988) — **all 6** had their review tmux session killed by stuck-task recovery ~10-21 minutes in, followed 1-5 seconds later by the real `approve` decision arriving. This is the exact race #2597 fixed months ago (review agent exits tmux on completion, before its result is delivered; a too-short no-session timeout beats the result to the punch) — but that fix (a 1800s `in_review_no_session_stuck_timeout` override) was only applied to the external-task recovery loop in `tick.rs`. The internal-task loop a few dozen lines below still uses the raw 600s `no_session_stuck_timeout`. Functionally low-impact (the re-dispatched review also approves within seconds, so tasks still land), but it means every internal review effectively runs twice and pollutes `task_activity` with misleading `error`/`aborted` entries. Filed as **#3612** with the full per-task timing table.

**opencode, claude, kimi had a clean day**: 26 claude/sonnet successes, 4 kimi/opus successes, only 2 codex model-unavailable failures (pre-dating the already-fixed #3608).

---

## Operational Health

### Task run outcomes (last 24h)

| Agent | Model | Outcome | Count |
|------|-------|---------|------:|
| claude | `sonnet` | `success` | 26 |
| kimi | `opus` | `success` | 4 |
| opencode | `opencode/nemotron-3-ultra-free` | `success` | 4 |
| opencode | `opencode/muse-spark-1.2-contributor-free` | `success` | 3 |
| opencode | `opencode/ling-3.0-flash-fin-free` | `success` | 2 |
| opencode | `opencode/nemotron-3.5-lightning-free` | `success` | 2 |
| codex | `gpt-5.4` | `failed` | 1 |
| codex | `gpt-5.5` | `failed` | 1 |
| opencode | `opencode/mimo-v2.5-free` | `success` | 1 |
| opencode | `opencode/muse-spark-1.3-contributor-free` | `success` | 1 |
| claude | `sonnet` | (in progress) | 1 |

47 runs, 44 clean successes, 2 pre-#3608 codex model-unavailable failures, 1 in-progress. Volume down slightly from yesterday (51→47), consistent with a quiet day.

`task_activity` (last 24h): `status_change` 155, `branch_delete` 66, `dispatch` 57, `push` 43, `routed` 28, `review_start` 18, `review_decision` 17, `pr_create` 17, `error` 5, `rerouted` 1.

The 5 `error` events: 3 are the internal review stuck-recovery race described above (now #3612), 2 are pre-#3608 codex model-unavailable failures already root-caused.

### Routing and cooldowns

`orch cooldown list`: `minimax:haiku` (1d23h, extended-tier backoff), `minimax:opus` (2d13h, extended-tier backoff). Both tracking as designed — `minimax:haiku` has accumulated 34 recorded failures (mostly router LLM classification-pool timeouts, e.g. today's own routing of this task timed out on `minimax:haiku` and fell back cleanly to weighted round-robin → `claude`), which is exactly the documented extended-tier exponential backoff (tested behavior in `cooldown.rs`, not a bug). No new or anomalous cooldowns.

One isolated watchdog stall + 88s slow tick at 23:01:33Z, coinciding with two jobs (`daily-review`, `evening-retrospective`) dispatching in the same tick and each synchronously creating a worktree + tmux session. Single occurrence, self-resolved, consistent with the long history of already-fixed watchdog/slow-tick issues (#2676, #3095, #3048, etc.) — not a new pattern, no action taken.

`/opt/homebrew/var/log/orch.error.log` is 0 bytes — nothing to report.

### Backlog and stuck work

- **#3599 / PR #3600** — still stuck, now 73h40m+, unchanged root cause from the last two reviews (see above).
- One long-standing blocked task outside this repo (downstream task, 75 days, GitHub Actions billing failure at merge time) — correct per-task boundary per settled policy, no action needed.

### Policy alignment

Behavior matches `prompts/skills/orch/SKILL.md`: no brew/version-drift recommendations, no config file edits, no manual `orch task retry`/`unblock` suggested. Checked `gh issue list --state open/closed` and `git log` before filing #3612 — confirmed it's not a re-file of #2597 (which fixed the same race for the external-task loop only) or any other closed issue.

---

## Issues Filed Today

- **#3612** — internal-task stuck-review recovery uses the 600s default threshold instead of the 1800s `in_review_no_session_stuck_timeout` override, so it fires on nearly every internal review (6/6 in the last 7 days) 1-5 seconds before the real approval arrives. Includes a full per-task timing table and the exact code divergence from the external-task loop.

---

## Priorities for Tomorrow

1. **Land #3612** — one-line-ish fix (reuse the same `in_review_config` override pattern already proven correct for external tasks), low risk, clear regression test target (assert the internal loop's threshold equals `in_review_no_session_stuck_timeout`).
2. **Keep watching #3599 / PR #3600** — the GitHub-side zombie `secrets` check-run is now 3+ days old; if `ci_pending_since` still isn't observed persisting on the next check, that's worth a fresh live KV probe rather than assuming the #3609 fix is broken.
3. **No other action needed** — minimax cooldowns tracking down as expected, opencode/claude/kimi volume normal, no new error patterns.

---

*Prepared by Orch automation (internal:169056) on 2026-09-11.*
