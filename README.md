# Orch

[![CI](https://github.com/gabrielkoerich/orch/actions/workflows/release.yml/badge.svg)](https://github.com/gabrielkoerich/orch/actions/workflows/release.yml)

An autonomous task orchestrator that delegates work to AI coding agents (Claude, Codex, OpenCode, Kimi, MiniMax). Runs as a background service, manages isolated worktrees, syncs with GitHub Issues, and handles the full task lifecycle from routing to PR creation.

## Features

- **Multi-agent support** — Route tasks to Claude, Codex, OpenCode, Kimi, or MiniMax based on task complexity
- **Multi-project** — Manage multiple repositories from a single service
- **GitHub Issues integration** — Two-way sync with GitHub Issues as the source of truth
- **GitHub Projects V2** — Automatic project board column sync on status changes
- **Isolated worktrees** — Each task runs in its own git worktree, never touching the main repo
- **Live session streaming** — Watch agents work in real-time via `orch stream` (all sessions) or `orch stream <task_id>`
- **Control session** — Conversational ops assistant via `orch chat` — ask about tasks, create new ones, check status in natural language
- **Internal tasks** — SQLite-backed tasks for cron jobs and maintenance (no GitHub issue clutter)
- **Job scheduler** — Cron-like scheduled tasks with native Rust cron matching
- **Automatic PR creation** — Branches pushed, PRs created, and comments posted automatically
- **Complexity-based routing** — Router assigns `simple|medium|complex` and config maps to models
- **Agent memory** — Learnings persist across retries so agents don't repeat mistakes
- **Per-task artifacts** — Organized per-repo, per-task, per-attempt directory structure

## Installation

```bash
brew tap gabrielkoerich/homebrew-tap
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

## GitHub Authentication

Orch requires GitHub authentication to sync with issues and create PRs. Three methods are supported:

### Personal Access Token (Recommended for individuals)

```bash
# Set as environment variable
export GH_TOKEN="ghp_xxxxxxxxxxxxxxxxxxxx"

# Or configure in ~/.orch/config.yml
gh:
  auth:
    token: "ghp_xxxxxxxxxxxxxxxxxxxx"
```

### GitHub App (Recommended for organizations)

Better audit trails and scoped permissions for team automation:

```yaml
github:
  token_mode: github_app
  app_id: "123456"
  private_key_path: "/path/to/app-private-key.pem"
```

The orchestrator automatically generates JWTs and refreshes installation tokens before expiry.

### gh CLI (Default fallback)

The simplest setup — just authenticate once and everything works:

```bash
gh auth login
```

Orch calls `gh auth token` automatically when `GH_TOKEN`/`GITHUB_TOKEN` are not set.
This fallback is enabled by default (`gh.allow_gh_fallback: true`).

To disable it (enforce explicit token configuration):

```yaml
# ~/.orch/config.yml
gh:
  allow_gh_fallback: false
```

See [Configuration](#configuration) for details and run `orch auth check` to verify your setup.

### Security: Service Deployments (Homebrew / launchd)

When running as a background service (e.g., `brew services start orch`), the service process does **not** inherit your shell environment. Pass the token securely:

**Option A — `~/.private` file** (sourced automatically by runner scripts):
```bash
# ~/.private  (chmod 600)
export GH_TOKEN="ghp_xxxxxxxxxxxxxxxxxxxx"
```

**Option B — launchd `EnvironmentVariables`** in the plist:
```xml
<key>EnvironmentVariables</key>
<dict>
  <key>GH_TOKEN</key>
  <string>ghp_xxxxxxxxxxxxxxxxxxxx</string>
</dict>
```

**Option C — GitHub App** (recommended for teams): no long-lived token needed; the service exchanges a short-lived JWT for an installation token automatically.

> **Important:** Orch never writes `GH_TOKEN` into runner scripts on disk. Tokens are injected into the tmux session environment at spawn time and exist only in process memory.

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
orch task close <id>          # Manually mark a task as done
orch task close <id> --note "msg"  # Mark done with a comment
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
orch stream                   # Stream ALL running agent sessions
orch stream <task_id>         # Stream a single task
orch log                      # Show last 50 log lines
orch log 100                  # Show last N lines
orch log watch                # Tail logs live
orch version                  # Show version
orch config <key>             # Read config value (e.g., orch config gh.repo)
orch completions <shell>      # Generate shell completions (bash, zsh, fish)
```

### Control Session (Chat)

```bash
orch chat                           # Interactive REPL
orch chat "what's running?"         # Single message mode
orch chat --session ops             # Use a named session profile
orch chat history                   # Show recent messages
orch chat history --search "bean"   # Search past conversations
```

In the REPL, use `/model` to switch models:
```
orch> /model sonnet                 # Infer agent (claude)
orch> /model minimax:sonnet         # Explicit agent:model
orch> /model opencode:minimax-m2.5-free  # OpenCode with specific model
orch> /model                        # Show current agent:model
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
  name: "orch[bot]"
  email: "orch@orch.bot"
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
├── control.rs           # Control session (orch chat) — context assembly, agent invocation
├── config/
│   └── mod.rs           # Config loading, hot-reload, multi-project
├── store.rs             # Unified SQLite task store (tasks, metrics, KV, rate limits)
├── parser.rs            # Agent response normalization
├── cron.rs              # Cron expression matching
├── template.rs          # Template rendering
├── tmux.rs              # tmux session management
├── security.rs          # Secret scanning + redaction
├── home.rs              # Home directory (~/.orch/) + per-repo state paths
├── cmd.rs               # Command execution helpers with error context
├── cmd_cache.rs         # Cached command results
├── repo_context.rs      # Per-repo task-local context
├── webhook_status.rs    # Webhook health tracking
├── backends/            # External task backends
│   ├── mod.rs           # ExternalBackend trait
│   └── github.rs        # GitHub Issues + Projects V2 sync
├── channels/            # Communication channels
│   ├── transport.rs     # Output broadcasting
│   ├── capture.rs       # tmux output capture
│   ├── notification.rs  # Unified notifications
│   ├── stream.rs        # Live output streaming
│   ├── tmux.rs          # tmux bridge
│   ├── github.rs        # GitHub webhooks
│   ├── slack.rs         # Slack integration
│   ├── telegram.rs      # Telegram bot
│   ├── discord.rs       # Discord registration + REST helpers
│   └── discord_ws.rs    # Discord Gateway websocket (real-time events)
├── cli/                 # CLI command implementations
│   ├── mod.rs           # Init, agents, board, project, metrics, stream
│   ├── chat.rs          # Control session (REPL, single-message, history)
│   ├── task.rs          # Task CRUD
│   ├── job.rs           # Job management
│   └── service.rs       # Service lifecycle
├── github/              # GitHub API helpers
│   ├── cli_wrapper.rs   # gh CLI wrapper
│   ├── http.rs          # Native HTTP client (reqwest, connection pooling)
│   ├── token.rs         # Token resolution (env, config, gh CLI, GitHub App)
│   ├── types.rs         # Issue, Comment, Label, PR review types
│   └── projects.rs      # Projects V2 GraphQL operations
└── engine/              # Core orchestration
    ├── mod.rs           # Main event loop, project init, struct defs
    ├── tick.rs          # Core tick phases (sessions, routing, dispatch, unblock)
    ├── sync.rs          # Periodic sync (cleanup, PR review, mentions, skills)
    ├── review.rs        # PR review pipeline (review agent, auto-merge, re-route)
    ├── cleanup.rs       # Worktree cleanup, merged-PR detection, store helpers
    ├── commands.rs      # Owner /slash commands in issue comments
    ├── tasks.rs         # Task manager (internal + external, unified store)
    ├── router/          # Agent routing (label, round-robin, LLM)
    │   ├── mod.rs       # Router logic and RouteResult
    │   ├── config.rs    # Router configuration
    │   └── weights.rs   # Routing weight signals
    ├── jobs.rs          # Job scheduler + self-review
    └── runner/          # Task execution
        ├── mod.rs       # Full task lifecycle
        ├── task_init.rs # Guard checks, worktree setup, invocation building
        ├── session.rs   # tmux session lifecycle and output collection
        ├── context.rs   # Prompt context building
        ├── worktree.rs  # Git worktree management
        ├── agent.rs     # Agent invocation + prompt building
        ├── agents/      # Per-agent runners (Claude, Codex, OpenCode)
        ├── response.rs  # Response parsing, weight signals
        ├── response_handler.rs # Success path: commit, push, PR, budget
        ├── fallback.rs  # Error classification and recovery strategies
        └── git_ops.rs   # Auto-commit, push, PR creation
```

## Task Artifacts

Task artifacts are organized per-repo, per-task, per-attempt:

```
~/.orch/state/{owner}/{repo}/tasks/{id}/
  attempts/
    1/
      prompt-sys.md         # System prompt
      prompt-msg.md         # Task prompt
      runner.sh             # Runner script
      exit.txt              # Exit code
      stderr.txt            # Agent stderr
      output.json           # Agent response
      result.json           # Parsed result (status, summary, etc.)
    2/                      # Retry attempt
      ...
```

Task metadata (branch, worktree, agent, model, attempts, pr_number, memory, etc.) is stored in the unified SQLite database at `~/.orch/orch.db`, not in per-task JSON files.

## Development

### Building from source

```bash
git clone https://github.com/gabrielkoerich/orch.git
cd orch
cargo build --release
```

### Running tests

```bash
cargo nextest run            # preferred (matches CI)
cargo test                   # fallback if nextest is not installed
```

Install nextest: `cargo binstall cargo-nextest` (requires [cargo-binstall](https://github.com/cargo-bins/cargo-binstall)).

### Logs

- Service log: `~/.orch/state/orch.log`
- Homebrew stdout: `/opt/homebrew/var/log/orch.log`
- Homebrew stderr: `/opt/homebrew/var/log/orch.error.log`

## Documentation

- [AGENTS.md](AGENTS.md) — Agent and developer notes
- [docs/architecture.md](docs/architecture.md) — System architecture and diagrams

## License

MIT
