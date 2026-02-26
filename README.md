# Orch

An autonomous task orchestrator that delegates work to AI coding agents (Claude, Codex, OpenCode, Kimi, MiniMax). Runs as a background service, manages isolated worktrees, syncs with GitHub Issues, and handles the full task lifecycle from routing to PR creation.

## Features

- **Multi-agent support** — Route tasks to Claude, Codex, OpenCode, Kimi, or MiniMax based on task complexity
- **Multi-project** — Manage multiple repositories from a single service
- **GitHub Issues integration** — Two-way sync with GitHub Issues as the source of truth
- **GitHub Projects V2** — Automatic project board column sync on status changes
- **Isolated worktrees** — Each task runs in its own git worktree, never touching the main repo
- **Live session streaming** — Watch agents work in real-time via `orch stream <task_id>`
- **Internal tasks** — SQLite-backed tasks for cron jobs and maintenance (no GitHub issue clutter)
- **Job scheduler** — Cron-like scheduled tasks with native Rust cron matching
- **Automatic PR creation** — Branches pushed, PRs created, and comments posted automatically
- **Complexity-based routing** — Router assigns `simple|medium|complex` and config maps to models
- **Agent memory** — Learnings persist across retries so agents don't repeat mistakes
- **Per-task artifacts** — Organized per-repo, per-task, per-attempt directory structure

## Installation

```bash
brew tap gabrielkoerich/orch
brew install orch
```

## Quick Start

### 1. Initialize orch for your project

```bash
cd /path/to/your/project
orch init
```

This creates:
- `~/.orch/config.yml` — global configuration (shared defaults + project registry)
- `.orch.yml` — project-specific configuration (in your project root)

### 2. Start the service

```bash
orch service start
```

Or use Homebrew services:
```bash
brew services start orch
```

### 3. Create a task

```bash
# Create an internal task (SQLite only, no GitHub issue)
orch task add "Fix authentication bug" --body "Users can't login with OAuth"

# Create a GitHub issue task
orch task add "Update README" --labels "documentation,good-first-issue"
```

### 4. Watch the magic

The service automatically:
1. Routes tasks to the best agent based on content
2. Creates isolated worktrees
3. Runs the agent in a tmux session
4. Pushes branches and creates PRs
5. Updates GitHub issue status and project board

## CLI Reference

### Service Management

```bash
orch serve                    # Run the orchestrator service (foreground)
orch service start            # Start background service
orch service stop             # Stop background service
orch service restart          # Restart service
orch service status           # Check service status
```

### Task Management

```bash
orch task list                # List all tasks
orch task list --status new   # Filter by status
orch task get <id>            # Get task details
orch task add "Title"         # Create internal task
orch task add "Title" --labels "bug,urgent"  # Create GitHub issue
orch task route <id>          # Manually route a task to an agent
orch task run <id>            # Manually run a task
orch task run                 # Run next available routed task
orch task retry <id>          # Reset task to new for re-routing
orch task unblock <id>        # Unblock a blocked task
orch task unblock all         # Unblock all blocked/needs_review tasks
orch task publish <id>        # Promote internal task to GitHub issue
orch task attach <id>         # Attach to running tmux session
orch task live                # List active agent sessions
orch task kill <id>           # Kill a running agent session
orch task cost <id>           # Show token cost breakdown
```

### Project Management

```bash
orch project list             # List all registered projects
orch project add              # Register current directory as a project
orch project add /path/to/dir # Register a specific project path
orch project remove /path     # Unregister a project
```

### GitHub Projects V2 Board

```bash
orch board list               # List accessible GitHub Projects V2 boards
orch board link <id>          # Link current repo to a board by ID
orch board sync               # Re-discover field IDs and update config
orch board info               # Show current board config
```

### Job Management (Scheduled Tasks)

```bash
orch job list                 # List scheduled jobs
orch job add "0 9 * * *" "Morning review"     # Daily at 9am
orch job add "*/30 * * * *" "Check CI" --type bash --command "scripts/ci-check.sh"
orch job remove <id>          # Remove a job
orch job enable <id>          # Enable a job
orch job disable <id>         # Disable a job
orch job tick                 # Run one scheduler tick (for testing)
```

### Utility Commands

```bash
orch init                     # Initialize orch for current project
orch init --repo owner/repo   # Initialize with specific repo
orch agents                   # List installed agent CLIs
orch metrics                  # Show task metrics summary (24h)
orch stream <task_id>         # Stream live output from a running task
orch log                      # Show last 50 log lines
orch log 100                  # Show last N lines
orch log watch                # Tail logs live
orch version                  # Show version
orch config <key>             # Read config value (e.g., orch config gh.repo)
orch sidecar get <id> <field> # Read task metadata
orch sidecar set <id> field=value  # Write task metadata
orch completions <shell>      # Generate shell completions (bash, zsh, fish)
```

## Configuration

### Global config (`~/.orch/config.yml`)

Shared defaults and project registry. All settings here apply to every project unless overridden.

```yaml
# Project registry — list of local paths
# Each path must contain a .orch.yml with gh.repo
projects:
  - /Users/me/Projects/my-app
  - /Users/me/Projects/my-lib

workflow:
  auto_close: true
  review_owner: "@owner"
  enable_review_agent: false
  max_attempts: 10
  timeout_seconds: 1800

router:
  mode: "llm"              # "llm" (default) or "round_robin"
  agent: "claude"          # which LLM performs routing
  model: "haiku"           # fast/cheap model for classification
  timeout_seconds: 120
  fallback_executor: "codex"

model_map:
  simple:
    claude: haiku
    codex: gpt-5.1-codex-mini
  medium:
    claude: sonnet
    codex: gpt-5.2
  complex:
    claude: opus
    codex: gpt-5.3-codex
  review:
    claude: sonnet
    codex: gpt-5.2

agents:
  claude:
    allowed_tools: [...]   # Claude Code tool allowlist
  opencode:
    permission: { ... }    # OpenCode permission config
    models: [...]          # Available models

git:
  name: "orchestrator[bot]"
  email: "orchestrator@orchestrator.bot"
```

