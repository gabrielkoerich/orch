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

1. **Issue** — created via `orchestrator task add` or `jobs_tick`
2. **Branch + Worktree** — orchestrator creates via `gh issue develop` + `git worktree add`
3. **Agent works** — runs inside worktree, edits files, commits changes
4. **Push** — orchestrator pushes the branch after agent finishes
5. **PR** — agent creates with `gh pr create --base main` and `Closes #N`
6. **Review** — opposite agent reviews the PR via `gh pr review` (approve / request changes / reject)
7. **Fix + Reply** — fix review findings, reply to each comment, resolve threads
8. **Merge** — squash merge with conventional commit prefix (`feat:` / `fix:`)
9. **Release** — CI auto-tags, generates changelog, creates GitHub release, updates Homebrew
10. **Cleanup** — (TODO) orchestrator detects merged PR, removes worktree + local branch

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
new → routed → in_progress → done → in_review → (merged externally)
                            → blocked
                            → needs_review
```

- **new**: task created (via `add` or `jobs_tick`)
- **routed**: LLM router assigned agent, model, profile, skills
- **in_progress**: agent is running
- **done**: agent completed successfully (no open PR)
- **in_review**: agent completed and a PR is open (review agent fires if enabled)
- **blocked**: agent hit a blocker or crashed
- **needs_review**: agent needs human help, or review agent requested changes

## Poll Loop

`serve.sh` ticks every 10s:

1. `poll.sh` — finds `new`/`routed` tasks, runs them in parallel (up to `POLL_JOBS=4`)
2. `poll.sh` — detects stuck `in_progress` tasks (no lock held, stale >30min), resets to `new`
3. `poll.sh` — checks blocked parents: if all children are `done`, unblocks parent
4. `jobs_tick.sh` — checks cron schedules, creates tasks for due jobs
5. `review_prs.sh` — auto-reviews open PRs (if review agent enabled)
6. `cleanup_worktrees.sh` — removes worktrees for merged PRs

## Worktrees

The orchestrator creates worktrees before launching agents. Agents do NOT create worktrees themselves.

**Worktree path:** `~/.orchestrator/worktrees/<project>/gh-task-<issue>-<slug>`

**Steps:**
1. `gh issue develop <issue> --base main --name <branch>` — registers branch with GitHub
2. `git branch <branch> main` — creates branch from main
3. `git worktree add ~/.orchestrator/worktrees/<project>/<branch> <branch>` — creates worktree
4. Agent runs inside the worktree directory (`PROJECT_DIR` is set to worktree)

**After agent finishes:**
- Orchestrator pushes the branch (`git push -u origin <branch>`) if there are unpushed commits
- Agent should NOT run `git push` itself

## Agent Invocation

`run_task.sh` runs the agent:

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

1. Status overridden to `in_review`
2. Opposite agent selected (codex wrote → claude reviews)
3. PR diff fetched via `gh pr diff`
4. Review agent evaluates and returns `approve`, `request_changes`, or `reject`
5. Real GitHub PR review posted via `gh pr review`

See the [Review Agent](@/review-agent.md) page for full details.

## Stuck Task Recovery

`poll.sh` detects stuck tasks:

1. **No agent assigned** — task stuck `in_progress` without an agent → set to `needs_review`
2. **Dead agent** — task has agent but no lock file and `updated_at` older than `stuck_timeout` (default 30min) → reset to `new`

Note: `stuck_timeout` is separate from the task execution timeout. Task execution is limited by `workflow.timeout_seconds` (or `workflow.timeout_by_complexity`), which controls how long an agent run is allowed to execute before being killed (exit 124 / TIMEOUT).

## Max Attempts

Default: 10 attempts per task (configurable via `config.yml`). After max attempts, task goes to `blocked` with error. Retry loop detection: if the same error repeats 3 times, task goes to `needs_review` instead of retrying.
