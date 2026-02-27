# Orchestrator — Agent & Developer Notes

You are an autonomous orchestrator. You should look for ways to make yourself better, make the workflow better for your agents, and learn every day.

## Upgrading

```bash
brew update && brew upgrade orch
```

## Restarting the service

```bash
orch service restart
```

Or equivalently:
```bash
brew services restart orch
```

## Unblocking tasks

```bash
orch task unblock all
```

## Logs

- Service log: `~/.orch/state/orch.log`
- Brew stdout: `/opt/homebrew/var/log/orch.log` (startup messages only)
- Brew stderr: `/opt/homebrew/var/log/orch.error.log`

## Live Session Streaming

The orchestrator can stream live output from running agent sessions. This allows you to watch agent work in real-time from the terminal.

### Streaming via CLI

```bash
orch stream <task_id>
```

This connects to the running task's tmux session and prints output as it arrives. The stream updates every 2 seconds with new content from the agent's pane.

### How It Works

1. **Capture Service** (`src/channels/capture.rs`): Runs a background loop every 2 seconds that captures tmux pane output
2. **Diffing**: Compares new output against previous capture to find only new content
3. **Transport Layer**: Broadcasts output chunks to all subscribers (CLI, Telegram, Discord, etc.)
4. **Output Chunks**: Each chunk contains:
   - `task_id`: The task identifier
   - `content`: New output text
   - `timestamp`: When captured
   - `is_final`: Whether this is the final output

### No Duplicate Output

The capture loop diffs against the previous capture, so multiple clients streaming the same session each receive only new content — no duplicates.

## Webhooks & Polling Fallback

The orchestrator has two modes for receiving GitHub events:

1. **Webhook mode** (instant) — via `webhook.enabled: true` in config
2. **Polling mode** — via periodic `sync_tick()` (every 120s by default)

### Webhook Server Production Hardening

The webhook server includes production-grade features for reliability and observability:

#### Graceful Shutdown

The webhook server coordinates shutdown with the engine lifecycle:
- Listens for shutdown signals (SIGINT/SIGTERM) via broadcast channel
- Stops accepting new connections immediately
- Allows in-flight requests to complete naturally
- Logs clear shutdown messages for observability

#### Retry Queue with Exponential Backoff

Failed webhook deliveries are automatically retried:
- **In-memory queue** for failed events
- **Exponential backoff**: 5s, 10s, 20s, 40s, 80s, max 5 minutes
- **Max 5 retry attempts** before giving up
- Background task processes retries every 5 seconds

#### Health Monitoring

Enhanced health check endpoint (`/health`) reports:
- Server status (always returns healthy if running)
- Recent webhook received status (within 5 minutes)
- Signature verification configuration status

#### Idempotency

Webhooks are deduplicated using SHA256 payload hashing:
- Duplicate deliveries detected via payload hash comparison
- Prevents processing the same event multiple times
- Tracked in `webhook_deliveries` database table

#### Webhook Delivery Tracking (Database)

All webhook deliveries are persisted for observability:

```sql
CREATE TABLE webhook_deliveries (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    delivery_id     TEXT UNIQUE,           -- GitHub's delivery GUID
    event_type      TEXT NOT NULL,
    action          TEXT,                  -- e.g., "opened", "created"
    payload_hash    TEXT NOT NULL,         -- SHA256 for idempotency
    repo            TEXT NOT NULL,
    processed_at    TEXT,
    error           TEXT,
    retry_count     INTEGER DEFAULT 0,
    created_at      TEXT
);
```

Access via `Db` methods:
- `record_webhook_delivery()` - Record a delivery attempt
- `is_webhook_processed()` - Check idempotency
- `get_webhook_stats_24h()` - Get 24h delivery statistics

### Polling Fallback

When webhooks are enabled but the local server becomes unavailable (e.g. port conflict, crash), the orchestrator automatically switches to polling fallback mode. When webhooks are disabled entirely, polling mode is used from the start.

- **Health check**: Pings the local webhook server's `/health` endpoint every 60 seconds (configurable). This verifies the local HTTP listener is running — it does not verify GitHub-side reachability or webhook secret validity.
- **Faster polling**: When in fallback mode, sync operations run every 30 seconds (configurable) instead of 120s
- **Logging**: Clear log messages when entering/exiting fallback mode:
  - `entering polling fallback mode` — webhook health check failed
  - `exiting polling fallback mode` — webhook health restored

### Configuration

