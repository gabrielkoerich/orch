+++
title = "Daily Retrospective — 2026-02-19"
description = "Evening retrospective on orchestrator service activity"
date = 2026-02-19
+++

## Executive Summary

Today was the first full day of multi-project orchestration. The service processed 27 tasks across
two projects (orchestrator and oblivion), completing 8 and leaving 15 in `needs_review`. The
**dominant failure pattern was environment tooling missing from Codex sandboxes** — `bun` not found
(7 tasks), `solana-test-validator` missing (9 tasks), and sandbox permission errors (2 tasks). These
are not agent logic failures; the agents wrote correct code but couldn't validate it.

The morning review job (`job:morning-review`) did not fire today — `last_run` is null and
`active_task_id` is null, suggesting the scheduler didn't pick it up. This needs investigation.

Yesterday's P0 improvement tasks (#21 gh_push fix, #22 batch yq, #23 empty sections) were
**all completed successfully** on the first orchestrator batch at 00:36 UTC. The P1 prompt
improvements (route.md, review.md) were also merged. Only `system.md` duplicate cleanup remains
from yesterday's findings.

## Morning Review Follow-Up

**Status: Morning review job did not run today.**

Yesterday's retrospective (task #20) identified 6 improvement tasks (#21-26). Progress:

| Task | Title | Status | Notes |
|------|-------|--------|-------|
| #21 | Fix gh_push redundant sync loop | Done | Completed, merged |
| #22 | Batch yq field reads in gh_push | Done | Completed, merged |
| #23 | Suppress empty sections in agent.md | Done | Completed, merged |
| #24 | Clarify route.md complexity guide | Done | Merged (prior) |
| #25 | Add reject-closes-PR warning to review.md | Done | Merged (prior) |
| #26 | Fix or disable test-bash-job | Done | Fixed |

**All 6 improvement tasks from yesterday are done.** The gh_push fix reduced API calls significantly.

## 1. Tasks Completed Today

| # | Task | Agent | Model | Duration | Tokens (in/out) |
|---|------|-------|-------|----------|-----------------|
| 21 | Fix gh_push redundant sync loop | codex | gpt-5.2 | 5m 15s | 1.3M/10K |
| 22 | Batch yq field reads in gh_push | codex | gpt-5.2 | 6m 34s | 1.5M/10K |
| 23 | Suppress empty sections in agent.md | codex | gpt-5.2 | 5m 18s | 1.9M/10K |
| 30 | keeper: add DCA execution loop | codex | gpt-5 | 7m 49s | 2.8M/18K |
| 40 | app: historical performance charts | codex | gpt-5 | 6m 47s | 3.0M/20K |
| 43 | Decide on skills registry | manual | — | — | — |
| 44 | Archive done items in GitHub Projects | codex | gpt-5-codex | 6m 35s | N/A |
| 47 | Create GitHub App for bot identity | codex | — | — | N/A |

**What went well:**
- Yesterday's orchestrator improvement tasks (#21-23) all completed on first attempt
- Codex performed well on shell script tasks for the orchestrator project
- Task #30 (DCA execution loop) is a significant feature addition completed in one run
- Task #44 (archive done items) added useful project board automation

**Total tokens consumed by completed tasks: ~10.5M input, ~68K output**

## 2. Tasks Failed / Needed Retries

### Failure Pattern Summary

