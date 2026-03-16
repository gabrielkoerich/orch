+++
title = "Evening Retrospective — 2026-03-16"
date = 2026-03-16T22:00:00Z
[extra]
job = "evening-retrospective"
+++

## Summary

Heavy day — 13 commits, 12 PRs merged — with a significant late-day discovery:
the code-review job was creating 9 identical duplicate issues before a dedup guard
landed. A security fix also shipped: `GH_TOKEN` was leaking through the tmux global
environment into new sessions. Both issues were fixed same-day. Three stale duplicate
PRs (#657, #663, #672) remain open and should be closed manually.

Morning review's three carry-over items (bean project verification, dispatch_key
regression test, code-development prompt gap) went unaddressed — system was busy
processing the panic/unsafe cleanup wave.

---

## Recent Changes (last 12 hours)

| Commit | Description |
|--------|-------------|
| `e67377a` | fix: dedup delegations to prevent duplicate GitHub issues |
| `bce527b` | fix: scrub GH_TOKEN from tmux global env after session cleanup |
| `249d40e` | Audit and replace panics/unwraps — runtime-critical modules (#669) |
| `698bf32` | Remove unnecessary `unsafe` env manipulation in tests (#659) |
| `3d83ebb` | fix: avoid panics in template and security (#666) |
| `9069cdc` | docs: align review reroute status (#662) |
| `0f4dd85` | test: harden env var tests (#658) |
| `26df4fd` | fix: skip release for chore/docs-only commits (#650) |
| `99af0b5` | Update base URL in config.toml |
| `c972662` | refactor: remove dead link_issue_to_branch from git_ops |
| `510ba32` | fix: link issues to branches via Development sidebar |
| `379bbb2` | fix: route successful tasks through review agent before marking done |
| `25f8e68` | fix: stop pulling main every tick when no worktrees need cleanup |

---

## What Completed Today

### Panic / unsafe cleanup wave

The code-review job delegated two tasks: "Audit and replace panics/unwraps in
runtime-critical modules" and "Remove unnecessary `unsafe` env manipulation in
tests." Both were real improvements:

- **Panics**: Only one actual production-code `expect()` was replaced (in
  `runner/mod.rs:443`). All other `unwrap`/`expect` calls were in test code or
  static regex init — legitimately acceptable per task guidelines.
- **Unsafe env**: Removed `unsafe { std::env::set_var/remove_var }` from tests,
  replaced with RAII `TempEnvVar` guard (later replaced with `temp-env` crate
  in the Kimi pass). Tests are now hermetic in parallel execution.

Both fixes are correct and landed cleanly. The problem was not the fix quality —
it was the 9x duplication (see below).

### fix: skip release for chore/docs-only commits (`26df4fd`)

CI was cutting a new GitHub release on every merge, including `docs:` and `chore:`
commits. Now checks commit type before tagging. Reduces release noise.

### fix: route successful tasks through review agent before marking done (`379bbb2`)

Tasks were being marked `Done` without going through the review agent first. This
meant agent work was merging without human-readable summaries or review comments.
Fix enforces the full lifecycle: `needs_review → in_review → done`.

### fix: link issues to branches via Development sidebar (`510ba32`)

`link_issue_to_branch` was dead code removed in a prior refactor. This fix
re-implements the link using the GitHub Issues API's development sidebar feature.

### fix: dedup delegations (`e67377a`) — ROOT CAUSE FIX

The code-review cron job ran periodically and produced the same two delegation
titles on each run. `process_delegations` had no dedup guard — it called
`create_sub_task` unconditionally, creating 9 copies of each issue before the fix
landed. The fix fetches existing open task titles before creating delegations and
skips any that already exist. This is the correct single-point fix.

### fix: scrub GH_TOKEN from tmux global env (`bce527b`) — SECURITY FIX

The old bash-era runner injected `GH_TOKEN` into the tmux global environment.
Global env in tmux persists across sessions — unlike per-session env which dies
with the session. After a session was cleaned up, the token remained visible to
new panes/windows. Fix: `cleanup_session` now calls `tmux set-environment -gu`
to unset both `GH_TOKEN` and `GITHUB_TOKEN` from the global env after killing
the agent session.

---

## Failures and Retries

### Delegation duplication — 9x flood

**Root cause**: `process_delegations` had no dedup. The code-review job (cron)
ran every tick, and on each run the review agent produced identical delegation
titles ("Audit and replace panics/unwraps..." and "Remove unnecessary `unsafe`
env manipulation..."). No check existed for existing open issues with the same
title, so 9 copies of each were created before `e67377a` landed.

**Visible damage**:
- Issues #651–676 — 9 duplicates of each title created and immediately closed
- PRs #657, #663, #672 — three stale PRs from duplicate tasks, still open
- Significant CI churn (18+ PR runs, most redundant)

**Fix landed**: `e67377a` — title-based dedup at delegation creation time.

**Residual**: Three open PRs (#657, #663, #672) targeting branches from duplicate
tasks. Their branch content is either superseded by the merged PRs or incomplete.
These should be closed and their branches deleted. Flag for manual cleanup — not
worth filing an issue.

### Morning priorities not addressed

| Priority | Status |
|----------|--------|
| Bean project SSH push verification | Not done — no bean task dispatched today |
| Dispatch_key race regression test | Not done — third day carried over |
| Code-development prompt gap (closed issues) | Not done |

---

## Agent Prompt Assessment

**`prompts/agent_system.md`** — The infrastructure-failure guard added on 2026-03-15
held: no spurious issues were filed about infrastructure failures today. The dedup
flood was a separate mechanism (delegation pipeline, not infrastructure errors).

**`prompts/review_task.md` / `prompts/review_system.md`** — The review agent's
delegation output is where the duplicate titles originated. The review agent produced
structurally correct JSON, but the engine's `process_delegations` lacked the dedup
guard. Prompt is fine; engine fix is the right layer.

**One gap remaining**: The code-development prompt still does not explicitly mention
`gh issue list --state closed --since 24h`. Engine-level dedup (`e67377a`) handles
this, but agents creating issues via other paths (direct `gh issue create`) bypass
it. Low priority — no active bug — but worth adding next time the prompt is touched.

---

## Routing Accuracy

| Task | Routed To | Outcome |
|------|-----------|---------|
| Morning review (`internal:1139`) | claude/sonnet | ✓ Correct |
| Panic/unwrap audit (#651–#669 series) | kimi/sonnet, opencode/gpt-5-mini, claude/sonnet | ✓ All correct for code cleanup |
| Skip release fix (#648→#650) | claude/sonnet | ✓ Correct — CI config change |
| Dedup delegations (`internal:~`) | claude/opus | ✓ Correct — subtle engine fix |
| GH_TOKEN scrub | claude/opus | ✓ Correct — security/tmux fix |
| Route tasks through review (#379bbb2) | claude/opus | ✓ Correct — lifecycle logic |

Routing was accurate. The only issue was **volume** — the same task was routed
9 times due to the delegation flood. Per-dispatch routing decisions were correct.

---

## Performance

- **13 commits, 12+ PRs merged** — high throughput but ~60% was redundant churn
  from the duplication flood
- **18+ duplicate issues created and closed** — significant GitHub API overhead
- **Zero stuck tasks**: no manual unblocking needed
- **Zero open issues after close of day** (flood issues auto-closed as duplicates)
- **Three stale PRs open** (#657, #663, #672) — manual cleanup needed
- **CI**: Multiple redundant runs from duplicate branches. All green. No flaky tests.

---

## Open Items

**Stale PRs to close manually (not worth filing a task)**:
- PR #657 — "Remove unnecessary `unsafe` env manipulation" (opencode, task #652)
- PR #663 — "Audit and replace panics/unwraps" (kimi, task #653)
- PR #672 — "Remove unnecessary `unsafe` env manipulation" (kimi, task #668)

All three are superseded by the merged PRs (#659, #669). Close with a comment
explaining the duplication.

---

## Issues Filed

**None.** Root causes addressed in-code today:
- `e67377a` — dedup delegations (prevents the flood recurrence)
- `bce527b` — GH_TOKEN scrub (security fix)

The three carry-over priorities do not warrant new issues — they are already known
and tracked in retrospective posts.

---

## Tomorrow's Priority

1. **Close stale duplicate PRs** (#657, #663, #672) — 5-minute manual cleanup.
   Closes noisy open PR state and cleans up dangling branches.

2. **Bean project end-to-end verification** — Third carry-over. The SSH push fix
   (`7d0b14f`) and per-project jobs fix (`e86c75c`) are both in. The first bean
   task dispatch is the integration test. Watch logs for push failures.

3. **Dispatch_key race regression test** — Low urgency but clean win. Prevents
   re-introduction of the silent-review-loss race (`a6d8b9a`). One targeted test
   in `sync.rs` is sufficient.

4. **Code-development prompt gap** — Add `gh issue list --state closed --since 24h`
   to the prompt. Belt-and-suspenders on top of engine dedup. Do this the next time
   the prompt is edited for any reason — not a standalone task.
