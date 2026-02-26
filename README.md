# Orch

An autonomous task orchestrator that delegates work to AI coding agents (Claude, Codex, OpenCode, Kimi, MiniMax). Runs as a background service, manages isolated worktrees, syncs with GitHub Issues, and handles the full task lifecycle from routing to PR creation.

## Features

- **Multi-agent support** — Route tasks to Claude, Codex, OpenCode, Kimi, or MiniMax based on task complexity
- **GitHub Issues integration** — Two-way sync with GitHub Issues as the source of truth
- **Isolated worktrees** — Each task runs in its own git worktree, never touching the main repo
- **Live session streaming** — Watch agents work in real-time via `orch stream <task_id>`
- **Internal tasks** — SQLite-backed tasks for cron jobs and maintenance (no GitHub issue clutter)
- **Job scheduler** — Cron-like scheduled tasks with native Rust cron matching
- **Automatic PR creation** — Branches pushed, PRs created, and comments posted automatically
- **Complexity-based routing** — Router assigns `simple|medium|complex` and config maps to models

## Installation

```bash
brew tap gabrielkoerich/orch
brew install orch
```

## Quick Start

### 1. Initialize orch for your project

```bash
orch init
```

This creates:
- `~/.orchestrator/config.yml` — global configuration
- `.orchestrator.yml` — project-specific configuration

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
5. Updates GitHub issue status

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
orch stream <task_id>         # Stream live output from a running task
orch log                      # Show last 50 log lines
orch log 100                  # Show last N lines
orch log watch                # Tail logs live
orch version                  # Show version
orch config <key>             # Read config value (e.g., orch config repo)
orch sidecar get <id> <field> # Read task metadata
orch sidecar set <id> field=value  # Write task metadata
```

## Configuration

### Global config (`~/.orchestrator/config.yml`)

```yaml
repo: owner/repo

workflow:
  auto_close: true
  review_owner: "@owner"
  enable_review_agent: false
  max_attempts: 10

router:
  mode: "llm"              # "llm" (default) or "round_robin"
  agent: "claude"          # which LLM performs routing
  model: "haiku"           # fast/cheap model for classification
  timeout_seconds: 120
  fallback_executor: "codex"
  max_route_attempts: 3
  agents:
    - claude
    - codex
    - opencode

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
```

### Project config (`.orchestrator.yml`)

```yaml
repo: owner/repo

# Override global settings for this project
workflow:
  max_attempts: 5
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
├── config.rs            # Config loading and hot-reload
├── db.rs                # SQLite for internal tasks
├── sidecar.rs           # JSON sidecar file I/O
├── parser.rs            # Agent response normalization
├── cron.rs              # Cron expression matching
├── template.rs          # Template rendering
├── tmux.rs              # tmux session management
├── backends/            # External task backends
│   ├── mod.rs           # ExternalBackend trait
│   └── github.rs        # GitHub Issues implementation
├── channels/            # Communication channels
│   ├── transport.rs     # Output broadcasting
│   ├── capture.rs       # tmux output capture
│   └── tmux.rs          # tmux bridge
├── cli/                 # CLI command implementations
│   ├── mod.rs
│   ├── task.rs
│   ├── job.rs
│   └── service.rs
└── engine/              # Core orchestration
    ├── mod.rs           # Main event loop
    ├── tasks.rs         # Task manager
    ├── router.rs        # Agent routing
    ├── runner.rs        # Task execution
    └── jobs.rs          # Job scheduler
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

- Service log: `~/.orchestrator/.orchestrator/orch.log`
- Homebrew stdout: `/opt/homebrew/var/log/orch.log`
- Homebrew stderr: `/opt/homebrew/var/log/orch.error.log`

## Documentation

- [specs.md](specs.md) — Architecture specs, roadmap, and design decisions
- [AGENTS.md](AGENTS.md) — Agent and developer notes
- [PLAN.md](PLAN.md) — Original migration plan (bash → Rust)

## License

MIT
