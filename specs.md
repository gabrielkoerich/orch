# Orchestrator — Specs & Roadmap

## What It Does

Orchestrator is an autonomous task management system that delegates work to AI coding agents (Claude, Codex, OpenCode, Kimi, MiniMax). It runs as a background service, picking up tasks, routing them to agents with specialized profiles, managing worktrees for isolation, syncing state to GitHub issues/projects, and handling retries/failures.

## Architecture

```
┌─────────────────────────────────────────────────────────────────────────┐
│                           orch serve (Rust)                              │
│                                                                          │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐  │
│  │   Engine     │  │   Router     │  │    Task      │  │   GitHub     │  │
│  │   Loop       │  │   (LLM/RR)   │  │   Manager    │  │   Backend    │  │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘  │
│         │                 │                 │                 │          │
│         ▼                 ▼                 ▼                 ▼          │
│  ┌──────────────────────────────────────────────────────────────────┐  │
│  │                        Task Lifecycle                             │  │
│  │  new → routed → in_progress → done/blocked/needs_review          │  │
│  └──────────────────────────────────────────────────────────────────┘  │
│                                                                          │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐  │
│  │  Job         │  │  Capture     │  │  Transport   │  │  SQLite      │  │
│  │  Scheduler   │  │  Service     │  │  Layer       │  │  (Internal)  │  │
│  └──────────────┘  └──────────────┘  └──────────────┘  └──────────────┘  │
└─────────────────────────────────────────────────────────────────────────┘
                                    │
                                    ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                         External Systems                                 │
│                                                                          │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐  │
│  │ GitHub API   │  │   tmux       │  │ Agent CLIs   │  │   git        │  │
│  │ (gh CLI)     │  │ (sessions)   │  │ (claude/     │  │ (worktrees)  │  │
│  │              │  │              │  │  codex/etc)  │  │              │  │
│  └──────────────┘  └──────────────┘  └──────────────┘  └──────────────┘  │
└─────────────────────────────────────────────────────────────────────────┘
```

### Core Components

| Component | File | Purpose |
|-----------|------|---------|
| **Engine** | `src/engine/mod.rs` | Main async event loop (10s tick + 120s sync) |
| **Router** | `src/engine/router.rs` | LLM-based agent selection + complexity |
| **Task Manager** | `src/engine/tasks.rs` | Unified internal + external task CRUD |
| **Runner** | `src/engine/runner.rs` | Agent invocation, worktree, git, PR creation |
| **GitHub Backend** | `src/backends/github.rs` | GitHub Issues via `gh api` |
| **Job Scheduler** | `src/engine/jobs.rs` | Cron-like scheduled tasks |
| **Capture Service** | `src/channels/capture.rs` | tmux output streaming |
| **Transport** | `src/channels/transport.rs` | Broadcast to subscribers |
| **SQLite DB** | `src/db.rs` | Internal tasks + state |

### Backend Architecture

Tasks are stored in **GitHub Issues** (external) or **SQLite** (internal). A pluggable backend interface (`src/backends/mod.rs`) with GitHub implementation (`src/backends/github.rs`):

- **Status** → prefixed labels (`status:new`, `status:in_progress`, etc.)
- **Agent/Complexity** → prefixed labels (`agent:claude`, `complexity:medium`)
- **History/Response** → structured issue comments with `<!-- orch:* -->` markers
- **Ephemeral fields** (branch, worktree, attempts) → local sidecar JSON (`~/.orchestrator/.orchestrator/{id}.json`)

### Key Design Decisions

- **GitHub Issues as source of truth (external tasks)** — no sync layer, no local DB for these
- **SQLite for internal tasks** — cron jobs, mentions, maintenance without issue clutter
- **One worktree per task** — agents never work in the main repo directory
- **Complexity-based model routing** — router returns `simple|medium|complex`, config maps to agent-specific models
- **Plan label for decomposition** — only manual `plan` label triggers subtask creation, not router
- **Parent/child linking** — GitHub sub-issues API
- **Rust-native everything** — no jq/yq/python3 forks, async I/O with tokio
- **Config-driven** — `~/.orchestrator/config.yml` + per-project `.orchestrator.yml` (merged)

## What's Working

### Phase 1-2 Complete (Rust Foundation + Engine)

