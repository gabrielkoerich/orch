+++
title = "Configuration"
description = "Config reference and per-project overrides"
weight = 7
+++

All runtime configuration lives in `~/.orch/config.yml` (`ORCH_HOME`), unless overridden via the `ORCH_HOME` environment variable.

## Config Reference

| Section | Key | Description | Default |
|---------|-----|-------------|---------|
| top-level | `project_dir` | Override project directory (auto-detected from CWD) | `""` |
| `engine` | `tick_interval` | Main engine tick interval in seconds | `10` |
| `engine` | `sync_interval` | Sync tick interval in seconds (GitHub ingest, cleanup) | `45` |
| `engine` | `max_parallel` | Max tasks dispatched concurrently | `4` |
| `engine` | `stuck_timeout` | Timeout (seconds) for detecting stuck `in_progress` tasks | `1800` |
| `engine` | `no_session_stuck_timeout` | Timeout (seconds) for `in_progress` tasks with no tmux session | `600` |
| `engine` | `in_review_no_session_stuck_timeout` | Seconds an `in_review` task may have no review session before it goes back to `needs_review`. orch raises it to the review timeout plus 120 seconds when it is lower | `1920` |
| `engine` | `webhook_health_check_interval` | Health check frequency for the local webhook server | `60` |
| `engine` | `silence_grace_period` | Seconds of agent silence before warning | `300` |
| `engine` | `silence_cooldown` | Seconds a model detected as silent stays in cooldown | `3600` |
| `engine` | `graceful_shutdown_timeout` | Seconds to wait for in-flight tasks on SIGTERM | `600` |
| `engine` | `upgrade_check_interval` | How often to check for new orch versions and notify channels (seconds). Upgrading is operator-only — the service never runs `brew` itself | `3600` |
| `engine` | `startup_failure_escalation_secs` | Seconds of continuous startup failure before escalating to ERROR log + push notification (0 disables) | `3600` |
| `workflow` | `auto_close` | Auto-close GitHub issues when tasks are `done` | `true` |
| `workflow` | `enable_review_agent` | Run a [review agent](@/review-agent.md) after task completion | `true` |
| `workflow` | `max_attempts` | Max attempts before marking task as blocked | `5` |
| `workflow` | `max_review_cycles` | Max PR review cycles before escalating to human | `2` |
| `workflow` | `max_reroute_attempts` | Max times a task may be re-routed automatically. Falls back to `max_attempts` when unset | `3` |
| `workflow` | `timeout_seconds` | Task execution timeout in seconds. Values under 1800 are raised to 1800 | `1800` |
| `workflow` | `review_timeout_seconds` | Review agent timeout, separate from the task timeout, never under 300 | `900` |
| `workflow` | `timeout_by_complexity` | `medium` and `complex` overrides (e.g. `complex: 7200`), never under `timeout_seconds` | `{}` |
| `workflow` | `allowed_tools` | Tool patterns the agent may use (allowlist) | `[]` |
| `workflow` | `disallowed_tools` | Tool patterns blocked via `--disallowedTools` | `["Bash(rm *)","Bash(rm -*)"]` |
| `workflow` | `permissions.mode` | Agent permission mode (`autonomous` / `supervised`) | `autonomous` |
| `workflow` | `permissions.sandbox` | Sandbox level for Codex (`workspace-write` / `full-access`) | `workspace-write` |
| `workflow` | `sandbox` | Run task and review agents in the OS sandbox (`os`), or not (`false`). A project's `.orch.yml` can set `false` to opt out. See [OS Sandbox](agents.md#os-sandbox) | `false` |
| `workflow` | `worktree_janitor_ttl_hours` | Janitor TTL before pruning orphaned worktrees | `24` |
| `workflow` | `worktree_janitor_dry_run` | Log what the janitor would prune without removing anything | `false` |
| `workflow` | `auto_create_followup_on_changes` | Auto-create follow-up task when review requests changes | `true` |
| `workflow` | `ci_poll_max_wait_secs` | Max seconds the merge path waits for CI | `600` |
| `workflow` | `ci_poll_interval_secs` | Base CI poll interval in seconds | `15` |
| `workflow` | `ci_check_cooldown_secs` | Min seconds between CI checks of one task | `60` |
| `workflow` | `mergeability_poll_secs` | Seconds to wait for GitHub to compute mergeability | `10` |
| `router` | `mode` | `llm`, `round_robin`, or `local` (Ollama) | `round_robin` |
| `router` | `agent` | Default router executor | `claude` |
| `router` | `model` | Router model name | `haiku` |
| `router` | `timeout_seconds` | Router call timeout in seconds. Values over 45 are lowered to 45 | `45` |
| `router` | `max_route_attempts` | LLM failures before falling back to round-robin | `3` |
| `router` | `max_tasks_per_tick` | Max routing decisions per engine tick (concurrency cap) | `1` |
| `router` | `default_skills` | Skills selected for every task | `[gh, git-worktree]` |
| `router` | `fallback_executor` | Fallback executor when router fails | `codex` |
| `router` | `allowed_tools` | Default tool allowlist used in routing prompts | `[yq, jq, bash, ...]` |
| `router` | `weighted_round_robin` | Use degraded-weight-aware round robin | `false` |
| `router` | `pool` | Agents and models the router may pick from | (see config.example.yml) |
| `dispatch` | `health_check_window_hours` | Lookback window for counting rate-limit rows toward degraded | `6` |
| `dispatch` | `degraded_rate_limit_threshold` | Rate-limit rows in that window that mark an agent degraded | `3` |
| `dispatch` | `min_healthy_agents` | Below this many healthy agents, dispatch runs one task at a time | `2` |
| `dispatch` | `sequential_delay_ms` | Delay between dispatches in that sequential mode | `1000` |
| `webhook` | `enabled` | Start the local webhook receiver (HTTP) | `false` |
| `webhook` | `port` | Webhook listener port | `8080` |
| `webhook` | `secret` | HMAC secret for verifying GitHub webhook payloads | `""` |
| `relay` | `url` | orch-relay base URL. orch connects out to `<url>/ws` and wakes the sync on each GitHub App event. Unset means off | `""` |
| `relay` | `token` | Bearer token for the relay. Write `passbox:<name>` to read it once from passbox at startup, so the config holds no secret | `""` |
| `review` | `mode` | `comment` or `app`, see [Review mode](#review-mode). A project's `.orch.yml` overrides the global value | `comment` |
| `gh.app` | `id` / `private_key` | GitHub App id and its key, `private_key` takes a `passbox:<name>` reference, see [GitHub App](#github-app) | `""` |
| `gh` | `repo` | Default repo (`owner/repo`) | `"owner/repo"` |
| `gh` | `project_id` | GitHub Project v2 ID | `""` |
| `gh` | `project_status_field_id` | Status field ID in Project v2 | `""` |
| `gh` | `project_status_map` | Mapping for `backlog/in_progress/review/done` option IDs | `{}` |
| `gh.backoff` | `base_seconds` | Initial backoff duration in seconds | `30` |
| `gh.backoff` | `max_seconds` | Max backoff duration in seconds | `900` |
| `gh` | `auth.token` | Explicit Personal Access Token | `""` |
| `channels.discord` | `bot_token` | Discord bot token | `""` |
| `channels.telegram` | `bot_token` / `chat_id` | Telegram bot credentials | `""` |
| `channels.slack` | `bot_token` / `channel_id` | Slack bot credentials | `""` |
| `notifications` | `level` | Which status changes reach channels: `all` (done, in_review, approved, blocked, failed), `verbose` (every change), `errors_only` (blocked, failed) or `none` | `all` |
| `agents` | (list of strings) | Agent CLIs to discover in `$PATH` (e.g. `[claude, codex, opencode, grok]`) | `[]` |
| `model_map` | `simple/medium/complex/review` | Per-complexity model name per agent | `{}` |
| `git` | `name` / `email` | Commit identity for task and review agents | `<agent>[bot]` |
| `jobs` | `bash_timeout_seconds` | Timeout for a `bash` job | `300` |

> Authoritative reference: `config.example.yml` in the orch repo. Run `orch config <key>` to read the live value for any dotted key (e.g. `orch config engine.tick_interval`).

## Per-Project Config

Drop a `.orch.yml` (legacy `.orchestrator.yml` is still supported) in your project root to override global config (project-level keys take precedence over global).

```yaml
# ~/projects/my-app/.orch.yml
gh:
  repo: "myorg/my-app"
  project_id: "PVT_..."
workflow:
  enable_review_agent: true
router:
  fallback_executor: "claude"
```

- Project config is deep-merged with global config (project wins)
- The engine reloads the file when it changes, without a restart (see [Hot reload](#hot-reload))
- Use `orch project board sync` to (re-)discover project / status / estimate field IDs and persist them into the project's `.orch.yml`

## Hot reload

The engine watches `~/.orch/config.yml` and every project's `.orch.yml`. A change reloads the values in place, with no restart and no interruption to a running agent. Each changed key is logged with its old and new value, so `orch log` shows what an edit did.

`engine.max_parallel` resizes the dispatch pool live. Raising it hands out the extra slots at once. Lowering it never cancels a running task: orch stops handing out slots and takes the permits back as tasks finish, so the limit in force settles at the new value. The `dispatch pool resized` log line reports `in_force` and `target` while a shrink is still settling.

A file that does not parse is ignored: the last good values stay in force and one error is logged per breakage. Fix the file and the next save reloads normally.

A project added to `projects:` is picked up on the next save — its backend connects, its subscribers start and its issues are ingested on the following sync tick. A failed GitHub health check does not hold the project back: it starts anyway and the next sync tick retries, so a project added while GitHub is rate limited or unreachable still runs.

These settings still need `orch service restart`. Editing one logs `config changed, takes effect after restart` with the key, so the reload never looks like it applied. The value is not logged, because `channels.*` holds bot tokens.

| Setting | Why |
|---------|-----|
| `channels.*` | Telegram and Discord gateways hold long-lived websocket connections |
| `webhook.*` | The HTTP listener binds its port at startup |
| `relay.*` | The relay client reads its token and opens its websocket at startup |
| Removing a project from `projects:` | Its subscriber tasks and already-dispatched agents outlive the config read |

## Skills

Skills extend agent capabilities with specialized knowledge. Skill repositories are cloned into `~/.orch/skills/` by the engine on its sync tick — there is no dedicated `orch skills` subcommand.

```yaml
# ~/.orch/skills.yml
repositories:
  - name: gabrielkoerich
    url: https://github.com/gabrielkoerich/skills
    description: Personal skill catalog
    pin: f9a2062d2a184f9625746262a6b7656d3b630973  # Optional: pin to a specific commit
  - name: anthropics
    url: https://github.com/anthropics/skills
    description: Anthropic official skills
    pin: 1ed29a03dc852d30fa6ef2ca53a67dc2c2c2c563
skills: []   # Reserved for inline skill definitions (rarely used)
```

Skills in `router.default_skills` (default `gh`, `git-worktree`) are always selected. In `llm` mode the router adds more per task.

## GitHub Authentication

Orch resolves GitHub tokens in this order — the first match wins:

1. `GH_TOKEN` environment variable
2. `GITHUB_TOKEN` environment variable
3. `gh.auth.token` config value
4. `gh auth token` CLI

The simplest setup is just `gh auth login`. No extra config needed.

### Environment Variables

```bash
export GH_TOKEN="ghp_xxxxxxxxxxxxxxxxxxxx"
```

### Explicit token in config

```yaml
# ~/.orch/config.yml
gh:
  auth:
    token: "ghp_xxxxxxxxxxxxxxxxxxxx"
```

### GitHub App

With an App configured, orch makes its own REST calls to a repository with that repository's installation token, so they count against the App's quota. Agents keep the owner's `gh`.

orch looks up the installation per repository (`GET /repos/{repo}/installation`), since your account and each organization are separate installations. A repository the App is not installed on, GraphQL, `GET /user` and Projects V2 on a personal account use the owner token. Without `gh.app.id`, orch behaves as before.

```yaml
# ~/.orch/config.yml
gh:
  app:
    id: "123456"
    private_key: "passbox:orch/github-app-key"   # preferred, or
    private_key_env: "ORCH_APP_KEY"               # a variable that holds the PEM, or
    private_key_path: "/path/to/app.pem"          # an absolute path
```

Agents run as your user and can read any file you can, so keep the key in passbox. orch reads a `passbox:<name>` value with `passbox exec --stdin <name> -- /bin/cat` and keeps the first line of the output, so store the PEM on one line with `\n` for each newline, for example `awk '{printf "%s\\n", $0}' app.pem | passbox add orch/github-app-key`. orch reads the key once after it starts, in the background, and keeps it in memory for the life of the process. A passbox read waits on Touch ID, so until it succeeds orch uses the owner token and comment reviews, logs one warning and retries every 5 minutes with at most one prompt pending. Debug builds and tests never load the App.

orch signs an RS256 JWT, exchanges it for an installation token per installation, and refreshes each token 10 minutes before it expires. If a token is unavailable, orch logs a warning and uses the owner token.

### Review mode

`review.mode` picks how a project's pull requests are reviewed. The project's `.orch.yml` wins, then the global config, then the default `comment`.

```yaml
review:
  mode: app   # or comment
```

| Mode | Decision | Gate |
|------|----------|------|
| `comment` | a `## Automated Review` comment | the `orch/review` commit status |
| `app` | a real `APPROVE` or `REQUEST_CHANGES` review from the App, with each issue on a changed line as an inline comment | the `orch/review` check run from the App, `in_progress` while the review runs, then `success` or `failure` |

Both modes post the `## Automated Review` comment, and the gate reads it when the App has no review on the head. A review and the check run belong to the head sha the agent reviewed, so a new push makes the PR pending again. Issues on lines outside the diff stay in the review body, since GitHub refuses an inline comment there. A project in `app` mode whose repository does not have the App installed, or a daemon whose App key is not read yet, reviews in `comment` mode.

## Cluster

The `cluster` section runs orch on several machines. Without it orch runs on one machine. Keys, roles and setup are in [Cluster](@/cluster.md).
