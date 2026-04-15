# Orch — Agent & Developer Notes

You are an autonomous orch agent. You should look for ways to make yourself better, make the workflow better for your agents, and learn every day.

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

## Cooldowns and unblocking

```bash
orch cooldown list                  # list all active cooldowns
orch cooldown clear <key>           # clear a cooldown (e.g., "claude", "kimi:opus")
orch cooldown clear --all           # clear all active cooldowns
orch task unblock all               # unblock blocked tasks
```

## Manual issue closure cleanup

**IMPORTANT:** When you manually close a GitHub issue, agents may still be working on it — tmux sessions and worktrees must be cleaned up or they'll interfere with future work.

### Stale sessions to check for

When closing issue `#NNNN`, look for:
- **Agent sessions:** `orch-orch-NNNN-*` (e.g., `orch-orch-1455-review`)
- **Chat sessions:** `orch-chat-*` (leftover from debugging)
- **Review sessions:** `orch-orch-*-review`

### Cleanup steps

```bash
# 1. List all orch tmux sessions
tmux list-sessions | grep orch

# 2. Kill the specific session(s)
tmux kill-session -t orch-orch-NNNN-review
tmux kill-session -t orch-chat-default

# 3. Remove associated worktrees
ls ~/.orch/worktrees/orch/ | grep -i "NNNN\|issue"
rm -rf ~/.orch/worktrees/orch/gh-issue-NNNN-*
```

**Do not leave stale sessions/worktrees** — they consume resources and can interfere with new work on the same issue.

## Logs

- Service log: `~/.orch/state/orch.log`
- Brew stdout: `/opt/homebrew/var/log/orch.log` (startup messages only)
- Brew stderr: `/opt/homebrew/var/log/orch.error.log`
- If you find any errors on `/opt/homebrew/var/log/orch.error.log`, first run `ls -lh /opt/homebrew/var/log/orch.error.log` to check the file modification date. This file is **truncated on every service restart**, so any content in it is from the current run only. DO NOT refile issues based on this file if its mtime predates the current session or is more than a few minutes old.

## Live Session Streaming

Orch can stream live output from running agent sessions. This allows you to watch agent work in real-time from the terminal.

### Streaming via CLI

```bash
orch stream              # stream ALL running sessions (auto-discovers new ones)
orch stream <task_id>    # stream a single task
```

Without arguments, `orch stream` discovers all `orch-*` tmux sessions and merges their output with `[repo-taskid]` prefixes. New sessions that start while streaming are picked up automatically (re-discovery every 3 seconds).

Single-task mode connects to one task's tmux session and prints output as it arrives. The stream updates every 2 seconds with new content from the agent's pane.

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

Orch has two modes for receiving GitHub events:

1. **Webhook mode** (instant) — via `webhook.enabled: true` in config
2. **Polling mode** — via periodic `sync_tick()` (every 45s by default)

### Polling Fallback

When webhooks are enabled but the local server becomes unavailable (e.g. port conflict, crash), orch automatically switches to polling fallback mode. When webhooks are disabled entirely, polling mode is used from the start.

- **Health check**: Pings the local webhook server's `/health` endpoint every 60 seconds (configurable). This verifies the local HTTP listener is running — it does not verify GitHub-side reachability or webhook secret validity.
- **Faster polling**: When in fallback mode, sync operations run every 30 seconds (configurable) instead of 45s
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
  sync_interval: 45         # Normal sync interval (seconds)
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

Orch re-routes tasks when PR reviews request changes, closing the feedback loop between code review and agent execution.

### How It Works

1. The engine periodically checks tasks in `in_review` status (every sync interval)
2. For each task with an open PR, it fetches PR reviews from GitHub
3. When a review requests changes (`CHANGES_REQUESTED`), it:
   - Stores the review feedback in `pr_review_context` field
   - Increments the `review_cycles` counter
   - Posts a review comment on the PR with the feedback
   - Re-routes the task back to `Routed` status for re-dispatch (skips LLM re-classification, reuses existing agent/model)
   - The agent reuses the existing worktree/branch and commits fixes (engine pushes to the same PR)
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

- Task is re-routed (`Routed`) when review requests changes — same branch/PR is reused
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
  timeout_seconds: 60     # routing timeout
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

Routing results are stored in the unified SQLite task store (`~/.orch/orch.db`):

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
  state/                 # runtime state (logs, prompts, per-task artifacts)
    control/{pid}/       #   control session temp files (per-process isolation)
  skills/                # cloned skill repositories
