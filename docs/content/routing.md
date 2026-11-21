+++
title = "Routing"
description = "How orch picks an agent for each task"
weight = 4
+++

Orch uses an LLM-as-classifier to route each task to the best agent. This is a non-agentic call (`--print`) — fast and cheap, no tool access needed.

## How It Works

1. The Rust router (`src/engine/router/`) sends the task title, body, and labels to a lightweight LLM (default: `claude --model haiku --print`).
2. The router LLM returns JSON:
   ```json
   {
     "executor": "claude",
     "complexity": "medium",
     "reason": "why this agent/model",
     "profile": {
       "role": "DeFi Protocol Engineer",
       "tools": ["Bash", "Edit", "Read"],
       "constraints": ["audit with sealevel attacks"]
     }
   }
   ```
3. Sanity checks run -- e.g. warns if a backend task gets routed to claude, or a docs task to codex. If the router fails repeatedly (configurable via `max_route_attempts`, default 3), it falls back to round-robin.
4. If the router fails, it falls back to `config.yml`'s `router.fallback_executor` (default: `codex`).

## Config

```yaml
router:
  mode: "llm"               # "llm" | "round_robin" | "local" | "profile", default round_robin
  agent: "claude"           # which LLM does the routing
  model: "haiku"            # fast/cheap model for classification
  timeout_seconds: 45       # values over 45 are lowered to 45
  max_route_attempts: 3     # LLM failures before falling back to round-robin
  max_tasks_per_tick: 1     # routing concurrency cap
  weighted_round_robin: false
  fallback_executor: "codex"
  allowed_tools:            # default tool allowlist
    - yq
    - jq
    - bash
```

To exclude an agent from routing entirely, remove it from the `agents:` discovery list (the router only considers installed, listed agents) — there is no separate `disabled_agents` knob.

## Available Executors

| Executor | Best for |
|----------|----------|
| `codex` | Coding, repo changes, automation, tooling |
| `claude` | Analysis, synthesis, planning, writing |
| `opencode` | Lightweight coding and quick iterations |
| `kimi` | Alternative coding agent |
| `minimax` | Alternative coding agent |
| `grok` | Coding agent (Grok CLI) |

Agents are auto-discovered from PATH. The router only picks from installed agents. A `grok` task also needs a `grok` model in the matching `model_map` tier. Take the id from `grok models`.

## Complexity-Based Model Selection

The router assigns a `complexity` level (`simple`, `medium`, `complex`) which maps to agent-specific models:

```yaml
model_map:
  simple:
    claude: "haiku"
    codex: "gpt-5.1-codex-mini"
  medium:
    claude: "sonnet"
    codex: "gpt-5.2"
  complex:
    claude: "opus"
    codex: "gpt-5.3-codex"
  review:
    claude: "sonnet"
    codex: "gpt-5.2"
```

The routing prompt is in `prompts/route.md`. The router only classifies — it never touches code or files.

When a task's stored model is no longer in its agent's model pools, orch replaces it with the agent's model for the task's complexity and records the reason in `last_error`.

## Named Profiles

`router.mode: profile` picks the agent, model and reasoning effort from a named entry in a top-level `profiles:` map, the way `model_map` maps complexity tiers. Two profiles can use the same CLI with different models or effort:

```yaml
router:
  mode: profile
profiles:
  designer:
    agent: codex
    model: gpt-5.3-codex
    effort: high
  implementer:
    agent: codex
    model: gpt-5.2
    effort: medium
```

Label an issue `profile:designer` or `profile:implementer`. Profile names are case-sensitive and `agent:` keeps its executor meaning. Other router modes ignore `profile:` labels.

- A task without a `profile:` label routes as in `round_robin` mode.
- More than one profile label, an unknown name, a malformed `profiles:` map, an empty model or effort, an agent missing from `agents:`, or a CLI that is not installed is a routing error. The task stays `new` and is retried on the next tick.
- The dependency hold runs first, so a task blocked by an open issue waits before any profile is read.
- The effort reaches the CLI as `-c model_reasoning_effort=...` for Codex, `--effort` for Claude and `--variant` for OpenCode (it takes precedence over a `provider/model@variant` suffix). Other agents accept profiles without `effort`. The CLI itself checks that it supports the model and effort.
- The task then goes through the normal route, dispatch and review flow. The choice is stored with the route and survives a restart. If the profile's agent or model is in cooldown, the task routes as in `round_robin` mode. After failover changes the agent or model, the stored failover choice wins and the profile's effort is dropped. Reviews use the usual review agent selection.

A profile changes only the agent, model and effort; it adds no role prompt, so the issue body carries the role. For a design then implementation handoff, open two issues and mark the implementation issue as blocked by the design issue.

## Pre-emptive Routability

The router performs a pre-emptive routability check before attempting to invoke agents or the router LLM. This does two things:

- It excludes agents that are currently in cooldown or have been marked degraded by the health check (rate-limit signals). These checks are authoritative — if an agent is degraded, it will be skipped regardless of weight.
- When `router.weighted_round_robin` is enabled, an additional weight-based threshold is applied after the degraded check. Agents whose routing weight has decayed below this threshold are skipped proactively to avoid sending work to recently rate-limited agents. This weight-based exclusion only layers on top of existing cooldown/degraded logic and does not replace it.

The threshold is fixed at `0.3`. No config key changes it.
