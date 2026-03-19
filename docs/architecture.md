# Orch Architecture

> Communicate with your agents from anywhere — Discord, Telegram, GitHub, or direct tmux attach.

## System Overview

```mermaid
graph TB
    subgraph "Channels (all equal)"
        GH_CH["GitHub App<br/>(webhooks + polling)"]
        TG_CH["Telegram Bot<br/>(forum topics)"]
        DC_CH["Discord Bot<br/>(gateway ws)"]
        TMUX_CH["tmux Bridge<br/>(capture/send-keys)"]
        CLI_CH["CLI<br/>(orch commands)"]
    end

    subgraph "orch (Rust binary, Tokio)"
        TRANSPORT["Transport Layer<br/>routes messages ↔ sessions<br/>broadcasts output"]

        subgraph "Engine"
            TASKS["Task Manager<br/>internal + GitHub"]
            ROUTER["Router<br/>LLM agent selection"]
            SCHEDULER["Cron Scheduler<br/>minute-precision"]
            RUNNER["Task Runner<br/>spawn + monitor"]
        end

        subgraph "Runner Modules"
            CTX["context.rs<br/>prompt building"]
            WT["worktree.rs<br/>git worktree mgmt"]
            AGT["agent.rs<br/>agent invocation"]
            RESP["response_handler.rs<br/>push, PR, status"]
            GITOPS["git_ops.rs<br/>commit, push, PR"]
        end

        CH_ROUTER["Channel Router<br/>project ↔ topic/channel"]
        CONFIG_RS["Config<br/>in-memory, hot-reload"]
        SQLITE["SQLite<br/>tasks, metrics, state"]
    end

    subgraph "Agent CLIs"
        CL["claude"]
        CO["codex"]
        OC["opencode"]
        KI["kimi"]
    end

    subgraph "tmux Sessions"
        T1["orch-repo-42 (claude)"]
        T2["orch-repo-43 (codex)"]
        T3["orch-repo-42-review (kimi)"]
    end

    subgraph "Data"
        GH_ISSUES["GitHub Issues<br/>(external tasks)"]
        LOCAL_DB["SQLite orch.db<br/>(unified task store)"]
        YAML[".orch.yml<br/>+ config.yml"]
    end

    GH_CH --> TRANSPORT
    TG_CH --> TRANSPORT
    DC_CH --> TRANSPORT
    TMUX_CH --> TRANSPORT
    CLI_CH --> TASKS

    TRANSPORT --> CH_ROUTER
    CH_ROUTER --> TASKS
    TRANSPORT --> T1
    TRANSPORT --> T2

    TASKS --> ROUTER
    TASKS --> SCHEDULER
    TASKS --> RUNNER

    RUNNER --> CTX
    RUNNER --> WT
    RUNNER --> AGT
    RUNNER --> RESP
    RESP --> GITOPS
    AGT --> T1
    AGT --> T2
    AGT --> T3

    T1 --> CL
    T2 --> CO
    T3 --> KI

    SQLITE --> LOCAL_DB
    CONFIG_RS --> YAML
    TASKS --> GH_ISSUES
```

## Process Flow Per Tick

```mermaid
sequenceDiagram
    participant S as engine (serve)
    participant P as tick.rs
    participant J as jobs.rs
    participant R as runner
    participant GH as reqwest → GitHub API
    participant T as tmux
    participant A as Agent (claude/codex)

    loop Every 10 seconds
        S->>P: tick()
        P->>P: check session completions
        P->>P: recover stuck tasks
        P->>P: route new tasks (LLM)
        P->>R: dispatch routed tasks

        R->>R: setup worktree
        R->>T: tmux new-session -d -s orch-{project}-{id}
        T->>A: agent runs (commits changes)
        loop Every 5 seconds
            R->>T: tmux has-session?
        end
        A-->>T: JSON response
        R->>R: parse response
        R->>R: git push (orch pushes, not agent)
        R->>GH: create PR + link issue
        R->>P: status → needs_review

        S->>J: jobs tick (cron match)
    end

    Note over S,GH: Every 120s: sync tick (ingest issues, review PRs, owner commands)
```

## Task Lifecycle

```
new → routed → in_progress → needs_review → in_review → done
                    │              ▲              │
                    │              │              ├── approve + merge → done
                    │              │              ├── request_changes → routed → ... → needs_review
                    │              │              ├── conflicts/CI/merge fail → needs_review (retry)
                    │              │              └── 3x retry limit → blocked
                    │              └──────────────┘
                    │
                    ├── push failed (1-2x) → routed (different agent)
                    └── push failed (3x) → blocked
```

### Status Semantics

- `new` — task created, waiting for routing
- `routed` — agent and complexity assigned, waiting for dispatch
- `in_progress` — agent running in tmux session
- `needs_review` — PR ready, queued for review agent
- `in_review` — review agent actively working
- `done` — task complete (PR merged or no code changes)
- `blocked` — requires human intervention

### Status Transitions