- **Core engine**: async tokio loop replacing `serve.sh` + `poll.sh`
- **GitHub sync**: bidirectional via `gh api` with backoff
- **Agent router**: LLM classification + label overrides + round-robin fallback
- **Agent profiles**: router generates role/skills/tools/constraints per task
- **Task runner**: full workflow (worktree, agent, git push, PR creation)
- **Retry & recovery**: stuck detection, max attempts, exponential backoff
- **Worktree lifecycle**: create branch, create worktree, agent works, auto-commit, push, create PR
- **Review agent**: optional post-completion review with reject support
- **Sub-issues**: child tasks linked as GitHub sub-issues via GraphQL
- **Catch-all PR creation**: detects pushed branches without PRs and creates them
- **Jobs**: cron-like scheduled tasks (native Rust cron, no python3)
- **Internal tasks**: SQLite-backed, no GitHub issue needed
- **Output streaming**: `orch stream <task_id>` for live agent output
- **Live session listing**: `orch task live` shows active tmux sessions
- **Mention detection**: scans for @orchestrator mentions, creates internal tasks
- **Merged PR detection**: auto-closes tasks when PRs merge
- **Worktree cleanup**: removes merged worktrees and branches
- **Release pipeline**: push → CI → auto-tag → GitHub release → Homebrew tap

### CLI Commands (All Implemented)

```bash
# Service
orch serve                    # Run service (foreground)
orch service start|stop|restart|status

# Tasks
orch task list|get|add|route|run|retry|unblock|attach|live|kill|publish

# Jobs
orch job list|add|remove|enable|disable|tick

# Utilities
orch init|agents|stream|log|version|config|sidecar|cron|template|parse
```

## Agent Sandbox

Agents are sandboxed to their worktree directory. The orchestrator creates the worktree from the main repo, then restricts agents from accessing the main project directory.

### Enforcement Layers

| Layer | Mechanism | Status |
|-------|-----------|--------|
| **Prompt** | System prompt tells agents the main project dir is read-only | Done |
| **Disallowed tools** | Dynamic `--disallowedTools` patterns block Read/Write/Edit/Bash on main dir | Done |
| **Worktree isolation** | Each task gets its own git worktree | Done |

### How It Works

1. `TaskRunner` saves `MAIN_PROJECT_DIR` before creating the worktree
2. Sandbox patterns are added to agent invocation to block access to main repo
3. Agents work in `~/.orchestrator/worktrees/<project>/<branch>/`

### Task Status Semantics

- **`blocked`** — waiting on a dependency (children not done, or infrastructure issue like missing worktree)
- **`needs_review`** — requires human attention (max attempts, review rejection, agent failures, retry loops, timeouts)
- `mark_needs_review()` sets `needs_review`, NOT `blocked`
- Only parent tasks waiting on children should be `blocked`
- Engine auto-unblocks parent tasks when all children are done

## GitHub App (Future)

### What It Would Do

A GitHub App could replace the current `gh` CLI-based API calls with proper app authentication. Benefits:

- **No personal access token needed** — app generates its own installation tokens
- **Fine-grained permissions** — only the permissions the app needs (issues, PRs, projects)
- **Webhooks** — instant event delivery instead of 60s polling
- **Rate limits** — higher API rate limits than personal tokens
- **Multi-user** — anyone can install the app on their repo, no shared credentials

### Decision

Optional future enhancement. The current `gh` CLI approach works fine for single-user setups. A GitHub App makes sense when:

- Multiple users need to install orchestrator on their repos
- Webhook-based instant sync is needed (eliminates 60s polling delay)
- Higher API rate limits are required (heavy sync workloads)

## What's Not Working / Known Issues

### Phase 3+ (Channels) — Scaffolding Done

- **Telegram bot** — stub only (`src/channels/telegram.rs`)
- **Discord bot** — stub only (`src/channels/discord.rs`)
- **GitHub webhooks** — stub only (`src/channels/github.rs`)
- **HTTP webhook server** — not implemented (needs axum)

These require webhook servers and async channel traits to be wired into the engine.

### Observability

- **Metrics collection** — success rate, avg duration, tokens per task, cost tracking
- **Web dashboard** — simple HTTP server showing task tree, status, logs
- **Alerting** — error log exists but no proactive alerting

