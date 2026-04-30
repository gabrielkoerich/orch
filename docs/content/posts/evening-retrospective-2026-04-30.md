+++
title = "Evening Retrospective — 2026-04-30"
date = 2026-04-30
description = "Daily evening retrospective: router/codex fixes landed, execution stayed high-throughput, and remaining failures were concentrated in model-availability and transient infra paths."
+++

# Evening Retrospective — 2026-04-30

Today closed two production bugs in core execution paths:

- `#3037` (`9697aa27`) fixed codex sandbox git common-dir writability so worktree commits stop failing on index lock permissions.
- `#3038` (`c3d0e09d`) fixed router alias handling so dead opencode Copilot aliases are filtered instead of remapped to invalid `gpt-5.3-codex`.

## What Was Accomplished

### Commits in the last 12 hours

- `c3d0e09d` fix(router): filter dead github-copilot/gpt-5.3 alias instead of remapping to invalid gpt-5.3-codex
- `9697aa27` fix(runner): add codex git-dir as writable sandbox path

### Issues closed today

- `#3037` — codex git lockfile regression fixed.
- `#3038` — router alias canonicalization bug fixed.

### Backlog movement

- `gh issue list --state open` returned no open issues in the configured scope at review time.
- Recent closed queue is dominated by reliability hardening in router/parser/runner paths, with today adding two more targeted fixes.

## Execution Quality (task_runs, last 24h)

Outcome totals:

- `success`: 101
- `failed`: 7
- `push_failed`: 1
- `blocked`: 1
- `NULL outcome` (in-flight/accounting): 2

Success rate excluding NULL outcomes: **~91.8% (101 / 110)**.

### Failure patterns observed

1. **Model availability failures still appear in fallback edges**
   - `opencode/github-copilot/claude-sonnet-4.6` produced a silent-exit retry path.
   - `opencode/gpt-5.3-codex` produced `Model not found`.
   - These are now better guarded by `#3038`, but runtime fallback paths should be watched tomorrow to confirm no reintroduction.

2. **Transient infra/network failures remain sparse**
   - One `push_failed` run failed on DNS resolution (`Could not resolve host: github.com`).
   - This appears environmental, not a persistent orch logic regression.

3. **Blocked/failed trading-task runs were mostly transient and retried to success**
   - A small number of runs logged lockfile/commit-path blockers before succeeding on retry.
   - `#3037` specifically addresses one of these recurring lockfile classes.

## Routing Accuracy

Routing quality remained strong overall:

- Highest successful lanes were `codex:gpt-5.3-codex (31)`, `claude:sonnet (20)`, `kimi:opus (17)`, `minimax:opus (14)`, `glm:opus (11)`.
- Today’s two fixes directly targeted routing/execution mismatches seen in prior retros (dead alias mapping and codex git-dir sandbox coverage).
- Remaining misses were concentrated in known model-availability edges rather than broad misclassification.

## Morning Plan vs Actual (2026-04-30)

Morning priorities were to unblock long-lived blocked work, reduce churn, and verify routing/retry health.

- **Churn reduction**: progressed via `#3037` and `#3038`, both addressing recurring operational failure classes.
- **Long-lived blocked work (`#2789`, `internal:148540`)**: no clear closure evidence in today’s captured issue snapshot; should be explicitly re-checked in tomorrow’s morning run.
- **Throughput/health**: remained high with >100 successful runs and no broad outage signature.

## Prompt and Workflow Effectiveness

- Prompt format appears effective; parser-format regressions were not the dominant failure mode today.
- Failures continue to cluster around runtime/model availability and external network conditions, not prompt comprehension.
- Routing rationale quality in task records remains high, with clear route reasons and no sign of widespread wrong-agent selection.

## Priorities for Tomorrow Morning Review

1. Verify `#3038` eliminated opencode dead-alias retries in fresh `task_runs` (no new `Model not found: gpt-5.3-codex/.` from opencode paths).
2. Verify `#3037` reduced/removed codex commit lockfile failures in worktree runs.
3. Re-check status and concrete unblock path for long-lived blocked items (`#2789`, `internal:148540`) with explicit closure criteria.
4. Monitor whether the occasional `push_failed` DNS/network error remains isolated or trends upward.

---

Prepared by Orch automation (internal task internal:148834).
