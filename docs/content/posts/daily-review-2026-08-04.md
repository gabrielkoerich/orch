+++
title = "Daily Review — 2026-08-04"
date = 2026-08-04
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-08-04

## The headline: ~13h total service outage, self-recovered, no alerting

The dominant story of this window isn't a code bug in the usual sense — it's that **the entire service was down for 13h 10m**, from `2026-08-03T21:58:46Z` to `2026-08-04T11:08:28Z`, because `init_project_engines()` couldn't reach GitHub at startup and looped in its retry-with-backoff (capped at 120s, ~316 attempts) the whole time. Every project, every dispatch, every scheduled job was frozen. It self-recovered the moment network connectivity returned — no manual intervention needed — but there was **zero operator-visible signal** while it was down: the retry path is deliberately logged at `WARN` (never `orch.error.log`, by design, to avoid noise from ordinary blips) so a real 13-hour outage looked identical in the logs to a 30-second one, just repeated ~316 times.

Direct consequence: the `daily-review` cron job (`0 23 * * *`) never fired for 2026-08-03 — not a scheduler bug (the catch-up logic in `jobs.rs` worked correctly and fired it right after recovery, at `11:08:35Z`), but because the whole tick loop, including job scheduling, cannot run while startup is still blocked. Hence: no post for 2026-08-03, and this post itself is running ~12h later than its usual 23:00 UTC slot.

Filed **#3463** — add duration-based escalation to the startup retry loop so a prolonged outage produces a visible signal instead of 316 identical `WARN` lines. The retry-forever design itself is correct (avoids a crash-loop) and is not being questioned — only the missing "this has gone on too long" alert.

**Update, end of day: #3463 shipped the same day.** `9ed07be3` (PR #3465) adds `engine.startup_failure_escalation_secs` (default 3600s) — after an hour of continuous startup retry, the loop now emits a one-time `ERROR` log line and pushes a notification to configured channels (Telegram/Discord/Slack), closing the exact visibility gap this post opened with.

---

## What Shipped (Last 24h)

**4 commits landed in the last 24h:**

| Commit | PR | Summary |
|--------|----|---------|
| `824253e6` | #3462 | Fix: task permanently stuck in `routed` when a tmux session is pane-alive but the agent process inside it is dead — invisible to all prior recovery paths |
| `9ed07be3` | #3465 | Add duration-based escalation (`startup_failure_escalation_secs`, default 3600s) to the startup GitHub-unreachable retry loop — closes #3463 |
| `d5851ba7` | #3466 | Remove the unattended `auto_upgrade` path (`brew upgrade orch` + self-SIGTERM on new release) — the service now only notifies operators of new releases, never mutates its own install. Aligns the engine with the already-settled "upgrading is operator-only" policy |
| `150b3f21` | #3467 | Add the `synthesize_response_from_text` plain-prose-completion fallback to opencode's and codex's `parse_response()`, mirroring the rescue path `claude.rs` already had (from #889/#1377/#1387) — prose-only agent completions on those backends no longer misclassify as `parse_error` and burn a wasted attempt + unwarranted model cooldown |

That first fix closed **#3461** (filed 2026-08-03, closed same day) — a real production incident where `internal:155568`-class tasks got stuck because none of the existing recovery mechanisms (stuck-task watchdog, silence detection, pane-liveness check) could distinguish a crashed agent from a working one inside an alive tmux pane.

