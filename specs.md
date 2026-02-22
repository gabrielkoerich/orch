# Orchestrator — Specs & Roadmap

## What It Does

Orchestrator is an autonomous task management system that delegates work to AI coding agents (Claude, Codex, OpenCode). It runs as a background service, picking up tasks, routing them to agents with specialized profiles, managing worktrees for isolation, syncing state to GitHub issues/projects, and handling retries/failures.

## Architecture

```
serve.sh (10s tick loop)
  ├── poll.sh              — pick new/routed tasks, detect stuck, unblock parents
  │     ├── route_task.sh  — LLM router assigns agent + complexity + profile
  │     └── run_task.sh    — build prompt, invoke agent, parse response, push branch, create PR
  ├── jobs_tick.sh         — run scheduled jobs (cron-like)
  ├── cleanup_worktrees.sh — remove merged worktrees + local branches
  └── review_prs.sh        — auto-review open PRs
```

### Backend Architecture

Tasks are stored directly in **GitHub Issues** — no local database. A pluggable backend interface (`backend.sh`) sources the GitHub implementation (`backend_github.sh`), which maps task fields to issue properties:

- **Status** → prefixed labels (`status:new`, `status:in_progress`, etc.)
- **Agent/Complexity** → prefixed labels (`agent:claude`, `complexity:medium`)
- **History/Response** → structured issue comments with `<!-- orch:* -->` markers
- **Ephemeral fields** (branch, worktree, attempts) → local sidecar JSON (`$STATE_DIR/tasks/{id}.json`)

### Key Design Decisions
- **GitHub Issues as source of truth** — no sync layer, no local DB
- **One worktree per task** — agents never work in the main repo directory
- **Complexity-based model routing** — router returns `simple|medium|complex`, config maps to agent-specific models
- **Plan label for decomposition** — only manual `plan` label triggers subtask creation, not router
- **Parent/child linking** — GitHub sub-issues API
- **Lock-based concurrency** — mkdir locks for per-task locks to prevent double-runs
- **Config-driven** — `~/.orchestrator/config.yml` + per-project `orchestrator.yml` (merged)

## What's Working

- **Core loop**: serve → poll → route → run → push/PR is solid
- **GitHub sync**: bidirectional, with backoff, comment dedup, project board status
- **Agent profiles**: router generates role/skills/tools/constraints per task
- **Retry & recovery**: stuck detection, max attempts, exponential backoff, agent switching on auth errors
- **Worktree lifecycle**: create branch, create worktree, agent works, auto-commit, push, create PR
- **Review agent**: optional post-completion review with reject (auto-close PR) support
- **Sub-issues**: child tasks linked as GitHub sub-issues via GraphQL
- **Catch-all PR creation**: detects pushed branches without PRs and creates them
- **Skills system**: SKILL.md-based catalog, required/reference skills injected into prompts
- **Jobs**: cron-like scheduled tasks (bash commands or task creation)
- **150 tests**: comprehensive bats test suite, all passing
- **Release pipeline**: push → CI → auto-tag → GitHub release → Homebrew tap update

## Agent Sandbox

Agents are sandboxed to their worktree directory. The orchestrator creates the worktree from the main repo, then restricts agents from accessing the main project directory.

### Enforcement Layers

| Layer | Mechanism | Status |
|-------|-----------|--------|
| **Prompt** | System prompt tells agents the main project dir is read-only | Done |
| **Disallowed tools** | Dynamic `--disallowedTools` patterns block Read/Write/Edit/Bash on main dir | Done (v0.19) |
| **Codex sandbox** | `--full-auto` runs in Docker container | Built-in but too restrictive |
| **Container** | Full Docker isolation with worktree mounted | Not planned — auth problem |

### How It Works
1. `run_task.sh` saves `MAIN_PROJECT_DIR` before creating the worktree
2. If `workflow.sandbox` is `true` (default), sandbox patterns are added to `--disallowedTools`:
   - `Read($MAIN_PROJECT_DIR/*)` — blocks reading main repo files
   - `Write($MAIN_PROJECT_DIR/*)` — blocks writing main repo files
   - `Edit($MAIN_PROJECT_DIR/*)` — blocks editing main repo files
   - `Bash(cd $MAIN_PROJECT_DIR*)` — blocks navigating to main repo
