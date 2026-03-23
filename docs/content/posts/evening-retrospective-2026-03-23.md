+++
title = "Evening Retrospective — 2026-03-23"
date = 2026-03-23T23:00:00Z
[extra]
job = "evening-retrospective"
+++

## Summary

A complete cool-down day. Zero commits, zero issues closed, zero open issues in the
queue. Yesterday's 18-commit sweep closed every outstanding bug — today the pipeline
has nothing left to run. That is a healthy sign: the system self-healed, ran out of
work, and is sitting idle correctly.

---

## What Was Done Today

### Commits (last 12h)

None. Last commit was `aa1a207` on 2026-03-22 (fix: codex and opencode auth classifiers
use `contains_http_status` for 401/403 — #811).

### Issues Closed Today

None. All outstanding issues were resolved on 2026-03-22.

---

## Recap: What 2026-03-22 Delivered

Yesterday was the most productive single day in recent history. For context:

| Issue | Summary |
|-------|---------|
| #811 | Codex/opencode auth classifiers still used bare "401"/"403" after #803 — fixed with `contains_http_status` |
| #808 | `tick_recover_stuck_tasks` checked wrong (main agent) session for InReview tasks — review sessions were always gone, wrong timeout applied |
| #806 | `ensure_label` missing before `add_labels` in failover agent label update |
| #805 | Job scheduler treated `Blocked` internal tasks as active — job never re-ran after block |
| #803 | `detect_auth_error` bare string match hit JSON numbers — transient errors misclassified as auth |
| #801 | `tick_recover_stuck_tasks` skipped InReview tasks entirely after restart |
| #800 | `review_and_merge` swallowed DB errors, misreported as "no worktree found" |
| #797 | Removed stale dead_code placeholders from routing.rs |
| #796 | Eliminated N×2 DB round-trips in `orch task show` |
| #793 | `auto_merge_pr` returned `Err` after setting `Blocked` — callers reset to `NeedsReview` |
| #792 | Stale `last_error="push failed"` blocked approved tasks — never cleared on success |
| #791 | Stale last_error blocked approved tasks — fixed |
| #789 | Deduplicated model resolution — review.rs had its own copy that could drift |
| #788 | ControlSession silently dropped message when no store available |
| #785 | channel_handler TaskSession used `engine_refs.first()` — wrong backend in multi-project setup |
| #784 | CI polling loops uncapped — N concurrent PRs = N×20 GitHub API calls |
| #783 | channel_handler silently dropped NewTask message when no project configured |

17 issues closed in a single day. The auth classifier bug alone had three separate
attempts (#781 filed the root cause, #803 fixed the regex, #810/811 caught the
remaining codex/opencode paths that still used bare string matching).

---

## Analysis

### What Went Well

- **Auth classifier chain fully resolved**: The "401"/"403" false-positive was a
  multi-step fix. #781 identified the root cause, #803 fixed the core logic, then
  code review caught that codex and opencode agents still used the old pattern —
  #811 closed the loop. The review pipeline caught what automated tests missed.

- **`tick_recover_stuck_tasks` hardened**: Two separate issues (#798, #807) hit the
  same function from different angles — first it skipped InReview tasks, then it
  checked the wrong session for those tasks. Both landed same day. The function is
  now significantly more reliable across restarts.

- **No open issues**: The queue is empty. No accumulated debt going into 2026-03-24.

### What Failed or Needed Attention

- **No morning review filed today**: The morning-review job was expected to run but
  no `morning-review-2026-03-23.md` was committed. Either the job didn't trigger, the
  agent had nothing to report (empty queue), or dispatch failed silently. With no
  open issues to analyze, a minimal "all clear" post would still be useful.

- **Recurring auth fix pattern**: Three PRs (#803, #810, #811) were needed to fully
  fix one bug because the fix wasn't applied uniformly across all agents. This suggests
  a shared test or linting check for auth error classifiers would prevent partial fixes
  from landing. A utility function or shared test vector covering all three agent
  response formats would be a good hardening task.

### Routing Accuracy

No new tasks were dispatched today, so routing accuracy cannot be evaluated. Yesterday's
routing was predominantly `agent:claude` (15 of 17 issues). This is consistent with
the bug-fix nature of the work — claude handles medium-complexity Rust fixes well —
but the concentration is worth noting. If the review or code-review jobs create tasks
tomorrow, watch whether any route to codex/opencode/kimi to avoid monoculture.

### Performance / Operational

- **Empty pipeline**: Healthy idle state. No stuck tasks, no error loops.
- **#728 (project picker)**: Was listed as `blocked` in yesterday's retro, now
  absent from the open issue list. Either it was closed or was cleaned up. If it
  was closed without implementation, the interactive project picker feature remains
  unimplemented — worth confirming with owner tomorrow.

---

## Open Issues

None currently open.

---

## Priorities for Tomorrow (2026-03-24)

1. **Confirm #728 status** — was the project picker issue closed/resolved, or is it
   simply missing from the feed? If still pending, it needs owner go/no-go after 4+
   days blocked.

2. **Morning review gap** — investigate why no morning review was committed today.
   If the job failed to dispatch, check the job scheduler and `orch log` for the
   morning-review task. If it ran but had nothing to say, consider whether a minimal
   "queue empty" post is still useful for continuity.

3. **Auth classifier shared test** — the three-step fix for #781 suggests a regression
   test covering all three agent formats (claude, codex, opencode) would prevent
   partial fixes. This is a code-review candidate, not an emergency.

4. **Channel routing smoke test** — now 6+ days pending. The multi-project
   channel_handler fixes (#780, #783, #785) are all merged. Create a deliberate
   cross-project test task to verify end-to-end dispatch works.

5. **Webhook re-enable** — still in polling fallback. With the pipeline stable and
   queue empty, this is a low-risk time to re-enable and verify instant delivery.
