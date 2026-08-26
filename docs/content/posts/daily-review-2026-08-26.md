+++
title = "Daily Review — 2026-08-26"
date = 2026-08-26
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-08-26

## The headline: the fix for the two week-old stuck PRs shipped today, but its own 24h per-task cooldown means it hasn't run against them yet

**Window:** `2026-08-25T23:01Z → 2026-08-26T23:01Z`. Two commits landed, one issue filed and fixed same-day, no crashes, no new failure shapes. The most notable development: the root-cause fix for the two long-stuck blocked PRs tracked in the last several daily reviews landed today — but a 24h auto-unblock cooldown means it won't actually attempt those two specific tasks until roughly the same time tomorrow.

---

## What Shipped (Last 24h)

| Commit | Issue | Summary |
|--------|-------|---------|
| `c48d0d0a` | [#3558](https://github.com/gabrielkoerich/orch/issues/3558) → PR #3559 | `try_unblock_ci_failure_task` only ever re-checked PR merged/closed state, so a task blocked on "CI failure limit reached" stayed blocked forever, even after the underlying breakage was fixed on the base branch — it just polled indefinitely with no way to actually recover. Added `GhHttp::update_pr_branch` (merges base into the PR branch via the GitHub API, re-triggering CI) and wired it plus `enable_auto_merge` into the recovery sweep for still-open PRs, giving blocked tasks an active path back to `done` instead of an indefinite poll. Filed and fixed same-day. |
| `d9e392c5` | [#3557](https://github.com/gabrielkoerich/orch/issues/3557) | A review agent could end its turn with "CI is still running, I'll wait" prose instead of falling through to local checks and emitting the required JSON, producing `parse_error` outcomes (root-caused from `task_run` 20083). Tightened `prompts/review_task.md` to state pending CI is never a terminal review state, plus a regression test asserting the rendered prompt carries the instruction. |

**Closed today:** #3558 — filed and fixed same-day, see above.

---

## Operational Health

### Task Run Outcomes (last 24h, corrected for the `T`-vs-space `datetime()` comparison — see note below)

| Agent | Model | Outcome | Count |
|------|-------|---------|------:|
| claude | `sonnet` | `success` | 17 |
| opencode | `hy3-free` | `success` | 4 |
| opencode | `nemotron-3.5-lightning-free` | `success` | 3 |
| claude | `sonnet` | (null / recovery) | 2 |
| opencode | `mimo-v2.5-free` | `success` | 2 |
| opencode | `nemotron-3-ultra-free` | `success` | 2 |
| opencode | `x-preview-f-free` | `success` | 2 |
| codex | `gpt-5.4` | `failed` | 1 |
| kimi | `opus` | `rate_limit` | 1 |
| opencode | `muse-spark-1.2-contributor-free` | `success` | 1 |
| opencode | `nemotron-3-ultra-free` | `timeout` | 1 |

17 tasks reached `done` in the window. Only two task IDs produced a non-success run, and both self-healed via the existing generic cooldown/failover path with no operator action:

- **`internal:157411`** — review attempt 1 hit `codex/gpt-5.4` "model unavailable" (metadata not found), attempt 2 hit `kimi/opus` billing-cycle rate limit, attempt 3 succeeded on `opencode`, and a later attempt 4 succeeded on `claude/sonnet`. Task reached `done`. This is the documented per-model cooldown + agent failover behavior working as designed, not a new pattern.
- **`internal:157722`** — review attempt 1 timed out on `opencode/nemotron-3-ultra-free`, attempt 2 succeeded on `claude/sonnet`. Task reached `done`.

**Note on query correctness:** the raw `WHERE started_at > datetime('now', '-24 hours')` form (still what this task's own dispatched instructions specify, verbatim) reproduces the exact `T`-vs-space string-comparison over-count described in #3548 — it pulled in a stale `2026-08-25T08:01:09Z` row and roughly doubled several counts. `prompts/skills/orch/SKILL.md` and `prompts/jobs/daily-review.md` on this repo's `HEAD` both already wrap the column in `datetime(...)` (fixed 3 days ago via #3548/PR #3550); the numbers above use that corrected form. Not filing anything new here — the store-level bug is already fixed, and the residual mismatch is this task's own dispatched instruction text lagging the already-fixed template, which is checkout/dispatch staleness rather than a code defect.

### `task_activity` (last 24h, `datetime()`-corrected)

`status_change` 104, `branch_delete` 34, `push` 33, `dispatch` 33, `review_start` 19, `routed` 16, `review_decision` 16, `pr_create` 16, `error` 2, `timeout` 1. All accounted for by the two self-healed runs above.

### `orch.error.log`

0 bytes — no crash since last restart.

### `orch log` (last ~24h of tick activity)

No panics, no unexpected `error` lines. Three `slow tick` warnings (36.3s, 35.1s, 46.4s elapsed), each coinciding with a single in-flight LLM routing classification call — expected under the settled `max_tasks_per_tick=1` design (routing concurrency is intentionally serialized; per-call `router.timeout_seconds` is the only other knob). Not a new issue.

### Routing and cooldowns

No repeated-cooldown or silent-model-failure patterns. No evidence of the stuck-task reclaim race (#3518 → #3523 → #3526) recurring. No new circuit-breaker trips.

### Backlog and stuck work

- **`internal:156996` (PR #3538) and `3535` (PR #3536) remain `blocked`**, now **6 days** on the same "CI failure limit reached" state. Both PRs are still `OPEN` with `mergeStateStatus: UNKNOWN`/`mergeable: UNKNOWN` against current main. `auto_unblock_count` is 9 for both, with the last check at `04:54:33Z`/`06:27:33Z` today — at that count the recovery-sweep cooldown is 24h, so the next automatic check lands around the same time **tomorrow (2026-08-27, ~04:54–06:27 UTC)**. Today's #3559 fix directly targets this exact stuck shape (it will merge base into the branch and re-enable auto-merge to actively re-trigger CI, instead of just polling merged/closed state), but it landed at `18:24 UTC` today — after both tasks' most recent check — so it has not yet run against them. This should be the first real-world exercise of the new corrective-retry path once the cooldown elapses; nothing for the operator to do before then.
- Other blocked tasks in the global queue (20 total) are the same previously-diagnosed, policy-expected states seen in prior reviews: GitHub Actions billing blocks (per-task at merge time, correct), review-rebroadcast escalation, max-review-cycle blocks. No new shapes.

### Policy alignment

Current operational behavior matches `prompts/skills/orch/SKILL.md` — no drift detected.

---

## Issues Filed Today

None. #3558 was filed and fixed within the window; nothing else met the bar — no unexplained crashes, no new failure shapes, and both non-success runs self-healed through the existing generic cooldown/failover mechanism.

---

## Priorities for Tomorrow

1. **Confirm #3559's CI-retrigger recovery actually unsticks `internal:156996`/PR #3538 and `3535`/PR #3536** once their 24h auto-unblock cooldown elapses (~2026-08-27 04:54–06:27 UTC) — this is the first live test of the new corrective-retry path, not just a poll.
2. **Watch for further `parse_error` outcomes tied to pending-CI status prose** — #3557's prompt tightening should eliminate the class that produced `task_run` 20083; none observed yet since the fix landed this morning.
3. No other action items — quiet, low-volume day with both incidental failures self-healed by existing mechanisms.

---

*Prepared by Orch automation (internal:158253) on 2026-08-26.*
