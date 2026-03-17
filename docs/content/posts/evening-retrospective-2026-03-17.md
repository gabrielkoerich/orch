+++
title = "Evening Retrospective — 2026-03-17"
date = 2026-03-17T20:45:00Z
[extra]
job = "evening-retrospective"
+++

## Summary

Strong day: 5 PRs merged, 4 real bug fixes, 1 quality-of-life improvement. The
`max_review_cycles` enforcement was completely broken (silently wiped on every
`RequestChanges`) and is now fixed. The 422 "no-commits" infinite retry fix from
this morning held. Bean project finally dispatched its first tasks tonight —
a 4-carry-over milestone resolved. Two carry-over items remain (dispatch_key
test, code-development prompt gap) and have been filed as issues.

---

## Today's Commits

| Commit | Description |
|--------|-------------|
| `40a624c` | fix: store_reset_counters wipes review_cycles on RequestChanges (#685) |
| `64e70a5` | feat: AGE + TRIES columns in `orch task list` (#683) |
| `01eb194` | docs: morning review 2026-03-17 |
| `fed6511` | fix: 422 "No commits between" spins task in review loop forever |
| `12ecb2b` | fix: auto_merge_pr returns Err when verify finds changes_requested (#682) |
| `2a68ed9` | fix: auto_close_task_on_approval and workflow.auto_merge inconsistent (#681) |

---

## Morning Priorities — Status

| Priority | Status |
|----------|--------|
| Verify internal:1993 clears | ✅ Fix (`fed6511`) held; no spin observed in logs |
| Bean project first dispatch | ✅ internal:3419 dispatched tonight to bean worktree |
| Dispatch_key regression test | ❌ 5th carry-over → filed as issue |
| Code-development prompt gap | ❌ 3rd carry-over → filed as issue |

---

## What Went Well

### `max_review_cycles` now actually enforced (#685)

`store_reset_counters` was called for **both** `Approve` and `RequestChanges`
outcomes in tick.rs. That reset `review_cycles` after every change request,
making `max_review_cycles` impossible to trigger. The fix splits the handler:
`Approve`/`Skipped` → full reset, `RequestChanges` → only reset per-attempt
counters (not the cycle count). This makes the escalation to human review
actually work.

### 422 "No commits between" — no repeat spin

`fed6511` extended the 422 guard from this morning. Logs show no repeat of the
`internal:1993` spin pattern today. Any analysis-only or mention-response task
now falls through cleanly to `done`.

### `auto_merge` / `auto_close_task_on_approval` consistency (#681 + #682)

Two related bugs fixed together: the review agent was ignoring
`workflow.auto_merge`, and `auto_merge_pr` returned `Ok(())` even when verify
found `changes_requested`, masking the failure. Both now behave correctly.

### `orch task list` AGE + TRIES columns (#683)

Immediate visibility into stuck tasks (old AGE) and looping tasks (high TRIES)
without inspecting each task individually. 5 unit tests included.

### Bean project dispatch milestone

`internal:3419` (bean evening retrospective) was routed, dispatched, worktree
created, and tmux session started at 20:46. This is the first bean project task
to run end-to-end. The agent is running in
`~/.orch/worktrees/bean/gh-task-internal-3419-...`.

---

## What Didn't Go Well

### Routing warning: "profile missing skills"

The LLM router emitted a sanity warning (`routing sanity warning: profile
missing skills`) for both retrospective tasks (internal:3418, internal:3419).
The router classified them as `medium` complexity but didn't include any
`selected_skills`. This is correct behavior for analysis tasks — no catalog
skill applies — but the warning fires regardless. Low priority: consider adding
a check that skips the warning when `selected_skills: []` is intentional for
the task type.

### `orch.error.log` "no valid projects configured" noise

The error log shows repeated "no valid projects configured — all backends failed
health checks" messages. These originate from CLI invocations (`orch task list`)
in worktree directories where no `.orch.yml` is present — the CLI can't resolve
the repo context from inside a worktree. **Not a service health issue** — the
service log shows both `gabrielkoerich/orch` and `gabrielkoerich/bean` connected
successfully. But the noise makes genuine errors harder to spot.

---

## Routing Quality

Both retrospective tasks routed correctly to `claude` at `medium` complexity.
The routing LLM gave well-reasoned responses (21s for bean, 13s for orch). No
misroutes today. The `fallback=round_robin` note in the log is config noise
(not a fallback trigger).

---

## Performance

No rate limit hits, no lock contention observed. Service started at 20:00:47,
two retrospective tasks dispatched at 20:45:34 and 20:45:58 — clean 45s tick
interval. Worktree cleanup for 4 stale tasks (PRs 651, 653, 670, 671) completed
cleanly at startup.

---

## Issues Filed

### #686 — dispatch_key regression test (5th carry-over)
Root cause: `a6d8b9a` dispatch_key race fix has no test. A concurrent
`dispatch_key` update and `sync_tick` read can silently drop review feedback.
One targeted test in `sync.rs` prevents re-introduction.

### #687 — code-development prompt: check closed issues before filing
Root cause: code-development job prompts `gh issue list --state open` but not
`--state closed --since 24h`, causing agents to file issues for problems already
resolved in recent commits. Three instances observed this sprint.

---

## Tomorrow's Priority

1. **Confirm bean internal:3419 outcome** — First bean task ever dispatched;
   verify it completed successfully and the retrospective post landed in the
   bean repo.
2. **dispatch_key regression test** (issue #686) — 5th carry-over; file and
   land the test.
3. **Code-development prompt gap** (issue #687) — low-effort fix: one line
   added to the job prompt. Prevents future duplicate issue filing.
4. **"profile missing skills" warning for analysis tasks** — Optional cleanup:
   suppress the warning when `selected_skills: []` is intentional.
