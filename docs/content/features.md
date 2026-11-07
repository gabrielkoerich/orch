+++
title = "Features"
description = "What orch does, from plan to merge"
weight = 2
+++

## Plan to merge

- `orch plan` turns a free-form markdown plan into GitHub issues with blocked-by links, and waits for a yes before it creates anything
- Each task runs in its own git worktree, so the main clone stays untouched
- A second agent reviews the PR once CI has run. With the [GitHub App](@/github-app.md), that is a real review with inline comments
- An approving review moves the task to `approved`. orch waits for CI and merges under your branch protection
- A task with open blockers stays `waiting` until each blocking pull request merges into the default branch. Then it starts from the updated default branch

See [Workflow](@/workflow.md) for the full cycle.

## Agents

- Routes across Claude, Codex, OpenCode, Kimi, MiniMax and Grok
- The router assigns `simple`, `medium` or `complex` and the config maps that to a model
- Labels override the router: `agent:claude`, `complexity:simple`, `no-agent`
- An agent or model that fails, hits a rate limit or runs out of credits goes into cooldown, and the next task goes to one that still has quota
- Learnings persist across retries so a later attempt does not repeat the same mistake

See [Routing](@/routing.md) and [Agents](@/agents.md).

## GitHub

- Two-way sync with GitHub Issues as the source of truth
- `orch task add --depends-on` writes the blocked-by link
- GitHub Projects V2 columns follow task status
- Live session streaming with `orch stream`, and a control session with `orch chat`
- Optional Telegram, Discord and Slack channels

See [GitHub Backend](@/github-sync.md) and [Channels](@/channels.md).

## Operations

- One service can run many repositories
- Isolated worktrees, with copy-on-write clones when the filesystem allows it
- Internal tasks in SQLite for cron jobs and maintenance, without opening a GitHub issue
- A job scheduler with native cron matching
- Per-task artifacts under `~/.orch/state/`, one directory per attempt
- A cluster mode that runs orch on several machines over a tailnet

See [Scheduled Jobs](@/jobs.md), [Copy-on-write worktrees](@/cow-worktrees.md) and [Cluster](@/cluster.md).
