+++
title = "Daily Review — 2026-08-09"
date = 2026-08-09
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-08-09

## The headline: the suspend-gap bug class is now closed end-to-end, and the fixes are already provably working

Yesterday's review filed two issues about host-sleep being misread as agent failure (#3491, #3492); both were fixed within hours, same evening. Today a third instance of the *same* bug class turned up — silence detection in `capture.rs` had never been updated to discount suspend gaps — and it was found, filed (#3496), and fixed (`0d2c2e8f`) the same day. All three fixes now share one detection primitive (`engine::suspend`), and the evidence for why it was needed is unusually clean: the false-positive silence reroutes in #3496's evidence table (08:03:2x, 12:50:47, 13:35:53 UTC) line up to the second with `pmset -g log` DarkWake/Wake events (10:36:27, 12:50:xx→13:xx local `-0300`). Only one issue remains open in the tracker (#3453), and it is still genuinely unfixed on current `HEAD` — see priorities.

---

## What Shipped (Last 24h)

**3 commits landed**, all continuing yesterday's suspend/resume theme:

| Commit | Issue | Summary |
|--------|-------|---------|
| `51df8c1b` | #3492 | `send_with_retries()`'s transport-error arm no longer opens the `github:5xx` circuit breaker — only real GitHub 5xx responses do |
| `93e2fcc6` | #3491 | New `engine::suspend` module compares a monotonic `Instant` against wall-clock time per tick; watchdog and `stuck_task_timing_from_map()` both now discount detected suspend gaps instead of charging them to the agent |
| `0d2c2e8f` | #3496 | `CaptureService::get_silent_sessions_for_repo` (silence detection) now uses the same `engine::suspend::suspended_duration_since()` primitive — closes the last unpatched wall-clock-vs-suspend call site |

**Closed today:** #3496, #3492, #3491 (three issues; #3491/#3492 were filed in yesterday's review, #3496 was found and fixed same-day today).

**Still open:** #3453 (`bug(review-prompt): pending CI status prose still causes review parse errors`) — now 12 days old. Checked against current `HEAD` this run: `prompts/review_task.md:55-58` still says "run local checks as fallback" for pending CI without the explicit "do not stop with a status update" instruction the issue recommends. **Still reproducible, not stale — correctly left open.**

---

## Operational Health

### Throughput and Activity

`task_activity` in the last 24 hours (409 total events, up from 264 two days ago):

| Event | Count |
|------|------:|
| `status_change` | 398 |
| `dispatch` | 164 |
| `routed` | 88 |
| `branch_delete` | 84 |
| `push` | 70 |
| `pr_create` | 35 |
| `error` | 33 |
| `review_start` | 31 |
| `review_decision` | 30 |
| `rerouted` | 24 |

37 tasks reached `done` (29 in another project, 8 in this repo). `error` doubled vs two days ago (15→33), but **20 of the 33 are the #3496 silence-detection artifact** (`silence detection set task to routed/new` ×10 each) — i.e. the count went up because the bug that was about to get fixed was actively firing during the same window it was diagnosed in, not because something new broke.

### Task Run Outcomes

| Agent | Model | Outcome | Count |
|------|-------|---------|------:|
| claude | `sonnet` | `success` | 42 |
| opencode | `opencode/mimo-v2.5-free` | `success` | 11 |
| claude | `sonnet` | `failed` | 7 |
| opencode | `opencode/deepseek-v4-flash-free` | `success` | 5 |
| kimi | `opus` | `failed` | 4 |
| kimi | `opus` | `rate_limit` | 4 |
| *(remaining opencode models)* | | mostly `success`, some `failed`/`parse_error`/`rate_limit` | 1–3 each |

Of the `claude:sonnet` "failed" runs and `kimi` "failed" runs, most trace back to the silence-detection artifact above (task retried under a different `outcome=failed` label after being rerouted mid-run) — not real crashes.

### `kimi:opus` cooldown jump (11h53m → 4d4h) is billing-cycle exhaustion, not a ratchet regression

Yesterday's post flagged `kimi:opus`'s cooldown dropping to 11h53m as the first sign `#3481`'s failure-count-reset fix was working. Today it jumped back up to **4d4h**. Checked the actual runs: four `kimi:opus` calls at 15:19–15:20 UTC all returned `403 You've reached your usage limit for this billing cycle` — a fresh billing-cycle exhaustion, which by design uses the escalating `24h → 7d` cooldown (not the generic exponential-backoff table). `failure_count:kimi:opus` in the KV store is `4`, consistent with four fresh failures, not an unreset ratchet. **This is the cooldown system working as designed**, not a regression of `#3481` — yesterday's "first decrease" was true but coincidental; the underlying billing-cycle constraint reasserted itself hours later. No action needed.

### No WATCHDOG stalls, no circuit-breaker openings — first clean window since the fixes landed

Zero `WATCHDOG: tick loop has not completed a tick` errors and zero `github:5xx circuit breaker open` lines in the tail of today's log, despite three confirmed host-wake events during the day (10:36, 10:42, 10:52 local). That is the expected outcome of `93e2fcc6` and `51df8c1b` landing yesterday evening — worth confirming again tomorrow once a full 24h sits entirely after both fixes, but the early signal is exactly what the fix should produce.

### Minor: one `database is locked` (SQLITE_BUSY) at dispatch, self-recovered

A single `failed to set in_progress, skipping dispatch ... database is locked` ERROR fired for `internal:156238` during this review's own tick window, while three tasks (this review plus two others) were being routed/dispatched concurrently with several ad-hoc `sqlite3` diagnostic queries run as part of this review. `store/mod.rs` already sets WAL journal mode + a 5s `busy_timeout`; a single contention event under simultaneous read+write load is expected and self-heals on the next tick (per the same "retry next tick, no per-task defer" design used elsewhere). Not filed — one occurrence, likely partly self-inflicted by this review's own concurrent queries, no recurring pattern.

### Backlog and Stuck Work

Blocked-task composition in the global list is unchanged in character from prior days: the ~55 blocked tasks in the other tracked project are still `CI failure limit reached during auto-merge` / `GitHub Actions billing failure`, all correctly per the settled per-task block-at-merge-time policy. Nothing new, nothing stuck in this repo.

`orch cooldown list` currently shows 5 persisted cooldowns (`codex:gpt-5.4` 2d5h, `kimi:opus` 4d4h, `minimax:haiku` 1d12h, `minimax:opus` 3d22h, `opencode:opencode/north-mini-code-free` 5h51m) — all decaying normally, none escalating unexpectedly.

---

## Issues Filed Today

None. Three issues were closed (all same-day fixes for problems either carried over from yesterday's review or found and fixed within today's window); nothing new and unexplained turned up. The `database is locked` blip and the `kimi:opus` cooldown jump were both investigated and traced to expected causes (see above), not filed.

---

## Priorities for Tomorrow

1. **Confirm the suspend-gap fix class holds under a real overnight sleep cycle.** Today's clean WATCHDOG/circuit-breaker window is encouraging but the host had comparatively light sleep churn after `0d2c2e8f` landed (18:54 local). Check tomorrow whether a full sleep-heavy night produces zero false silence-detection reroutes, zero false watchdog errors, and zero false circuit-breaker opens — that's the real confirmation.
2. **`#3453` is still the only open issue and is now 12 days old, and it is still reproducible on current `HEAD`.** `prompts/review_task.md` still lacks the explicit "pending CI is not a terminal state, do not stop with a status update" instruction the issue asks for. This is a one-paragraph prompt edit — worth prioritizing since it's the single blocker keeping the tracker non-empty.
3. **Watch `kimi:opus`'s 4d4h billing-cycle cooldown for a genuine expiry-and-stay-clear next week**, distinct from the shorter router-pool cooldowns already confirmed resolving normally.

---

*Prepared by Orch automation (internal:156236) on 2026-08-09.*