| Error Category | Tasks Affected | Root Cause |
|---------------|---------------|------------|
| `agent_exit_1` (generic) | 13 tasks | Various — usually cascading from env issues |
| Missing `solana-test-validator` | 9 tasks (#28,29,31,33,34,36,37,38,39) | Codex sandbox lacks Solana toolchain |
| Missing `bun` | 7 tasks (#29,31,33,34,35,38,39) | Codex sandbox PATH doesn't include bun |
| PR review rejected | 2 tasks (#33,39) | Claude review agent requested changes |
| Sandbox permission denied | 2 tasks (#36,37) | Bun tempdir AccessDenied, git config locked |
| npm cache permission | 1 task (#31) | Root-owned npm cache |
| Auth/billing error | 1 task (#45) | Codex auth failed, switched to claude |
| Stuck recovery | 1 task (#36) | Stuck in_progress for 7h, auto-recovered |

### Critical Finding: Codex Sandbox Environment Gaps

The **#1 failure cause today is missing tooling in Codex sandboxes** for the oblivion project:
- `bun`: Not in PATH for Codex workers. Agents can't run `bun run tsc`, `bun run lint`, or `bun install`
- `solana-test-validator`: Not installed. `anchor test --skip-build` fails universally
- `bun tempdir AccessDenied`: Sandbox filesystem restrictions prevent bun from writing temp files

This caused a cascade: agents completed their code changes but failed validation, leading to
`needs_review` status. The agents correctly reported the environment issue, but the orchestrator
re-ran them anyway, wasting tokens on the same failure.

### Second Failure Wave (02:21 UTC)

At 02:21 UTC, 14 tasks were bulk-retried (`retried from needs_review`). Within seconds, all
immediately failed with `exit 1` — suggesting the environment issue was not resolved between
attempts. This is a **retry loop without remediation**.

### Worktree Creation Failures (20:24 UTC)

The current server session (v0.36.7) had multiple worktree creation failures:
- Tasks #76, #77, #78, #79, #80 all failed worktree creation
- These are non-orchestrator project tasks whose bare repos may not be set up

## 3. Agent Prompt Effectiveness

### Changes Since Yesterday's Retrospective

| Issue | Fixed? | Commit |
|-------|--------|--------|
| agent.md empty sections on first attempt | Yes | f5adf61 |
| route.md complexity→model tier mapping | Yes | 37d4196 |
| review.md reject-closes-PR disclosure | Yes | 99824df |
| system.md duplicate "NEVER" statements | **No** | — |
| agent.md GIT_DIFF only shows --stat | **No** | — |

### Remaining Issues

1. **system.md duplicates**: Lines 6 and 13 both say "NEVER commit to main". Lines 7 and 15 both
   prohibit main directory work. Should be consolidated.

2. **agent.md GIT_DIFF limitation**: `build_git_diff()` in lib.sh uses `git diff --stat HEAD`
   which only shows file names, not actual changes. On retries, agents need to see the actual diff
   to understand what was already attempted.

3. **System prompt doesn't guide agents on missing tooling**: When `bun` or `solana-test-validator`
   is missing, agents should report `blocked` with a clear env dependency reason, not attempt
   workarounds that waste tokens. The prompt should tell agents: "If required build/test tools are
   missing, report blocked immediately."

## 4. Routing Accuracy

All 21 tasks active today were routed to **codex** (except #45 which fell back to claude after
codex auth error). This routing was:

| Assessment | Count | Notes |
|-----------|-------|-------|
| Correct | 8 | #21-23 (shell scripts), #42,44 (orchestrator features) |
| Acceptable | 5 | #30,40 (Solana/TS code) — codex can handle but failed on env |
| Questionable | 8 | #28-29,31-39 — complex Solana/TS tasks that failed repeatedly |

**Key routing issues:**
- **No differentiation between orchestrator vs oblivion tasks**: The router sends everything to
  codex regardless of project-specific requirements (Solana toolchain)
- **Complexity was appropriate**: complex tasks got complex routing, medium got medium
- **Missing project-aware routing**: oblivion tasks need `bun` and `solana-test-validator`. The
  router doesn't consider project environment requirements

## 5. Performance Bottlenecks

### GitHub API Rate Limiting

Rate limit events earlier today, including hard `HTTP 403` responses and a 900s (15min) backoff at
02:36 UTC. The backoff mechanism worked correctly. After the server restart at 20:35 UTC (v0.36.7),
the log shows only **184 entries** — a dramatic drop from yesterday's 622+ gh_push lines per session,
confirming that yesterday's fixes (#21, #22) are effective. However, the earlier session (before
restart) likely had higher volume before the fixes were loaded.

### Token Waste on Retries

Failed tasks consumed significant tokens before hitting the same environment error:
- 9 tasks × ~2M input tokens average = ~18M input tokens wasted on tasks that could never succeed
- The system should detect "same error, same cause" and stop retrying

### Codex Shell Snapshot Errors

Multiple `codex_core::shell_snapshot: Shell snapshot validation failed` errors in stderr. These
don't block execution but indicate Codex internal instability.

### Stuck Task Recovery

Task #36 was stuck `in_progress` for 7 hours (25,953 seconds) before auto-recovery kicked in.
The stuck detection threshold should be lower for faster recovery.

## 6. Improvement Recommendations (Priority Ordered)

### P0 — Fix Tomorrow

1. **Investigate morning review job not firing**: `last_run: null` suggests the scheduler never
   matched the schedule or the orchestrator wasn't running at 08:00. Check cron matching and
   server uptime.

2. **Add Codex sandbox environment validation**: Before running a task, check if required tools
   (`bun`, `solana-test-validator`, etc.) are available in the agent's PATH. Block the task
   immediately if missing, with a clear message like "Task requires bun but it's not in PATH.
   Configure CODEX_SANDBOX or install the tool."

3. **Prevent retry loops on environment failures**: If a task fails with "command not found" or
   "No such file or directory" for a required tool, don't retry automatically. These errors
   won't resolve themselves.

### P1 — This Week

4. **Deduplicate system.md instructions**: Consolidate the repeated "NEVER commit to main" and
   "NEVER work in main directory" rules. One clear statement each.

5. **Enhance GIT_DIFF with actual diff content**: Change `build_git_diff()` from `--stat` to
   include actual diff lines (truncated to ~200 lines) so retry agents see what was already
   changed.

6. **Add project-aware routing hints**: Allow `.orchestrator.yml` to specify required tools per
   project (e.g., `required_tools: [bun, anchor]`). The router can then ensure the agent
   environment is compatible.

7. **Fix worktree creation for managed projects**: Tasks #76-80 all failed worktree creation.
   The worktree path generation may be incorrect for non-primary projects.

### P2 — Nice to Have

8. **Lower stuck task detection threshold**: 7 hours is too long for auto-recovery. Consider
   30-60 minutes for the stuck detection threshold.

9. **Add per-project token budgets**: Track token spending per project to identify expensive
   patterns early.

10. **Review agent review quality**: The review agent rejected tasks #33 and #39 with
    `request_changes`. Verify these reviews were substantive vs overly conservative.

## Metrics

| Metric | Value |
|--------|-------|
| Total tasks active today | 27 |
| Tasks completed | 8 (30%) |
| Tasks needs_review | 15 (56%) |
| Tasks blocked | 0 |
| Success rate (first attempt) | 6/27 = 22% |
| GitHub rate limit events | Multiple (pre-restart session) |
| GitHub sync log entries | 184 (post-restart); higher pre-restart |
| Total input tokens (completed) | ~10.5M |
| Token waste on failed retries | ~18M (estimated) |
| Morning review job | Did not fire |
| Codex auth failures | 1 (task #45) |
| PR reviews (by review agent) | 2 (both request_changes) |
| Server version | v0.36.7 |

## What Tomorrow's Morning Check-In Should Tackle

1. **Why didn't the morning-review job fire?** Check `jobs_tick.sh` matching, server uptime at
   08:00, and whether the job is properly enabled.

2. **Unblock the 15 needs_review tasks**: Most are blocked on environment issues. Either:
   - Configure Codex sandbox to include `bun` and Solana tools, OR
   - Route oblivion tasks to `claude` which has full environment access

3. **Confirm gh_push improvements sustained**: Post-restart session showed only 184 log entries
   (vs 622+ yesterday). Verify this holds across a full 24h cycle.

4. **Fix worktree creation for managed projects**: Multiple tasks (76-80) blocked on this.

5. **Clean up system.md duplicates**: Quick win from yesterday's retrospective.