```

- **User-managed projects** (e.g. `~/Projects/foo`): user clones, runs `orch init`. Project dir stays where the user put it.
- **Orch-managed projects** (`orch project add owner/repo`): bare clone at `~/.orch/projects/<owner>/<repo>.git`.
- **Worktrees**: always at `~/.orch/worktrees/<project>/<branch>/` regardless of project type.
- `ORCH_WORKTREES` env var overrides the worktrees base directory.

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

## Graceful Shutdown

On SIGTERM (e.g. `orch service restart` or `brew services restart orch`), the engine:

1. Resets all `in_progress` tasks → `routed` so they re-dispatch into existing worktrees on next start
2. Resets all `in_review` tasks → `needs_review` so review agents are re-spawned on next start (review agent tmux sessions are killed when the process exits)
3. Kills all `orch-*` tmux sessions to release resources
4. Waits up to `engine.graceful_shutdown_timeout` seconds (default: 600) for in-flight work to settle

No work is lost — tasks resume from their worktrees on restart.

## Task status semantics

- **`new`** — just created, awaiting routing
- **`routed`** — agent and model selected, awaiting dispatch
- **`in_progress`** — agent is working on the task in a tmux session
- **`needs_review`** — agent completed, awaiting review agent dispatch (automatic, not human)
- **`in_review`** — review agent is actively reviewing the PR
- **`done`** — PR merged, worktree cleaned up
- **`blocked`** — requires human attention (max review cycles, max attempts, agent failures, unrecoverable errors)
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

### Config files are off-limits

Agents must NEVER modify `~/.orch/config.yml`, `config.example.yml`, or any `.orch.yml` project config. These files control the running service and model routing — changes have immediate, global impact.

If a config change is needed, describe the required change in the issue body or PR description. The human operator will apply it. Do not write config changes directly, do not include config file edits in PRs, and do not suggest "just update config.yml" as a fix.

Issues #1261 and #1172 were caused by agents modifying config files without permission.

### Cooldown and failure recovery is generic — do not special-case models or agents

The cooldown system (`src/engine/cooldown.rs`) and failure recovery (`src/engine/sync.rs`) are designed to handle **all** agent and model failures generically.

All cooldowns are **persisted to SQLite KV** (`cooldown:{key}`) so they survive service restarts. Failure counts are stored separately (`failure_count:{key}`) to drive exponential backoff.

- Agent failures → exponential backoff starting at 5 min, capping at 4h → router picks a different agent
- Model failures → exponential backoff starting at 5 min, capping at 4h → router picks a different model
- Rate limits with "try again at" → cooldown set to that vendor-specified timestamp (always authoritative)
- Silence detection → model cooldown + short 120s agent cooldown → re-route
- Credit exhaustion (`out_of_credits`) → exponential from 1h, capping at 8h
- Org-level disabling (`org_level_disabled`) → exponential from 2h, capping at 8h
- Billing cycle exhaustion → escalating from 24h, capping at 7 days (monthly event; flat 24h caused daily retry-fail cycles)
- On successful completion → failure counts reset via `record_agent_success()` so next failure starts from base again

**Exception for pre-emptive routability checks**: The router performs proactive checks to skip agents/models that are likely to fail based on routing weight decay and cooldown states. This is not considered special-casing because it uses the same generic cooldown system and weight decay mechanisms that feed into the routing decision process.

**Do not file issues to add special handling for specific models or agents** (e.g., "copilot models need longer cooldowns", "add fallback for kimi rate limits"). If a model silently fails, the existing silence detection + cooldown handles it. If a model is rate-limited, `parse_retry_at` handles it. If all models for an agent are cooled, the router picks a different agent.

Issue #1286 was closed as invalid — it proposed special-casing copilot model failures when the generic system already handles them.

If you believe the generic system has a bug (e.g., cooldowns not being applied, silence not detected), file an issue about the **generic mechanism**, not about a specific model.

**The exponential backoff approach is settled — do not replace it with flat cooldowns or per-agent special cases.** The formula `min(base * 3^(attempt-1), max)` is in `compute_backoff()`. Failure counts are persisted in `failure_count:{key}` KV keys. Incremental improvements (e.g. tuning constants, adding decay windows, improving reset logic) are welcome as proposals, but the base mechanism must not be reverted.

#### Pre-emptive Routability and Circuit-Breaker Behavior

The router now implements two key optimizations to prevent unnecessary agent invocations:

1. **Pre-emptive routability check**: Before attempting to route a task, the router evaluates each agent/model combination's routing weight. If the weight has decayed below a configurable threshold (indicating poor recent performance), that combination is skipped entirely.

2. **Circuit-breaker behavior**: When specific failure events occur (`out_of_credits` or `org_level_disabled`), the router applies extended cooldown periods (typically 2-4x longer than standard cooldowns) to prevent repeated futile attempts.

These mechanisms integrate with the generic cooldown system as follows:
- Routing weight decay is tracked in the same system that handles failure recovery
- Extended cooldowns for special events are still managed by `src/engine/cooldown.rs`
- The router queries the cooldown system to determine if an agent/model is available
- All cooldown state remains the single source of truth in the SQLite store

**Monitoring and alerting guidance**:
- Monitor the percentage of agent/model combinations that are skipped due to low routing weight
- Set up alerts when 3+ agents show degraded routing weight (weight < threshold) simultaneously
- Track the frequency and duration of extended cooldowns for `out_of_credits` and `org_level_disabled` events
- Review router logs for messages indicating skipped agents due to weight decay or circuit-breaker activation

**Actual integration points** (in `src/engine/router/mod.rs`):
- `router.refresh_health(&store)` — called each tick; delegates to `cooldown::refresh_degraded_agents()` which queries the `rate_limits` table and updates the in-memory degraded set
- `router.agent_is_routable(agent, complexity)` — guards all routing paths; returns `false` when `cooldown::is_agent_in_cooldown(agent)` or `cooldown::is_agent_degraded(agent)` is true; additionally skips agents whose `AgentWeights::get_weight` has fallen below `router.skip_limited_threshold` when `weighted_round_robin` is enabled
- `router.available_agents_for_complexity(complexity)` — filters `available_agents` through `agent_is_routable`; used by round-robin, weighted, and LLM routing paths
- `cooldown::record_credit_exhaustion(agent, reason)` — applies exponential agent-wide cooldown (1h→8h for `out_of_credits`, 2h→8h for `org_level_disabled`, 24h→7d for `billing_cycle_exhausted`)
- `cooldown::record_agent_success(agent, model)` — called by the runner on success; resets `failure_count:*` KV keys so the next failure restarts backoff from the base duration
- `weights.get_weight(agent)` (in `AgentWeights`) — used by weighted-round-robin; decays on each `record_rate_limit` call and recovers toward 1.0 over time
- `router.skip_limited_threshold` (`RouterConfig`) — weight threshold below which an agent is considered too degraded for proactive routing; default `0.3`; only evaluated when `weighted_round_robin` is enabled

These checks happen before LLM-based routing, preventing unnecessary invocation attempts when success is unlikely.

### Migrations are immutable — NEVER modify existing migration files

Once a migration file has been applied to any database, its checksum is locked by SQLx. Modifying an existing migration file changes the checksum, causing `sqlx::migrate!()` to fail on every startup — breaking the service completely.

**Always create a NEW migration file** (e.g., `014_new_column.sql`) instead of editing an existing one. Issue `d02cdda` modified `012_auto_unblock.sql` and broke the service for 70+ minutes.

### No external endpoints

Orch is an internal tool running on a local machine with no external network access. There are no publicly reachable HTTP/webhook endpoints. The webhook receiver in the config exists but only works when the machine happens to be reachable (rare). GitHub polling fallback is the default mode.

External consumers (CLI, local debugging tools) connect via **localhost-only websocket** (`127.0.0.1`). Do not add externally-reachable servers, do not assume inbound connections from GitHub or other services will work, and do not design features that depend on webhook delivery.

### Routing concurrency is controlled by `max_tasks_per_tick` + `llm_budget_secs` — no semaphore

LLM routing concurrency is governed by exactly two knobs:

- **`router.max_tasks_per_tick`** (default: 1) — caps how many tasks enter the routing loop per tick. With the default of 1, only one LLM classification call can ever be in flight at a time.
- **`router.llm_budget_secs`** (default: `timeout_seconds`, 45s) — total wall-clock budget for the entire pool cascade before falling back to round-robin. Prevents N pool entries × 45s/each from blocking the tick.

**Do not add a semaphore, worker pool, or `ORCH_ROUTER_MAX_PARALLEL_LLMS` env var.** Issue #2676 / PR #2677 introduced an `llm_semaphore` field on `Router` that was redundant with `max_tasks_per_tick=1` at its default value and added no protection that the budget timeout doesn't already provide. It was removed as dead complexity.

If routing is too slow: tune `router.llm_budget_secs` (lower it to fall back to round-robin faster) or reduce `router.max_tasks_per_tick`. Do not add a third concurrency mechanism.

### Security leak detection — ALL patterns block GitHub posting

`has_leaks()` in `src/security.rs` checks **all** `LEAK_PATTERNS` regardless of the `high_confidence` flag. Nothing is posted to GitHub if any pattern matches — low-confidence patterns included.

**Do not change `has_leaks()` to filter by `high_confidence`.** Issue #2645 / PR #2648 made this mistake: it changed `has_leaks()` to skip low-confidence patterns to reduce "false positive redactions", but the correct policy is to err on the side of caution — if anything looks like a secret, don't post it. A false positive (overly cautious) is far preferable to a false negative (leaking credentials).

The `high_confidence` flag exists only to let `scan()` / `redact()` distinguish severity for display purposes — it has no effect on whether posting is blocked.

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
- `none` — bypasses all Codex sandboxing (orch is the sandbox)

## Control Session (`orch chat`)

An interactive conversational control plane. Talk to orch in natural language — ask about running tasks, create new ones, check status, unblock things.

### How It Works

Uses one-shot agent invocations with SQLite-backed continuity:

```
1. Each named session stores conversation history, summaries, memories, and a session UUID in SQLite
2. Every message assembles fresh context from that stored state
3. The agent runs one-shot via `bash -c` with a timeout, not inside tmux
4. Claude-compatible agents receive the stored session UUID for conversation continuity
5. Changing `/model` or `/agent` resets the stored session UUID so the next message starts fresh with the new selection