```yaml
webhook:
  enabled: true
  port: 8080
  secret: "${WEBHOOK_SECRET}"

engine:
  tick_interval: 10          # Main tick interval (seconds)
  sync_interval: 120        # Normal sync interval (seconds)
  fallback_sync_interval: 30 # Faster sync when webhooks fail
  webhook_health_check_interval: 60 # Health check frequency
```

### Features Affected

| Feature | Webhook | Polling Fallback |
|---------|---------|------------------|
| Issue creation | Instant | Next sync |
| @mention detection | Instant | Next sync |
| PR review comments | Instant | Next sync |
| Issue close/reopen | Instant | Next sync |
| PR merge events | Instant | Next sync |

## PR Review Integration

The orchestrator automatically creates follow-up tasks when PR reviews request changes, closing the feedback loop between code review and agent execution.

### How It Works

1. The engine periodically checks tasks in `in_review` status (every sync interval)
2. For each task with an open PR, it fetches PR reviews from GitHub
3. When a review requests changes (`CHANGES_REQUESTED`), it:
   - Creates an internal follow-up task for each actionable review comment
   - Links the follow-up to the parent task via `source_id`
   - Stores PR context (PR number, branch, reviewer, file path) in the sidecar
   - Routes the follow-up to the same agent that created the original PR
   - Posts a comment on the original issue about the follow-up task

### Configuration

```yaml
workflow:
  # Auto-create follow-up tasks when PR reviews request changes (default: true)
  auto_create_followup_on_changes: true
  # Auto-close task (mark Done) when all PR reviews are approved (default: false).
  # Note: this does NOT merge the PR -- only updates the task status.
  auto_close_task_on_approval: false
```

### Follow-up Task Content

Follow-up tasks include:
- File path and line number from the review comment
- Diff context (truncated if > 2000 chars)
- The reviewer's comment
- Overall review notes from the review body
- Link to the parent task

### Status Updates

- Parent task remains in `in_review` while follow-ups are addressed
- When a review is approved and `auto_close_task_on_approval` is enabled, the parent task is marked as `done`

## Complexity-based model routing

The router assigns `complexity: simple|medium|complex` instead of specific model names. The actual model is resolved per agent from `config.yml`:

```yaml
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

See `model_for_complexity()` in the router module.

## Router Module (Rust)

The agent router is implemented in `src/engine/router.rs`. It selects the best agent (claude/codex/opencode) and model for each task based on task content, labels, and configured routing rules.

### Router Configuration

```yaml
router:
  mode: "llm"              # "llm" (default) or "round_robin"
  agent: "claude"          # which LLM performs routing
  model: "haiku"           # fast/cheap model for classification
  timeout_seconds: 120     # routing timeout
  fallback_executor: "codex"  # fallback if routing fails
  max_route_attempts: 3    # after N LLM failures, fall back to round-robin
  agents:                  # agents to discover in PATH
    - claude
    - codex
    - opencode
    - kimi
    - minimax
  allowed_tools:           # default tools for agent profiles
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
  default_skills:          # skills always included
    - gh
    - git-worktree
```

### Routing Logic

The router follows this priority order:

1. **Label-based override**: If task has `agent:*` label (e.g., `agent:claude`), use that agent directly
2. **Round-robin mode**: If `router.mode` is `round_robin`, cycle through available agents by task ID
3. **LLM classification**: Call the configured router LLM with the routing prompt
4. **Parse response**: Extract executor, complexity, profile, and selected skills from JSON
5. **Fallback**: If LLM fails, use `router.fallback_executor`

### Label-Based Routing

Override the router by adding labels to tasks:

| Label | Effect |
|-------|--------|
| `agent:claude` | Force Claude executor |
| `agent:codex` | Force Codex executor |
| `agent:opencode` | Force OpenCode executor |
| `complexity:simple` | Use simple model tier |
| `complexity:medium` | Use medium model tier |
| `complexity:complex` | Use complex model tier |

### RouteResult Struct

Routing results are stored in the sidecar file (`~/.orchestrator/state/{task_id}.json`):

```rust
pub struct RouteResult {
    pub agent: String,           // "claude", "codex", or "opencode"
    pub model: Option<String>,   // e.g., "sonnet", "opus"
    pub complexity: String,      // "simple", "medium", "complex"
    pub reason: String,          // why this agent was selected
    pub profile: AgentProfile,   // role, skills, tools, constraints
    pub selected_skills: Vec<String>,
    pub warning: Option<String>, // routing sanity check warnings
}
```

### AgentProfile Struct

```rust
pub struct AgentProfile {
    pub role: String,           // e.g., "backend specialist"
    pub skills: Vec<String>,    // focus skills for this task
    pub tools: Vec<String>,     // tools allowed
    pub constraints: Vec<String>, // constraints for this task
}
```

### Environment Variables

The runner passes routing results to the agent invocation via:

- `ORCH_AGENT` — the selected agent (claude/codex/opencode)
- `ORCH_MODEL` — the specific model to use

### Routing Prompt

The routing prompt template is at `prompts/route.md`. It includes:
- Available executors
- Skills catalog
- Task details (ID, title, labels, body)
- Expected JSON output format

## Directory layout

```
~/.orch/
  tasks.yml              # task database (all projects, filtered by dir)
  config.yml             # global config
  jobs.yml               # scheduled jobs
  projects/              # bare clones added via `orch project add`
    owner/repo.git       #   each has .orch.yml inside
  worktrees/             # agent worktrees (all projects)
    repo/branch/         #   created by the runner, one per task
  state/                 # runtime state (logs, prompts, pid, locks)
