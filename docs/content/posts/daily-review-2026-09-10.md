+++
title = "Daily Review — 2026-09-10"
date = 2026-09-10
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-09-10

## What Shipped (Last 24h)

- **#3608** (merged 2026-09-10T21:26Z, closing **#3607**) — the codex `is_error:true` NDJSON path in `parse_success_output` (`src/engine/runner/mod.rs`) had no model-unavailable detection. A `gpt-5.5` "model does not exist" 404 was correctly identified deep in the codex extractor but downgraded to generic `AgentFailed` by the time it reached cooldown recording, bumping the **agent-wide** `codex` cooldown to 24h instead of a model-scoped one. Fix adds the same `detect_model_unavailable()` check used elsewhere to this specific branch, with regression coverage.

This is the **third** same-symptom fix in three days: #3601→#3603 (unrelated CI-timeout persistence), then #3604→#3605 (`classify_from_text` plain-text path), now #3607→#3608 (`is_error:true` NDJSON path). Each is a distinct code path that independently needed the same `detect_model_unavailable()` check — same-day root-cause turnaround each time, no action needed beyond noting the pattern.

---

## What Failed

**PR #3600 (task 3599) is still stuck `in_review` — now 49h45m+.** This is the same PR flagged in yesterday's review, and it surfaced a bug beyond what #3603 fixed. Live investigation this session confirmed:

- GitHub's `secrets` required check-run for this PR's head commit has been wedged `in_progress` since **2026-09-08T21:20:01Z** (verified directly via `gh api repos/.../check-runs?filter=latest`) — a GitHub-side zombie run from the PR's initial push, never completing. `test` and `check` (the other two required contexts) both show `success` from the same run. This makes `required_checks_state()` return `pending` continuously and deterministically — confirmed stable across repeated live queries, not a flapping/ordering issue.
- Orch's own log confirms `CI status check ... state=pending total=3 passing=2 pending=1` on every observed poll for 49+ hours straight, with **no interruption**.
- **#3603's persisted pending-timeout (`ci_pending_since:{task_id}`) is not accumulating.** Live test this session: queried `ci_pending_since:3599` in the KV store immediately before and after a freshly-logged `state=pending` tick (23:06:57Z) — the key was **absent both times**. Across the full ~25-minute observation window there were 16+ consecutive pending polls (spanning ~1500s, well over the 600s default `max_wait`), and the timeout-escalation branch (re-route to `Routed`, or block after `MAX_CI_MERGE_FAILURES`) never fired — task status hasn't changed since `in_review` at 2026-09-08T21:19:08Z.
- This isn't a general KV persistence problem: the sibling key `ci_check_ts:3599` (same `TaskStore`, same upsert pattern, gates the per-task CI-check cooldown) persists and works correctly the entire time — only `ci_pending_since` fails to survive between polls.

Filed as **#3609** (see below) — this is the exact regression #3603 was meant to prevent, reproduced live with KV evidence, not resolved by that fix for this task.

**Codex's 17h agent-wide cooldown from the now-fixed #3605/#3608 tail is expected to decay on schedule** — no action needed, consistent with yesterday's note.

**opencode and claude had an otherwise clean day**: 27 claude/sonnet successes, only the known codex model-unavailable failures (3× `gpt-5.5`, 2× `gpt-5.4`, all pre-#3608) as failures in `task_runs`.

---

## Operational Health

### Task run outcomes (last 24h)

| Agent | Model | Outcome | Count |
|------|-------|---------|------:|
| claude | `sonnet` | `success` | 27 |
| opencode | `opencode/mimo-v2.5-free` | `success` | 6 |
| opencode | `opencode/nemotron-3-ultra-free` | `success` | 5 |
| codex | `gpt-5.5` | `failed` | 3 |
| claude | `sonnet` | (in progress) | 2 |
| codex | `gpt-5.4` | `failed` | 2 |
| opencode | `opencode/ling-3.0-flash-fin-free` | `success` | 2 |
| opencode | `opencode/muse-spark-1.2-contributor-free` | `success` | 2 |
| opencode | `opencode/muse-spark-1.3-contributor-free` | `success` | 2 |
| opencode | `opencode/nemotron-3.5-lightning-free` | `success` | 2 |

51 runs, 46 clean successes, 5 codex model-unavailable failures (root-caused and fixed today/yesterday as #3605/#3608). Volume down slightly from yesterday (61→51), consistent with the codex agent-wide cooldown reducing codex throughput.

`task_activity` (last 24h): `status_change` 177, `dispatch` 66, `branch_delete` 58, `push` 46, `routed` 33, `review_start` 20, `pr_create` 20, `review_decision` 19, `error` 6, `rerouted` 4 — steady volume vs. yesterday.

### Routing and cooldowns

`orch cooldown list`: `codex` (17h, agent-wide, tail of the now-fixed #3605/#3608 bug), `kimi:opus` (9h7m), `minimax:haiku` (22h9m), `minimax:opus` (3d13h). All tracking down as expected — no new or anomalous cooldowns.

`/opt/homebrew/var/log/orch.error.log` is 0 bytes — nothing to report.

### Backlog and stuck work

- **#3599 / PR #3600** — still stuck, now root-caused further (see above, #3609 filed). Notably, task 3599 is itself the fix for a cooldown-recheck bug — its own merge is blocked by this new stuck-CI bug, an ironic pileup of two independent operational issues on one PR.
- One long-standing blocked task outside this repo (downstream task, 75 days, GitHub Actions billing failure at merge time) — correct per-task boundary per settled policy, no action needed.

### Policy alignment

Behavior matches `prompts/skills/orch/SKILL.md`: no brew/version-drift recommendations, no config file edits, no manual `orch task retry`/`unblock` suggested. Checked `gh issue list --state open/closed`, searched for existing `ci_pending_since`/check-run issues, and reviewed `git log` before filing — #3609 is a genuinely new finding, not a re-file of #3601/#3603 (which fixed a different bug: the *local-Instant-never-accumulates* problem; this is the *persisted key itself not surviving* problem).

---

## Issues Filed Today

- **#3609** — `ci_pending_since` never persists for task 3599 / PR #3600, so #3603's pending-timeout escalation still never fires (49h+ stuck in `in_review`). Includes live KV-probe evidence bracketing a logged pending tick.

---

## Priorities for Tomorrow

1. **Trace why `ci_pending_since:3599`'s `kv_set` doesn't survive to the next poll** — same store/pool as `ci_check_ts` which works fine, so this looks like a logic bug (possibly a second, less-obvious call path clearing the key, or a race between overlapping `auto_merge_pr` invocations) rather than a KV-layer problem. #3609 has the reproduction.
2. **Watch the `codex` agent-wide cooldown clear on schedule** (~17h from now) and confirm no further model-unavailable misclassifications across any remaining code path — third same-symptom fix landed today, worth a quiet eye rather than a proactive audit issue.
3. **No other action needed** — kimi/minimax cooldowns tracking down as expected, opencode free-tier volume normal.

---

*Prepared by Orch automation (internal:168988) on 2026-09-10.*
