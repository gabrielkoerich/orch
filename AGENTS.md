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

```bash
orch stream              # stream ALL running sessions (auto-discovers new ones)
orch stream <task_id>    # stream a single task
```

Implementation: `src/channels/capture.rs` — diffs tmux pane output every 2 s and broadcasts chunks to subscribers (CLI, Telegram, Discord). Diffing prevents duplicates across multiple clients on the same session.

## Discord Gateway (Websocket)

The Discord channel uses the Discord Gateway websocket (`wss://gateway.discord.gg`) for real-time `MESSAGE_CREATE` events. Implementation: `src/channels/discord/`.

Bot token must have `GUILDS`, `GUILD_MESSAGES`, and the privileged `MESSAGE_CONTENT` intent (enable under *Privileged Gateway Intents* in the Discord Developer Portal).

```yaml
channels:
  discord:
    bot_token: "${DISCORD_BOT_TOKEN}"
    channel_id: "..."          # optional: restrict to a single channel
    shard_id: 0                # only needed for bots in 2 500+ guilds
    shard_count: 1
```

Reconnect uses `resume_gateway_url` + `session_id` + last seq; falls back to re-identify on invalid session. Backoff: 1 s → 60 s.

## Webhooks & Polling Fallback

Two modes for receiving GitHub events: webhook (instant) and polling (`sync_tick()` every 45 s). If the local webhook server's `/health` fails, orch auto-switches to faster polling (30 s) and logs `entering polling fallback mode`; recovery logs `exiting polling fallback mode`.

```yaml
webhook:
  enabled: true
  port: 8080
  secret: "${WEBHOOK_SECRET}"

engine:
  tick_interval: 10
  sync_interval: 45
  fallback_sync_interval: 30
  webhook_health_check_interval: 60
```

See also `## DO NOT TOUCH → No external endpoints`. In practice polling is the default — webhooks rarely work without a public endpoint.

## PR Review Integration

When a PR review on an `in_review` task returns `CHANGES_REQUESTED`, the engine stores feedback in `pr_review_context`, increments `review_cycles`, re-routes the task back to `Routed` (reuses existing agent/model/worktree/branch — skips LLM re-classification), and the agent commits fixes to the same PR.

Task becomes `blocked` when `review_cycles >= max_review_cycles`, and `done` only when the PR is actually merged.

```yaml
workflow:
  max_review_cycles: 2
```

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

Implementation: `src/engine/router/` (selects agent + model per task). Prompt: `prompts/route.md`. Structs: `RouteResult`, `AgentProfile` in source.

Priority order: label override → round-robin mode → LLM classification → `router.fallback_executor`. Configured under the `router:` key in `~/.orch/config.yml`.

### Label-Based Routing

| Label | Effect |
|-------|--------|
| `agent:claude` \| `agent:codex` \| `agent:opencode` | Force that executor |
| `complexity:simple` \| `medium` \| `complex` | Use that model tier |

### Environment Variables

The runner passes routing results to the agent via `ORCH_AGENT` (selected agent) and `ORCH_MODEL` (specific model).

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
orch -V                                      # 5. verify CLI version
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
- Billing cycle exhaustion → when the failing model is known: persistent model-level cooldown (4h base → 7d cap); when no model is identified: escalating agent-wide cooldown from 24h, capping at 7 days. Model-scoped exhaustion (e.g. a provider sub-model like `github-copilot/gpt-5-mini`) must not block unrelated models on the same agent.
- On successful completion → failure counts reset via `record_agent_success()` so next failure starts from base again

**Exception for pre-emptive routability checks**: The router performs proactive checks to skip agents/models that are likely to fail based on routing weight decay and cooldown states. This is not considered special-casing because it uses the same generic cooldown system and weight decay mechanisms that feed into the routing decision process.

**Do not file issues to add special handling for specific models or agents** (e.g., "copilot models need longer cooldowns", "add fallback for kimi rate limits"). If a model silently fails, the existing silence detection + cooldown handles it. If a model is rate-limited, `parse_retry_at` handles it. If all models for an agent are cooled, the router picks a different agent.

