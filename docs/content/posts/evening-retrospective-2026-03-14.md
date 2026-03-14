+++
title = "Evening Retrospective — 2026-03-14"
date = 2026-03-14T22:00:00Z
[extra]
job = "evening-retrospective"
+++

## Summary

Extremely productive day — 23 commits merged across 10 PRs. The reliability
initiative (unwrap/expect hardening across core modules) reached completion.
Several important race condition fixes landed. The system is now meaningfully
more robust than 24 hours ago. However, a significant efficiency problem
emerged: the same two issues were created 8–9 times each by agents running
concurrently, wasting API quota and agent cycles. This is the #1 item to
address tomorrow.

---

## Recent Changes (last 12 hours)

| Commit | Description |
|--------|-------------|
| `855a44e` | fix: use SQLite status as source of truth in task list |
| `fc9a3a3` | fix: show all tasks in `orch task list` and skip link for internal tasks |
| `c817bbc` | fix: ingest unlabeled GitHub issues into SQLite store |
| `148804b` | fix: treat zero CI check-runs as success in get_combined_status |
| `2ae3202` | fix: prevent auto-merge race condition with per-task dispatching lock |
| `62a7bbf` | Code development: orch (#622) |
| `e29e830` | fix: stop pulling main every 45s in cleanup tick, pull only when needed |
| `c55a8c8` | docs: update all markdown to reflect current architecture (#621) |
| `d7e55d6` | fix: tell agents not to pollute summary with push failure messages |
| `5c16bc5` | fix: skip build-macos on chore/docs commits to save runner minutes |
| `9de4a8d` | fix: break dispatch loop when work already merged + fix CI tmux test |
| `a14f1ee` | fix: recover from poisoned mutex in tick, slack, telegram |
| `0cae1cb` | fix: recover from poisoned mutex in telegram, slack, and runner modules |
| `5c6a879` | Reliability: Audit & remove panicking unwrap/expect (http, tmux, sidecar, token) |
| `ee3bb02` | fix: recover from poisoned mutex in webhook status guards |
| `cb4f70a` | fix: recover from poisoned mutex in ensure_watcher |
| `ea0730d` | fix: replace risky unwrap/expect in cli, slack, and http core modules |

---

## What Completed Today

**Reliability initiative (PRs #602, #605, #608, #614, #615)** — The
unwrap/expect audit across core modules is complete. Five PRs merged, covering
http, tmux, sidecar, token, template, home, cli, slack, webhook, tick. The
engine can no longer panic on a poisoned mutex or missing config in any of
these paths. This was the top priority from the morning review.

**Race condition sweep** — Three distinct races fixed:
1. Auto-merge race: per-task dispatching lock prevents two ticks from
   simultaneously dispatching the same task (#2ae3202).
2. Dispatch loop: when an agent's branch has no diff from main (already
   merged), the task now correctly falls through to `done` instead of
   looping through `needs_review → new` (#620).
3. Cleanup tick: stopped unconditional `git pull` on every 45s cleanup tick
   — only pulls when actually needed (#e29e830).

**SQLite as source of truth** — Task list now reads status from SQLite for
internal tasks, and unlabeled GitHub issues are correctly ingested (#855a44e,
#c817bbc). The CLI `orch task list` output is now accurate.

**Docs** — Comprehensive architecture update (#621) and AGENTS.md/PLAN.md
corrections.

**Morning review items** — All three items from the 2026-03-13 retro were
addressed: PR #562 (SQLite store migration) is confirmed merged, dispatch
loop fixed (#620), SQLite improvements landed.

---

## Failures and Retries

**Issue duplication explosion** — The day's most significant failure mode.
Two issues were each created 8–9 times:
- "BUG: GitHub HTTP pagination may panic when Link header missing" — issues
  #589, #593, #596, #599, #603, #606, #609, #612 (8 duplicates, 10:39–12:23 UTC)
- "Reliability: Audit & remove panicking unwrap/expect across core modules" —
  issues #590, #594, #597, #600, #604, #607, #610, #613, #619 (9 duplicates)

Root cause: when a code-development agent creates an issue and its PR merges
quickly, the issue closes. The NEXT code-development agent dispatch checks
`has_open_issue_with_title` → gets `false` → creates a new issue for the
same bug. Agents running concurrently also all see "no open issue" and
simultaneously file duplicates. The dedup logic in `src/engine/jobs.rs` only
checks OPEN issues, not recently-closed ones.

**PR #624 "Failed to link issue to branch PR"** — An error message
propagated into an issue title. An agent encountered a `gh issue develop`
failure ("failed to link issue to branch"), and this error string became the
issue title and PR title. Merged at 20:19 UTC. This suggests agent error
handling in the code-development task is insufficient — errors during setup
should abort rather than propagate into issue/PR creation.

**Multiple closed PRs for same branch** — PRs #601, #616 (closed without
merge) and #598 for the same reliability audit work. These correspond to
agent attempts that were superseded by later attempts. Expected behavior
once the dispatch loop fix lands.

---

## Agent Prompt Assessment

**Code-development agent prompt needs strengthening.** The prompt instructs
agents to check `git log --since 48h` and open issues before creating new
ones, but agents are not checking recently-closed issues. Adding "also check
`gh issue list --state closed --search '<title>' --since 24h`" to the
code-development prompt would prevent re-creating issues for bugs fixed today.

**Error propagation in setup steps** is insufficiently guarded. The PR #624
incident shows an agent treating a `gh issue develop` failure as a bug to
file rather than an abort condition. The agent system prompt should explicitly
state: "if GitHub setup commands fail (issue develop, branch creation), STOP
immediately — do not file issues for infrastructure failures."

**Routing prompts** are working well. All tasks were correctly classified.
No routing misfires observed.

---

## Routing Accuracy

| Task | Routed To | Outcome |
|------|-----------|---------|
| `internal:57` (opencode fix, existing worktree) | opencode | ✓ Correct — reused branch |
| `internal:58` (code development) | claude/sonnet | ✓ Correct — Rust fixes |
| `internal:993` (code development, #622) | claude/opus | ✓ Correct — complex reliability work |
| `internal:995` (code review, #623) | kimi | ✓ Correct — focused review |
| `internal:1004` (this retro) | claude/sonnet | ✓ Correct — analysis task |

---

## Performance

- **Throughput**: 23 commits, 10 PRs merged in 12 hours. Highest daily
  throughput in recent history.
- **CI**: Green across all runs. nextest migration (#580) working well.
- **API quota**: Wasted on ~16 duplicate issues and their associated
  label/comment operations. Estimate 60–80 excess API calls.
- **Cleanup tick**: No longer pulls main on every 45s cycle — significant
  reduction in git network traffic.

---

## Open Items

**Issue dedup does not check recently-closed issues** — The dedup guard in
`create_self_improvement_issue` (jobs.rs:509) only calls
`has_open_issue_with_title`. When an issue closes within the same day, the
next agent dispatch creates a new one. Fix: extend
`has_open_issue_with_title` to also check issues closed within the last 24h,
OR add a cooldown table in SQLite per issue title.

**Agent prompt gap: error propagation** — Code-development agents should
abort on infrastructure failures (branch creation, issue linking) rather
than filing issues about the failure. One-line addition to agent_system.md.

**Router timeout 120s → 60s** — Still not addressed. Still a one-line
change in `src/engine/router/config.rs:24`. Low priority.

---

## Issues Filed

**1 issue filed: duplicate issue creation when open-issue dedup misses recently-closed issues.**

This is the root cause of the 8–9x duplication today. Filed as a targeted
bug with the specific files and fix scoped.

---

## Tomorrow's Priority

1. **Fix issue dedup to include recently-closed issues (24h window)** —
   Root cause of today's most wasteful failure pattern. The fix is in
   `src/engine/jobs.rs` (extend `has_open_issue_with_title`) and optionally
   the code-development agent prompt (add closed-issue check). One issue
   filed today.

2. **Add abort guard to agent setup steps** — If `gh issue develop` or
   branch creation fails, the agent should stop — not file a new issue about
   the failure. Small addition to `prompts/agent_system.md`.

3. **Verify all race condition fixes hold** — Three races were fixed today.
   Watch the first few dispatch cycles tomorrow for any recurrence of
   duplicate dispatching or spurious `needs_review` loops.
