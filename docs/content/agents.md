+++
title = "Agents"
description = "Agentic mode, safety rules, and context enrichment"
weight = 5
+++

Once routed, agents run in full agentic mode with tool access:

- **Claude**: `-p` flag (non-interactive agentic mode), `--permission-mode acceptEdits`, `--output-format json`, system prompt via `--append-system-prompt`
- **Codex**: `-q` flag (quiet non-interactive mode), `--json`, system+agent prompt combined
- **OpenCode**: `opencode run --format json` with combined prompt

Agents execute inside an isolated git worktree created for the task (not your main repo), so they can read project files, edit code, and run commands.

## tmux Runner

Agent sessions run inside tmux sessions. The orchestrator spawns the agent CLI as the tmux session shell, so tmux provides the PTY. There is no external PTY process or `portable_pty` crate. Prompt files are written to per-attempt directories for auditability.

## Agent Output

The agent writes a JSON file to `~/.orch/state/{repo}/tasks/{id}/attempts/{n}/output.json` and the orchestrator also stores a sidecar at `~/.orch/state/{repo}/tasks/{id}/sidecar.json`:

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

The orchestrator sources this file before launching agents, so any tool on your PATH will be available to agents when the runner is configured to inherit the shell environment.

**Option 2: Default fallback**

If `~/.path` doesn't exist, orchestrator automatically adds common paths:

- `$HOME/.bun/bin`
- `$HOME/.cargo/bin`
- `$HOME/.local/share/solana/install/active_release/bin`
- `$HOME/.local/bin`
- `/opt/homebrew/bin`
- `/usr/local/bin`

## Safety Rules

Agents are constrained by rules in the system prompt and by runner-enforced tool allowlists/denylists:

- **No `rm`**: `--disallowedTools` blocks `rm` — agents must use `trash` (macOS) or `trash-put` (Linux)
- **No commits to main**: agents must always work in feature branches
- **Required skills**: skills listed in `workflow.required_skills` are marked `[REQUIRED]` in the agent prompt
- **GitHub issue linking**: if a task has a linked issue, the agent receives the issue reference for branch naming and PR linking
- **Cost-conscious sub-agents**: agents are instructed to use cheap models for routine sub-agent work

## Worktrees

The orchestrator creates worktrees before launching agents. Agents do NOT create worktrees themselves.

**Path:** `~/.orch/worktrees/<project>/<branch>/` — worktrees are always placed under `~/.orch/worktrees` with a project/branch layout. Worktrees live in the orchestrator home directory (`ORCH_HOME`, default `~/.orch`) and are safe to remove by the cleanup process once branches are merged.

**Steps:**
1. `gh issue develop <issue> --base main --name <branch>` — registers branch with GitHub
2. `git branch <branch> main` — creates branch from main
3. `git worktree add <path> <branch>` — creates worktree
4. Agent runs inside the worktree directory

After an agent finishes, the orchestrator pushes the branch if there are unpushed commits. Note: the runner injects `GH_TOKEN` into the spawned runner environment so agents do not need to authenticate with `gh` themselves and agents are not expected to call GitHub directly. Attribution footers (for example: `Created by claude[bot] via Orch`) are added to issue and PR comments so it's clear which agent produced the content.

## Context Enrichment

Every agent receives a rich context built from multiple sources:

| Context | Source | When |
|---------|--------|------|
| System prompt | `prompts/agent_system.md` | Always |
| Task details | SQLite store (`~/.orch/orch.db`) | Always |
| Agent profile | Router-generated role/skills/tools/constraints | Always |
| Error history | Task store + sidecar | On retries |
| Last error | Task store `last_error` field | On retries |
| GitHub issue comments | GitHub API | If issue linked |
| Prior run context | `contexts/task-{id}.md` | On retries |
| Tool call summaries | `.orch/tools-{id}.json` | On retries |
| Repo tree | `git ls-files` | Always |
| Project instructions | `CLAUDE.md` + `AGENTS.md` + `README.md` | If files exist |
| Skills docs | `skills/{id}/SKILL.md` | If skills selected |
| Parent/sibling context | Parent task summary + accomplished | If child task |
| Git diff | Uncommitted changes | On retries |

## Error Handling

When a task fails:
1. Error is recorded in `last_error` and `history`
2. After repeated failures the task is moved to `needs_review` (human attention) instead of being permanently blocked; the orchestrator also removes any forced `agent:*` label so an owner can reassign or inspect the task. The change in behavior ensures tasks don't remain stuck with a forced agent after owner intervention is needed.
3. A structured comment is posted on the linked GitHub issue with an attribution footer (for example: `Created by claude[bot] via Orch`) so it's clear which agent produced the comment
4. A `needs_review` label is applied to the issue and the configured review owner is notified

**Retry loop detection**: if the same error repeats 3 times (4+ attempts), the task is moved to `needs_review` to avoid wasting cycles.

**Max attempts**: default 10 per task (configurable via `workflow.max_attempts`).

```bash
orch task retry <id>       # reset any task to new
orch task unblock <id>     # reset a needs_review/stalled task to new
orch task unblock all      # reset all needs_review/blocked tasks
```

## Codex Sandbox Notes

Codex supports multiple sandbox configurations. When a workspace-write sandbox is required the runner enables networking and inherits the shell environment to allow tools like `bun` and language runtimes to work. Recommended settings when enabling workspace-write sandboxing:

- `sandbox_workspace_write.network_access=true`
- `shell_environment_policy.inherit=all`

These are configured by the orchestrator when spawning the Codex runner — do not rely on agents to set these themselves.