Issue #1286 was closed as invalid — it proposed special-casing copilot model failures when the generic system already handles them.

If you believe the generic system has a bug (e.g., cooldowns not being applied, silence not detected), file an issue about the **generic mechanism**, not about a specific model.

**The exponential backoff approach is settled — do not replace it with flat cooldowns or per-agent special cases.** The formula `min(base * 3^(attempt-1), max)` is in `compute_backoff()`. Failure counts are persisted in `failure_count:{key}` KV keys. Incremental improvements (e.g. tuning constants, adding decay windows, improving reset logic) are welcome as proposals, but the base mechanism must not be reverted.

#### NEVER hardcode model names or aliases in router code

There must be **zero literal model-identifier strings** in the routing/dispatch code paths (e.g. `"github-copilot/gpt-5.3"`, `"gpt-5.3-codex"`, `"opencode/..."`). No `match` arms keyed on specific models, no "known unavailable" allow/deny lists, no canonicalization tables that rewrite alias A → alias B, no per-model `if agent == "opencode" && model == "X"` branches anywhere in `src/engine/router/`, `src/engine/runner/`, or `src/engine/cooldown.rs`.

When a model fails (including `Model not found` / `ProviderModelNotFoundError` / "model unavailable"), the **only** correct response is:

1. The runner classifies the error as `AgentError::ModelUnavailable`.
2. `cooldown::record_persistent_model_failure(agent, model)` puts that **specific model** into a long cooldown (4h base → 7d max).
3. The router's next selection skips that model via `is_model_in_cooldown()` and picks the next entry in the pool — or fails over to a different agent if none remain.
4. The agent itself is not penalized — its other models still route normally.

That's it. There is no list to update, no alias to add, no `is_known_unavailable_model()` to extend. If the cooldown is being cleared too early, fix the cooldown duration. If the wrong error variant is returned, fix the parser in the agent runner. If the model keeps coming back from config, that's a **config** problem (`~/.orch/config.yml` is human-managed — see "Config files are off-limits").

Cautionary tales — these are exactly the mistake to never repeat:

- **PR #3020** (`fix(router): canonicalize dead opencode copilot model alias`) added `canonicalize_model_alias()` and `is_known_unavailable_model()` with hardcoded `"github-copilot/gpt-5.3"` → `"gpt-5.3-codex"` rewrites in `src/engine/router/config.rs`. Wrong on two counts: (a) the rewrite produced an identifier the target CLI also rejected, and (b) there should never have been a hardcoded model string in code in the first place.
- **PR #3040** (`fix(router): filter dead github-copilot/gpt-5.3 alias …`) tried to "fix" #3020 by reordering the same hardcoded match arms. It compounded the mistake instead of removing it.
- **Issue #3051** (`gpt-5.3-codex not filtered for opencode agent — is_known_unavailable_model only covers …`) proposed adding **another** model name to the same hardcoded list. Closed as invalid for the same reason.

Both PRs were reverted. If you are tempted to add a model name to a `match` or a `Vec<&str>` of "bad models", **stop**: the generic per-model cooldown already handles it the moment the dispatch fails, and any list you write rots the next time a model is renamed/added/removed. Do not file issues like #3051; do not write code like #3020.

