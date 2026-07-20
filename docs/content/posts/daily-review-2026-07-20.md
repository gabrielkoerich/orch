+++
title = "Daily Review — 2026-07-20"
date = 2026-07-20
description = "Daily review: what shipped, what failed, operational health, and priorities for tomorrow."
+++

# Daily Review — 2026-07-20

## What Shipped (Last 24h)

**3 commits, all same-day fix-forward cycles** — each bug was found, fixed, reviewed, and merged within the 24h window:

| Commit | PR | Fixes | Summary |
|--------|----|----|---------|
| `c03e2312` | #3415 | #3413 | `dispatch_routed_tasks_global` sweep — tasks routed for an inactive/removed repo were stranded in `routed` forever; dispatch is now global like routing already was (#3408) |
| `cd23bf90` | #3418 | #3416 | `resolve_project_dir()` now errors instead of silently falling back to service CWD when a project dir can't be resolved — the fallback was producing "fatal: not a git repository" worktree failures |
| `b4969c79` | #3419 | #3417 | `normalize_status()` gained the `changes_requested_addressed` alias, same class as the existing `changes_addressed` |

**3 issues closed** in the window: #3413, #3416, #3417 (all three above). **Zero issues open** as of this review.

Notably, #3416 was a bug *exposed by* #3415's own fix minutes after it shipped (dispatch sweep started reaching worktrees that had never needed real path resolution before) — caught and fixed same day rather than lingering.

---

## Operational Health

### Task Run Outcomes (last 24h, corrected ISO cutoff)

18 distinct (agent, model, outcome) combinations, dominated by success:

| Agent | Model | Outcome | Count |
|------|-------|---------|------:|
| claude | sonnet | success | 31 |
| codex | gpt-5.4 | success | 13 |
| claude | sonnet | failed | 5 |
| opencode | hy3-free | success | 4 |
| kimi | opus | rate_limit | 2 |
| opencode | nemotron-3-ultra-free | success | 2 |
| opencode | north-mini-code-free | success | 2 |
| codex | gpt-5.4-mini | success | 1 |
| kimi | opus | success | 1 |
| kimi | sonnet | rate_limit | 1 |
| minimax | sonnet | rate_limit | 1 |
| opencode | deepseek-v4-flash-free | success | 1 |
| opencode | mimo-v2.5-free | success | 1 |
| opencode | hy3-free / nemotron-3-ultra-free | failed | 2 |

The 5 `claude:sonnet` failures and 2 `opencode` failures break down as:
- 2× `unrecognized status: changes_requested_addressed` (tasks 3417, 2642) — the exact bug #3417/#3419 fixed; both tasks self-recovered and are now `done`.
- 2× `failed to create worktree ... fatal: not a git repository` (tasks 490, 493) — the exact bug #3416/#3418 fixed; both self-recovered, now `needs_review`.
- 1× `max attempts reached` (internal:155200) — a Claude Code session hit its own 10/10 turn limit; task self-recovered on retry and is `done`. Not an orch bug.
- 1× `unrecognized status: pushed to internal-155195-...` (internal:155195) — agent embedded a branch name in its status string instead of a normalizable alias. Same non-actionable "domain terms leaking as status" category as prior entries (e.g. `funding_rate`); would need an agent-prompt fix, not a parser fix. Only 1 occurrence, task self-recovered, now `done`. Already logged in the orch skill as not worth filing.
- kimi (3×) and minimax (1×) `rate_limit` — all billing-cycle exhaustion, all correctly cooled (`kimi:opus` 47m, `kimi:sonnet` 2h9m, `kimi:haiku` 1d, `minimax:sonnet` 4d10h remaining per `orch cooldown list`). No action needed.

**Net: every failure in the window either was the direct evidence for one of today's three same-day fixes, or is a known non-actionable pattern already documented.** No new unhandled failure classes.

### Active Cooldowns

| Key | Remaining | Reason |
|-----|-----------|--------|
| `minimax:opus` | 2d11h | persisted billing-cycle exhaustion, 7-day cap, unchanged from yesterday |
| `minimax:sonnet` | 4d10h | new billing-cycle exhaustion today, correctly cooled |
| `kimi:opus` | 47m | billing-cycle exhaustion |
| `kimi:sonnet` | 2h9m | billing-cycle exhaustion |
| `kimi:haiku` | 1d | billing-cycle exhaustion |

All cooldowns are model-scoped where the failing model was identified — no agent-wide over-blocking observed.

### Routing Accuracy

One router LLM pool timeout (`internal:155236`, agent=minimax model=haiku, 45s) advanced to the next pool entry and will retry next tick as designed — no misroute reached dispatch. Two pre-emptive health-check WARNs marked `kimi` and `minimax` "degraded (all models cooled)" at 22:34 UTC, consistent with the cooldowns above (both agents had all models cooled at that moment) — expected behavior, not a bug.

### Logs and Service Health

- `orch.error.log` is 0 bytes — no new service-level errors.
- No `ERROR`-level lines in the last 200 lines of the service log; only expected WARNs (router timeout clamp, pre-emptive degraded-agent marks, one slow tick at 50.1s coinciding with the router timeout retry).
- Service restarted cleanly at 22:33 UTC (graceful shutdown → restart, no work lost).

---

## Stuck / Blocked Tasks

Task status distribution is unchanged in shape from yesterday:

| Status | Count |
|-------|------:|
| `done` | 5,115 |
| `blocked` | 50 |
| `in_progress` | 2 |
| `needs_review` | 2 |

`blocked` count held flat at 50 — same pre-existing backlog as prior reports: one downstream repo's CI failures blocked at merge time (per-task, correctly scoped, no repo-wide block per settled architecture), another downstream repo's GitHub Actions billing failure blocking a handful of recurring tasks. Both are external, human-side billing/CI issues outside orch's control — no orch-side action needed, consistent with every prior report.

The two `in_progress` tasks are this review and its sibling evening-retrospective task, both dispatched seconds ago. `routed` is back to 0 (the two tasks stuck there yesterday — subject of #3413 — dispatched successfully once #3415 shipped).

---

## Issues

**0 issues filed this run.** All three bugs surfaced in the window (#3413, #3416, #3417) were already found, fixed, and closed by the time this review ran — the pipeline caught and resolved its own problems same-day without waiting for the daily review to file them. No new operational problem found beyond what's already documented as non-actionable in the orch skill.

---

## Priorities for Tomorrow

1. **Confirm #3415/#3418/#3419 stay fixed** — watch for any recurrence of dispatch-stranding, worktree CWD fallback, or `changes_requested_addressed` parse failures over the next 24-48h as more traffic exercises the new code paths.
2. **Watch `minimax:sonnet`'s new 4d10h cooldown** — first time this specific model has hit billing exhaustion; confirm it clears cleanly on 2026-07-25 without immediate re-exhaustion (same watch pattern as `minimax:opus`, clearing 2026-07-23).
3. **`blocked` backlog (50) stable, not growing** — continue monitoring only for count changes; both root causes are external (downstream billing/CI), not orch bugs.
4. **`routed` returning to 0** confirms #3415's dispatch sweep is working end-to-end — worth a multi-day trend check same as `new` got after #3408.

---

*Prepared by Orch automation (internal:155235) at 2026-07-20 UTC.*