Storage: context assembled from SQLite (live state + memories)
```

All data persisted to `control_messages` table (history + summary + tokens + cost).

### CLI Usage

```bash
orch chat                           # interactive REPL
orch chat "what's running?"         # single message
orch chat --session ops             # use a named session profile
orch chat history                   # show recent messages
orch chat history --search "bean"   # search past conversations
```

### Model Selection

```bash
/model sonnet                       # infer agent (claude)
/model minimax:sonnet               # explicit agent:model
/model opencode:minimax-m2.5-free   # opencode with specific model
/agent codex                        # switch agent and its default model
/agent                              # show current agent:model
/model                              # show current agent:model
```

Model selection validates before saving:
1. Agent must be in `DEFAULT_AGENTS` (from `router/config.rs`)
2. Agent binary must exist in PATH (via `cmd_cache::command_exists`)
3. For opencode: pre-checks against `opencode models` list
4. **Always**: test invocation to verify model actually works (catches rate limits, missing API keys, expired credits)

### Multi-Session Support

Sessions isolate conversation history and memories. Default session is `"default"`.

```bash
orch chat --session ops             # ops profile
orch chat --session dev             # dev profile
```

Each session has its own:
- Message history in `control_messages` table (filtered by `session_id`)
- Memories in KV store (keys: `control:memory:{session_id}:*`)
- Model/agent selection is global (shared across sessions)

### Storage

All in `~/.orch/orch.db`:

- `control_messages` table — full conversation history with session_id, role, content, summary, model, agent, tokens, cost
- `kv` table — model state (`control:model`, `control:agent`) and memories (`control:memory:{session}:*`)

### Architecture

- `src/control.rs` — context assembly, agent invocation, response parsing
- `src/cli/chat.rs` — CLI handlers (REPL, single-message, history)
- `prompts/control_system.md` — system prompt template with `{current_state}`, `{memories}`, `{recent_summaries}` placeholders
- Reuses runner infrastructure: `get_runner()`, `build_command()`, `parse_response()`, `classify_error()` from `src/engine/runner/agents/`
- Agent invocations run via `bash -c` with timeout (45s), not tmux

### Configuration

```yaml
# No config needed — works with defaults (claude:sonnet)
# Model/agent stored in KV, changed via /model or /agent commands
```

## Landing the Plane (Session Completion)

> **Task agents dispatched by orch:** If `ORCH_AGENT` or `TASK_ID` env vars are set, SKIP this section entirely. The engine handles pushing, PR creation, and cleanup. You should only commit — never push.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **Commit your changes**
5. **Clean up** - Clear stashes, prune remote branches
6. **Verify** - All changes committed
7. **Hand off** - Provide context for next session