If `record_persistent_model_failure` is somehow not firing for your scenario, file an issue about the **classifier** (why didn't `AgentError::ModelUnavailable` come back?) or about the **cooldown duration** (why is the model being retried too soon?) — never about adding a specific model name to a filter.

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

### Routing concurrency is controlled by `max_tasks_per_tick` — no semaphore, no wall-clock budget

LLM routing concurrency is governed by exactly one knob:

- **`router.max_tasks_per_tick`** (default: 1) — caps how many tasks enter the routing loop per tick. With the default of 1, only one LLM classification call can ever be in flight at a time.

Per-call timeouts are already enforced by `router.timeout_seconds`. If a route call hangs longer than that, it errors and the cascade moves on. There is no separate wall-clock "budget" wrapped around the cascade.

**Do not add a semaphore, worker pool, `ORCH_ROUTER_MAX_PARALLEL_LLMS` env var, or `llm_budget_secs` field.** Issue #2676 / PR #2677 introduced an `llm_semaphore` field on `Router` that was redundant with `max_tasks_per_tick=1` and added no protection that the per-call timeout doesn't already provide. It was removed as dead complexity. A subsequent `llm_budget_secs` wall-clock wrapper was removed for the same reason (see "No 'budget' features" below).

If routing is too slow: lower `router.timeout_seconds`, or reduce `router.max_tasks_per_tick`. Do not add a third concurrency mechanism.

### No per-task routing defer — `AllAgentsCooledError` must retry next tick, not write a timer

There must be **zero per-task deferral state** anywhere in the routing path. No `route_defer_until:*` KV keys, no `route_defer_key()` / `compute_route_defer_secs()` / `route_defer_remaining_secs()` / `set_route_defer_until()` / `clear_route_defer()` helpers, no "back off this task for N seconds because all agents are cooled," no per-task timer of any other name (`route_backoff_until`, `next_route_attempt_at`, `cooled_skip_until`, etc.).

When `router.route()` returns `AllAgentsCooledError`, the **only** correct response in `tick_route_tasks` is:

1. `tracing::error!(...)` — loud, every tick, can't be hidden.
2. `continue` — task stays in `new`.
3. Next tick (~10s later), the routing loop tries again.

The agent-level cooldown system in `src/engine/cooldown.rs` is already the single source of truth for "is this agent/model available right now." `available_agents_for_complexity()` filters cooled agents via `is_agent_in_cooldown` / `is_model_in_cooldown` / `is_agent_degraded` *before* any LLM call, so when everything is cooled, `router.route()` returns `AllAgentsCooledError` in microseconds — no tokens, no network, no LLM. There is nothing to "defer" away from.

**Why a parallel timer is wrong**: the defer key was a wall-clock value (`now + remaining_secs / 2`) written into SQLite. It counted down independently of the real cooldowns. Long cooldowns (billing-cycle / out_of_credits at 24h → 7d) produced per-task defers of 12h → 3.5d. When agents became available again — minutes or hours before the defer expired — the task remained stranded in `new` because the unrelated future timestamp hadn't passed. There was no mechanism to recompute the defer when cooldowns shortened or cleared.

Cautionary tale — exactly the mistake never to repeat:

- **PR #3243** removed `route_defer_*` after a live incident in `gabrielkoerich/bean`: `orch cooldown list` reported `No active cooldowns.` while 9 `route_defer_until:*` KV keys held tasks in `new` for hours. Two `gabrielkoerich/orch` tasks (issue #3236 and an internal self-improvement task) hit the same pattern. The defer was guarding against a cost that doesn't exist — `AllAgentsCooledError` returns before any LLM call — and in exchange introduced a timer that drifted out of sync with reality.

If you are tempted to add a "back off this task" timer because the ERROR log is noisy: **stop**. The log volume is the feature. It tells the operator that all agents are unavailable *right now*, every 10s, until the situation resolves. Suppressing it with a per-task timer is how the bug came back. If a specific operational need genuinely requires throttling routing retries: tune `engine.tick_interval`, lower `router.max_tasks_per_tick`, or fix the upstream cooldown duration — never write a parallel timer.

**Do not file issues like "add backoff for AllAgentsCooled", "rate-limit the route_attempts log spam", "remember the last-cooled time per task", or "skip routing for N seconds after an all-cooled event."** They will be closed as invalid.

### No 'budget' features — agents run on subscription / fixed pricing

There must be **zero "budget" tracking, accounting, or enforcement** anywhere in orch. No per-task token budgets, no per-PR budget warnings, no routing wall-clock budgets, no `llm_budget_secs` / `llm_bypass_*` knobs, no `budget_warning` / `budget_exceeded` columns, no `check_token_budget()` pre-run or post-run, no `TokenBudgetExceeded` failure category, no `BudgetCheckOutcome` enum, no "budget exhausted" cooldown reason.

**Why**: all agents orch dispatches to (claude, codex, opencode, kimi, minimax, copilot, …) run on **subscription or fixed-price plans**. There is no per-token billing for orch to throttle against. Counting tokens to "save money" produces no real signal and just adds heavy bookkeeping (extra SQLite columns, extra runner branches, extra PR comments, extra failure categories) that breaks routinely and confuses every subsequent change to the runner / router / store.

The correct mechanisms for limiting work already exist and must be used instead:

- **Hung / silent agents** → silence detection + agent cooldown (`src/engine/cooldown.rs`)
- **Slow router calls** → `router.timeout_seconds` (per-call timeout)
- **Tick stalls** → watchdog (already wired)
- **Runaway retries** → `max_route_attempts`, `max_review_cycles`, per-task attempt counters
- **Rate limits / out of credits** → existing cooldown variants (`record_rate_limit`, `record_credit_exhaustion`, etc.)

Do **not**:

- Re-add `budget_warning` / `budget_exceeded` columns to `tasks` (migration `027_drop_budget_columns.sql` removed them — stay removed).
- Re-add `check_token_budget()` to `src/engine/runner/task_init.rs` or `src/engine/runner/response_handler.rs`.
- Re-add `TokenBudgetExceeded` to `FailureCategory` in `src/engine/sync.rs`.
- Re-add `BudgetCheckOutcome` or any "pre-run budget guard" branch in the runner.
- Re-add `llm_budget_secs`, `llm_bypass_fail_threshold`, `llm_bypass_window_secs`, `llm_bypass_duration_secs`, `llm_budget_fail_count`, `llm_budget_window_start`, `llm_bypass_until`, `record_llm_budget_timeout()`, `reset_llm_budget_counters()`, or `llm_bypass_active()` to the router.
- Re-add a `tokio::time::timeout(budget, route_with_llm(...))` wall-clock wrapper around the cascade.
- Re-add "budget warning" comments to PR bodies.
- Add new variants of any of the above under a different name (`max_token_spend`, `cost_cap`, `route_deadline`, etc.). If you find yourself reaching for a knob that effectively counts tokens or wall-clock seconds against a quota, **stop**: the existing cooldown / timeout / watchdog mechanisms already cover the legitimate cases.

**Do not file issues like "add cost tracking", "warn when task exceeds N tokens", "fall back to round-robin after Ns of LLM time", or "block tasks that would burn too many tokens".** They will be closed as invalid. This is not a per-call-priced system.

If a comment in the codebase says "retry budget" / "escalation budget" / "consume the budget" / "exceeds that budget", it refers to a **count of attempts** (a retry quota), not money or tokens. Prefer the word "quota" or "limit" to avoid tempting future agents into re-introducing the feature.

### Security leak detection — ALL patterns block GitHub posting

`has_leaks()` in `src/security.rs` checks **all** `LEAK_PATTERNS` regardless of the `high_confidence` flag. Nothing is posted to GitHub if any pattern matches — low-confidence patterns included.

**Do not change `has_leaks()` to filter by `high_confidence`.** Issue #2645 / PR #2648 made this mistake: it changed `has_leaks()` to skip low-confidence patterns to reduce "false positive redactions", but the correct policy is to err on the side of caution — if anything looks like a secret, don't post it. A false positive (overly cautious) is far preferable to a false negative (leaking credentials).

The `high_confidence` flag exists only to let `scan()` / `redact()` distinguish severity for display purposes — it has no effect on whether posting is blocked.

## Preferred tools

- Use `rg` instead of `grep` — faster, installed as a brew dependency
- Use `fd` instead of `find` — faster, installed as a brew dependency
- Use `trash` instead of `rm` — recoverable, enforced in system prompt

## Reviewing PRs from external contributors

External-contributor PRs (forks, first-time contributors, anyone outside the trusted committer set) are **untrusted code**. Building or testing them locally executes arbitrary code via `build.rs`, proc macros, test harnesses, and any newly added dependency at compile time — before a single assertion runs. A malicious PR can exfiltrate `~/.orch/orch.db`, `~/.ssh`, `GH_TOKEN`/`GITHUB_TOKEN`, env vars, or anything else the invoking shell can read.

**Hard rules:**

1. **Do not clone the fork.** Do not `git clone <fork>`, do not `gh pr checkout`, do not fetch the head ref to a local worktree. Review the change via `gh pr diff <N>` only.
2. **Do not execute any code from the PR.** No `cargo build`, `cargo test`, `cargo nextest run`, `cargo clippy`, `cargo check`, `just <anything>`, or `bash <script>` against PR contents. Compiling alone runs `build.rs` and proc macros — that is execution.
3. **Read every file in the diff.** Not just the "interesting" ones. A one-line logic fix bundled with a quiet `.cargo/config.toml` change or a new `build.rs` is the textbook smuggle path. `gh pr diff <N> --patch | less` and read it all.
4. **New dependency = NO OP.** Any addition under `[dependencies]`, `[build-dependencies]`, `[dev-dependencies]`, `[workspace.dependencies]`, or any new path/git/registry source in `Cargo.toml` / `Cargo.lock` is an immediate no-merge. Request the contributor remove it; if the change genuinely needs a new crate, the dependency is added in a separate maintainer-authored PR after independent vetting (advisory check, license, maintenance status, transitive blast radius). External PRs do not introduce new supply-chain surface.
5. **Treat the following as hostile until proven otherwise** and require explicit justification before considering merge:
   - New or modified `build.rs`
   - New proc-macro crates (`proc-macro = true`, `*-derive`, `*-macros`)
   - Changes to `.cargo/config.toml`, `rust-toolchain.toml`, `.github/workflows/`, `justfile`, `Makefile`, or any shell/Python script
   - Network or filesystem access in tests (`reqwest`, `std::fs::write`, `std::process::Command`, `env::var` reads of credentials)
   - Paths referencing `~/.orch`, `~/.ssh`, `~/.config`, `gh auth token`, `GH_TOKEN`, `GITHUB_TOKEN`, `ANTHROPIC_API_KEY`

**Use the `just review-pr <N>` recipe** for the standard workflow. It:

1. Runs `scripts/review/hooks-check.sh` against the PR diff and refuses to proceed if any tripwire fires (Cargo manifest, build.rs, agent/IDE hooks, CI workflows, shell scripts — full list in the script).
2. Resolves the fork URL + head ref via `gh pr view`.
3. Spins up two short-lived Docker containers from `scripts/review/Dockerfile.fetch` and `scripts/review/Dockerfile.run`:
   - **Stage A (fetch)** — network ON, no compilation. `git clone --depth=50 --branch <ref> <fork>` into a volume, then `cargo fetch --locked`. No `build.rs` runs at this point.
   - **Stage B (run)** — `--network=none`. `cargo nextest run --offline`. Untrusted code first executes here, with no network egress, no host bind mounts, no host credentials.
4. Cleans up the source + git-deps volumes on exit. The crates.io registry cache and `target/` dir persist across reviews for speed (wipe with `just review-pr-clean`).

`just hermetic-tests` runs the current worktree through the same network-off stage-B container — use this to catch tests that silently depend on network, a `tmux` binary on PATH, non-root file-mode semantics, etc.

**If `just review-pr` is not an option** (rare — most fixes can be reasoned about statically), there are two acceptable manual paths:

1. **Re-implement in a maintainer-authored branch.** Don't run the fork's commits — recreate the diff yourself, push to a branch on this repo, let CI run there. The fork's bytes never touch a trusted machine.
2. **Clone and execute inside Docker.** The clone itself happens inside the container — the fork's bytes never land on the host filesystem.
   - Start a fresh container from a clean base image (e.g. `rust:slim`).
   - **No bind mounts of host paths.** No `-v ~:/host`, no `-v $(pwd):/work`, no `-v ~/.ssh:...`, no `-v ~/.orch:...`. The container is empty when it starts.
   - **No host credentials.** No `-e GH_TOKEN`, no `-e GITHUB_TOKEN`, no `-e ANTHROPIC_API_KEY`, no `--env-file ~/.env`. If `gh pr diff` is needed inside, use a scoped throwaway token with read-only access to public repos, not the maintainer's real token.
   - **Network gated.** `--network=none` if possible. If the build genuinely needs network (Cargo registry fetch), use a fresh network namespace, and never the host network.
   - Clone the fork **inside** the container (`git clone <fork> /work && cd /work`), then `cargo build` / `cargo nextest run` there.
   - When done, `docker rm -f` the container. It is disposable — never reuse.

   Skeleton:
   ```bash
   docker run --rm -it \
     --network=none \
     --read-only --tmpfs /tmp --tmpfs /work \
     rust:slim bash
   # inside container:
   apk add --no-cache git || apt-get update && apt-get install -y git
   git clone https://github.com/<fork-owner>/<repo>.git /work && cd /work
   git checkout <pr-branch>
   cargo nextest run
   ```
   (Network must be re-enabled briefly for the `git clone` and Cargo fetch; the right pattern is to enable network for fetch, then drop it before `cargo build`. Or pre-vendor deps in a trusted base image and run the actual build offline.)
   - A fresh ephemeral cloud VM / Codespace works the same way: clone inside, run inside, destroy after. Do not reuse for credentialed work.
   - The container is the trust boundary. If the build/test exfiltrates from inside it, there is nothing to exfiltrate. If you find yourself thinking "I just need to mount `~/.orch` so the test can find the DB," you don't have a sandbox — you have a slower host shell.

Sandboxing is **not** a substitute for static review — read every file in the diff first, regardless of where you intend to run it. The sandbox limits blast radius; static review is what actually catches the bad change.

A clean `cargo nextest run` on an untrusted fork is **not** evidence the PR is safe — by the time tests finish, a malicious `build.rs` has already run. Static review is the safety boundary.

**CI does not run on external PRs by design.** All workflows in `.github/workflows/` trigger on `push:` only — no `pull_request:` trigger. PRs from forks do **not** auto-execute Actions, so a malicious `build.rs` cannot exfiltrate repo/org secrets via the CI runner. This is intentional and must stay that way. Do not add `pull_request:` (or worse, `pull_request_target:`, which runs the workflow definition from the base branch but checks out the PR head with full secrets access — the classic supply-chain attack vector) to any workflow. If CI verification is needed on a fork's branch, a maintainer pulls the change into a branch on this repo (where `push:` triggers it) after static review.

## Agent sandbox

Agents run in worktrees, NOT the main project directory. Orch enforces this:

1. **Prompt-level**: system prompt tells agents the main project dir is read-only
2. **Tool-level**: dynamic `--disallowedTools` blocks Read/Write/Edit/Bash targeting the main project dir
3. Config: `workflow.sandbox: false` to disable (not recommended)

## Codex sandbox config

Codex runs under `exec` with `--sandbox workspace-write -c 'approval_policy="never"'` and network access enabled by default. `--full-auto` is deprecated (Codex 0.128+) and must not be reintroduced.

The sandbox level comes from `workflow.permissions.sandbox` (`workspace-write` | `full-access`) and the approval mode from `workflow.permissions.mode` (`autonomous` | `supervised`). See `src/engine/runner/agents/codex.rs` for the exact flag combinations.

Network access is enabled via `-c 'sandbox_workspace_write.network_access=true'` and the shell environment is inherited via `-c 'shell_environment_policy.inherit=all'`. Both flags must precede `exec` — placing them after silently fails.

## Control Session (`orch chat`)

Interactive conversational control plane — natural-language interface to orch state. Implementation: `src/control.rs`, `src/cli/chat.rs`, prompt at `prompts/control_system.md`. Persistence in `orch.db` (`control_messages` table + `kv` keys `control:model`, `control:agent`, `control:memory:{session}:*`).

```bash
orch chat                           # interactive REPL
orch chat "what's running?"         # single message
orch chat --session ops             # named session (isolated history + memories)
orch chat history [--search QUERY]  # show / search past messages
```

Inside the REPL: `/model <name>` or `/model <agent>:<model>`, `/agent <name>`. Changing model/agent resets the stored session UUID so the next message starts fresh. Selection is validated by a test invocation before being saved (catches rate limits, missing API keys, expired credits). Sessions isolate history + memories; model/agent selection is global.

Agent invocations run one-shot via `bash -c` with a 45 s timeout, not in tmux.

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
