+++
title = "Configuration"
description = "Config reference and per-project overrides"
weight = 7
+++

All runtime configuration lives in `~/.orchestrator/config.yml`.

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
| `model_map` | `simple/medium/complex` | Agent-specific model names per complexity level | `{}` |

## Per-Project Config

Drop a `.orchestrator.yml` in your project root to override global config:

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
- `gh_project_apply.sh` writes to global config, not the project overlay

## Skills

Skills extend agent capabilities with specialized knowledge:

```yaml
# ~/.orchestrator/skills.yml
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
