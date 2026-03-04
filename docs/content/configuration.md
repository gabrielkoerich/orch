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
| `workflow` | `auto_close` | Auto-close GitHub issues when tasks are `done` | `true` |
| `workflow` | `review_owner` | GitHub handle to tag when review is needed | `@owner` |
| `workflow` | `enable_review_agent` | Run a [review agent](@/review-agent.md) after task completion | `false` |
| `workflow` | `review_agent` | Fallback reviewer when opposite agent unavailable | `claude` |
| `workflow` | `max_attempts` | Max attempts before marking task as blocked | `10` |
| `workflow` | `stuck_timeout` | Timeout (seconds) for detecting stuck in_progress tasks | `1800` |
| `workflow` | `timeout_seconds` | Task execution timeout (0 disables timeout) | `1800` |
| `workflow` | `timeout_by_complexity` | Per-complexity task timeouts (takes precedence) | `{}` |
| `workflow` | `required_skills` | Skills always injected into agent prompts (marked `[REQUIRED]`) | `[]` |
| `workflow` | `disallowed_tools` | Tool patterns blocked via `--disallowedTools` | `["Bash(rm *)","Bash(rm -*)"]` |
| `router` | `agent` | Default router executor | `claude` |
| `router` | `model` | Router model name | `haiku` |
| `router` | `timeout_seconds` | Router timeout (0 disables timeout) | `120` |
| `router` | `disabled_agents` | Agents to exclude from routing (e.g. `[opencode]`) | `[]` |
| `router` | `fallback_executor` | Fallback executor when router fails | `codex` |
| `router` | `allowed_tools` | Default tool allowlist used in routing prompts | `[yq, jq, bash, ...]` |
| `router` | `default_skills` | Skills always included in routing | `[gh, git-worktree]` |
| `llm` | `input_format` | CLI input format override | `""` |
| `llm` | `output_format` | CLI output format override | `"json"` |
| `gh` | `enabled` | Enable GitHub sync | `true` |
| `gh` | `repo` | Default repo (`owner/repo`) | `"owner/repo"` |
| `gh` | `sync_label` | Only sync tasks/issues with this label (empty = all) | `"sync"` |
| `gh` | `project_id` | GitHub Project v2 ID | `""` |
| `gh` | `project_status_field_id` | Status field ID in Project v2 | `""` |
| `gh` | `project_status_names` | Mapping for `backlog/in_progress/review/done` status option names (used to resolve option IDs) | `{}` |
| `gh` | `project_status_map` | Mapping for `backlog/in_progress/review/done` option IDs | `{}` |
| `gh.backoff` | `mode` | Rate-limit behavior: `wait` or `skip` | `"wait"` |
| `gh.backoff` | `base_seconds` | Initial backoff duration in seconds | `30` |
| `gh.backoff` | `max_seconds` | Max backoff duration in seconds | `900` |
| `github` | `token_mode` | Token resolution mode: `env`, `github_app`, or `legacy` | `"env"` |
| `github` | `app_id` | GitHub App ID (for `token_mode: github_app`) | `""` |
| `github` | `private_key_path` | Path to GitHub App private key (.pem) | `""` |
| `github` | `allow_legacy_fallback` | Allow `gh auth token` fallback when primary mode fails | `false` |
| `model_map` | `simple/medium/complex` | Agent-specific model names per complexity level | `{}` |

## Authentication

The orchestrator supports three authentication methods for GitHub API access:

### Personal Access Token (PAT)

```yaml
gh:
  auth:
    mode: token
    token: "ghp_xxxxxxxxxxxxxxxxxxxx"  # Or use GH_TOKEN/GITHUB_TOKEN env var
```

Create tokens at [GitHub Settings → Developer settings → Personal access tokens](https://github.com/settings/tokens).

### GitHub App

Recommended for organization automation with better audit trails:

```yaml
gh:
  auth:
    mode: github_app
    app_id: "123456"
    private_key: "/path/to/app-private-key.pem"
    # Optional: specific installation ID (auto-detected if not set)
    # installation_id: "78901234"
```

The orchestrator automatically:
- Generates JWTs from your App credentials
- Exchanges JWTs for installation access tokens
- Refreshes tokens before they expire (valid for 1 hour)

### gh CLI (Legacy)

```yaml
gh:
  auth:
    mode: gh_cli
```

Requires `gh auth login` to be run interactively. Not recommended for service environments — prefer `GH_TOKEN`/`GITHUB_TOKEN` or GitHub App credentials and run `orch auth check`.

## Per-Project Config

Drop a `.orch.yml` or `.orchestrator.yml` in your project root to override global config (project-level keys take precedence over global).

```yaml
# ~/projects/my-app/.orchestrator.yml
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
- The server restarts automatically when `.orchestrator.yml` changes
- `gh_project_apply.sh` / `orch project info --fix` writes project IDs into the global config overlay when run from the server context

## Skills

Skills extend agent capabilities with specialized knowledge:

```yaml
# ~/.orch/skills.yml (or ~/.orchestrator/skills.yml for backwards compatibility)
repositories:
  - url: "https://github.com/user/skills-repo"
    commit: "abc123"
catalog:
  - id: "solana-best-practices"
    name: "Solana Best Practices"
    description: "Reviews Solana/Anchor programs for development best practices"
```

```bash
orch skills sync    # clone/update skill repositories
orch skills list    # show available skills
```

Skills listed in `workflow.required_skills` are always injected into agent prompts. Other skills are selected per-task by the router.

## GitHub Authentication

Orch uses a centralized [`TokenResolver`](https://github.com/gabrielkoerich/orch/blob/main/src/github/token.rs) for GitHub API authentication. This provides secure, flexible token resolution without spawning subprocesses during per-task operations.

### Token Resolution Modes

| Mode | Description | Configuration |
|------|-------------|---------------|
| `env` (default) | Read from `GH_TOKEN` → `GITHUB_TOKEN` environment variables | `github.token_mode: env` |
| `github_app` | Generate JWT from GitHub App credentials | `github.token_mode: github_app` |
| `legacy` | Use `gh auth token` CLI command | `github.token_mode: legacy` |

### Environment Variables (Default)

The default mode reads tokens from environment variables (checked in order):

```bash
export GH_TOKEN="ghp_xxx"          # Preferred
export GITHUB_TOKEN="ghp_xxx"      # Fallback
```

### GitHub App Authentication

For GitHub App authentication, configure the App ID and private key path:

```yaml
# ~/.orch/config.yml
github:
  token_mode: github_app
  app_id: "123456"
  private_key_path: "/path/to/app.pem"
```

The TokenResolver automatically generates a JWT from the private key (valid for 9 minutes) and caches it until expiration.

### Legacy Fallback

To allow the legacy `gh auth token` CLI fallback when the primary mode fails:

```yaml
# ~/.orch/config.yml
github:
  token_mode: env
  allow_legacy_fallback: true
```

This is useful when `gh` CLI is available but environment variables are not set.

### Agent Session Tokens

When spawning agents in tmux sessions, tokens are injected via the tmux session environment (`tmux set-environment`) rather than being embedded in runner scripts. This prevents token leakage to disk and enables centralized token rotation without restarting agents.