## Improvement Ideas

### Short Term

- [ ] **Config hot-reload** — `notify` watcher exists but not connected to engine
- [ ] **Multi-project support** — engine currently handles one repo
- [ ] **Token budget** — config `max_tokens_per_task`, abort agent if exceeded
- [ ] **Cost tracking** — estimate cost from input/output tokens + model pricing table

### Medium Term

- [ ] **Webhook receiver** — GitHub webhook for instant issue sync instead of 60s polling
- [ ] **Agent memory** — persist agent learnings across retries (what worked, what didn't)
- [ ] **PR review integration** — parse GitHub PR review comments, create follow-up tasks
- [ ] **Parallel task execution** — currently limited by semaphore (4), could be dynamic

### Long Term

- [ ] **Self-improvement loop** — orchestrator creates issues for its own improvements
- [ ] **Cost optimization** — track spend per task, auto-downgrade complexity for retries
- [ ] **Agent benchmarking** — A/B test agents on similar tasks, track success rates
- [ ] **Plugin system** — custom hooks for pre/post task execution
- [ ] **Team mode** — multiple users, role-based access, task assignment

## CLI Reference

```
orch serve                    # Run the orchestrator service (foreground)
orch version                  # Show version
orch init [--repo O/R]        # Initialize for a project
orch agents                   # List installed agent CLIs
orch stream <task_id>         # Stream live output from a running task
orch log [N|watch]            # View logs (default 50 lines, or watch)
orch config <key>             # Read config value
orch sidecar get|set          # Read/write task metadata
orch cron <expression>        # Check if cron matches now
orch template <path>          # Render template with env vars
orch parse <path>             # Parse agent JSON response

# Tasks
orch task list [--status] [--source]
orch task add <title> [--body] [--labels] [--source]
orch task get <id>
orch task status [--json]
orch task route <id>
orch task run [<id>]
orch task retry <id>
orch task unblock <id>|all
orch task attach <id>
orch task live
orch task kill <id>
orch task publish <id> [--labels]

# Jobs
orch job list
orch job add <schedule> <title> [--body] [--type] [--command]
orch job remove <id>
orch job enable <id>
orch job disable <id>
orch job tick

# Service
orch service start
orch service stop
orch service restart
orch service status
```

## Config Structure

```yaml
# ~/.orchestrator/config.yml
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
    - kimi
    - minimax
  allowed_tools:
    - yq
    - jq
    - bash
    - just
    - git
    - rg
    - sed
    - awk
    - python3
    - node
    - npm
    - bun
  default_skills:
    - gh
    - git-worktree

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

gh:
  repo: "owner/repo"
  sync_label: ""
  project_id: ""
```

## File Map

| File | Purpose |
|------|---------|
| `Cargo.toml` | Rust dependencies |
| `src/main.rs` | CLI entrypoint (clap) |
| `src/config.rs` | Config loading (config.yml + .orchestrator.yml) |
| `src/db.rs` | SQLite for internal tasks |
| `src/sidecar.rs` | JSON sidecar I/O |
| `src/parser.rs` | Agent response normalization |
| `src/cron.rs` | Native cron matching |
| `src/template.rs` | Template rendering |
| `src/tmux.rs` | tmux session management |
| `src/backends/mod.rs` | ExternalBackend trait |
| `src/backends/github.rs` | GitHub Issues implementation |
| `src/channels/mod.rs` | Channel trait + registry |
| `src/channels/transport.rs` | Broadcast transport layer |
| `src/channels/capture.rs` | tmux output capture |
| `src/channels/tmux.rs` | tmux channel |
| `src/engine/mod.rs` | Main engine loop |
| `src/engine/tasks.rs` | Task manager |
| `src/engine/router.rs` | Agent router |
| `src/engine/runner.rs` | Task runner |
| `src/engine/jobs.rs` | Job scheduler |
| `src/cli/mod.rs` | CLI utilities |
| `src/cli/task.rs` | Task commands |
| `src/cli/job.rs` | Job commands |
| `src/cli/service.rs` | Service commands |
| `prompts/*.md` | Prompt templates |
| `.github/workflows/release.yml` | CI/CD pipeline |
