+++
title = "Getting Started"
description = "Install orch and run your first task"
weight = 1
+++

## Install

```bash
brew tap gabrielkoerich/homebrew-tap
brew install orch
```

All dependencies (`yq`, `jq`, `just`, `python3`, `rg`, `fd`) are installed automatically.

### Agent CLIs

Install at least one agent CLI:
```bash
brew install --cask claude-code   # Claude (if available)
brew install --cask codex         # Codex (if available)
brew install opencode             # OpenCode
```

Optional: `gh` for GitHub sync, `bats` for tests.

## Quick Start

### Option A: Existing local repo

```bash
cd ~/projects/my-app
orch init               # configure project (optional GitHub setup)
orch task add "title"   # add a task
orch task run <id>      # run a specific task
orch service start      # start background service
```

### Option B: Any GitHub repo

```bash
orch project add owner/repo          # bare clone + import issues
cd ~/.orch/projects/owner/repo.git   # or any local checkout of that repo
orch task add "title"                # add a task to the current project
orch service start                   # service picks it up automatically
```

`orch task add` always targets the current project (resolved from CWD). To create tasks for another project, `cd` into it first. Bare clones live at `~/.orch/projects/<owner>/<repo>.git`. Agents always work in worktrees — never in the main clone. Worktrees are under `~/.orch/worktrees/<project>/<branch>/`.

## GitHub Authentication

The simplest setup is `gh auth login` — orch picks up the token automatically. No extra config needed.

Tokens are resolved in this order (first match wins):

1. `GH_TOKEN` environment variable
2. `GITHUB_TOKEN` environment variable
3. `gh.auth.token` in `~/.orch/config.yml`
4. `gh auth token` CLI (enabled by default via `gh.allow_gh_fallback: true`)

For organizations, GitHub App auth is supported:

```yaml
# ~/.orch/config.yml
github:
  token_mode: github_app
  app_id: "123456"
  private_key_path: "/path/to/app-private-key.pem"
```

Orch generates JWTs from the private key (valid for 9 minutes) and caches the installation token until expiry.

### Security: Service Deployments (Homebrew / launchd)

When orch runs as a background service (`brew services start orch`), it does **not** inherit your shell environment. The service process starts with a minimal environment, so you must pass the token through one of these channels:

**Option A — `~/.private` file** (recommended for local installs):

```bash
# Create ~/.private with mode 600
echo 'export GH_TOKEN="ghp_xxxxxxxxxxxxxxxxxxxx"' >> ~/.private
chmod 600 ~/.private
```

The service sources `~/.private` if it exists.

**Option B — launchd `EnvironmentVariables`** in the plist (for system-wide installs):

```xml
<key>EnvironmentVariables</key>
<dict>
  <key>GH_TOKEN</key>
  <string>ghp_xxxxxxxxxxxxxxxxxxxx</string>
</dict>
```

Edit your plist with `sudo launchctl edit <label>` or update it via the Homebrew formula.

**Option C — GitHub App** (recommended for teams and CI):

Use `mode: github_app` in `~/.orch/config.yml` — no long-lived token is stored; the service generates short-lived JWTs and installation tokens automatically.

> **Security guarantee:** Orch never embeds `GH_TOKEN` in runner scripts on disk. Tokens are injected into the tmux session environment at spawn time (via `tmux set-environment`) and live only in process memory, not in `~/.orch/state/` artifacts.

## Files

All runtime state lives in `~/.orch/` (`ORCH_HOME`):

| File | Description |
|------|-------------|
| `config.yml` | Global runtime configuration |
| `orch.db` | SQLite task database (internal tasks, metrics, KV store) |
| `skills.yml` | Approved skill repositories and catalog |
| `skills/` | Cloned skill repositories (refreshed automatically on the engine's sync tick) |
| `projects/` | Bare clones added via `orch project add` |
| `worktrees/` | Agent worktrees (`<project>/<branch>/`) |
| `state/` | Runtime state (logs, prompts, per-task artifacts) |

Per-project config lives at `{project_path}/.orch.yml` (for GitHub repo and job definitions).

Source files:

| File | Description |
|------|-------------|
| `prompts/agent_system.md` | System prompt (workflow, rules, output format) |
| `prompts/agent_message.md` | Task message (task details, context, memory) |
| `prompts/route.md` | Routing + profile generation prompt |
| `prompts/review_system.md` | Review agent system prompt |
| `prompts/review_task.md` | Review task prompt (diff, commits, criteria) |

## How It Works

1. **Add a task** via `orch task add` (or via a GitHub issue / scheduled job).
2. **Route the task** — the LLM router chooses executor + builds a specialized profile and selects skills.
3. **Run the task** — the chosen executor runs in agentic mode inside a tmux session in the task worktree (`PROJECT_DIR`) with controlled tool access.
4. **Output** — the agent writes results to `~/.orch/state/{repo}/tasks/{id}/attempts/{n}/output.json`; task metadata is updated in the SQLite store (`~/.orch/orch.db`).
5. **Review** — if enabled, a different agent reviews the PR and posts a GitHub review comment.
6. **Delegation** — if the agent returns `delegations`, child tasks are created and the parent is blocked until children finish.
7. **Error handling** — if the agent fails or returns `blocked`, the error is commented on the GitHub issue with a `needs_review` label.
