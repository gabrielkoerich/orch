+++
title = "Getting Started"
description = "Install orchestrator and run your first task"
weight = 1
+++

## Install

```bash
brew tap gabrielkoerich/tap
brew install orchestrator
```

All dependencies (`yq`, `jq`, `just`, `python3`, `rg`, `fd`) are installed automatically.

### Agent CLIs

Install at least one:
```bash
brew install --cask claude-code   # Claude
brew install --cask codex         # Codex
brew install opencode             # OpenCode
```

Optional: `gh` for GitHub sync, `bats` for tests.

## Quick Start

### Option A: Existing local repo

```bash
cd ~/projects/my-app
orch init               # configure project (optional GitHub setup)
orch task add "title"   # add a task
orch task next          # route + run next task
orch start              # start background server
```

### Option B: Any GitHub repo

```bash
orch project add owner/repo          # bare clone + import issues
orch task add "title" -p owner/repo  # add a task to that project
orch start                           # serve loop picks it up
```

Bare clones live at `~/.orchestrator/projects/<owner>/<repo>.git`. Agents always work in worktrees — never in the main clone.

`orch` is a short alias for `orchestrator` — both work interchangeably.

## Files

All runtime state lives in `~/.orchestrator/` (`ORCH_HOME`):

| File | Description |
|------|-------------|
| `tasks.yml` | Task database |
| `jobs.yml` | Scheduled job definitions |
| `config.yml` | Runtime configuration |
| `skills.yml` | Approved skill repositories and catalog |
| `skills/` | Cloned skill repositories (via `skills-sync`) |
| `contexts/` | Persisted context files per task/profile |
| `projects/` | Bare clones added via `project add` |
| `worktrees/` | Agent worktrees (`<project>/<branch>/`) |
| `.orchestrator/` | Runtime state (pid, logs, locks, output, tool history, prompts) |

Source files:

| File | Description |
|------|-------------|
| `prompts/agent_system.md` | System prompt (workflow, rules, output format) |
| `prompts/agent_message.md` | Task message (task details, context, memory) |
| `prompts/agent_memory_entry.md` | Memory entry template (per-attempt learnings) |
| `prompts/allowed_tools.md` | Tool permissions (appended to system when configured) |
| `prompts/route.md` | Routing + profile generation prompt |
| `prompts/review_system.md` | Review agent system prompt |
| `prompts/review_task.md` | Review task prompt (diff, commits, criteria) |
| `scripts/*.sh` | Orchestration commands |
| `tests/orchestrator.bats` | Tests (bats framework) |

## How It Works

1. **Add a task** to `tasks.yml` (or via `orchestrator task add`).
2. **Route the task** with an LLM that chooses executor + builds a specialized profile and selects skills.
3. **Run the task** with the chosen executor in agentic mode. The agent runs inside `$PROJECT_DIR` with full tool access.
4. **Output**: the agent writes results to `.orchestrator/output-{task_id}.json`.
5. **Review**: if enabled, a different agent reviews the PR and posts a GitHub review.
6. **Delegation**: if the agent returns `delegations`, child tasks are created and the parent is blocked until children finish.
7. **Error handling**: if the agent fails or returns `blocked`, the error is commented on the GitHub issue with a `blocked` label.
