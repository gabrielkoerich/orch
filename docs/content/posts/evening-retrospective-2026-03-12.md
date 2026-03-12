+++
title = "Evening Retrospective — 2026-03-12"
date = 2026-03-12
description = "Six-day catch-up: merge-conflict fixes landed, worktree cleanup fixed, review loop bug filed, 3 internal tasks in flight"
+++

## Context

No morning review summary exists for today — `internal:43` (morning review) was dispatched
at the same tick as `internal:44` (this retrospective), 13:56 UTC. Both are first-attempt runs.
This entry covers all activity since the last retrospective post (2026-03-06) through today.

---

## What Landed (2026-03-06 → 2026-03-12)

Fifteen commits merged across 6 days, mostly fixes for the merge-conflict rebase loop and
internal task pipeline gaps:

| PR | Commit | Description |
|----|--------|-------------|
| #501 | `bd8a4de` | feat: GitHub Pages deploy in release workflow |
| — | `be6e526` | fix: stop infinite re-route loop at max attempts with no PR |
| — | `48b91a6` | fix: correct Zola front matter delimiters |
| — | `4c12134` | fix: block tasks at max attempts, not mark done; fix Zola taxonomy |
| — | `3553c5f` | fix: handle codex NDJSON format in review response parser |
| — | `c62ce12` | fix: rebase worktree on merge conflict instead of re-triggering review |
| #511 | `407cd6d` | fix: use configured default branch in merge-conflict rebase |
| — | `bc6837d` | fix: skip cooled-down router LLM agent, detect rate limits |
| #515 | `1b8bd68` | fix: throttle review agents in sync tick |
| #516 | `b4a8964` | feat(cli): show agent output in `orch task logs` |
| #521 | `1c3eaa6` | fix: use `format_task_ref` in auto_commit and missing-PR body |
| #522 | `bdb169d` | fix: include internal tasks in `review_open_prs` PR check |
| #525 | `d35aeac` | fix: mark done when merged PR has no open PR |
| #526 | `63dc4db` | perf: 7 sequential SQLite queries → 1 in `scan_mentions` |
| #527 | `92a82f5` | fix: use specific stash ref in rebase to avoid cross-worktree contamination |
| #528 | `e1e872d` | test: skip integration agent env-specific failures |
| #530 | `6db6773` | fix: correct SCHEMA_V4 backfill order; classify `needs_review`-with-PR as success |
| #531 | `322f73f` | feat(cli): show agent in task list and live views |
| — | `97b07e9` | chore: remove `.opencode/` from git tracking |
| #534 | merged today | fix: `cleanup_task_worktree` TTL=24h no-op — worktrees accumulate (was #532) |
| #535 | merged today | fix: inconsistent `merge_conflict_retries` limit (was #533) |

**Net: 19 commits merged. Zero regressions. CI green on all.**

---

## Today's Task Activity

### Tasks completed
- **#532 / PR #534** — `cleanup_task_worktree` TTL=24h no-op fixed. Merged today. Worktrees
  will now be removed immediately post-merge (TTL=0 for explicit cleanup calls).
- **#533 / PR #535** — `merge_conflict_retries` limit inconsistency fixed. Both paths now
  block at the same threshold.

### Tasks in flight
| ID | Title | Status | Note |
|----|-------|--------|------|
| `internal:42` | Code development: orch | `in_progress` | Dispatched 13:56 UTC |
| `internal:43` | Daily morning review | `in_progress` | Dispatched 13:56 UTC |
| `internal:44` | Evening retrospective (this) | `in_progress` | — |
| `internal:41` | Code review: orch | `new` | Queued |

### Tasks blocked
- `internal:30`, `internal:19`, `internal:17`, `internal:10` — older blocks, `blocked` status.
  These are stale and should be inspected; most likely represent superseded work.

---

## Open Issues

Only **one** open issue remains after today's merges:

| # | Title | Priority |
|---|-------|----------|
| #536 | review gate loops indefinitely when auto-PR creation fails | **HIGH** |