| Transition | Owner | Location | Trigger |
|---|---|---|---|
| new → routed | engine tick | `engine/tick.rs` | LLM router assigns agent + complexity |
| routed → in_progress | engine dispatch | `engine/tick.rs` | Agent spawned in tmux |
| in_progress → needs_review | runner | `runner/response_handler.rs` | Agent completes + PR exists |
| in_progress → done | runner | `runner/response_handler.rs` | Agent completes, no PR created |
| in_progress → routed | runner | `runner/response_handler.rs` | Push failed (reroute to different agent) |
| in_progress → blocked | runner | `runner/response_handler.rs` | Push failed 3+ times |
| needs_review → in_review | engine tick | `engine/tick.rs` | Review agent spawned |
| in_review → done | engine | `engine/review.rs` | PR approved + merged |
| in_review → routed | engine | `engine/review.rs` | Changes requested → re-dispatch |
| in_review → needs_review | engine | `engine/review.rs` | Review agent failed/crashed |
| in_review → blocked | engine | `engine/review.rs` | Max review cycles, merge failures |

## Responsibility Split

### Agents (commit only)
- Write code, run tests, fix issues
- Commit changes with conventional commit messages
- Report status via JSON output
- Do NOT push, create PRs, or call GitHub write APIs

### Orch Engine (git operations + lifecycle)
- Push branches after agent completes
- Create PRs and link to issues
- Check CI status
- Trigger review agent
- Merge PRs after approval
- Cleanup worktrees and branches
- Route notifications to channels

## Live Session Streaming

```
┌─────────────┐     ┌─────────────┐     ┌─────────────┐
│  Telegram    │     │  Discord    │     │  CLI        │
│  topic #42   │     │  #orch-dev  │     │  orch task  │
│              │     │             │     │  stream 42  │
└──────┬───────┘     └──────┬──────┘     └──────┬──────┘
       │                    │                    │
       ▼                    ▼                    ▼
┌─────────────────────────────────────────────────────┐
│                    Transport                         │
│                                                      │
│  capture-pane (every 2s) ──→ diff ──→ broadcast      │
│  user input ──→ send-keys ──→ tmux session           │
└──────────────────────┬───────────────────────────────┘
                       │
                       ▼
              ┌─────────────────┐
              │  tmux:orch-*-42 │
              │  (claude agent) │
              └─────────────────┘
```

- **Watch** — capture-pane diffs stream to all connected channels in real-time
- **Join** — messages from any channel route to tmux via send-keys
- **Multi-viewer** — multiple channels watch the same session simultaneously

## Channel Routing

```mermaid
graph LR
    subgraph "Telegram Supergroup"
        TG_GEN["General topic"]
        TG_ORCH["#orch topic"]
        TG_BEAN["#bean topic"]
    end

    subgraph "Discord Server"
        DC_GEN["#general"]
        DC_ORCH["#orch"]
        DC_BEAN["#bean"]
    end

    subgraph "Channel Router"
        CR["ChannelRouter<br/>(topic/channel → repo)"]
    end

    TG_ORCH -->|"topic_id: 42"| CR
    TG_BEAN -->|"topic_id: 87"| CR
    TG_GEN -->|"general"| CR
    DC_ORCH -->|"channel_id: 1111"| CR
    DC_BEAN -->|"channel_id: 2222"| CR
    DC_GEN -->|"general"| CR

    CR -->|"owner/orch"| E1["Engine (orch)"]
    CR -->|"owner/bean"| E2["Engine (bean)"]
    CR -->|"project picker"| PICKER["Interactive Picker"]
```

- Dedicated channels route messages to their project automatically
- General channel shows a project picker for task creation
- Notifications go to dedicated channels + subscribed channels

## Engine Architecture

```
┌──────────────────────────────────────────────────┐
│ Engine (tokio event loop)                        │
│                                                  │
│  Tick (every 10s):                               │
│    1. Check session completions (tmux poll)      │
│    2. Recover stuck tasks (no session > 10min)   │
│    3. Route new tasks (LLM classification)       │
│    4. Dispatch routed tasks (spawn agents)       │
│    5. Unblock parents (sub-issue check)          │
│    6. Job scheduler (cron matching)              │
│                                                  │
│  Sync (every 120s):                              │
│    - Ingest external tasks (GitHub issues)       │
│    - Review open PRs                             │
│    - Scan for owner commands (/retry, /close)    │
│    - Skills sync (git pull)                      │
│                                                  │
│  Channels:                                       │
│    - Telegram (forum topics, inline keyboards)   │
│    - Discord (multi-channel, button interactions)│
│    - GitHub webhooks (instant events)            │
│    - Tmux capture (output streaming)             │
│                                                  │
│  Shutdown:                                       │
│    - SIGTERM → reset in_progress → routed        │
│    - Tasks re-dispatch into existing worktrees   │
└──────────────────────────────────────────────────┘
```

## Directory Layout

```
~/.orch/
  config.yml             # global config (credentials, engine settings)
  orch.db                # SQLite (tasks, metrics, KV, rate limits, job state, subscriptions)
  projects/              # bare clones added via `orch project add`
  worktrees/             # agent worktrees (all projects)
    repo/branch/         # one per task
  state/                 # runtime state (logs, prompts)
  skills/                # cloned skill repositories
```

## Settled Decisions (DO NOT TOUCH)

- **`src/github/token.rs`** — Token resolution uses `std::process::Command` (blocking, cached 1h). Intentional.
- **`src/engine/runner/`** — Tmux IS the PTY. No external PTY runners.
- **`src/github/auth.rs`** — Deleted (dead code). Do not recreate.