### Project config (`.orch.yml`)

Place in your project root. Values here override global defaults.

```yaml
# Required — identifies this project on GitHub
gh:
  repo: "owner/repo"
  project_id: "PVT_..."           # optional: GitHub Projects V2
  project_status_field_id: "..."   # optional
  project_status_map:              # optional
    backlog: "option-id-1"
    in_progress: "option-id-2"
    review: "option-id-3"
    done: "option-id-4"

# Optional overrides
workflow:
  max_attempts: 5

router:
  fallback_executor: "codex"

required_tools:
  - cargo
  - bun

# Per-project scheduled jobs
jobs:
  - id: code-review
    schedule: "0 4,17 * * *"
    task:
      title: "Code review"
      body: "Review the codebase for bugs and improvements"
      labels: [review]
    enabled: true
```

## Task Statuses

| Status | Description |
|--------|-------------|
| `new` | Task created, awaiting routing |
| `routed` | Agent assigned, awaiting execution |
| `in_progress` | Agent actively working |
| `done` | Task completed successfully |
| `blocked` | Waiting on dependencies or children |
| `in_review` | PR created, awaiting review |
| `needs_review` | Requires human attention |

## Label-Based Routing

Override the router by adding labels to GitHub issues:

| Label | Effect |
|-------|--------|
| `agent:claude` | Force Claude executor |
| `agent:codex` | Force Codex executor |
| `agent:opencode` | Force OpenCode executor |
| `agent:kimi` | Force Kimi executor |
| `agent:minimax` | Force MiniMax executor |
| `complexity:simple` | Use simple model tier |
| `complexity:medium` | Use medium model tier |
| `complexity:complex` | Use complex model tier |
| `no-agent` | Skip agent routing (manual task) |

## Architecture

Orch is built in Rust with a modular architecture:

```
src/
├── main.rs              # CLI entrypoint (clap)
├── config/
│   └── mod.rs           # Config loading, hot-reload, multi-project
├── db.rs                # SQLite for internal tasks
├── sidecar.rs           # JSON sidecar file I/O + agent memory
├── parser.rs            # Agent response normalization
├── cron.rs              # Cron expression matching
├── template.rs          # Template rendering
├── tmux.rs              # tmux session management
├── security.rs          # Secret scanning + redaction
├── home.rs              # Home directory (~/.orch/) + per-repo state paths
├── backends/            # External task backends
│   ├── mod.rs           # ExternalBackend trait
│   └── github.rs        # GitHub Issues + Projects V2 sync
├── channels/            # Communication channels
│   ├── transport.rs     # Output broadcasting
│   ├── capture.rs       # tmux output capture
│   ├── notification.rs  # Unified notifications
│   ├── tmux.rs          # tmux bridge
│   ├── github.rs        # GitHub webhooks
│   ├── telegram.rs      # Telegram bot
│   └── discord.rs       # Discord bot
├── cli/                 # CLI command implementations
│   ├── mod.rs           # Init, agents, board, project, metrics
│   ├── task.rs          # Task CRUD
│   ├── job.rs           # Job management
│   └── service.rs       # Service lifecycle
├── github/              # GitHub API helpers
│   ├── cli.rs           # gh CLI wrapper
│   ├── types.rs         # Issue, Comment, Label types
│   └── projects.rs      # Projects V2 GraphQL operations
└── engine/              # Core orchestration
    ├── mod.rs           # Main event loop + PR review integration
    ├── tasks.rs         # Task manager (internal + external)
    ├── router.rs        # Agent routing (label, round-robin, LLM)
    ├── jobs.rs          # Job scheduler + self-review
    └── runner/          # Task execution
        ├── mod.rs       # Full task lifecycle
        ├── context.rs   # Prompt context building
        ├── worktree.rs  # Git worktree management
        ├── agent.rs     # Agent invocation + prompt building
        ├── agents/      # Per-agent runners (Claude, Codex, OpenCode)
        ├── response.rs  # Response handling, cooldowns, memory
        └── git_ops.rs   # Auto-commit, push, PR creation
```

## Task Artifacts

Task artifacts are organized per-repo, per-task, per-attempt:

```
~/.orch/state/{owner}/{repo}/tasks/{id}/
  sidecar.json              # Task metadata
  attempts/
    1/
      prompt-sys.md         # System prompt
      prompt-msg.md         # Task prompt
      runner.sh             # Runner script
      exit.txt              # Exit code
      stderr.txt            # Agent stderr
      output.json           # Agent response
    2/                      # Retry attempt
      ...
```

## Development

### Building from source

```bash
git clone https://github.com/gabrielkoerich/orch.git
cd orch
cargo build --release
```

### Running tests

```bash
cargo test
```

### Logs

- Service log: `~/.orch/state/orch.log`
- Homebrew stdout: `/opt/homebrew/var/log/orch.log`
- Homebrew stderr: `/opt/homebrew/var/log/orch.error.log`

## Documentation

- [PLAN.md](PLAN.md) — Architecture, migration plan, and feature roadmap
- [AGENTS.md](AGENTS.md) — Agent and developer notes

## License

MIT