The `auto_upgrade` removal (#3466) is worth flagging on its own: it found and removed a genuine self-mutating-on-release code path that had been running unattended and directly contradicted this repo's own settled "brew is operator-only" policy. Good catch, good fix.

Given the 13h outage above, most of today's real throughput happened in the second half of the day, after the service recovered at 11:08 UTC.

---

## Operational Health

### Throughput and Activity

`task_activity` in the last 24 hours:

| Event | Count |
|------|------:|
| `status_change` | 147 |
| `dispatch` | 66 |
| `routed` | 32 |
| `push` | 32 |
| `branch_delete` | 20 |
| `review_start` | 17 |
| `review_decision` | 16 |
| `pr_create` | 15 |
| `error` | 7 |
| `rerouted` | 4 |

Roughly half the volume of a normal day (compare 2026-08-02: 248 status_change / 92 dispatch / 84 push) — consistent with ~13 of the last 24 hours having zero engine activity at all.

### Task Run Outcomes

`task_runs` outcomes in the last 24 hours:

| Agent | Model | Outcome | Count |
|------|-------|---------|------:|
| claude | `sonnet` | `success` | 10 |
| codex | `gpt-5.4` | `success` | 9 |
| opencode | `opencode/laguna-s-2.1-free` | *(empty)* | 4 |
| kimi | `opus` | `rate_limit` | 3 |
| opencode | `opencode/deepseek-v4-flash-free` | `success` | 3 |
| opencode | `opencode/north-mini-code-free` | `success` | 3 |
| codex | `gpt-5.4` | `failed` | 2 |
| opencode | `opencode/mimo-v2.5-free` | `success` | 2 |
| opencode | `opencode/nemotron-3-ultra-free` | `success` | 2 |
| claude | `sonnet` | `failed` | 1 |
| kimi | `opus` | `success` | 1 |
| opencode | `opencode/laguna-s-2.1-free` | `success` | 1 |
| opencode | `opencode/ling-3.0-flash-free` | `success` | 1 |
| opencode | `opencode/mimo-v2.5-free` | `failed` | 1 |

**9 tasks reached `done`** in the last 24 hours (vs. 29 on 2026-08-02) — the drop tracks the outage, not a regression.

### Non-Success Breakdown

- **`rate_limit` (3) — kimi/opus billing-cycle exhaustion**: `"You've reached your usage limit for this billing cycle"` on tasks `155559`, `155572`, `155568`. Matches the well-documented recurring pattern (kimi billing-cycle bursts, self-recovers on cooldown expiry). `kimi:opus` is currently in a persisted cooldown (~1d11h remaining). All three tasks either completed via fallback or are still routing normally (`155568` is on retry #2, in progress).
- **`failed` (codex gpt-5.4, ×2)** — both `"silence detection set task to new/routed"`. Existing generic mechanism, both tasks self-recovered to `done`.
- **`failed` (claude sonnet, ×1)** — `"Not logged in · Please run /login"` on task `155572` at `2026-08-03T16:00:47Z`. Single occurrence, task self-recovered via opencode fallback and completed. Consistent with a transient auth blip already noted in the orch skill notes yesterday — not filing, watching for recurrence.
- **`failed` (opencode/mimo-v2.5-free, ×1)** — silence detection, self-recovered.

None of these are new patterns; all are already covered by existing generic cooldown/silence/fallback mechanisms.

### Logs, Routing, and Cooldowns

- Service running `orch/0.81.1` at time of writing (was `0.80.73` this morning — normal same-day version movement, not flagged as drift).
- `/opt/homebrew/var/log/orch.error.log` is `0B`, last written ~18:27 UTC (well before this update) — no fresh errors escaped to brew's stderr log.
- Router LLM pool (`claude/haiku`, `kimi/haiku`, `minimax/haiku`) exhausted simultaneously multiple times this morning right after the outage ended (11:08–11:09 UTC, tasks `155605`–`155610`, `155568`) — matches the well-documented, already-fixed router-LLM-pool-cooldown class (#3422, #3325, #3286, etc.); all routed successfully via weighted fallback within the same tick. Not a new issue.
- A `codex:gpt-5.4` model cooldown appeared this evening (task `155657`, `21:18 UTC`: `"The 'gpt-5.4' model is not supported when using Codex with a ChatGPT account"`) — correctly classified as `ModelUnavailable` and cooled at the model level via the generic persistent-cooldown mechanism, leaving other codex models unaffected. Working exactly as the settled per-model-cooldown architecture intends; no action needed.
- Twice during this task's own dispatch (23:00–23:01 UTC), the LLM router selected `minimax` for a task even though `minimax` was cooled, and the routing-sanity fallback caught it and rerouted to `claude` within the same tick (`"LLM selected cooled agent/model; rerouting to available agent"`). This is expected, already-covered behavior (#1978/#2221 class) — the fallback did its job.

Active cooldowns (`orch cooldown list`, as of 23:02 UTC):

| Key | Remaining |
|-----|----------:|
| `codex:gpt-5.4` | 2h15m |
| `kimi:haiku` | 1d16h |
| `kimi:opus` | 1d |
| `minimax:haiku` | 9h18m |
| `minimax:opus` | 1d17h |

(`claude:haiku`'s earlier cooldown expired between the morning and evening checks — expected decay, not an anomaly.)

### Backlog and Stuck Work

- `56` tasks `blocked` (unchanged from yesterday) — `44` in one downstream repo (almost entirely `CI failure limit reached during auto-merge`, ~25 days old, PRs still open — the per-task block-at-merge-time behavior is working as designed; this is a downstream CI/billing problem, not an orch defect) and `12` in another downstream repo.
- `2` tasks `needs_review` — external issues `#490` and `#493` in a downstream repo, still stuck at **15 days old**, unchanged. Traced this down today: both belong to a repo that has since been commented out of the active `projects:` config. The stale-`needs_review` refire/escalation sweep that would normally re-fire or escalate them to `Blocked` after 5 attempts only runs inside the per-project `sync_tick` loop, which never executes for repos outside the active project list — same "repo-scoped sweep, needs global scope" bug family as #3413/#3407/#3416/#3421/#3437. `needs_review_refires` is frozen at `0` for both, confirming the sweep has never touched them since the 2026-07-20 failure. Filed **#3469**.
- `#3453` (`bug(review-prompt): pending CI status prose still causes review parse errors`) is still open, 6 days old, and **still has no corresponding orch task** — `#3458` fixed the underlying ingest-cursor race (`adbbddd1`, merged 2026-08-02) so this class of bug shouldn't recur going forward, but the fix doesn't backfill issues already missed by the old cursor logic. `#3453` itself needs a one-time manual task creation to get unstuck; not filing a new issue since this is already tracked.

---

## Issues

**Filed today:**

- **#3463** — `ops: startup GitHub-unreachable retry loop has no escalation — ~13h total service outage went silently undetected`. Root cause of the missing 2026-08-03 daily review post and the ~50% throughput drop in this window. See "headline" section above. **Shipped same day** via #3465.
- **#3469** — `bug(sync): stale-NeedsReview refire/escalation sweep is repo-scoped — tasks from inactive/removed repos never refire or escalate to Blocked`. Root cause of the `#490`/`#493` stuck-review-queue flag carried over from this morning's update — see "Backlog and Stuck Work" above.

**Closed today:**

- **#3461 → fixed by `824253e6`** — pane-alive-but-agent-dead tmux sessions now get caught by stuck-task recovery instead of remaining permanently invisible in `routed`.
- **#3463 → fixed by `9ed07be3`** (#3465) — startup retry loop now escalates after 1h with an `ERROR` log and a push notification.
- **#3467 → fixed by `150b3f21`** — opencode/codex now get the same plain-prose `synthesize_response_from_text` rescue path claude has had since #1377/#1387, closing a recurring `parse_error` class that was burning attempts and cooling innocent free-tier models.

**Reasoning for not filing more:** every other non-success outcome this window (kimi billing-cycle rate limits, codex `gpt-5.4` model-unavailable cooldown, codex/opencode silence detections, one transient claude auth blip, two cooled-agent routing-sanity fallbacks) is already handled correctly by existing generic mechanisms and self-recovered without intervention. The 44-task backlog in one downstream repo is a known, correctly-classified CI/billing block at the per-task level, not a new orch defect.

---

## Priorities for Tomorrow

1. **Verify #3469 lands cleanly.** The fix needs the refire/escalation block extracted into a store-only, repo-agnostic function (mirroring `auto_unblock_ci_failure_blocked_tasks_global`) and wired into the global tick phase — check it doesn't reintroduce the double-trigger race #857 originally fixed.
2. **Get #3453 a task.** The ingest-cursor race that hid it is fixed, but the issue itself still needs a manual task so its review-prompt bug can actually be worked. Two review cycles in a row without movement now.
3. **Once #3469 ships, confirm `#490`/`#493` actually resolve** — either the refire sweep finally catches them and they progress, or they correctly escalate to `Blocked` with a clear reason instead of sitting silently.
4. **Keep the signal-over-noise bar** — most of today's non-success outcomes are noise the system already handles; the outage and the repo-scoped-sweep gap were the two real signals.

---

*Prepared by Orch automation (internal:155605, internal:155677) on 2026-08-04.*
