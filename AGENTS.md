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

## Discord Gateway (Websocket)

The Discord channel uses Discord's Gateway websocket API (`wss://gateway.discord.gg`) instead of HTTP polling. This delivers MESSAGE_CREATE events in real-time with sub-second latency and avoids repeated API requests.

### How It Works

1. **Connect** — Opens a persistent websocket connection to the Gateway URL
2. **Hello** — Server sends `heartbeat_interval` (typically 41.25 s)
3. **Identify** — Client sends bot token, intents, and shard info
4. **Ready** — Server responds with `session_id` and `resume_gateway_url`
5. **Events** — `MESSAGE_CREATE` dispatches arrive in real-time
6. **Heartbeat** — Client sends `op=1` every `heartbeat_interval` ms; server ACKs with `op=11`
7. **Reconnect** — On disconnect or server-requested reconnect, the client resumes with `session_id + seq`; falls back to re-identify on invalid session

### Required Bot Token Scopes

The bot token must have these **Gateway Intents** enabled in the [Discord Developer Portal](https://discord.com/developers/applications):

| Intent | Bit | Required for |
|--------|-----|-------------|
| `GUILDS` | `1 << 0` | Receiving guild context |
| `GUILD_MESSAGES` | `1 << 9` | MESSAGE_CREATE events in guilds |
| `MESSAGE_CONTENT` | `1 << 15` | Reading message text (privileged) |

> **Note:** `MESSAGE_CONTENT` is a privileged intent. Enable it in the bot's settings under *Privileged Gateway Intents* in the Discord Developer Portal.

### Configuration

```yaml
channels:
  discord:
    bot_token: "${DISCORD_BOT_TOKEN}"   # Required: bot token from Discord Developer Portal
    channel_id: "1234567890123456789"   # Optional: restrict to a single channel ID
    shard_id: 0                          # Optional: shard index (default: 0)
    shard_count: 1                       # Optional: total shards (default: 1)
```

Sharding is only needed for bots in 2 500+ guilds. For most deployments, the defaults (`shard_id: 0, shard_count: 1`) are correct.

### Reconnect & Backoff

- On disconnect, the gateway reconnects using the `resume_gateway_url` from READY + `session_id` + last sequence number
- If the session is invalid (op=9 with `resumable=false`), the client re-identifies after a 5 s delay
- Backoff starts at 1 s and doubles on each failed connect attempt, capping at 60 s

## Webhooks & Polling Fallback

The orchestrator has two modes for receiving GitHub events:

1. **Webhook mode** (instant) — via `webhook.enabled: true` in config
2. **Polling mode** — via periodic `sync_tick()` (every 120s by default)

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

The orchestrator re-routes tasks when PR reviews request changes, closing the feedback loop between code review and agent execution.

### How It Works

1. The engine periodically checks tasks in `in_review` status (every sync interval)
2. For each task with an open PR, it fetches PR reviews from GitHub
3. When a review requests changes (`CHANGES_REQUESTED`), it:
   - Stores the review feedback in `pr_review_context` field
   - Increments the `review_cycles` counter
   - Posts a review comment on the PR with the feedback
   - Re-routes the task back to `New` status for re-dispatch
   - The agent reuses the existing worktree/branch and pushes fixes to the same PR
4. If `review_cycles >= max_review_cycles`, the task is blocked for human review

### Configuration

```yaml
workflow:
  # Max review cycles before escalating to human (default: 2)
  max_review_cycles: 2
  # Auto-close task (mark Done) when all PR reviews are approved (default: false).
  # Note: this does NOT merge the PR -- only updates the task status.
  auto_close_task_on_approval: false
```

### Status Updates

- Task is re-routed (`New`) when review requests changes — same branch/PR is reused
- Task is blocked when max review cycles exceeded — requires human intervention
- When a review is approved and `auto_close_task_on_approval` is enabled, the task is marked as `done`

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

Routing results are stored in the sidecar file (`~/.orch/state/{task_id}.json`):

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
  config.yml             # global config
  orch.db                # SQLite database (tasks, metrics, KV store, rate limits)
  projects/              # bare clones added via `orch project add`
    owner/repo.git       #   each has .orch.yml inside
  worktrees/             # agent worktrees (all projects)
    repo/branch/         #   created by the runner, one per task
  state/                 # runtime state (logs, prompts, pid, locks, sidecar JSON)
  skills/                # cloned skill repositories
```

- **User-managed projects** (e.g. `~/Projects/foo`): user clones, runs `orch init`. Project dir stays where the user put it.
- **Orch-managed projects** (`orch project add owner/repo`): bare clone at `~/.orch/projects/<owner>/<repo>.git`.
- **Worktrees**: always at `~/.orch/worktrees/<project>/<branch>/` regardless of project type.
- `ORCH_WORKTREES` env var overrides the worktrees base directory.

## Specs & Roadmap

See [specs.md](specs.md) for architecture overview, what's working, what's not, and improvement ideas.

## Required checks before every commit

Always run these before committing — CI enforces them and will fail otherwise:

```bash
cargo fmt -- --check                    # formatting
cargo clippy --all-targets -- -D warnings # lints (warnings are errors, incl. test code)
cargo nextest run                       # unit tests (faster, matches CI)
```

Or all at once:

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo nextest run
```

**Installing nextest locally:** CI uses prebuilt binaries via `taiki-e/install-action`. For local use,
install with `cargo binstall cargo-nextest` (requires [cargo-binstall](https://github.com/cargo-bins/cargo-binstall)),
or fall back to `cargo test` if nextest is unavailable.

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

## DO NOT TOUCH — Settled architecture decisions

These areas have been deliberately designed and must not be changed without an explicit human decision. Do not file issues, refactor, or "improve" them.

### GitHub token resolution (`src/github/token.rs`)

The auth flow is: `GH_TOKEN` env → `GITHUB_TOKEN` env → `gh.auth.token` config → `gh auth token` CLI.

- `try_gh_command()` uses `std::process::Command` (blocking). **This is intentional.** The result is cached for 1 hour — this runs at most once at startup. It is not a Tokio antipattern in this context.
- `TokenResolver` is a process-wide singleton via `shared()`. Do not add a second resolver, re-export the token, or set env vars.
- Do not add `spawn_blocking`, `tokio::process::Command`, or `orch auth check` wiring here.
- Issues #418 and #421 were filed by agents making this exact mistake and were closed as invalid.

**If you see `std::process::Command` in `try_gh_command` — leave it alone.**

### PTY runner (`src/engine/runner/`)

The agent runner uses tmux as the PTY (tmux already provides a PTY to whatever runs inside it). There is no `portable_pty` crate and no external PTY process. Do not reintroduce PTY-based runners that spawn the agent outside tmux and forward output via `send-keys`. Issue #416 explains why this was removed.

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
# In config.yml or .orch.yml
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
