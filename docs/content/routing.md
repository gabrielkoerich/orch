+++
title = "Routing"
description = "How the orchestrator picks an agent for each task"
weight = 4
+++

The orchestrator uses an LLM-as-classifier to route each task to the best agent. This is a non-agentic call (`--print`) — fast and cheap, no tool access needed.

## How It Works

1. The Rust router (`src/engine/router/`) sends the task title, body, labels, and the skills catalog to a lightweight LLM (default: `claude --model haiku --print`).
2. The router LLM returns JSON:
   ```json
   {
     "executor": "claude",
     "complexity": "medium",
     "reason": "why this agent/model",
     "profile": {
       "role": "DeFi Protocol Engineer",
       "skills": ["Solana", "Anchor"],
       "tools": ["Bash", "Edit", "Read"],
       "constraints": ["audit with sealevel attacks"]
     },
     "selected_skills": ["solana-best-practices"]
   }
   ```
3. Sanity checks run -- e.g. warns if a backend task gets routed to claude, or a docs task to codex. If the router fails repeatedly (configurable via `max_route_attempts`, default 3), it falls back to round-robin.
4. If the router fails, it falls back to `config.yml`'s `router.fallback_executor` (default: `codex`).

## Config

```yaml
router:
  agent: "claude"           # which LLM does the routing
  model: "haiku"            # fast/cheap model for classification
  timeout_seconds: 120
  fallback_executor: "codex"  # safety net if routing fails
  disabled_agents:          # exclude agents from routing
    - opencode
  allowed_tools:            # default tool allowlist
    - yq
    - jq
    - bash
  default_skills:           # always included in routing
    - gh
    - git-worktree
```

## Available Executors

| Executor | Best for |
|----------|----------|
| `codex` | Coding, repo changes, automation, tooling |
| `claude` | Analysis, synthesis, planning, writing |
| `opencode` | Lightweight coding and quick iterations |
| `kimi` | Alternative coding agent |
| `minimax` | Alternative coding agent |

Agents are auto-discovered from PATH. The router only picks from installed agents.

## Complexity-Based Model Selection

The router assigns a `complexity` level (`simple`, `medium`, `complex`) which maps to agent-specific models:

```yaml
model_map:
  simple:
    claude: "haiku"
    codex: "gpt-4.1-mini"
  medium:
    claude: "sonnet"
    codex: "gpt-4.1"
  complex:
    claude: "opus"
    codex: "o3"
```

The routing prompt is in `prompts/route.md`. The router only classifies — it never touches code or files.