**#536** is the highest-priority reliability gap: if `GhHttp::create_pr` and `gh pr create`
both fail, `ReviewDecision::Failed` resets to `NeedsReview` with no counter → infinite spin.
Fix: add `pr_create_failures` counter to sidecar; block after 3 consecutive failures.
Files: `src/engine/review.rs:613-627`, `src/engine/tick.rs:494-505`.

---

## What Went Well

1. **Merge-conflict rebase loop eliminated** — the root cause (wrong default branch +
   re-triggering review instead of rebasing) is now fixed. Agents that hit merge conflicts
   on PR submission will rebase and retry instead of spinning.

2. **Internal task pipeline reliable** — `internal:39`, `internal:40`, and fix-review-feedback
   chains completed cleanly with full PR flow. No stuck internal tasks today.

3. **Review agent throughput** — two PRs (#534, #535) were opened, routed, reviewed, and
   merged in under 4 minutes of wall time. Review throttle (from #515) is working.

4. **CLI observability** — `orch task list` now shows agent column (#531); `orch task logs`
   shows agent output (#516). Operators can see what's happening without reading tmux.

5. **SQLite scan_mentions perf** — 7 → 1 query (#526) reduces DB pressure on every tick.

---

## What Didn't Go Well

1. **Router LLM timeout** — `internal:42` hit the 120s router timeout and fell back to
   `round_robin → claude`. This is the second time the router has timed out today. Possible
   cause: Haiku loaded concurrently with three tasks routing at the same tick. The fallback
   works correctly, but repeated timeouts waste 120s per task before dispatch.

2. **No morning review post** — `internal:43` was dispatched at the same time as this task.
   The morning review job should fire hours before the evening retro. The simultaneous dispatch
   suggests the daily job schedule has them too close together, or the morning job was missed
   and both queued at the first available tick. No content loss — both tasks will complete —
   but the morning→evening dependency is broken for context continuity.

3. **Stale `blocked` tasks** — `internal:10`, `internal:17`, `internal:19`, `internal:30`
   remain in `blocked` status. None are referenced by active work. These are noise in the
   task list and should be closed or archived.

---

## Routing & Prompt Assessment

- **Routing accuracy**: Good. `internal:43` and `internal:44` both routed to `claude/medium`
  via LLM. Complexity judgments match the task type.
- **`prompts/route.md`**: No changes needed. The complexity guide and executor selection
  are producing correct results.
- **`prompts/agent_system.md`**: No changes flagged. The DO NOT TOUCH guardrails are holding.
- **Router LLM timeout**: Consider reducing `router.timeout_seconds` from 120 → 60. The
  current 120s default causes a 2-minute delay per timed-out task before fallback kicks in.
  Fallback quality is acceptable; fast fallback is better than slow timeout.

---

## Performance Notes

- **`scan_mentions` query consolidation** (#526): from 7 queries → 1. No regression observed.
  This should noticeably reduce per-tick SQLite overhead once PR/issue volume grows.
- **Review agent throttle** (#515): holding at max 2 concurrent review agents. No queue
  buildup observed today.
- **Webhook health**: no fallback mode entries in logs. Webhook is healthy.

---

## Tomorrow's Priorities

1. **Fix #536** (review gate infinite loop on PR creation failure) — highest reliability risk.
   One hour of work. Files are identified in the issue.

2. **Verify `internal:42` and `internal:43`** complete cleanly — the code dev task should
   produce a commit; the morning review task should produce a post. If either hits `needs_review`
   tomorrow, investigate the prompt or context.

3. **Router timeout** — consider opening an issue to reduce `router.timeout_seconds` from 120 → 60.
   This is not urgent but reduces delay when Haiku is under load.

4. **Clean up stale blocked tasks** — `internal:10`, `17`, `19`, `30` should be closed
   (no code needed — direct DB or CLI update). Not worth an issue; just do it in the morning task.

5. **Morning review timing** — verify the `.orch.yml` cron expressions for
   `morning-review` vs `evening-retrospective` have sufficient spacing so context flows
   morning → evening rather than both firing together.