3. These patterns are added to the configured `workflow.disallowed_tools` list
4. Config: `workflow.sandbox: false` to disable (not recommended)

### Codex Limitations
Codex's `--full-auto` mode runs in a Docker sandbox that blocks network access and external tools. This means:
- No `gh` CLI (GitHub API calls fail with "error connecting to api.github.com")
- No `bun` (missing from container PATH)
- No `solana-test-validator` or other system tools
- Codex can only do pure code changes — no CI, no API calls, no package installs

Container-based sandboxing for other agents (Claude, OpenCode) is impractical because:
- Agents require authenticated subscriptions (Claude Code login, API keys)
- Interactive auth flows (1Password, biometric) don't work in containers
- Each agent would need its own container image with auth pre-configured

### Task Status Semantics
- **`blocked`** — waiting on a dependency (children not done, or infrastructure issue like missing worktree)
- **`needs_review`** — requires human attention (max attempts, review rejection, agent failures, retry loops)
- `mark_needs_review()` in lib.sh sets `needs_review`, not `blocked`
- Only parent tasks waiting on children should be `blocked`
- `poll.sh` auto-unblocks parent tasks when all children are done; `needs_review` tasks require manual action

## GitHub App (Investigation)

### What It Would Do
A GitHub App could replace the current `gh` CLI-based API calls with proper app authentication. Benefits:
- **No personal access token needed** — app generates its own installation tokens
- **Fine-grained permissions** — only the permissions the app needs (issues, PRs, projects)
- **Webhooks** — instant event delivery instead of 60s polling
- **Rate limits** — higher API rate limits than personal tokens
- **Multi-user** — anyone can install the app on their repo, no shared credentials

### What It Needs
- A server to receive webhooks (or use GitHub Actions as a relay)
- App registration on GitHub (name, description, permissions, webhook URL)
- Private key for JWT signing (stored securely)
- Installation token refresh logic (tokens expire every hour)

### Permissions Required
- **Issues**: read/write (create, update, comment, close)
- **Pull requests**: read/write (create, review, merge)
- **Contents**: read/write (push branches)
- **Projects**: read/write (board status updates)
- **Metadata**: read (repo info)

### Installation
- Anyone can install via "Install App" button on the GitHub App page
- One-click install per repo or org-wide
- No coding required for the installer — just approve permissions

### Architecture Options
1. **Hosted app**: Run a small server (Cloudflare Worker, Vercel, etc.) that receives webhooks and calls orchestrator APIs
2. **GitHub Actions relay**: App triggers a workflow, workflow runs orchestrator commands
3. **Hybrid**: App handles webhooks for instant sync, orchestrator still runs locally for agent execution

### Decision
Optional enhancement. The current `gh` CLI approach works fine for single-user setups. A GitHub App makes sense when:
- Multiple users need to install orchestrator on their repos
- Webhook-based instant sync is needed (eliminates 60s polling delay)
- Higher API rate limits are required (heavy sync workloads)

## What's Not Working / Known Issues

### PR Creation Gaps
- PR creation only happens inside `run_task.sh` for `done|in_progress` status. If the script crashes between push and PR creation, the branch is orphaned.

### Agent Reliability
- Agents sometimes produce empty or malformed responses, especially on timeout. Current fallback is `needs_review` but no automatic retry with a different model.
- No token budget enforcement — agents can burn unlimited tokens on a single task.
- SSH/1Password interactive prompts can block agents silently. Detection exists but recovery is just a log warning.

### Observability
- Dashboard (`dashboard.sh`) exists but is basic. No web UI.
- No metrics collection (success rate, avg duration, tokens per task, cost tracking).
- Error log exists but no alerting.

## Improvement Ideas

