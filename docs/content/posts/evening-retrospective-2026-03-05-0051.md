+++
title = "Evening Retrospective — 2026-03-05 (final)"
date = 2026-03-05
description = "Review agent pipeline closes for internal tasks; open issue triage"
+++

## Summary

This is the final addendum for 2026-03-05. One commit landed after the 0022 retrospective,
closing the last gap in the internal task pipeline: review agent coverage.

The day ends with 5 open issues covering known gaps (CLI, unblock, status routing), all filed,
none duplicated. No new issues needed.

---

## Since the 0022 Retrospective

One commit landed:

| Commit | Description | Impact |
|--------|-------------|--------|
| `478124a` | fix: internal tasks go through review agent like external tasks | Internal tasks that produce PRs now trigger automated review — same quality gate as external tasks |

**What was wrong**: `is_internal_id` was used as a gate to skip the review agent for internal
tasks. This meant self-improvement tasks (morning reviews, evening retros) that created PRs were
never reviewed — they'd complete and land without a second agent checking the work.

**What changed**: the gate is removed. All tasks with a PR branch go through the review agent,
regardless of origin (SQLite or GitHub). Status updates in the review trigger now route through
`task_manager.update_task_status()` so internal IDs hit SQLite and external IDs hit GitHub labels.

---

## Attempt Count Note

This retrospective task (`internal:14`) reached **attempt #8** before completing. Attempts #6 and
#7 failed with auth errors (Claude auth token expired mid-session). Attempt #5 failed with a
duplicate tmux session (`orch-orch-internal_14`).

The retry loop is working, but 8 attempts for a documentation-only task is high. Contributing
factors:
1. Auth token expiry mid-session — no graceful retry within the agent itself
2. Duplicate tmux session names — stale session from previous attempt not cleaned up

These are not new bugs, but worth tracking. The stuck detection threshold reduction (still pending)
would help recover from silent auth failures faster.

---

## Open Issues Status

| # | Title | Status |
|---|-------|--------|
| #446 | `orch task status` hides internal tasks | Open — internal overview gap |
| #443 | External task not updated to NeedsReview on runner infrastructure failure | In review |
| #441 | `orch task unblock` ignores internal task IDs | In progress |
| #435 | `resolve_repo_root` uses wrong path for bare-clone projects | Needs review |
| #431 | Bidirectional channel interaction | Needs review |

No new issues filed. All observed problems are already tracked.

---

## Prompt Effectiveness

| Prompt | Assessment |
|--------|-----------|
| `prompts/agent_system.md` | No changes. Guardrails holding. |
| `prompts/review_task.md` | Updated yesterday to read plan + AGENTS.md before reviewing. |
| `prompts/route.md` | Working. Internal tasks routed correctly via DB path. |

The DO NOT TOUCH sections added to AGENTS.md are the most impactful change this week for agent
reliability. Structural prevention (code + prompt guardrails) outperforms adding more instructions.

---

## Tomorrow's Priorities

1. **#441 (unblock internal tasks)** — in progress, highest operational value. Stuck internal
   tasks cannot be recovered via CLI today.
2. **#446 (task status includes internal)** — operators need visibility into internal task state.
3. **Reduce stuck detection thresholds** — `no_session_stuck_timeout` 600s → 300s; reduces
   recovery time from ~30min to ~15min for failed sessions. Still no issue filed — create one.
4. **Morning review** — should fire tomorrow given the dispatch pipeline is now complete end-to-end.
   If it doesn't appear by 10:00 UTC, check the job scheduler cron expression.
