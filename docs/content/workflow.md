+++
title = "Workflow"
description = "How the orchestrator runs tasks end-to-end"
weight = 3
+++

How the orchestrator runs tasks end-to-end.

## Full Development Cycle

```
Issue → Branch + Worktree → Agent works → Push → PR → Review Agent → Merge → Cleanup
```

1. **Issue** — created via `orch task add` or a scheduled job (`orch job tick`)
2. **Branch + Worktree** — engine creates via `gh issue develop` + `git worktree add`
3. **Agent works** — runs inside worktree, edits files, commits changes
4. **Push** — engine pushes the branch after agent finishes
5. **PR** — agent creates with `gh pr create --base main` and `Closes #N`
6. **Review** — opposite agent reviews the PR via `gh pr review` (approve / request changes / reject)
7. **Fix + Reply** — fix review findings, reply to each comment, resolve threads
8. **Merge** — squash merge with conventional commit prefix (`feat:` / `fix:`)
9. **Release** — CI auto-tags, generates changelog, creates GitHub release, updates Homebrew
10. **Cleanup** — engine detects merged PR, removes worktree + local branch

## Mention-Driven Tasks

When someone comments `@orchestrator ...` on a GitHub issue/PR, the GitHub mentions listener can create a task like:

```
Respond to @orchestrator mention in #<N>
```

Expected outcome:

- Read the mention body + any referenced issues/PRs
- Reply back on the *target* issue with a concise status update and clear next steps
- Avoid including `@orchestrator` in automated replies or agent summaries (use `orchestrator` without the `@`) to prevent mention-task feedback loops
- If no code/docs changes are required, the task can be completed without opening a PR

## Task Lifecycle

```
new → routed → in_progress → needs_review → in_review → done (merged)
                            → done (no PR)
                            → blocked
```

- **new**: task created (via `orch task add` or a scheduled job)
- **routed**: LLM router assigned agent, model, profile, skills
- **in_progress**: agent is running
- **needs_review**: PR exists and is queued for review, or max attempts exceeded
- **in_review**: review agent is actively running on the PR
- **done**: PR merged (or agent completed with no code changes)
- **blocked**: waiting on child tasks to complete

## Engine Tick

The Rust engine ticks every `engine.tick_interval` seconds (default 10s):

1. **Sync** — imports new GitHub issues, syncs labels, detects PR events (webhook or polling)
2. **Route** — assigns agent, model, and profile to `new` tasks via LLM router
3. **Dispatch** — launches routed tasks in tmux sessions inside worktrees
4. **Unblock** — if all children of a blocked parent are `done`, resets parent to `new`
5. **Review** — when a task transitions `needs_review → in_review`, launches review agent
6. **Jobs** — runs due scheduled jobs (cron, per-project)
7. **Recovery** — detects stuck `in_progress` tasks (no tmux session, >10 min) and resets to `new`

### Channels & Live Sessions

The engine supports bidirectional channels (Telegram, Discord, Slack, GitHub, tmux). Incoming messages are routed to tmux sessions or turned into internal tasks. The capture service polls tmux panes and broadcasts diffs to all connected channel threads with per-channel rate limiting and message-splitting to satisfy platform limits.

## Worktrees

The engine creates worktrees before launching agents. Agents do NOT create worktrees themselves.

**Worktree path:** `~/.orch/worktrees/<project>/<branch>/`

**Steps:**
1. `gh issue develop <issue> --base main --name <branch>` — registers branch with GitHub
2. `git branch <branch> main` — creates branch from main
3. `git worktree add ~/.orch/worktrees/<project>/<branch> <branch>` — creates worktree
4. Agent runs inside the worktree directory (`PROJECT_DIR` is set to worktree)

**After agent finishes:**
- Engine pushes the branch (`git push -u origin <branch>`) if there are unpushed commits. The runner injects `GH_TOKEN` into the spawned runner environment so agents do not need to authenticate with `gh` themselves and agents should avoid calling GitHub directly.
- Agent should NOT run `git push` itself

## Agent Invocation

The Rust runner spawns the agent inside a tmux session:

```bash
claude -p \
  --model <model> \
  --permission-mode acceptEdits \
  --allowedTools "Write" \
  --disallowedTools "Bash(rm *)" \
  --output-format json \
  --append-system-prompt <system_prompt> \
  <agent_message>
```

## Agent Output

```json
{
  "status": "done|in_progress|blocked|needs_review",
  "summary": "what was done",
  "reason": "why blocked/needs_review (empty if done)",
  "accomplished": ["list of completed items"],
  "remaining": ["list of remaining items"],
  "blockers": ["list of blockers"],
  "files_changed": ["list of modified files"],
  "needs_help": false,
  "delegations": [{"title": "...", "body": "...", "labels": [], "suggested_agent": "codex"}]
}
```

## Review Agent

After agent completion, if a PR is open and `enable_review_agent` is true:

1. Status transitions to `needs_review`, then `in_review` (this transition is the atomic guard)
2. Opposite agent selected (codex wrote → claude reviews)
3. PR diff fetched via `gh pr diff`
4. Review agent evaluates and posts a comment with `## Automated Review — Approve/Changes Requested` header
5. CI workflow reads the review comment to determine approval

See the [Review Agent](@/review-agent.md) page for full details.

## Stuck Task Recovery

The engine detects stuck tasks:

1. **No tmux session found** — task `in_progress` with no live session and age >10 min → reset to `new`
2. **Max attempts exceeded** — task goes to `needs_review` (not `blocked`) and the forced `agent:*` label is removed so an owner can reassign or inspect the task

Note: `stuck_timeout` is separate from the task execution timeout. Task execution is limited by `workflow.timeout_seconds` (or `workflow.timeout_by_complexity`), which controls how long an agent run is allowed to execute before being killed (exit 124 / TIMEOUT).

## Max Attempts

Default: 10 attempts per task (configurable via `config.yml`). After max attempts, task goes to `needs_review` (not `blocked`) and the forced `agent:*` label is removed so an owner can reassign or inspect the task. Retry loop detection: if the same error repeats 3 times, task also goes to `needs_review` instead of retrying.
