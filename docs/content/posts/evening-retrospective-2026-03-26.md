+++
title = "Evening Retrospective -- 2026-03-26"
date = 2026-03-26T23:59:00Z
[extra]
job = "evening-retrospective"
+++

## Summary

Exceptional throughput day: ~20 issues closed, 34 commits landed. The dominant theme was
**review system correctness** — response parsing false positives/negatives, hardcoded timeouts,
stale state corruption, and systematic default-branch detection failures across multiple subsystems.
By end of day the review pipeline is substantially more robust, though two issues required human
escalation (blocked).

---

## Accomplished Today

### Review Response Parsing (5 fixes)

- **False approval on negation phrases** (#1059) — `"not approved"` was triggering the approval
  path because the keyword match was a substring search. Fixed with phrase-level negation checks.
- **Substring match too broad in synthesize_response** (#1055, #1058) — `"incomplete"` / `"not done"`
  were being marked as done due to broad substring matching; `"approved"` was similarly over-matching.
- **Missing approval keywords** (9d9df5a) — `"all checks passed"`, `"no issues found"`, etc. were
  not recognized as approval signals. Tasks were being re-routed unnecessarily.
- **Issue/comment plain-text acceptance** (#1053) — runner now accepts issue-creation and
  comment-posting acknowledgments as `done` status, preventing false `needs_review` states.

### Hardcoded Timeouts Eliminated (4 fixes)

All four were the same root cause — timeout was hardcoded rather than reading `workflow.timeout_seconds`:

- Session runner hardcoded 30min (#1073)
- Review agent hardcoded 600s (#1040 / #1042)
- `gh pr checks --watch` had no timeout at all (#1060 / #1063)

### Default Branch Detection Fixed Systematically (#1044, #1045, #1046, #1048, #1057)

`origin/main` was hardcoded or config-read in five separate locations. All now use
`detect_default_branch()` against the remote. This was causing silent failures on
non-`main` default branches across rebase, build_git_log, build_git_diff, and auto-merge.

### Review Infrastructure Hardening

- **Stale output.json corruption** (#1065) — review attempt directory was reused on failure,
  causing crash detection to read stale data from a previous attempt.
- **Review agent excludes all previous agents** (210afdb) — was only excluding agents that failed,
  not all prior review agents. Could re-dispatch to the same agent on non-failure cycles.
- **Pre-existing failure baseline** (#1069 / #1078) — review prompt was blocking PRs for test
  failures that existed before the PR. Now checks a baseline before requesting changes.
- **CI trust qualification** (4f0e942, f434909, 195f667) — review prompt now only trusts CI when
  branch is up to date with main; simplified to CI-first without contradictory fallback logic.
- **Review failure comments posted on PR** (a62fd7b) — failures were not being surfaced to the
  PR, making debugging opaque.
- **PR kept open when task is blocked** (0d8af1c) — blocked tasks were closing their PR.

### Runner / Infrastructure

- **DispatchGuard key leak on panic** (#1072) — currently in review.
- **Service version written at startup** (#1037 / #1043) — enables CLI/service drift detection.
- **Kill orch tmux sessions on graceful shutdown** (0b2ad9d) — orphaned sessions were surviving
  service restarts, causing stuck task re-dispatch.
- **Pricing model coverage extended** (#1061) — github-copilot, free-tier, deepseek, gpt-5 codex
  models were missing from cost reports.
- **DispatchGuard** counter separation (#1056) — push failures were depleting `pr_create_failures`,
  causing premature task blocking.
- **Test isolation** (#1070 → #1079) — build_runner_script tests were sharing `~/.orch/state/`
  with the running service, causing flakes and state pollution. Fixed with isolated tmp dirs.
- **Migration rename** (ea75e32) — duplicate migration 009 renamed to 010 to prevent silent
  schema skip.

### Agent Prompt Quality

- **Checklist item 2 qualified for non-code tasks** (#1064) — mandatory commit requirement was
  confusing read-only / issue-creation tasks into infinite retry loops.
- **NDJSON parser uses production parser in integration tests** (#1038 / #1049) — tests were
  validating a copy of the parser, not the production path.

---

## What Failed / Needed Escalation

### Blocked: #1039 — Stale remote refs before review

Closed as `blocked`. The review agent was starting without a `git fetch`, so `build_git_diff` and
baseline checks were operating on stale remote refs. No fix landed — escalated to human. This could
cause false diffs or missing baseline data on repos with active main branch.

**Status**: Needs a follow-up fix. Should be straightforward: add `git fetch --prune` at the start
of the review agent invocation, before any diff or baseline check runs.

### Blocked: #1070 — Superseded by #1079

Closed as `blocked` but the root cause was resolved by the follow-up fix in #1079 (test isolation
via tmp dirs). The blocked status is a naming artifact — effectively done.

---

## Routing Accuracy

Today's closed issues used a mix of agents:

| Agent    | Issues Closed |
|----------|--------------|
| claude   | ~8           |
| opencode | ~7           |
| kimi     | 1            |

Complexity routing: most issues tagged `simple` or `medium`. No `complex` tasks today.
Routing accuracy appeared good — no obvious misroutes. The kimi dispatch (#1041) for a simple
bare-repo branch name fix is appropriate.

The #1039 blocked case is worth noting: `agent:claude` + `complexity:medium` — the task was
feasible but was closed as blocked rather than fixed. Could indicate the task prompt was
under-specified or the agent gave up rather than solving it.

---

## Patterns & Health

**Positive:**
- Volume sustained: pipeline processed ~20 issues in a single day with no human intervention.
- The review system is now substantially more robust — false approvals, false rejections, and stale
  state corruption were all addressed in one day.
- Hardcoded timeout elimination was systematic — all four were found and fixed.

**Concerning:**
- **Prompt drift**: review_task.md received 4+ commits today (CI trust, baseline check, timeout,
  simplification). This level of churn suggests the review prompt is complex and fragile. Each fix
  adds a constraint; the prompt may be accumulating contradictions.
- **Two blocked closes**: #1039 and #1070. While #1070 was a naming artifact, #1039 is a real
  unresolved issue. Blocking and closing rather than filing a follow-up is a data-loss risk.

---

## Open at End of Day

| ID     | Status      | Title                                                              |
|--------|-------------|---------------------------------------------------------------------|
| #1076  | in_progress | Tests use ~/.orch/state/ — causes flaky tests and pollutes real state |
| #1072  | in_review   | task runner dispatch doesn't use DispatchGuard — key leaks on panic |

Both are in-flight. #1076 is the broader test isolation fix (beyond build_runner_script); #1072 is
the DispatchGuard leak.

---

## Tomorrow's Priorities

1. **#1039 follow-up** — stale remote refs before review agent. File a new issue or re-open #1039.
   Simple fix: `git fetch --prune` at review agent start. High impact: affects diff correctness.
2. **#1076 completion** — broader test isolation pass across all test files touching `~/.orch/state/`.
3. **#1072 review** — DispatchGuard key leak; monitor review agent outcome.
4. **Review prompt stability** — after 4+ changes today, do a read-through of `review_task.md`
   and `review_system.md` for contradictions or over-constraints that could cause the next round
   of unexpected behavior.
5. **CLI version drift** — `brew upgrade orch` to sync CLI with service version (#1037 landed
   service-side version reporting; CLI should now reflect the correct version post-upgrade).
