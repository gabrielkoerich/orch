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
5. **PR** — engine creates PR and links it to the issue
6. **Review** — different agent reviews the PR (approve / request changes)
7. **Fix + Reply** — fix review findings, commit fixes (engine pushes)
8. **Merge** — engine merges PR after review approval and CI passes
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

The Rust engine ticks every `engine.tick_interval` seconds (default 10s). Each tick runs sequential phases:

1. **Poll sessions** — checks tmux for finished agent sessions, collects results
2. **Recover stuck tasks** — detects `in_progress` tasks with no tmux session and age >10 min, resets to `new`
3. **Route** — assigns agent, model, and profile to `new` tasks via LLM router
4. **Dispatch** — launches routed tasks in tmux sessions inside worktrees; breaks the loop early if work is already merged
5. **Unblock** — if all children of a blocked parent are `done`, resets parent to `new`
6. **Jobs** — runs due scheduled jobs (cron, per-project)

A separate **sync tick** (~45s) handles less-frequent operations:

1. **Cleanup** — removes worktrees for done tasks (runs in background), deletes orphaned remote branches, pulls main once per cycle
2. **Merged PR detection** — marks tasks done when their PRs are merged
3. **Mention scanning** — detects `@orchestrator` mentions in issue comments
4. **PR review processing** — re-routes tasks when reviews request changes (not child task creation), resets `ci_merge_failures` counter on re-route
5. **Review agent** — spawns review agent for `needs_review` tasks
6. **Stale review detection** — resets `in_review` tasks with no active tmux session (>5 min)
7. **Owner commands** — scans for `/slash` commands in issue comments
8. **Skills sync** — clones/pulls skill repositories

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
- Engine pushes the branch (`git push -u origin <branch>`) if there are unpushed commits
- Engine creates the PR and links it to the issue
- Engine checks CI status and triggers the review agent
- Agents should NOT push, create PRs, or call GitHub write APIs — the engine handles all git operations beyond committing

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
