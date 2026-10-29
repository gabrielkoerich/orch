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

On Linux, install the prebuilt binary with [cargo-binstall](https://github.com/cargo-bins/cargo-binstall), or build from source with cargo:

```bash
cargo binstall orch-cli      # prebuilt x86_64 binary
cargo install orch-cli       # from source, needs libssl-dev and pkg-config
orch service install         # systemd user unit for `orch serve`
```

`orch service start|stop|restart|status` drives it through `systemctl --user`. A user service stops at logout unless lingering is on (`loginctl enable-linger $USER`).

The Homebrew formula targets macOS on Apple silicon and installs `gh` and `tmux`. On Linux, install `gh` and `tmux` yourself.

### Agent CLIs

Install at least one agent CLI:
```bash
brew install --cask claude-code   # Claude (if available)
brew install --cask codex         # Codex (if available)
brew install opencode             # OpenCode
```

The `grok` CLI must be on `PATH`. Orch discovers it by the binary name `grok`.


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
4. `gh auth token` CLI

### Security: Service Deployments (Homebrew / launchd)

When orch runs as a background service (launchd on macOS, the systemd user unit on Linux), it does **not** inherit your shell environment. The service process starts with a minimal environment, so you must pass the token through one of these channels:

**Option A — `~/.private` file** (recommended for local installs):

```bash
# Create ~/.private with mode 600
echo 'export GH_TOKEN="ghp_xxxxxxxxxxxxxxxxxxxx"' >> ~/.private
chmod 600 ~/.private
```

At startup orch loads the `export KEY=VALUE` lines of `~/.private` into its environment, if the file exists.

**Option B — launchd `EnvironmentVariables`** in the plist (for system-wide installs):

```xml
<key>EnvironmentVariables</key>
<dict>
  <key>GH_TOKEN</key>
  <string>ghp_xxxxxxxxxxxxxxxxxxxx</string>
</dict>
```

Edit your plist with `sudo launchctl edit <label>` or update it via the Homebrew formula.

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
2. **Route the task:** the router picks an agent and the agent's model for the task's complexity. `router.mode` is `round_robin` by default. In `llm` mode the router also builds a profile and selects skills.
3. **Run the task** — the chosen executor runs in agentic mode inside a tmux session in the task worktree (`PROJECT_DIR`) with controlled tool access.
4. **Output** — the agent writes results to `~/.orch/state/{repo}/tasks/{id}/attempts/{n}/output.json`; task metadata is updated in the SQLite store (`~/.orch/orch.db`).
5. **Review:** a different agent reviews the PR when one is available (the authoring agent reviews otherwise) and posts an `## Automated Review` comment, or an App review in `review.mode: app`. An approved task waits in `approved` until CI passes and orch merges the PR.
6. **Delegation** — if the agent returns `delegations`, child tasks are created and the parent is blocked until children finish.
7. **Error handling:** a failed run puts the agent or model in cooldown and routes the task again. After `workflow.max_attempts` attempts (default 5) the task is `blocked`.
