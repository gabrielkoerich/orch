+++
title = "Morning Review — 2026-03-15"
date = 2026-03-15T14:55:00Z
[extra]
job = "morning-review"
+++

## Summary

Highly productive overnight session — 15 commits merged since yesterday's
morning review. The #1 priority from last night's retro (issue dedup for
recently-closed issues) landed before the review was even written (#628).
All three race condition fixes from yesterday appear stable. CI is green.
Three bugs are open and being dispatched. One new issue filed this morning
(push auth failure for SSH-remote projects). Two small inline fixes applied.

---

## Recent Changes (last 24 hours)

| Commit | Description |
|--------|-------------|
| `f7184c4` | docs: evening retrospective 2026-03-14 (#627) |
| `1f0adf0` | fix: extend issue dedup check to include recently-closed issues (#628) ✅ retro #1 |
| `855a44e` | fix: use SQLite status as source of truth in task list |
| `fc9a3a3` | fix: show all tasks in `orch task list` and skip link for internal tasks |
| `c817bbc` | fix: ingest unlabeled GitHub issues into SQLite store |
| `148804b` | fix: treat zero CI check-runs as success in get_combined_status |
| `2ae3202` | fix: prevent auto-merge race condition with per-task dispatching lock |
| `e29e830` | fix: stop pulling main every 45s in cleanup tick |
| `d7e55d6` | fix: tell agents not to pollute summary with push failure messages |
| `5c16bc5` | fix: skip build-macos on chore/docs commits to save runner minutes |
| `6918397` | fix: mark tmux integration test as #[ignore] to fix CI |
| `9de4a8d` | fix: break dispatch loop when work already merged + fix CI tmux test |

15 commits in 24 hours. Heavy reliability and correctness focus.

---

## Evening Retro Priorities: Status

From the 2026-03-14 evening retrospective:

| Item | Status |
|------|--------|
| Fix issue dedup to include recently-closed issues (24h window) | ✅ Done — `1f0adf0` (#628) |
| Add abort guard to agent setup steps (error propagation) | ✅ Done — added to `prompts/agent_system.md` this morning |
| Verify race condition fixes hold | ✅ No recurrence observed in logs |
| Router timeout 120s → 60s | ✅ Done — reduced in `src/engine/router/config.rs` this morning |

All four carry-over items are resolved.

---

## Health Check

**CI**: Green. Recent runs: success on main, skipped on docs/chore branches.

**Open Issues**: 3 active bugs:
- #631: bug: CI timeout in auto_merge_pr re-routes to NeedsReview instead of New
- #630: bug: list_tasks with status filter bypasses SQLite store, reads stale GitHub labels
- #629: bug: job scheduler treats NeedsReview/InReview as terminal — creates duplicate tasks

**Service**: Running at v0.14.45. Engine dispatched this morning review task at 14:53:40Z.

**Log observations**:
- Push failure for `bean` project task 10 (14:53:34Z): SSH remote converted to
  bare HTTPS without token — push failed, task marked done with work undelivered.
  Root cause traced and issue filed (#633).
- Cleanup tick reconciling many stale tasks (IDs 1–16) from old `bean` project runs.
  All correctly marked done. Expected after re-ingestion fix.
- Repeated "dispatchable tasks found count=1" every 10s is the dispatch loop
  polling while this task runs. Normal behavior.
- `orch.error.log`: continued "no valid projects configured" spam from CLI
  invocations outside the project directory. Benign.

---

## Inline Fixes Applied This Morning

**1. `prompts/agent_system.md` — infrastructure failure guard**

Added explicit instruction: if GitHub setup operations fail (branch creation,
`gh issue develop`, GraphQL link errors), stop immediately and set status to
`needs_review` — do NOT create issues about infrastructure failures. This
addresses the retro priority 2 and prevents recurrence of PR #624.

**2. `src/engine/router/config.rs` — router timeout 120s → 60s**

Reduced `timeout_seconds` default from 120 to 60. Carry-over from the 2026-03-13
retro. Low-risk one-line change — routing LLM calls are fast and 120s was
overly conservative.

---

## Issues Filed

**1 issue: #633** — `bug: push fails for SSH-remote projects — token not injected
when converting git@github.com to HTTPS`

Root cause: `push_branch()` in `git_ops.rs` uses
`url.https://github.com/.insteadOf=git@github.com:` to convert SSH to HTTPS
but does not inject the GitHub token. Orch-managed projects (bare HTTPS clone
with embedded token) are unaffected. Local projects with SSH remotes (e.g.
`bean`) fail on push. Fix: inject token as
`url.https://x-access-token:<token>@github.com/.insteadOf=git@github.com:`.

---

## Notes

**Job scheduler bugs (#629, #630, #631)** — Three related bugs were filed
overnight, all touching task status and dispatch logic. These are likely
interrelated: the SQLite status bypass (#630) could explain why the job
scheduler sees NeedsReview tasks as dispatchable (#629). Worth addressing
#630 first as it's the most foundational.

**Bean worktree auth** — The bean project currently has SSH as origin. After
the push auth fix (#633) lands, agents will be able to push to bean repos.
Until then, bean tasks will complete but not push (work is lost).
