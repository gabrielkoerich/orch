+++
title = "Agents"
description = "Agentic mode, safety rules, and context enrichment"
weight = 5
+++

Once routed, agents run in full agentic mode with tool access:

- **Claude**: `-p` flag (non-interactive agentic mode), `--permission-mode acceptEdits`, `--output-format json`, system prompt via `--append-system-prompt`
- **Codex**: `-q` flag (quiet non-interactive mode), `--json`, system+agent prompt combined
- **OpenCode**: `opencode run --format json` with combined prompt

Agents execute inside `$PROJECT_DIR` (the directory you ran `orchestrator` from), so they can read project files, edit code, and run commands.

## Agent Output

The agent writes a JSON file to `.orchestrator/output-{task_id}.json`:

```json
{
  "status": "done|in_progress|blocked|needs_review",
  "summary": "what was done",
  "reason": "why blocked/needs_review",
  "accomplished": ["list of completed items"],
  "remaining": ["list of remaining items"],
  "blockers": ["list of blockers"],
  "files_changed": ["list of modified files"],
  "needs_help": false,
  "delegations": [{"title": "...", "body": "...", "labels": [], "suggested_agent": "codex"}]
}
```

## PATH Configuration

When orchestrator runs as a service (e.g. via `brew services`), agents start with a minimal PATH that may not include tools like `bun`, `anchor`, `cargo`, or `solana`. There are two ways to fix this:

**Option 1: Create `~/.path` (recommended)**

Create a `~/.path` file that exports your development tool paths:

```bash
# ~/.path
export PATH="/opt/homebrew/bin:$PATH"
export PATH="$HOME/.bun/bin:$PATH"
export PATH="$HOME/.cargo/bin:$PATH"
export PATH="$HOME/.local/share/solana/install/active_release/bin:$PATH"
export PATH="$HOME/.local/bin:$PATH"
```

Orchestrator sources this file before launching agents, so any tool on your PATH will be available to agents.

**Option 2: Default fallback**

If `~/.path` doesn't exist, orchestrator automatically adds common paths:

- `$HOME/.bun/bin`
- `$HOME/.cargo/bin`
- `$HOME/.local/share/solana/install/active_release/bin`
- `$HOME/.local/bin`
- `/opt/homebrew/bin`
- `/usr/local/bin`

## Safety Rules

Agents are constrained by rules in the system prompt:

- **No `rm`**: `--disallowedTools` blocks `rm` — agents must use `trash` (macOS) or `trash-put` (Linux)
- **No commits to main**: agents must always work in feature branches
- **Required skills**: skills listed in `workflow.required_skills` are marked `[REQUIRED]` in the agent prompt
- **GitHub issue linking**: if a task has a linked issue, the agent receives the issue reference for branch naming and PR linking
- **Cost-conscious sub-agents**: agents are instructed to use cheap models for routine sub-agent work

## Worktrees

The orchestrator creates worktrees before launching agents. Agents do NOT create worktrees themselves.

**Path:** `~/.orchestrator/worktrees/<project>/gh-task-<issue>-<slug>` (or `task-<id>-<slug>` without an issue)

**Steps:**
1. `gh issue develop <issue> --base main --name <branch>` — registers branch with GitHub
2. `git branch <branch> main` — creates branch from main
3. `git worktree add <path> <branch>` — creates worktree
4. Agent runs inside the worktree directory

After agent finishes, orchestrator pushes the branch if there are unpushed commits.

## Context Enrichment

Every agent receives a rich context built from multiple sources:

| Context | Source | When |
|---------|--------|------|
| System prompt | `prompts/system.md` | Always |
| Task details | `tasks.yml` | Always |
| Agent profile | Router-generated role/skills/tools/constraints | Always |
| Error history | `tasks.yml` `.history[]` | On retries |
| Last error | `tasks.yml` `.last_error` | On retries |
| GitHub issue comments | GitHub API | If issue linked |
| Prior run context | `contexts/task-{id}.md` | On retries |
| Tool call summaries | `.orchestrator/tools-{id}.json` | On retries |
| Repo tree | `git ls-files` | Always |
| Project instructions | `CLAUDE.md` + `AGENTS.md` + `README.md` | If files exist |
| Skills docs | `skills/{id}/SKILL.md` | If skills selected |
| Parent/sibling context | Parent task summary + accomplished | If child task |
| Git diff | Uncommitted changes | On retries |

## Error Handling

When a task fails:
1. Error is recorded in `last_error` and `history`
2. Task is blocked or set to `needs_review`
3. A structured comment is posted on the linked GitHub issue
4. A red `blocked` label is applied to the issue
5. `@owner` is tagged for attention

**Retry loop detection**: if the same error repeats 3 times (4+ attempts), the task is permanently blocked instead of retrying.

**Max attempts**: default 10 per task (configurable via `workflow.max_attempts`).

```bash
orch task retry <id>       # reset any task to new
orch task unblock <id>     # reset a blocked task to new
orch task unblock all      # reset all blocked tasks
```
