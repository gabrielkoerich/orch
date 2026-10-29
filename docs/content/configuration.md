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
| top-level | `required_tools` | Tools that must exist on PATH before launching an agent | `[]` |
| `engine` | `tick_interval` | Main engine tick interval in seconds | `10` |
| `engine` | `sync_interval` | Sync tick interval in seconds (GitHub ingest, cleanup) | `45` |
| `engine` | `max_parallel` | Max tasks dispatched concurrently | `4` |
| `engine` | `stuck_timeout` | Timeout (seconds) for detecting stuck `in_progress` tasks | `1800` |
| `engine` | `no_session_stuck_timeout` | Timeout (seconds) for `in_progress` tasks with no tmux session | `600` |
| `engine` | `webhook_health_check_interval` | Health check frequency for the local webhook server | `60` |
| `engine` | `silence_grace_period` | Seconds of agent silence before warning | (see config.example.yml) |
| `engine` | `silence_cooldown` | Seconds of agent silence before marking model unavailable | (see config.example.yml) |
| `engine` | `graceful_shutdown_timeout` | Seconds to wait for in-flight tasks on SIGTERM | `600` |
| `engine` | `upgrade_check_interval` | How often to check for new orch versions and notify channels (seconds). Upgrading is operator-only — the service never runs `brew` itself | `3600` |
| `engine` | `startup_failure_escalation_secs` | Seconds of continuous startup failure before escalating to ERROR log + push notification (0 disables) | `3600` |
| `workflow` | `auto_close` | Auto-close GitHub issues when tasks are `done` | `true` |
| `workflow` | `enable_review_agent` | Run a [review agent](@/review-agent.md) after task completion | `false` |
| `workflow` | `max_attempts` | Max attempts before marking task as blocked | `10` |
| `workflow` | `max_review_cycles` | Max PR review cycles before escalating to human | `2` |
| `workflow` | `max_reroute_attempts` | Max times a task may be re-routed automatically | `3` |
| `workflow` | `timeout_seconds` | Task execution timeout (0 disables timeout) | `1800` |
| `workflow` | `timeout_by_complexity` | Per-complexity overrides (e.g. `complex: 7200`) | `{}` |
| `workflow` | `required_skills` | Skills always injected into agent prompts | `[]` |
| `workflow` | `allowed_tools` | Tool patterns the agent may use (allowlist) | `[]` |
| `workflow` | `disallowed_tools` | Tool patterns blocked via `--disallowedTools` | `["Bash(rm *)","Bash(rm -*)"]` |
| `workflow` | `permissions.mode` | Agent permission mode (`autonomous` / `supervised`) | `autonomous` |
| `workflow` | `permissions.sandbox` | Sandbox level for Codex (`workspace-write` / `full-access`) | `workspace-write` |
| `workflow` | `sandbox` | Run task and review agents in the OS sandbox (`os`), or not (`false`). A project's `.orch.yml` can set `false` to opt out. See [OS Sandbox](agents.md#os-sandbox) | `false` |
| `workflow` | `worktree_janitor_ttl_hours` | Janitor TTL before pruning orphaned worktrees | (see config.example.yml) |
| `workflow` | `auto_create_followup_on_changes` | Auto-create follow-up task when review requests changes | `false` |
| `router` | `mode` | `llm`, `round_robin`, or `local` (Ollama) | `llm` |
| `router` | `agent` | Default router executor | `claude` |
| `router` | `model` | Router model name | `haiku` |
| `router` | `timeout_seconds` | Router timeout (0 disables timeout) | `60` |
| `router` | `max_route_attempts` | LLM failures before falling back to round-robin | `3` |
| `router` | `max_tasks_per_tick` | Max routing decisions per engine tick (concurrency cap) | `1` |
| `router` | `fallback_executor` | Fallback executor when router fails | `codex` |
| `router` | `allowed_tools` | Default tool allowlist used in routing prompts | `[yq, jq, bash, ...]` |
| `router` | `weighted_round_robin` | Use degraded-weight-aware round robin | `false` |
| `router` | `pool` | Agents and models the router may pick from | (see config.example.yml) |
| `webhook` | `enabled` | Start the local webhook receiver (HTTP) | `false` |
| `webhook` | `port` | Webhook listener port | `8080` |
| `webhook` | `secret` | HMAC secret for verifying GitHub webhook payloads | `""` |
| `gh` | `repo` | Default repo (`owner/repo`) | `"owner/repo"` |
| `gh` | `project_id` | GitHub Project v2 ID | `""` |
| `gh` | `project_status_field_id` | Status field ID in Project v2 | `""` |
| `gh` | `project_status_map` | Mapping for `backlog/in_progress/review/done` option IDs | `{}` |
| `gh.backoff` | `base_seconds` | Initial backoff duration in seconds | `30` |
| `gh.backoff` | `max_seconds` | Max backoff duration in seconds | `900` |
| `gh` | `auth.token` | Explicit Personal Access Token | `""` |
| `channels.discord` | `bot_token` | Discord bot token | `""` |
| `channels.telegram` | `bot_token` / `chat_id` | Telegram bot credentials | `""` |
| `notifications` | `level` | Minimum notification level (`info` / `warn` / `error`) | `info` |
| `agents` | (list of strings) | Agent CLIs to discover in `$PATH` (e.g. `[claude, codex, opencode, grok]`) | `[]` |
| `model_map` | `simple/medium/complex/review` | Per-complexity model name per agent | `{}` |

> Authoritative reference: `config.example.yml` in the orch repo. Run `orch config <key>` to read the live value for any dotted key (e.g. `orch config engine.tick_interval`).

## Per-Project Config

Drop a `.orch.yml` (legacy `.orchestrator.yml` is still supported) in your project root to override global config (project-level keys take precedence over global).

```yaml
# ~/projects/my-app/.orch.yml
required_tools: ["bun"]
gh:
  repo: "myorg/my-app"
  project_id: "PVT_..."
workflow:
  enable_review_agent: true
  required_skills: []
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

Skills listed in `workflow.required_skills` are always injected into agent prompts. Other skills are selected per-task by the router.

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

With an App configured, orch makes its own REST calls to a repository with that repository's installation token, so they count against the App's quota. It also submits reviews as the App, because GitHub does not let an account approve its own pull request. Agents keep the owner's `gh`.

orch looks up the installation per repository (`GET /repos/{repo}/installation`), since your account and each organization are separate installations. A repository the App is not installed on, GraphQL, `GET /user` and Projects V2 on a personal account use the owner token. Without `gh.app.id`, orch behaves as before.

```yaml
# ~/.orch/config.yml
gh:
  app:
    id: "123456"
    private_key_env: "ORCH_APP_KEY"       # name of a variable that holds the PEM, or
    private_key_path: "/path/to/app.pem"  # absolute path
```

Agents run as your user and can read any file you can, so prefer `private_key_env` with the key injected at launch over a `.pem` file. orch signs an RS256 JWT, exchanges it for an installation token per installation, and refreshes each token 10 minutes before it expires. If a token is unavailable, orch logs a warning and uses the owner token.

The review gate counts an App review only when its `commit_id` is the current head. A review on an older sha does not count. The `## Automated Review` comment is still posted, and the gate reads it when the App has no review on the head.

## Cluster

The `cluster` section runs orch on several machines. Without it orch runs on one machine. Keys, roles and setup are in [Cluster](@/cluster.md).
