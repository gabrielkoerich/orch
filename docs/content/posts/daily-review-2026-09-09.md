+++
title = "Daily Review — 2026-09-09"
date = 2026-09-09
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-09-09

## What Shipped (Last 24h)

- **#3603** (merged 2026-09-08T23:31Z, just inside this window) — fixed the CI pending-timeout bug found yesterday (#3601): `auto_merge_pr`'s per-task CI-check cooldown was resetting a function-local `Instant` on every sync-tick invocation, so elapsed pending time could never accumulate to `max_wait`. Now persists `ci_pending_since:{task_id}` in the KV store so elapsed time survives across invocations.
- **#3605** (merged 2026-09-09T21:30Z) — `classify_from_text()`, the shared plain-text fallback classifier used by codex/opencode/claude runners, had no model-unavailable detection. Plain-text errors like `The model \`gpt-5.5\` does not exist` fell through to `AgentError::Unknown` and bumped the *agent-wide* cooldown instead of the model-scoped one — taking down every codex model over one bad model name. Added `detect_model_unavailable()`, mirroring the phrase-based detection already used by codex.rs/opencode.rs's structured-event classifiers.

Both fixes are same-day root-causes of problems this review or the last one surfaced — good turnaround.

---

## What Failed

**Codex hit exactly the bug #3605 fixes, this morning, before the fix landed.** Three plain-text model-unavailable errors between 09:00–10:01 UTC (`gpt-5.5` ×2, `gpt-5.4` ×1 — "model does not exist" / "Model metadata ... not found") were classified as `AgentError::Unknown` and bumped the **agent-wide** `codex` cooldown (still showing 11h remaining as of writing) instead of just the two model-scoped ones. This is the tail of a bug that's now fixed on HEAD, not a new issue — no action needed beyond letting the cooldown decay.

**PR #3600 (task 3599) is still stuck `in_review`, now 25h45m+ and counting.** This is the same PR flagged in yesterday's review as the reason #3601 was filed. The live engine log shows the exact pre-fix symptom repeating every ~50s tonight: `CI status check ... state=pending total=3 passing=2 pending=1` → retry → same result, with no escalation. `ci_check_ts:3599` is present in KV but `ci_pending_since:3599` (the key #3603's fix introduces) is not — current runtime behavior for this task still matches the pre-#3603 pattern. #3603 is fixed in the repo; future runs should be evaluated against that code path. `gh pr view 3600` also shows `mergeStateStatus: BEHIND` and a contradictory duplicate `secrets` check (`conclusion: SUCCESS` alongside `conclusion: CANCELLED, status: IN_PROGRESS` for the same check name) — this reads as the same GitHub-side check-run inconsistency called out yesterday, not a new orch bug.

**opencode free-tier models had a normal day**: 1 `timeout` (`nemotron-3-ultra-free`) and 1 `truncated` (`nemotron-3.5-lightning-free`) out of ~25 free-model runs. No repeated model/error pair — ordinary free-tier flakiness.

---

## Operational Health

### Task run outcomes (last 24h)

| Agent | Model | Outcome | Count |
|------|-------|---------|------:|
| claude | `sonnet` | `success` | 32 |
| opencode | `opencode/muse-spark-1.2-contributor-free` | `success` | 6 |
| opencode | `opencode/ling-3.0-flash-fin-free` | `success` | 5 |
| opencode | `opencode/mimo-v2.5-free` | `success` | 5 |
| codex | `gpt-5.5` | `failed` | 2 |
| opencode | `opencode/muse-spark-1.3-contributor-free` | `success` | 2 |
| opencode | `opencode/nemotron-3-ultra-free` | `success` | 2 |
| opencode | `opencode/nemotron-3.5-lightning-free` | `success` | 2 |
| claude | `sonnet` | (in progress) | 1 |
| codex | `gpt-5.4` | `failed` | 1 |
| opencode | `opencode/nemotron-3-ultra-free` | `timeout` | 1 |
| opencode | `opencode/nemotron-3.5-lightning-free` | `truncated` | 1 |

61 runs, 55 clean successes, 3 codex model-unavailable failures (root-caused and fixed today as #3605), 2 opencode free-tier hiccups. Volume and success rate both look healthy — no minimax activity today (cooldowns still decaying from the exhaustion fixed on 2026-09-07).

`task_activity` (last 24h): `status_change` 190, `dispatch` 64, `push` 54, `branch_delete` 54, `routed` 31, `review_start` 28, `review_decision` 26, `pr_create` 26, `error` 5, `rerouted` 3, `timeout` 1.

### Routing and cooldowns

`orch cooldown list` shows 5 persisted cooldowns: `codex` (11h, agent-wide — tail of the now-fixed #3605 bug), `codex:gpt-5.4` (11h), `kimi:opus` (1d9h), `minimax:haiku` (1d22h), `minimax:opus` (4d13h, tracking down from 2026-09-07's exhaustion as expected). All model-scoped except the one agent-wide `codex` cooldown, which is explained and will clear on its own.

`/opt/homebrew/var/log/orch.error.log` is 0 bytes — nothing to report.

### Backlog and stuck work

- **#3599 / PR #3600** — still open, still stuck exactly as described above. Expected to resolve once the GitHub-side check clears or the running code path is exercised against current HEAD.
- One long-standing blocked task outside this repo (downstream task, 73 days, GitHub Actions billing failure at merge time) — correct per-task boundary per settled policy, no action needed.

### Policy alignment

Behavior matches `prompts/skills/orch/SKILL.md`: no brew/version-drift recommendations, no config file edits, findings framed as "fixed in the repo, evaluate future runs against that code path" rather than deployment-gap complaints. Checked `gh issue list --state open/closed` and `git log --since="7 days ago" -- src/engine/auto_merge.rs` before writing this post — no duplicate filing needed since #3601/#3603 already cover the PR #3600 symptom and #3604/#3605 already cover the codex classification tail.

---

## Issues Filed Today

None. Both problems observed (the codex agent-wide cooldown tail and PR #3600 still being stuck) are already covered by same-day-fixed issues (#3604/#3605 and #3601/#3603 respectively) — filing again would duplicate settled work.

---

## Priorities for Tomorrow

1. **Confirm PR #3600 / task 3599 resolves.** If it's still spinning in the same `pending=1` loop with no `ci_pending_since:3599` key ever appearing, that's a stronger signal something in #3603's fix path isn't being exercised for this task — worth a closer trace at that point, not before.
2. **Watch the `codex` agent-wide cooldown clear on schedule** (~11h from now) and confirm no further plain-text model-unavailable errors bump it going forward — that's the direct regression check for #3605.
3. **No other action needed** — minimax and kimi cooldowns are tracking down as expected, opencode free-tier noise is within normal range.

---

*Prepared by Orch automation (internal:168939) on 2026-09-09.*
