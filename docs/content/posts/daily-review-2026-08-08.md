+++
title = "Daily Review — 2026-08-08"
date = 2026-08-08
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-08-08

## The headline: yesterday's four fixes all landed, and the day's only real anomaly turned out to be the host machine sleeping, not orch misbehaving

Every issue filed in yesterday's review shipped and closed within the window — #3478, #3479, #3481, #3482 — plus one new bug found and fixed same-day (#3489, a masked exit code in the runner script). Throughput was up again (35 tasks reached `done` vs 27 yesterday).

The one thing that looked alarming — **six `WATCHDOG: tick loop has not completed a tick` errors** — is not a stall in orch. Every single one lines up to the second with a macOS wake-from-sleep event. That is still worth fixing, because orch currently charges the suspended wall-clock time to whichever agent happened to be mid-run, cooling models that never actually hung.

---

## What Shipped (Last 24h)

**7 commits landed** (5 fixes + 1 review-classifier fix + yesterday's review post):

| Commit | Issue | Summary |
|--------|-------|---------|
| `5abf4ad1` | #3489 | opencode/codex runner script read the wrong `PIPESTATUS` index — the agent's real exit code was always masked as `0` |
| `4703f321` | #3481 | Router LLM pool models ratcheted into multi-day cooldowns because successful routes never reset failure counts |
| `12a94950` | #3479 | `task_runs` rows leaked with `NULL outcome` — `finalize_incomplete_runs` was wired only to shutdown, not to stuck-task/silence recovery |
| `5644d9df` | #3482 | Skip transient provider network errors in the review failure classifier |
| `3fdef626` | #3478 | Reset `failure_count` on successful review runs |
| `cfd51b1a` | #3480 | RAII `SessionGuard` stops tmux session leaks in `tick.rs` tests |
| `270dd10c` | — | Daily review post for 2026-08-07 |

`#3489` deserves a note: it was found, filed, fixed, reviewed, and merged inside a single day, and it is the highest-leverage of the batch. A runner that always reports exit code `0` means every downstream failure signal (classification, cooldown, retry decision) was working from a lie for opencode and codex runs. Expect the accuracy of tomorrow's `task_runs` outcome table to improve as a result.

**Closed today:** #3489, #3482, #3481, #3480, #3479, #3478 (six issues; #3478–#3482 were all filed in yesterday's review).

**Still open:** #3453 (`bug(review-prompt): pending CI status prose still causes review parse errors`) — the single open issue in the tracker, unchanged since 2026-07-29. See priorities.

---

## Operational Health

### Throughput and Activity

`task_activity` in the last 24 hours:

| Event | Count | Δ vs yesterday |
|------|------:|---------------:|
| `status_change` | 264 | +49 |
| `dispatch` | 101 | +22 |
| `push` | 71 | +13 |
| `branch_delete` | 64 | +10 |
| `routed` | 50 | +10 |
| `review_start` | 39 | +4 |
| `review_decision` | 34 | +6 |
| `pr_create` | 33 | +6 |
| `error` | 15 | +3 |
| `rerouted` | 10 | +4 |

Up across every dimension for the second day running. **35 tasks reached `done`** (10 in this repo, 25 in another project). Error count grew slower than dispatch count, so the error *rate* improved.

### Task Run Outcomes

| Agent | Model | Outcome | Count |
|------|-------|---------|------:|
| claude | `sonnet` | `success` | 54 |
| kimi | `opus` | `success` | 7 |
| kimi | `opus` | `rate_limit` | 3 |
| opencode | `opencode/ling-3.0-tiny-free` | `parse_error` | 3 |
| opencode | `opencode/longcat-2.0-free` | `success` | 3 |
| opencode | `opencode/deepseek-v4-flash-free` | `success` | 2 |
| opencode | `opencode/laguna-s-2.1-free` | `success` | 2 |
| opencode | `opencode/ling-3.0-flash-free` | `failed` | 2 |
| opencode | `opencode/nemotron-3-ultra-free` | `success` | 2 |
| opencode | *(six models)* | `success` / `failed` / `parse_error` / *(in-flight)* | 1 each |
| claude | `sonnet` | `failed` | 1 |
| codex | `gpt-5.4` | `failed` | 1 |

**Success rate ≈ 75 %** (74 `success` out of 99 terminal runs). `claude:sonnet` carried the day at 54/56 successful — the weighted round-robin fallback leaned on it heavily because the entire router LLM pool was cooled (see below), and it held up.

### Non-Success Breakdown

- **`silence detection` (×4)** — three of these (`internal:156072`, `internal:156073`, `internal:156076`) are the *sleep artifact* described below, not real agent hangs. One earlier one was genuine.
- **`opencode invalid response` / `parse_error` (×5)** — all five predate or immediately follow the `#3473` truncation fix (latest at 07:00 UTC, fix merged 22:48 UTC the prior day but the service runs from the installed binary, so the fix is not yet exercised). Not a regression; recheck next window.
- **`kimi:opus rate_limit` (×3)** — same billing-cycle 403 as prior days. `#3478` (reset `failure_count` on successful review runs) shipped today, so the ratchet should stop; `kimi:opus` is already down from 1d11h to **11h53m** remaining, which is the first observed *decrease* in that cooldown and is consistent with the fix's intent.
- **`codex:gpt-5.4` (`failed`, ×1)** — `Model metadata for 'gpt-5.4' not found`. Long-known cooled model (3d4h remaining). Working as designed.
- **`claude:sonnet` (`failed`, ×1)** — truncated Anthropic network error, self-recovered.

### The WATCHDOG "stalls" are host sleep, not orch

Six `WATCHDOG: tick loop has not completed a tick` ERROR lines fired, with gaps of 275 s, 387 s, 682 s, 695 s, 987 s, and 1039 s. Correlating against `pmset -g log` (converting UTC → local `-0300`):

| Watchdog fires (UTC) | Local | Nearest host wake |
|---|---|---|
| 21:54:16 | 18:54:16 | DarkWake 18:54:07 |
| 22:12:09 | 19:12:09 | DarkWake 19:12:04 |
| 22:23:55 | 19:23:55 | DarkWake 19:23:52 |
| 23:14:45 | 20:14:45 | DarkWake 20:14:39 |
| 23:33:59 | 20:33:59 | DarkWake 20:33:52 |
| 23:39:54 | 20:39:54 | Wake (lid) 20:39:51 |

Six for six, every one within 3–9 seconds of a wake event. The machine was on battery doing clamshell / maintenance sleep cycles all evening. The tick loop was not stalled — it was not running, because the host was not running.

The harmless part is the noisy ERROR line. The **harmful** part is that the same wall-clock assumption feeds stuck-task recovery: `stuck_task_timing_from_map()` computes `chrono::Utc::now() - updated_at`, so 33 minutes of host suspend reads identically to 33 minutes of a hung agent. At 22:57 UTC, immediately after a wake, two internal tasks were reclaimed as "timed out with active session" and **1-hour model cooldowns were applied to `opencode/ling-3.0-tiny-free` and `opencode/north-mini-code-free`** — two models that had done nothing wrong. Both are visible in `orch cooldown list` right now. That is a real (if modest) routing-quality regression caused by an environmental event. Filed as an issue.

### GitHub transport failures open the 5xx circuit breaker

Two circuit-breaker openings today (21:55 and 23:22 UTC), both from `HTTP send failed` *transport* errors — connection failures, not GitHub 5xx responses — and both immediately after a wake, when the network interface has not come back up. Each opened a 300 s `github:5xx` breaker, and the routing phase was skipped for the duration: **126 `GitHub 5xx circuit breaker open — skipping routing phase` log lines** across the day. Also filed.

### Router LLM Pool: fix shipped, cooldowns not yet cleared

`all router LLM pool entries on cooldown — will try fallback` fired on essentially every routing decision today. All three pool entries remain cooled:

| Key | Remaining | Δ vs yesterday |
|-----|----------:|---------------:|
| `codex:gpt-5.4` | 3d4h | −1d |
| `kimi:haiku` | 23h1m | −2h |
| `claude:haiku` | 22h2m | −0h |
| `kimi:opus` | 11h53m | **−23h** |
| `minimax:haiku` | 10h39m | −0h |
| `minimax:opus` | 4d21h | −1d |
| `opencode:opencode/ling-3.0-tiny-free` | 35m | *(new — sleep artifact)* |
| `opencode:opencode/north-mini-code-free` | 35m | *(new — sleep artifact)* |

Every entry is decaying normally; nothing is escalating. `#3481`'s fix (reset failure counts on successful pool calls) landed today but the currently-outstanding cooldowns were written *before* it, so they must expire on their own. Expect the three `*:haiku` entries to clear naturally within ~24 h, and — critically — **not to come back**. That is the observation to make tomorrow.

Routing accuracy in the meantime was fine: weighted round-robin fallback picked `claude` and `opencode` sensibly, and `claude:sonnet` returned a 96 % success rate on that traffic. The fallback path is doing its job.

### Logs and Data Hygiene

- `/opt/homebrew/var/log/orch.error.log` — `0B`, mtime Aug 4. Stale and empty; nothing to report.
- **`task_runs` NULL-outcome leak: effectively fixed.** Only 2 rows in the last 24 h have a NULL outcome, and both are *currently in-flight* runs (this review task and one other), which is correct. The historical total is 301 rows; the last non-trivial leak day was 2026-08-03 (6 rows). `#3479` shipped today and the evidence already matches.
- Recurring `telegram getUpdates failed` warnings throughout the day — same network-unavailable-during-sleep root cause as the GitHub transport failures. Not filed separately; it is a symptom of the same environmental condition and does not affect task processing.

### Backlog and Stuck Work

56 blocked tasks, same composition as prior days and unchanged in character:

| Reason | Count |
|--------|------:|
| `CI failure limit reached during auto-merge` (PR still open) | 42 |
| *(empty reason)* | 10 |
| `GitHub Actions billing failure` | 5 |
| `review agent rebroadcast escalated` | 1 |
| `max review cycles (2) exceeded` | 1 |

All in one downstream project, all correctly per settled policy (per-task block at merge time; the operator resolves the billing/CI condition and runs `orch task unblock all`). The 10 with an empty `block_reason` are old rows predating reason-tracking. No new pattern, no stuck work in this repo.

---

## Issues Filed Today

Two, both root-cause, both with same-day live evidence:

- **Host suspend/resume is charged to the agent** — the watchdog and `stuck_task_timing_from_map()` both measure wall-clock time, so machine sleep is indistinguishable from an agent hang. Result: six false ERROR-level stall reports and two innocent models put into 1-hour cooldowns. Suggested direction is a monotonic-clock or "large clock gap detected" guard in both paths, not a threshold bump.
- **Transport-level HTTP failures open the `github:5xx` circuit breaker** — `send_with_retries()`'s `Err(e)` transport arm calls `set_agent_cooldown("github:5xx", …)` on the same key and duration as a genuine server error. A local connectivity blip is thereby reported and treated as a GitHub outage, halting the routing phase for 5 minutes.

Both are narrow and single-call-site-shaped, in keeping with the recent fix cadence. Neither proposes a config change, a cooldown-duration tweak, or any per-model special-casing.

---

## Priorities for Tomorrow

1. **Verify `#3481` and `#3478` actually broke the ratchet.** The three `*:haiku` router-pool cooldowns should expire within ~24 h and *stay* gone; `kimi:opus` already dropped 23 h in one day, which is the first positive signal. If any of them re-escalate after clearing, the reset is firing on the wrong path and the fix needs a second look.
2. **Confirm `#3489`'s exit-code fix changes the outcome distribution.** With opencode/codex exit codes no longer masked to `0`, some runs previously logged as `success` may correctly reclassify. A shift in the outcome table is the expected, healthy result — do not misread it as a regression.
3. **Re-check the `opencode/ling-3.0-*` `parse_error` count.** Still 5 today, but every one predates the fix being exercised by the running service. A genuine drop is the confirmation for `#3473`; a flat count means the truncation detection needs revisiting.
4. **`#3453` is now the only open issue and is 10 days old.** Worth deciding explicitly whether it is still reproducible on current `HEAD` or should be closed — an empty tracker with one stale entry is worse than either state.

---

*Prepared by Orch automation (internal:156075) on 2026-08-08.*
