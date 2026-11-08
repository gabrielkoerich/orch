+++
title = "Weekly review 2026-11-08"
date = 2026-11-08
+++

## What shipped

78 commits landed on `main`. About 45 of them are features and fixes. The rest are version bumps and refactors.

Push and merge:

- A GitHub-side push failure on a new branch no longer blocks as a rebase conflict (#4020).
- A push that fails during a GitHub outage retries the push with backoff (#4031).
- A transient push failure on a task that already has a PR retries the push (#4051, #4054).
- A push that needs approval counts as a denied push, so orch pushes for the agent (#3980).
- Cleanup pushes a rescue branch before it would delete unpushed commits (#4000). Startup and doctor skip that rescue for merged PRs (#4011).

Review and CI:

- A conflicting PR is rebased, or sent back to its author, before review starts (#3974). Dispatch aborts the leftover conflicted rebase before the agent starts (#3984).
- Reviews have their own dispatch pool (#3972).
- One CI failure per head SHA (#4047).
- A CI bootstrap task for a repo with no Actions workflow, and a comment when those default-branch checks fail (#4041, #4055, #4056).
- A live `in_progress` session keeps its own run timeout, so stuck recovery does not kill it at 30 minutes (#3981).

Routing and cooldowns:

- The router pool cools an agent on credit exhaustion and counts one exhaustion once (#3970).
- Claude `rate_limit_event` cools the agent before the quota runs out. Utilization comes from `unifiedWindows` (#4021, #4028, #4030).
- A timeout reroutes even when every other agent is cooled (#3998). Timeout failover keeps the stored agent, so a label does not send the task back to the one that timed out (#4048). `timeout_retries` resets after a successful run (#4010).
- A task whose tier has no available model takes one from the tier above or below (#4001).

Product:

- Priority labels order dispatch, and a task with an open PR dispatches before `priority:high` (#3960, #3961).
- `orch plan` turns a free-form plan into a roadmap (#4039). `orch init` writes a sparse config and starts the service (#4040).
- Cost is what each agent reports. The price table and model names left the code (#4002).
- Screenshots on the PR when a task changes what a user sees (#4050).
- Agent output fixtures from real runs (#4023).
- MIT license file.

About 45 issues closed since the last review. The newest are #4051 to #4056: push retry, CI bootstrap comments, and squash-merged bootstrap cleanup.

## What failed

398 runs in the last 7 days. 322 succeeded (179 agent, 143 review). Reported cost $137.63.

| Agent / model | Outcome | Runs |
|---|---|---|
| claude / opus | success | 168 |
| claude / sonnet | success | 138 |
| claude / sonnet | aborted | 18 |
| grok / grok-4.6 | success | 14 |
| grok / grok-4.6 | credit_exhausted | 12 |
| claude / opus | aborted | 9 |
| claude / sonnet | failed | 7 |
| claude / sonnet | push_failed | 7 |
| claude / sonnet | timeout | 4 |
| claude / sonnet | blocked | 3 |
| grok / grok-4.6 | timeout | 3 |
| grok / grok-4.7 | credit_exhausted | 3 |
| grok / grok-4.7 | success | 2 |

Six empty outcomes are live runs from this review's window.

Why runs failed:

- Grok Build 402s: 15 `credit_exhausted` (9 agent, 6 review). Generic cooldown.
- Aborts: 27, mostly graceful shutdown and stuck-recovery. The 15 live-session kills all landed on 1-3 Nov, before #3981.
- Push failures: 7. Three are #4022 hitting GitHub `fatal error in commit_refs`. Three are a downstream rebase conflict. One remote branch was already gone.
- Timeouts: five Claude runs on 3 Nov, three Grok runs at the 45 minute limit on 7 Nov.
- Silence detection: eight failed runs.

## Operational health

`task_activity` in the window: 3111 status changes, 461 dispatches, 366 branch deletes, 345 pushes, 198 routes, 176 PR creations, 152 review starts, 149 review decisions, 131 review agent fallbacks, 79 errors, 70 failed review attempts, 36 reroutes, 9 timeouts.

All 131 review fallbacks are `all available agents excluded (author + previous reviewers)`. Last week's fallbacks were review timeouts. This week's are the single-executor case: the only available agent wrote the code, so that same agent reviews it. Absolute fallbacks dropped (297 to 131) with fewer reviews (471 starts to 152). Failed review attempts dropped (180 to 70). Many of the 70 are GitHub REST API rate limits.

`orch.error.log` is empty. Its mtime is today, from the latest start.

The last 1000 log lines show `degraded mode: using sequential dispatch healthy_agents=1 threshold=2`. The router LLM pool is grok-4.5. When it sat in `model_error` cooldown, routing fell through to Claude and still picked Grok as executor. Sync ticks ran in about 2-12s.

`orch cooldown list` at review time: `grok:grok-4.5` a few minutes of `model_error`. Router weights `{grok: 1.0}`.

157 tasks reached `done`. Current behavior matches `SKILL.md` and the settled repo policy: generic cooldowns, per-task billing block at merge time, retry next tick when every agent is cooled, `done` is terminal. The push-failure and stuck-recovery bugs that fired this week are closed.

## Stuck tasks

| ID | Status | Why |
|---|---|---|
| 4022 | blocked | Claude still gets the system prompt file path as `--append-system-prompt`. The agent committed `--append-system-prompt-file` locally. GitHub rejected the force-push three times (`fatal error in commit_refs`). The commit is in the worktree, 1 ahead and 28 behind main. No PR. |
| 4037 | routed | Landing page, draft PR #4043. `no-agent`. Operator hold after a merge conflict. |
| 4044 | new | Design, `no-agent`. |
| 3936 | new | Design, `no-agent`. |
| internal scheduled jobs | blocked | Unattended runs could not unlock credentials. One finished job could not push because the remote branch was gone. |

#4058 (remove skills) is in progress.

## Routing accuracy

Claude did most of the finished work: 306 successes. Grok-4.6 had 14 successes and 12 credit exhaustions. Grok-4.7 had 2 successes. Kimi, minimax and codex recorded no runs this week.

With Grok as the only configured executor, dispatch is sequential and every review falls back to the author, a capacity issue. Credit exhaustion and model errors took the generic cooldown path. The router LLM fallback to Claude worked when grok-4.5 was cooled.

Claude, and the agents that reuse `ClaudeRunner`, still receive the system prompt file's path. Grok cats the file (`--system-prompt-override "$(cat …)"`). Codex and opencode cat sys+msg. #4022 is the fix, blocked on the push. Until it merges, Claude runs without the output-format and worktree rules unless the agent opens the file itself.

## Priorities for next week

1. Unblock #4022 so the system prompt file reaches Claude. The commit is in the worktree. The GitHub `commit_refs` failure that blocked it was a transient GitHub error, and the retry path shipped in #4031 and #4051.
2. A second healthy executor, so reviews can use a different agent.
3. Operator decisions on #4037 (draft PR #4043), #3936, #3720, #3730, #4044.
