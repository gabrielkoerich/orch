+++
title = "Evening Retrospective — 2026-03-18"
date = 2026-03-18T20:45:00Z
[extra]
job = "evening-retrospective"
+++

## Summary

Highly productive day. Five commits landed, all fix or improvement quality. The
review cycle loop bug — the most important correctness issue this week — is now
fully resolved. Morning priorities were met. No open issues remain.

---

## Today's Commits

| Commit | Description |
|--------|-------------|
| `44f44bf` | fix: mark task Done on approval when auto_close_task_on_approval=false (#701) |
| `20fb743` | fix: preserve review_cycles when re-routing after CI failure or human review (#700) |
| `8c0e8f0` | fix: normalize DOW=0 to DOW=7 in cron expressions |
| `4a6387f` | cli: surface diagnostic store fields for external and internal tasks (#697) |
| `4959837` | fix: move job runtime state from .orch.yml to SQLite |

---

## Morning Review Priorities — Status

| Priority | Status |
|----------|--------|
| Monitor internal:3444 (code-review) and internal:3445 (code-development) | ✅ Both completed successfully |
| Continue bean monitoring — internal:3447 morning-briefing | ✅ bean internal:3493 (trading scan) completed — PR 68 merged |
| "no valid projects" stderr noise investigation | ⚠️ Still present — root cause confirmed (see below) |

---

## What Went Well

**1. Review cycle correctness — two-part fix landed**

Both `#700` and `#701` close the loop on the review cycle lifecycle bug discovered
last retro. The combined fix ensures:
- `review_cycles` is preserved across CI-failure and human-review re-routes
  (was being reset to 0, making `max_review_cycles` unenforceable)
- Approved tasks transition to `Done` correctly when
  `auto_close_task_on_approval: false` (default) — previously they looped
  indefinitely in `NeedsReview/InReview`

These two were the critical correctness issues flagged yesterday. Both closed today.

**2. Job state moved out of `.orch.yml` (4959837)**

Job runtime state (`last_run`, `last_task_status`, `active_task_id`) was being
serialized into the declarative `.orch.yml` config, causing noisy git diffs and
non-atomic YAML mutations. Moved to a new `job_state` SQLite table. Config files
are now truly declarative — no runtime state bleeds back into them.

**3. Cron `DOW=0` normalization (8c0e8f0)**

The `cron` crate uses `DOW=0` for Sunday in some contexts. Normalizing to
`DOW=7` before matching prevents missed Sunday jobs — a subtle correctness issue
that was discovered proactively rather than from a symptom report.

**4. CLI diagnostics surfaced (4a6387f)**

`orch task list` now shows diagnostic store fields (`last_error`, `pr_number`,
`branch`) for both internal and external tasks. Agents and humans debugging stuck
tasks can now see the reason without reading logs. Good ergonomics improvement.

**5. Bean project running cleanly**

`internal:3493` (trading scan) completed end-to-end: agent ran, pushed PR 68 to
bean, review agent (kimi) approved, CI passed, merge succeeded, worktree cleaned up.
Full pipeline health confirmed across both projects.

---

## What Didn't Go Well

**Duplicate evening retrospective tasks created (3495 + 3496)**

The job system fired `evening-retrospective` for both orch and bean at the same
second (20:45:03). This is expected — each project has its own `jobs:` config and
the engine ticks both. The resulting tasks target different worktrees and projects,
so there is no actual problem here. Noted only for clarity.

**"no valid projects" stderr noise persists**

`orch.error.log` continues to emit repeated "no valid projects configured — all
backends failed health checks" messages. Root cause is confirmed: `orch` CLI
invocations from within worktrees (which have no `.orch.yml`) cause the CLI to
fail health checks and exit with this error. The service itself is unaffected —
these are agent-side CLI calls, not the service. The fix is to suppress this
error when the CLI is running in a context with no local `.orch.yml` and should
gracefully degrade instead of printing an alarming error. This has been noted in
every retro since 03-13; it is now worth a proper fix task.

---

## Prompt / Routing Quality

**Routing was accurate today:**
- `internal:3495` (this task): routed `claude/complex` — correct for multi-source
  retrospective synthesis
- `internal:3496` (bean retro): routed `claude/medium` — correct
- `internal:3493` (trading scan): routed `kimi/sonnet` for review — passed in ~45s

No routing anomalies observed.

**Prompt quality:** The job prompts are working well. The code-development task
produced a clean, scoped PR (diagnostic field surfacing + `truncate_err` helper)
without scope creep. No prompt changes needed today.

---

## Open Tasks Filed

No new tasks filed. All today's bugs were already fixed by today's commits.
Only one issue is worth a new task:

### Task: suppress "no valid projects" error when orch CLI runs in worktrees without .orch.yml

**Root cause**: When an agent runs `orch task list` (or any `orch` subcommand) from
inside a worktree with no `.orch.yml`, the health check fails and prints a
misleading fatal-looking error to stderr. The binary exits with an error even
though the service is healthy.

**Fix**: In the CLI path, when no local `.orch.yml` is found and the global config
has no matching project for the current directory, suppress the health-check
failure message (or downgrade it to a debug log). The service path is unaffected —
only the CLI invocation from an unrelated directory needs the graceful degradation.

**Why file now**: 5+ retros have flagged this. The error floods `orch.error.log`
with 30+ lines every session, making real errors harder to spot.

---

## Tomorrow's Priority

1. **File and dispatch the "orch CLI no-project graceful degradation" task** — the
   only recurring annoyance. Root cause is clear, fix is scoped, it will clean up
   logs for all future sessions.

2. **Monitor bean internal:3496** (bean evening retro) — it was dispatched at the
   same time as this task; verify it completes and produces a doc post.

3. **Check orch version after today's five merges** — confirm CI auto-tagged and
   Homebrew formula updated. If `orch version` shows pre-fix version, run
   `brew upgrade orch && brew services restart orch`.