### Short Term
- [ ] **Worktree janitor**: poll.sh cleans up merged worktrees + branches on `done` tasks
- [ ] **Token budget**: config `max_tokens_per_task`, abort agent if exceeded
- [ ] **Batch closed-issue check**: single API call in gh_pull instead of N+1
- [ ] **Issue reopen handling**: gh_pull detects reopened issues → reset task to `new`
- [ ] **Review model in config**: already in `model_map.review`, wire up to config UI in `init.sh`
- [ ] **Cost tracking**: estimate cost from input/output tokens + model pricing table

### Medium Term
- [ ] **Web dashboard**: simple HTTP server showing task tree, status, logs, token usage
- [ ] **Webhook receiver**: GitHub webhook for instant issue sync instead of 60s polling
- [ ] **Agent memory**: persist agent learnings across retries (what worked, what didn't)
- [ ] **PR review integration**: parse GitHub PR review comments, create follow-up tasks
- [ ] **Multi-repo orchestration**: single orchestrator managing tasks across multiple repos
- [ ] **Parallel task execution**: currently sequential within a poll tick, could use job queue

### Long Term
- [ ] **Self-improvement loop**: orchestrator creates issues for its own improvements, agents implement them
- [ ] **Cost optimization**: track spend per task, auto-downgrade complexity for retries
- [ ] **Agent benchmarking**: A/B test agents on similar tasks, track success rates
- [ ] **Plugin system**: custom hooks for pre/post task execution
- [ ] **Team mode**: multiple users, role-based access, task assignment

## CLI Reference

```
orchestrator init                    # interactive setup
orchestrator status                  # task counts overview
orchestrator dashboard               # full TUI dashboard

# Tasks
orchestrator task list|tree|add|plan|route|run|next|poll
orchestrator task retry|unblock|agent|stream|watch|unlock

# Service
orchestrator start|stop|restart|info
orchestrator service install|uninstall

# GitHub
orchestrator gh pull|push|sync

# Projects
orchestrator project info|create|list

# Jobs
orchestrator job add|list|remove|enable|disable|tick

# Skills
orchestrator skills list|sync

# Other
orchestrator chat                    # interactive chat with context
orchestrator log [watch]             # view logs
orchestrator agents                  # list available agents
orchestrator version                 # current version
```

## Config Structure

```yaml
# ~/.orchestrator/config.yml
workflow:
  auto_close: true
  review_owner: "@owner"
  enable_review_agent: false
  review_agent: "claude"
  max_attempts: 10

router:
  agent: "claude"
  model: "haiku"              # model for the router itself
  timeout_seconds: 120
  fallback_executor: "codex"

model_map:
  simple:   { claude: haiku, codex: gpt-5.1-codex-mini }
  medium:   { claude: sonnet, codex: gpt-5.2 }
  complex:  { claude: opus, codex: gpt-5.3-codex }
  review:   { claude: sonnet, codex: gpt-5.2 }

gh:
  repo: "owner/repo"
  sync_label: ""
  project_id: ""
```

## File Map

| File | Purpose |
|------|---------|
| `justfile` | CLI entrypoint, dispatches to scripts |
| `scripts/lib.sh` | Shared helpers (log, lock, yq, config, model_for_complexity) |
| `scripts/backend.sh` | Backend interface loader + jobs CRUD (YAML-backed) |
| `scripts/backend_github.sh` | GitHub Issues backend implementation |
| `scripts/serve.sh` | Main loop (10s tick) |
| `scripts/poll.sh` | Pick tasks, detect stuck, unblock parents |
| `scripts/route_task.sh` | LLM router → agent + complexity + profile |
| `scripts/run_task.sh` | Invoke agent, parse response, push, PR |
| `scripts/output.sh` | Shared formatting (tables, sections) |
| `prompts/route.md` | Router prompt template |
| `prompts/system.md` | Agent system prompt template |
| `prompts/agent.md` | Agent message template |
| `prompts/review.md` | Review agent prompt template |
| `prompts/plan.md` | Plan/decompose prompt template |
| `tests/orchestrator.bats` | 200 bats tests |
| `tests/gh_mock.sh` | Comprehensive gh CLI mock for testing |