```

- **User-managed projects** (e.g. `~/Projects/foo`): user clones, runs `orch init`. Project dir stays where the user put it.
- **Orch-managed projects** (`orch project add owner/repo`): bare clone at `~/.orch/projects/<owner>/<repo>.git`.
- **Worktrees**: always at `~/.orch/worktrees/<project>/<branch>/` regardless of project type.
- `ORCH_WORKTREES` env var overrides the worktrees base directory.

## Specs & Roadmap

See [specs.md](specs.md) for architecture overview, what's working, what's not, and improvement ideas.

## Release pipeline

1. Push to `main`
2. CI runs tests, auto-tags (semver from conventional commits)
3. GitHub release created, Homebrew tap formula updated automatically
4. `brew upgrade orch` picks up the new version
5. `orch service restart` to load new code

**Do NOT manually edit the tap formula** — the CI pipeline handles it. The `Formula/orch.rb` in this repo is a local reference copy, not the real tap.

### Post-push workflow

After pushing to main, always complete the full cycle:

```bash
git push                                    # 1. push
gh run watch --exit-status                  # 2. watch CI (tests → release → deploy)
brew update && brew upgrade orch            # 3. pull new formula + install
brew services restart orch                  # 4. restart service with new code
orch version                                # 5. verify
```

Do not skip steps — the service runs from the Homebrew cellar, not the repo.

## Task status semantics

- **`blocked`** — waiting on a dependency (parent blocked on children, missing worktree/dir)
- **`needs_review`** — requires human attention (max attempts, review rejection, agent failures, retry loops, timeouts)
- `mark_needs_review()` sets `needs_review`, NOT `blocked`
- Only parent tasks waiting on children should be `blocked`
- Engine auto-unblocks parent tasks when all children are done (Phase 4 of tick)

## Preferred tools

- Use `rg` instead of `grep` — faster, installed as a brew dependency
- Use `fd` instead of `find` — faster, installed as a brew dependency
- Use `trash` instead of `rm` — recoverable, enforced in system prompt

## Agent sandbox

Agents run in worktrees, NOT the main project directory. Orch enforces this:

1. **Prompt-level**: system prompt tells agents the main project dir is read-only
2. **Tool-level**: dynamic `--disallowedTools` blocks Read/Write/Edit/Bash targeting the main project dir
3. Config: `workflow.sandbox: false` to disable (not recommended)

## Codex sandbox config

Codex runs with `--full-auto` + network access enabled by default. Configurable:

```yaml
# In config.yml or .orchestrator.yml
agents:
  codex:
    sandbox: full-auto  # full-auto | workspace-write | danger-full-access | none
```

Or per-run: `CODEX_SANDBOX=danger-full-access orch task run 5`

Modes:
- `full-auto` (default) — filesystem sandboxed, network enabled
- `workspace-write` — same sandbox, explicit mode
- `danger-full-access` — no sandbox (for tasks needing bun, solana-test-validator, etc.)
- `none` — bypasses all Codex sandboxing (orchestrator is the sandbox)

## Landing the Plane (Session Completion)

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **PUSH TO REMOTE** - This is MANDATORY:
   ```bash
   git pull --rebase
   git push
   git status  # MUST show "up to date with origin"
   ```
5. **Clean up** - Clear stashes, prune remote branches
6. **Verify** - All changes committed AND pushed
7. **Hand off** - Provide context for next session

**CRITICAL RULES:**
- Work is NOT complete until `git push` succeeds
- NEVER stop before pushing - that leaves work stranded locally
- NEVER say "ready to push when you are" - YOU must push
- If push fails, resolve and retry until it succeeds
