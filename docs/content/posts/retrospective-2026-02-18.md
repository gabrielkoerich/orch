+++
title = "Daily Retrospective — 2026-02-18"
description = "Evening retrospective on orchestrator service activity"
date = 2026-02-18
+++

## Executive Summary

The orchestrator service started today at 20:52 UTC. Server activity was dominated by
GitHub sync overhead — **76% of log lines (622/818) were gh_push operations**,
most of them redundant re-pushes of already-synced tasks. No new agent tasks were
routed or run today (besides this retrospective task #20). The scheduled jobs tick
ran at 21:00 UTC, creating task #20 and running the test-bash-job (which failed
with exit=1 because its `dir` points to a deleted temp directory).

## 1. Tasks Completed Today

No tasks were completed today. The server started fresh at 20:52 and all pre-existing
tasks were already in terminal states from prior days.

### Current Task Inventory (20 tasks)

| Status | Count | Tasks |
|--------|-------|-------|
| done | 15 | #1-13, #16-19 |
| in_review | 2 | #14 (vault docs), #15 (keeper README) |
| in_progress | 1 | #20 (this retrospective) |
| blocked/needs_review | 0 | — |

## 2. What Failed / Needed Retries

### Today's Failures

- **test-bash-job** (bash job): exit=1 — the job's `dir` points to a deleted temp
  directory (`/var/folders/.../tmp.ALBxJiKaXt`). This will fail every hour until fixed.

### Historical High-Retry Tasks (from prior days)

| Task | Attempts | Root Cause |
|------|----------|------------|
| #2 (Referrer points) | 8 | Invalid YAML output + GitHub DNS failures + codex billing errors |
| #6 (Cargo deps) | 7 | GitHub DNS failures + codex auth/billing errors |
| #12 (CONTRIBUTING.md) | 7 | Same infrastructure failures + empty body |
| #1 (Global reservers) | 5 | Multiple agent failures |
| #5 (Test bash job) | 5 | Codex auth error, empty task body |

**Root causes cluster into 3 categories:**
1. **GitHub DNS/network failures** (2026-02-16 to 17) — api.github.com unreachable from codex sandbox
2. **Codex auth/billing errors** (2026-02-17) — forced fallback to claude mid-attempt
3. **Invalid YAML/JSON agent responses** (2026-02-16 23:00-00:00) — systematic, not per-task

## 3. Agent Prompt Effectiveness

### prompts/system.md
- **Good**: Clear output schema, specific examples of good vs bad summaries, explicit safety rules
- **Issues**:
  - Duplicate instructions: "NEVER commit to main" appears twice (lines 6 and 13)
  - "NEVER work in main project directory" appears three times
  - Missing guidance on what to do when there's no test suite
  - Doesn't mention `gh issue comment` as the tool for posting issue comments
  - `done` → `in_review` override (by orchestrator) is not documented

### prompts/agent.md
- **Good**: Clean template structure, all context in one place
- **Issues**:
  - Empty section headers render when values are blank (first attempt noise)
  - `GIT_DIFF` label says "current changes" but only shows `--stat` (file names)
  - `AGENT_PROFILE_JSON` injected as raw JSON without usage instructions

### prompts/route.md
- **Good**: Concise, focused on single JSON output
- **Issues**:
  - `profile.tools` silently overridden by config — router's tool recommendations discarded
  - No guidance that `selected_skills: []` is acceptable (triggers false warning)
  - Complexity guide doesn't mention it controls model tier selection
  - Labels contain routing metadata (agent:*, role:*, complexity:*) that router should ignore

### prompts/review.md
- **Good**: Tight scope, clear decision criteria
- **Issues**:
  - Doesn't disclose that `reject` closes the PR (significant side effect)
  - Diff truncation (500 lines) not disclosed to reviewer
  - No preference guidance for `request_changes` vs `reject` ambiguity

## 4. Routing Accuracy

| Task | Routed To | Appropriate? | Notes |
|------|-----------|-------------|-------|
| #13 (yield adapter docs) | codex → timed out | No | Docs task routed to code agent |
| #14 (vault lifecycle docs) | claude/sonnet | Yes | Good match |
| #15 (keeper README) | codex:haiku → claude | No initially | README sent to weakest model |
| #16-17 (truncated tasks) | codex | N/A | Task data corrupted |
| #18 (.editorconfig) | codex | Yes | Simple config file |
| #19 (shellcheck directives) | codex | Yes | Script edits |
| #20 (retrospective) | claude | Yes | Analysis/synthesis task |

**Key issue**: Documentation tasks routed to codex. The route.md description of codex
as "best for coding" is correct but claude has equal code capabilities — docs should
prefer claude.

**Task title/body truncation bug**: Tasks #16 and #17 have titles "Add" with single-word
bodies. Labels contain word fragments from the original text. Parsing bug in task creation.

## 5. Performance Bottlenecks

### Critical: gh_push Redundant Sync Loop

622 out of 818 log lines today (76%) were gh_push operations.

Root cause chain:
1. 5 tasks are `done` but `gh_state=open` (GitHub issues not closed)
2. Task #19 has `gh_state=OPEN` (uppercase) — case-sensitivity bug
3. The dirty-count check counts `done` + `!closed` as dirty
4. When any task is dirty, the loop iterates ALL 20 tasks
5. Even "clean" tasks get logged and call `sync_project_status` (3 API calls each)
6. Per-task field extraction uses 30+ individual `yq` calls per task

**Impact**: ~600 unnecessary API calls per hour, ~60 unnecessary `yq` invocations
per tick, potential GitHub rate limiting.

### Secondary: Per-Task yq Overhead

Lines 242-271 of gh_push.sh read 30 fields with individual `yq` calls per task.
For 20 tasks = 600 `yq` invocations per run. A single `yq -o=json` per task would
reduce this to 20 calls.

### Secondary: test-bash-job Failing Every Hour

The test-bash-job `dir` is `/var/folders/.../tmp.ALBxJiKaXt` (deleted). Fails with
exit=1 every hour, creating noise.

## 6. Improvement Recommendations (Priority Ordered)

### P0 — Fix today

1. **Fix gh_push redundant sync loop** — Skip done+synced tasks entirely. Don't count
   done+open as dirty if `updated_at==gh_synced_at`. Add case-insensitive gh_state
   comparison.
2. **Fix or disable test-bash-job** — Point at valid directory or disable.

### P1 — This week

3. **Batch yq field reads in gh_push** — Single `yq -o=json` extraction per task.
4. **Suppress empty sections in agent.md** — Don't render headers for blank variables.
5. **Clarify route.md complexity guide** — Note that complexity controls model tier.
6. **Add reject=closes-PR warning to review.md** — Disclose side effect to reviewers.

### P2 — Nice to have

7. **Deduplicate system.md instructions** — Merge repeated rules.
8. **Route docs tasks to claude by default** — Add routing hint.
9. **Investigate task title truncation** — Find and fix the bug for tasks #16-17.
10. **Add content-hash dedup for project board sync** — Skip `sync_project_status`
    when status hasn't changed since last sync.

## Metrics

| Metric | Value |
|--------|-------|
| Total tasks | 20 |
| Tasks completed today | 0 (server started at 20:52) |
| Tasks in_review | 2 |
| Tasks in_progress | 1 (this task) |
| Server ticks today | 19 |
| gh_push log lines | 622 (76% of all activity) |
| Jobs triggered today | 2 (test-bash: failed, retrospective: created #20) |
| Highest-attempt task | #2 (8 attempts) |
| Codex token waste | ~4.7M input tokens for 4 trivial config tasks (#16-19) |
