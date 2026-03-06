# Orch v1 — The Agent Orchestrator

> Communicate with your agents from anywhere — Discord, Telegram, GitHub, or direct tmux attach.

## ⚠️ DO NOT TOUCH — Settled decisions

Before creating any issues or making changes, read `AGENTS.md` for the full list. Key settled areas:

- **`src/github/token.rs` — GitHub token resolution**: Stable and intentional. `try_gh_command()` uses `std::process::Command` (blocking) — correct, result is cached for 1h. Do NOT refactor, add `spawn_blocking`, or file issues about this. Issues #418 and #421 were closed as invalid.
- **`src/engine/runner/` — Agent runner**: Tmux IS the PTY. Agents run inside the tmux session shell. Do NOT reintroduce external PTY runners. Issue #416 explains the removal.
- **`src/github/auth.rs`**: Deleted — was dead code. Do not recreate it.

## Table of Contents

1. [Current Architecture (v0)](#current-architecture-v0)
2. [What's Wrong with v0](#whats-wrong-with-v0)
3. [Future Architecture (v1)](#future-architecture-v1)
4. [What Stays in Bash](#what-stays-in-bash)
5. [What Moves to Rust](#what-moves-to-rust)
6. [Internal vs GitHub Tasks](#internal-vs-github-tasks)
7. [Measurable Improvements](#measurable-improvements)
8. [Is It Worth the Move?](#is-it-worth-the-move)
9. [Stabilize v0 First](#stabilize-v0-first)
10. [Implementation Phases](#implementation-phases)
11. [Module Structure](#module-structure)
12. [Data Flow Examples](#data-flow-examples)
13. [Brew Upgrade Path](#brew-upgrade-path)
14. [Name Change: orchestrator → orch](#name-change)

---

## Current Architecture (v0)

```mermaid
graph TB
    subgraph "Entry Points"
        BREW["brew services<br/>(launchd)"]
        CLI["orch task run/add/..."]
        JUST["justfile<br/>(task runner)"]
    end

    subgraph "Service Loop — serve.sh"
        SERVE["serve.sh<br/>main loop<br/>(sleep 10s)"]
        POLL["poll.sh<br/>task dispatcher"]
        JOBS["jobs_tick.sh<br/>cron scheduler"]
        MENTIONS["gh_mentions.sh<br/>@mention scanner"]
        REVIEW["review_prs.sh<br/>PR review agent"]
        CLEANUP["cleanup_worktrees.sh<br/>worktree janitor"]
    end

    subgraph "Task Execution — run_task.sh"
        ROUTE["route_task.sh<br/>LLM router"]
        RUN["run_task.sh<br/>agent orchestration"]
        TMUX["tmux sessions<br/>orch-{project}-{id}"]
    end

    subgraph "Agent CLIs"
        CLAUDE["claude<br/>--output-format json"]
        CODEX["codex<br/>exec --json"]
        OPENCODE["opencode<br/>run --format json"]
    end

    subgraph "External Tools (subprocess per call)"
        GH["gh CLI<br/>GitHub API"]
        GIT["git<br/>worktrees, push"]
        JQ["jq<br/>JSON parsing"]
        YQ["yq<br/>YAML config"]
        PY["python3<br/>cron, parse, render"]
    end

    subgraph "Data Storage"
        ISSUES["GitHub Issues<br/>(source of truth)"]
        SIDECAR["Sidecar JSON<br/>.orchestrator/tasks/*.json"]
        CONFIG["config.yml<br/>+ .orchestrator.yml<br/>(includes jobs)"]
    end

    BREW --> SERVE
    CLI --> JUST --> SERVE
    SERVE -->|"every 10s"| POLL
    SERVE -->|"every 10s"| JOBS
    SERVE -->|"every 120s"| MENTIONS
    SERVE -->|"every 120s"| REVIEW
    SERVE -->|"every 120s"| CLEANUP

    POLL -->|"xargs -P 4"| RUN
    RUN --> ROUTE
    RUN --> TMUX
    TMUX --> CLAUDE
    TMUX --> CODEX
    TMUX --> OPENCODE

    RUN --> GH
    RUN --> GIT
    RUN --> JQ
    RUN --> PY
    JOBS --> YQ
    JOBS --> PY
    ROUTE --> GH

    GH --> ISSUES
    RUN --> SIDECAR
    YQ --> CONFIG
```

### Current Process Flow Per Tick

```mermaid
sequenceDiagram
    participant S as serve.sh
    participant P as poll.sh
    participant J as jobs_tick.sh
    participant R as run_task.sh
    participant GH as gh CLI → GitHub API
    participant T as tmux
    participant A as Agent (claude/codex)

    loop Every 10 seconds
        S->>P: poll.sh
        P->>GH: GET /issues?labels=status:in_progress (5 status checks)
        GH-->>P: issues list
        P->>GH: GET /issues?labels=status:new
        GH-->>P: new tasks
        P->>R: xargs -P 4 run_task.sh {id}

        R->>GH: GET /issues/{id} (load task)
        R->>GH: POST /issues/{id}/labels (status:in_progress)
        R->>GIT: git worktree add
        R->>PY: python3 render_template (prompt)
        R->>T: tmux new-session -d -s orch-{project}-{id}
        T->>A: claude/codex/opencode
        loop Every 5 seconds
            R->>T: tmux has-session -t orch-{project}-{id}?
        end
        A-->>T: JSON response
        R->>PY: python3 normalize_json.py
        R->>GH: POST /issues/{id}/labels (status:done)
        R->>GH: POST /issues/{id}/comments (result)
        R->>GIT: git push
        R->>GH: gh pr create

        S->>J: jobs_tick.sh
        J->>YQ: yq read jobs.yml
        J->>PY: python3 cron_match.py
    end

    Note over S,GH: Every 120s: mentions + review_prs + cleanup_worktrees
```

### Task Lifecycle (Status Flow)

```
in_progress → needs_review → in_review → done
                  ▲              │
                  │              ├── approve + merge → done
                  │              ├── request_changes → routed → ... → needs_review
                  │              ├── conflicts/CI/merge fail → needs_review (retry)
                  │              └── 3x retry limit → blocked
                  └──────────────┘
```

`needs_review` = "PR ready, queued for review agent"
`in_review` = "review agent actively working"
`blocked` = only status requiring human intervention

| Transition | Owner | Location | Trigger |
|---|---|---|---|
| new → routed | engine tick | `engine/tick.rs` | LLM router assigns agent + complexity |
| routed → in_progress | engine dispatch | `engine/tick.rs` | Agent spawned in tmux |
| in_progress → needs_review | runner | `runner/mod.rs` | Agent completes + PR exists |
| in_progress → done | runner | `runner/mod.rs` | Agent completes, no PR created |
| needs_review → in_review | engine tick/sync | `engine/tick.rs`, `engine/sync.rs` | Review agent spawned (status transition is the guard) |
| in_review → done | engine | `engine/review.rs` (auto_merge_pr) | PR approved + merged |
| in_review → routed | engine | `engine/review.rs` (review_open_prs / handle_review_changes) | Changes requested → re-dispatch |
| in_review → needs_review | engine | `engine/review.rs` | Review agent failed/crashed, conflict/CI retry |
| in_review → blocked | engine | `engine/review.rs` | Merge conflict retries ≥ 3, non-conflict merge failure, max review cycles exceeded |
| stale in_review → needs_review | sync tick | `engine/sync.rs` | No active tmux review session detected |

### Recent doc updates (docs/content)

- Updated docs to reflect actual runtime paths (`~/.orch`), worktree layout, and sidecar locations
- Documented that `GH_TOKEN` is injected into runner env at spawn time and that agents should not call GitHub directly
- Documented centralized GitHub token resolution (`github.token_mode`, GitHub App config, and `gh.allow_gh_fallback` (default true))
- Clarified that jobs are per-project in `.orch.yml` (preferred) and scheduler runs from engine tick
- Changed max-attempts behavior: repeated failures now move tasks to `needs_review` and forced `agent:*` labels are removed
- Review agent selection clarifications: review agent excludes the original task agent to avoid self-review
- Aligned `workflow.md`, `getting-started.md`, `cli.md`, and `_index.md` with Rust v1: removed bash script references (`serve.sh`, `poll.sh`, `run_task.sh`), replaced with `orch` subcommands, updated engine tick description, removed "orch is alias for orchestrator" copy, and file tables reflect SQLite database
 - Completed bidirectional channel wiring and live output fanout: `handle_channel_message()` routes incoming messages to tmux or commands, `fanout_output()` enforces per-channel rate limits, splits long messages, and flushes final chunks; added integration tests for tmux capture -> transport and a test channel for fanout

### New CLI: `orch task logs <id>`

- Purpose: print a concise post-mortem (summary + memory + token/costs + recent tmux output) for a completed task.
- Works with internal tasks (`internal:<n>`) and external GitHub tasks (issue number).
- Fields shown: ID, title, status, agent, model, attempts, cost summary (tokens + USD), recent memory entries (learnings, errors, files modified), sidecar path. If a live tmux session exists, appends recent pane output.
- Missing sidecar: prints a clear message with the inspected path instead of failing.

Usage example:

  orch task logs internal:8

Implemented in `src/cli/task.rs:664` (`pub async fn logs`) and wired into the main CLI dispatch in `src/main.rs`.

### Review Agent & Status Invariants

**Review agent**: triggered by engine when a task is in `needs_review` and has a branch. The engine transitions the task to `in_review` before spawning — the status itself is the duplicate guard (no sidecar flags needed). On failure, the engine resets to `needs_review` for retry.

**Key invariant**: `done` means task is finished (PR merged or no code changes). `needs_review` means PR exists and is queued for review. `in_review` means a review agent is actively running. `blocked` means human intervention is required. The runner decides: if agent said "done" AND a PR exists → `needs_review`; otherwise → agent's reported status.

### Subprocess Cost Per Tick (Measured)

| Operation | Subprocesses | Tools |
|-----------|-------------|-------|
| Status checks (5 statuses) | 5 | `gh` |
| Normalize new issues | 1 | `gh` |
| Config reads | ~10 | `yq` |
| Cron matching (4 jobs) | 4-8 | `python3` |
| **Quiet tick total** | **~25** | |
| | | |
| Per active task (run_task) | ~30 | `gh`, `git`, `python3`, `jq` |
| Per 120s sync window | ~10 per project | `gh`, `git` |
| **Busy tick (4 tasks)** | **~150+** | |

---

## What's Wrong with v0

### Bug Classes We've Hit

| Bug | Root Cause | Bash-Specific? |
|-----|-----------|----------------|
| Mention handler infinite loop (#265) | No dedup — 73 branches, 60+ junk issues | Fragile text matching |
| `with_lock()` shadowing `status` var | `local status=0` clobbered exported var | Yes — bash scoping |
| `db_load_task` multiline body truncation | `read -r` stops at newlines | Yes — bash IFS handling |
| Self-destructing auto-update job | `orchestrator restart` SIGTERMs own parent | Process management |
| Stale `active_task_id` blocking jobs | Manual YAML state → no automatic cleanup | YAML as database |
| Comment spam / duplicate comments | Race condition in timestamp comparison | Subprocess timing |
| 88 orphaned branches | No cleanup for stuck/failed tasks | Missing lifecycle mgmt |
| Label sprawl | No validation, agents create arbitrary labels | Late-added validation |

**At least 4 of these are bash-specific** (variable scoping, IFS parsing, fragile subprocess coordination). The rest are architectural and would exist in any language.

### Performance Bottlenecks

| Issue | Impact | Fix in Rust? |
|-------|--------|-------------|
| 25+ subprocess forks per quiet tick | ~200ms overhead, adds up | Yes — native JSON/YAML/cron |
| `yq` called ~10x per tick for config | 10 forks for config reads | Yes — in-memory config |
| `python3` for cron/parse/render | 4-8 forks per tick | Yes — native cron/parser |
| `gh` CLI startup time (~150ms each) | 5+ calls just for status checks | ✅ Done — `src/github/http.rs` (reqwest, connection pooling, ~100ms/call) |
| 10s polling loop (sleep in bash) | Tasks wait up to 10s to start | Yes — async event loop |
| Sequential label operations | 3-5 API calls per status change | Yes — batch API calls |

### Capability Gaps

| Gap | Why Bash Can't | Rust Can |
|-----|---------------|----------|
| Webhooks (GitHub App) | Can't run HTTP server | axum |
| Telegram/Discord bots | Can't maintain websocket | tokio |
| Output streaming | Polling tmux capture-pane | Async broadcast channels |
| Concurrent I/O | `xargs -P` is crude | tokio::spawn |
| Internal tasks | No local DB without sqlite3 deps | Built-in SQLite (rusqlite) |
| Graceful shutdown | Trap-based, fragile | tokio signal handlers |

---

## Future Architecture (v1)

```mermaid
graph TB
    subgraph "Channels (all equal)"
        GH_CH["GitHub App<br/>(webhooks)"]
        TG_CH["Telegram Bot<br/>(long poll/webhook)"]
        DC_CH["Discord Bot<br/>(gateway ws)"]
        TMUX_CH["tmux Bridge<br/>(capture/send-keys)"]
        CLI_CH["CLI / HTTP<br/>(local API)"]
    end

    subgraph "orch (Rust binary, Tokio)"
        TRANSPORT["Transport Layer<br/>routes messages ↔ sessions<br/>broadcasts output"]

        subgraph "Engine"
            TASKS["Task Manager<br/>internal + GitHub"]
            ROUTER["Router<br/>agent selection"]
            SCHEDULER["Cron Scheduler<br/>minute-precision"]
            RUNNER["Task Runner<br/>Rust native"]
        end

        subgraph "Runner Modules"
            CTX["context.rs<br/>prompt building"]
            WT["worktree.rs<br/>git worktree mgmt"]
            AGT["agent.rs<br/>agent invocation"]
            RESP["response.rs<br/>error classification"]
            GITOPS["git_ops.rs<br/>commit, push, PR"]
        end

        CONFIG_RS["Config<br/>in-memory, hot-reload"]
        SQLITE["SQLite<br/>internal tasks + state"]
    end

    subgraph "Agent CLIs (unchanged)"
        CL["claude"]
        CO["codex"]
        OC["opencode"]
    end

    subgraph "tmux Sessions (unchanged)"
        T1["orch-myproject-42 (claude)"]
        T2["orch-myproject-43 (codex)"]
        T3["orch-main (chat)"]
    end

    subgraph "Data"
        GH_ISSUES["GitHub Issues<br/>(external tasks)"]
        LOCAL_DB["SQLite<br/>(internal tasks)"]
        YAML[".orchestrator.yml<br/>+ config.yml"]
        SIDE["Sidecar JSON<br/>(ephemeral state)"]
    end

    GH_CH --> TRANSPORT
    TG_CH --> TRANSPORT
    DC_CH --> TRANSPORT
    TMUX_CH --> TRANSPORT
    CLI_CH --> TRANSPORT

    TRANSPORT --> TASKS
    TRANSPORT --> T1
    TRANSPORT --> T2
    TRANSPORT --> T3

    TASKS --> ROUTER
    TASKS --> SCHEDULER
    TASKS --> RUNNER

    RUNNER --> CTX
    RUNNER --> WT
    RUNNER --> AGT
    RUNNER --> RESP
    RUNNER --> GITOPS
    AGT --> T1
    AGT --> T2

    T1 --> CL
    T2 --> CO
    T3 --> OC

    SQLITE --> LOCAL_DB
    CONFIG_RS --> YAML
    RUNNER --> SIDE
```

### Core Concepts

**Channel** — Bidirectional async interface. Receives messages, sends updates, streams output. All channels implement the same trait. The engine is channel-agnostic.

**Transport** — The multiplexer. Maps channel threads to tmux sessions. Telegram reply → tmux send-keys. Agent output → broadcast to all connected channels.

**Engine** — Task lifecycle, routing, scheduling. Publishes events. Doesn't know about channels.

**tmux Bridge** — Both a channel (users can "attach" from Telegram/Discord) and the execution backend. Captures pane output, pushes through transport.

### Live Session Streaming & Interaction

Today, agent sessions are black boxes — `claude -p --output-format json` runs silently in tmux and dumps JSON at the end. No visibility until it finishes.

In v1, the tmux bridge changes this completely:

**Watch** — The bridge runs `tmux capture-pane -t orch-{id} -p` every few seconds, diffs against the last capture, and streams new content through the transport to all connected channels. You see every step the agent takes — file reads, tool calls, code edits — in real-time on Telegram, Discord, or a local CLI stream.

**Join** — From any channel, attach to a running session. The transport routes your message via `tmux send-keys -t orch-{id}` directly to the agent. Type a correction, answer a question, or give new instructions — the agent sees it as input immediately.

**Intervene** — Mid-run course corrections. "Use postgres not mysql", "skip the tests for now", "focus on the API first". The agent adjusts without restarting. This turns every agent session from a fire-and-forget job into an interactive collaboration.

**Multi-viewer** — Multiple people can watch/interact with the same session simultaneously. The transport broadcasts output to every connected channel thread. One person watches on Telegram, another on Discord, a third via `orch task stream 301` in their terminal.

```
┌─────────────┐     ┌─────────────┐     ┌─────────────┐
│  Telegram    │     │  Discord    │     │  CLI        │
│  thread #42  │     │  thread #42 │     │  orch task  │
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

**v0 workaround:** You can already attach to any session with `TMUX= tmux attach-session -t orch-{project}-{id}` and interact directly — it's just not connected to external channels yet.

---

## What Stays in Bash

> **Update (v1.1):** Almost everything originally planned to stay in bash has been ported to Rust.
> Agent invocation now prefers a PTY-based runner (no on-disk runner scripts). A legacy tmux
> command runner remains as a fallback behind a config flag.

| Component | Status | Notes |
|-----------|--------|-------|
| ~~`run_task.sh`~~ | **Ported to Rust** | `src/engine/runner/` — context, worktree, agent, response, git_ops |
| ~~Git operations~~ | **Ported to Rust** | `src/engine/runner/git_ops.rs` — auto-commit, push, PR creation via `tokio::process::Command` |
| ~~`gh pr create/merge`~~ | **Ported to Rust** | `git_ops::create_pr_if_needed()` via `gh` CLI subprocess |
| Agent invocation | **PTY-first** | Rust spawns agent CLIs under a PTY and streams into tmux; legacy tmux command runner behind config |
| ~~`justfile`~~ | **Deleted** | All recipes ported to native `orch` CLI subcommands |
| Prompt templates | **Unchanged** | Markdown files in `prompts/`, rendered by Rust `template.rs` |
| Config | **Consolidated** | Per-project `.orch.yml` + `~/.orch/config.yml` global (see [Config Architecture](#config-architecture)) |

**Principle:** Rust owns the entire lifecycle. PTY-based agent spawning preserves TTY semantics without runner scripts; the legacy tmux command runner remains as a fallback only. All orchestration logic, git operations, and API calls are native Rust.

---

## What Moves to Rust

| Component | v0 (bash) | v1 (Rust) | Status |
|-----------|-----------|-----------|--------|
| Service loop | `serve.sh` (sleep 10s) | Tokio event loop | **Done** |
| Task polling | `poll.sh` (5x gh CLI calls) | Native reqwest HTTP + `serde` | **Done** |
| Config loading | `yq` subprocess per read | In-memory struct, hot-reload | **Done** |
| Cron matching | `python3 cron_match.py` | Native `cron` crate | **Done** |
| JSON parsing | `jq` subprocesses | `serde_json` | **Done** |
| Response normalization | `python3 normalize_json.py` | Native parser | **Done** |
| GitHub API calls | `gh` CLI + `jq` parsing | Native reqwest HTTP + `serde` | **Done** |
| Task execution | `run_task.sh` (bash) | `src/engine/runner/` (Rust) | **Done** |
| Git operations | bash `git` + `gh pr` | `runner/git_ops.rs` | **Done** |
| Agent invocation | bash script | `runner/agent.rs` (PTY runner + tmux fallback) | **Done** |
| Mention detection | `gh_mentions.sh` (polling) | Rust polling (webhook future) | **Done** (polling) |
| PR review trigger | `review_prs.sh` (polling) | Rust polling (webhook future) | **Done** (polling) |
| Sidecar I/O | `jq` read/write | Direct file I/O | **Done** |
| Template rendering | `python3 render_template()` | Native Rust | **Done** |
| Internal task DB | Not supported | `rusqlite` (embedded SQLite) | **Done** |
| CLI entry point | `justfile` (34+ recipes) | Native `clap` subcommands | **Done** |

### Total Subprocess Savings

| Scenario | v0 (subprocesses/tick) | v1 (subprocesses/tick) | Savings |
|----------|----------------------|----------------------|---------|
| Quiet tick (no tasks) | ~25 | ~0 (native HTTP, no subprocesses) | **100%** |
| Per active task | ~30 | ~3 (git + agent CLI only) | **90%** |
| Busy tick (4 tasks) | ~150+ | ~14 (git + agent only) | **91%** |

---

## Internal vs GitHub Tasks

### The Problem

Not every task needs a GitHub issue:
- **Cron jobs** (morning review, evening retro) → creates clutter on the issue tracker
- **Mention response tasks** → caused 73 junk branches and 60+ issues from the loop bug
- **Internal maintenance** (cleanup, retry, reroute) → noise in the project board
- **Quick one-off tasks** (via Telegram/Discord) → don't need the overhead of an issue

### The Solution: Two Layers — External Backend Trait + Internal SQLite

```
┌───────────────────────────────────────────────────────┐
│                    Task Manager                       │
│                                                       │
│  ┌─────────────────────────────────┐  ┌────────────┐ │
│  │       ExternalBackend trait     │  │  Internal   │ │
│  │                                 │  │  (SQLite)   │ │
│  │  ┌──────────┐  ┌────────────┐  │  │             │ │
│  │  │  GitHub   │  │  Linear /  │  │  │ • Cron jobs │ │
│  │  │  Issues   │  │  Jira /    │  │  │ • Mentions  │ │
│  │  │          │  │  GitLab    │  │  │ • Maintenance│ │
│  │  │ (v1)     │  │  (later)   │  │  │ • Quick     │ │
│  │  └──────────┘  └────────────┘  │  │             │ │
│  └────────────────┬────────────────┘  └──────┬──────┘ │
│                   │                          │        │
│                   └────────────┬─────────────┘        │
│                                │                      │
│                    Same lifecycle:                     │
│                    new → routed → in_progress          │
│                    → done/blocked/needs_review         │
│                                                       │
│                    Same agent interface                │
│                    Same tmux sessions                  │
│                    Same routing                        │
└───────────────────────────────────────────────────────┘
```

### ExternalBackend Trait

GitHub Issues is just the first implementation. The trait is designed so Linear, Jira, GitLab, or any issue tracker can be swapped in later.

```rust
#[async_trait]
trait ExternalBackend: Send + Sync {
    /// Human-readable name (e.g. "github", "linear", "jira")
    fn name(&self) -> &str;

    /// Create a task in the external system, return its external ID
    async fn create_task(&self, title: &str, body: &str, labels: &[String]) -> Result<ExternalId>;

    /// Fetch a task by its external ID
    async fn get_task(&self, id: &ExternalId) -> Result<ExternalTask>;

    /// Update task status (maps to labels, states, or columns depending on backend)
    async fn update_status(&self, id: &ExternalId, status: Status) -> Result<()>;

    /// List tasks by status
    async fn list_by_status(&self, status: Status) -> Result<Vec<ExternalTask>>;

    /// Post a comment / activity note
    async fn post_comment(&self, id: &ExternalId, body: &str) -> Result<()>;

    /// Set metadata labels / tags
    async fn set_labels(&self, id: &ExternalId, labels: &[String]) -> Result<()>;

    /// Remove a label / tag
    async fn remove_label(&self, id: &ExternalId, label: &str) -> Result<()>;

    /// Check if connected and authenticated
    async fn health_check(&self) -> Result<()>;
}
```

**Implementation mapping:**

| Trait Method | GitHub Issues | Linear | Jira |
|-------------|---------------|--------|------|
| `create_task` | `gh api repos/O/R/issues` | `issueCreate` mutation | `POST /issue` |
| `update_status` | `gh api` label swap | State change | Transition |
| `list_by_status` | `gh api repos/O/R/issues?labels=` | `issues(filter:)` query | JQL search |
| `post_comment` | `gh api repos/O/R/issues/N/comments` | `commentCreate` | `POST /comment` |
| `set_labels` | `gh api repos/O/R/issues/N/labels` | `issueAddLabel` | Tag update |

### GitHub Backend: `gh` CLI, not raw HTTP

The GitHub backend shells out to `gh api` rather than using `reqwest` directly. Reasons:

1. **Auth is free** — `gh` handles OAuth, tokens, SSH keys, SSO. No JWT/App setup needed.
2. **Everyone has it** — any user with `gh` installed can use orch immediately.
3. **Rate limit handling** — `gh` has built-in retry/backoff for 429s.
4. **Less code** — no token refresh, no auth middleware, no credential storage.

The Rust side handles structured I/O: builds the `gh api` command args, parses JSON output via `serde`. No `jq` needed — Rust deserializes directly.

```rust
impl ExternalBackend for GitHubBackend {
    async fn create_task(&self, title: &str, body: &str, labels: &[String]) -> Result<ExternalId> {
        let mut cmd = Command::new("gh");
        cmd.args(["api", &format!("repos/{}/issues", self.repo), "-X", "POST"]);
        cmd.args(["-f", &format!("title={title}"), "-f", &format!("body={body}")]);
        for label in labels {
            cmd.args(["-f", &format!("labels[]={label}")]);
        }
        let output = cmd.output().await?;
        let issue: GitHubIssue = serde_json::from_slice(&output.stdout)?;
        Ok(ExternalId(issue.number.to_string()))
    }
    // ...
}
```

**Future backends** (Linear, Jira) would use `reqwest` directly since they don't have equivalent CLIs. The trait doesn't prescribe HTTP vs CLI — each backend picks what works best.

### Rules

- `type: external` → delegates to whichever `ExternalBackend` is configured (GitHub by default)
- `type: internal` → SQLite only, no external system, no branch, no PR
- Cron jobs default to `internal` unless `external: true` is set
- User can promote internal → external: `orch task publish {id}`
- External tasks are the source of truth in their backend (no bidirectional sync)
- Internal tasks live only in SQLite

**No bidirectional sync complexity.** External backends own their tasks. Internal tasks are local. They share the same engine but different storage backends.

---

## Measurable Improvements

### Performance (quantified)

| Metric | v0 (bash) | v1 (Rust) | Improvement |
|--------|-----------|-----------|-------------|
| Tick latency (quiet) | ~500ms (25 forks × ~20ms) | ~10ms (no forks) | **50x faster** |
| Tick latency (busy, 4 tasks) | ~3s (150+ forks) | ~100ms | **30x faster** |
| Config read | ~20ms (yq fork) | ~0.1ms (in-memory) | **200x faster** |
| GitHub API call | ~300ms (gh startup + HTTP) | ~100ms (pooled HTTP) | **3x faster** |
| Cron matching | ~50ms (python3 fork) | ~0.01ms (native) | **5000x faster** |
| Memory (service) | ~8MB (bash) + subprocess churn | ~15MB (steady, no churn) | Stable footprint |
| Task detection time | 0-10s (polling) | <100ms (webhooks) | **~100x faster** |
| Mention detection | 0-120s (polling) | <1s (webhook) | **~120x faster** |

### Reliability (bug classes eliminated)

| Bug Class | v0 Risk | v1 Fix |
|-----------|---------|--------|
| Variable scoping (`local` shadows) | High — caused 2 critical bugs | Rust compiler prevents this |
| IFS/read multiline parsing | High — caused data corruption | `serde` structured parsing |
| Race conditions (file state) | Medium — timestamp races | In-memory state + atomic ops |
| Subprocess coordination | Medium — SIGTERM cascades | Tokio task management |
| YAML as database | Medium — stale state | SQLite transactions |
| Missing error handling | Medium — `|| true` hides failures | `Result<T>` forces handling |

### New Capabilities Unlocked

| Capability | Value |
|------------|-------|
| GitHub App with webhooks | Instant event processing, no polling |
| Telegram bot | Manage agents from phone |
| Discord bot | Team-wide agent management |
| Output streaming | Watch agent work in real-time from any channel |
| Internal tasks | No issue clutter for maintenance work |
| Connection pooling | Fewer API calls, better rate limit management |
| Concurrent task management | True async, not `xargs -P` |
| Graceful shutdown | Clean session handoff, no orphaned processes |

---

## Is It Worth the Move?

### The Honest Assessment

**YES, but incrementally.** Here's the breakdown:

#### What You Get

1. **Multi-channel communication** — this is the killer feature. Telegram, Discord, GitHub all equal. Can't do this in bash.
2. **Internal tasks** — eliminates the biggest source of GitHub issue clutter (73 junk issues from one bug).
3. **Webhooks replace polling** — 120s mention detection → instant. This alone justifies the effort.
4. **~85% fewer subprocesses** — less CPU churn, faster response, more predictable behavior.
5. **Compiler-enforced correctness** — the variable scoping bug, the multiline parsing bug, the subprocess race conditions — Rust's type system prevents entire categories of these.

#### What It Costs

1. **~2-4 weeks** of development for Phase 1 (foundation + engine).
2. **Two codebases** to maintain during transition (Rust core + bash scripts).
3. **Learning curve** for contributors (Rust vs bash).
4. **Binary distribution** — need cross-compile CI pipeline.

#### The Kill Shot: Do Nothing vs. Do Something

**If you stay pure bash:**
- No Telegram/Discord channels (can't run websockets/webhooks)
- Keep hitting bash-specific bugs (scoping, IFS, subprocess races)
- Keep polling GitHub every 120s instead of instant webhooks
- Keep creating GitHub issues for internal tasks
- Performance stays at ~25 forks/tick

**If you add Rust core:**
- All channels become possible
- Eliminate entire bug classes
- Instant event processing
- Internal tasks for internal work
- ~2 forks/tick on quiet ticks

**Verdict:** The multi-channel vision alone makes it worth it. The reliability and performance gains are bonuses. The key is doing it incrementally — Phase 1 replaces internal tools, Phase 2 replaces the service loop, channels come after. Bash keeps working throughout.

---

## Stabilize v0 First

Before any Rust work, the current bash version needs to be rock-solid. This gives us:

1. **Baseline metrics** — measure current tick latency, API calls, error rates
2. **Bug discovery** — find all the edge cases before encoding them in Rust
3. **Test coverage** — the 259 bats tests are the contract the Rust version must match
4. **Operational confidence** — if v0 runs clean for 2 weeks, we know the design is right

### Known Issues to Fix

- [x] Mention handler infinite loop (#265) — fixed
- [x] Auto-update job SIGTERM — fixed (backgrounded restart)
- [x] 88 orphaned branches — cleaned up (96 deleted)
- [x] Stale job `active_task_id` — fixed
- [x] PR #190 owner slash commands — merged (in orchestrator repo)
- [x] PR #191 auto-reroute on usage limits — merged (in orchestrator repo)
- [x] README is outdated — refreshed (#70)
- [x] Internal tasks design — implemented (SQLite + engine integration)
- [x] Weighted round-robin for agent routing — PR #94 merged
- [x] Worktree cleanup for stuck/failed tasks — implemented in sync_tick

### Metrics to Collect Before v1

| Metric | How to Measure |
|--------|---------------|
| Tick latency | Timestamp start/end in serve.sh |
| API calls per tick | Count `gh_api` invocations |
| Error rate | grep error logs |
| Task completion time | history timestamps |
| Agent success rate | done vs needs_review ratio |
| Subprocess count | strace/dtrace per tick |

---

## Implementation Phases

### Phase 0: Stabilize v0 ✅ DONE
- [x] Fix all known bugs
- [x] Run clean for 2+ weeks
- [x] Collect baseline metrics
- [x] Grow test suite to cover all edge cases
- [x] Update README — refreshed (#70)

### Phase 1: Foundation (replace internal tools) ✅ DONE

**Goal:** Single Rust binary (`orch`) that replaces jq/python3/yq calls.

- [x] Config loading (config.yml, .orchestrator.yml) — `src/config.rs` (hot-reload via `notify`)
- [x] Sidecar JSON I/O (read/write/merge) — `src/sidecar.rs`
- [x] GitHub API client (gh CLI wrapper with serde parsing) — ~~`src/github/cli.rs`~~ removed, `src/github/types.rs`
- [x] Native HTTP client (reqwest, connection pooling, header-based rate limiting) — `src/github/http.rs` (supersedes `cli.rs`)
- [ ] ~~GitHub App auth (JWT, token refresh, GH_TOKEN export)~~ — using `gh auth token` / `GH_TOKEN` env instead
- [x] Agent response parser — `src/parser.rs`
- [x] Cron matcher — `src/cron.rs`
- [x] Template renderer — `src/template.rs`
- [x] CLI: `orch config`, `orch sidecar`, `orch parse`, `orch cron`, `orch template`, `orch stream`

### Phase 2: Engine (replace serve.sh/poll.sh) ✅ MOSTLY DONE

**Goal:** Tokio event loop replaces the bash 10s tick loop.

- [x] ExternalBackend trait — `src/backends/mod.rs` (Status, ExternalTask, ExternalId)
- [x] GitHub backend — `src/backends/github.rs` (implements ExternalBackend via native reqwest HTTP)
- [x] Engine main loop — `src/engine/mod.rs` (tokio::select! with 10s tick + 120s sync)
- [x] Task polling (GitHub API via native reqwest HTTP) — Phase 3 of tick()
- [x] Task runner — `src/engine/runner/` (context, worktree, agent, response, git_ops)
- [x] Stuck task recovery — Phase 2 of tick()
- [x] Parent/child unblocking — Phase 4 of tick()
- [x] Job scheduler (native cron with catch-up) — `src/engine/jobs.rs`
- [x] Internal task SQLite database — `src/db.rs` (schema, CRUD, migrations)
- [x] Internal task API — `src/engine/internal_tasks.rs`
- [x] TaskManager (unified internal + external) — `src/engine/tasks.rs`
- [x] Agent router (label-based, round-robin, LLM classification) — `src/engine/router.rs`
- [x] Router wired into engine dispatch loop (Phase 3a route → Phase 3b dispatch)
- [x] tmux bridge (capture-pane, send-keys, session lifecycle) — `src/tmux.rs`
- [x] Output capture service (2s polling loop) — `src/channels/capture.rs`
- [x] Transport layer (pub/sub broadcast) — `src/channels/transport.rs`
- [x] Graceful shutdown (SIGTERM/SIGINT handlers)
- [x] CLI: `orch serve`
- [x] Sync tick: cleanup done worktrees — `cleanup_done_worktrees()` in Rust
- [x] Sync tick: check merged PRs — `check_merged_prs()` in Rust
- [x] Sync tick: scan @mentions — `scan_mentions()` in Rust
- [x] Sync tick: review open PRs — `review_open_prs()` in Rust
- [x] Task CRUD CLI commands — `orch task list/add/get/publish`
- [x] Security module — `src/security.rs`
- [x] **Rewrite run_task.sh in Rust** — `src/engine/runner/` (context, worktree, agent, response, git_ops)
- [x] **Route task in Rust** — router wired into dispatch loop, `run_with_context()` calls runner
- [x] Multi-project support — PR #82 merged
- [x] Config hot-reload wired into engine — PR #78 merged

### Phase 3: Channels

**Goal:** Multi-channel I/O for task management and live streaming.

- [x] Channel trait + ChannelRegistry — `src/channels/mod.rs`
- [x] Transport layer — `src/channels/transport.rs` (session bindings, broadcast)
- [x] Tmux channel — `src/channels/tmux.rs` (pane monitoring)
- [x] Capture service — `src/channels/capture.rs` (output diffing + streaming)
- [x] GitHub channel (polling implementation) — PR #81 merged
- [x] Telegram channel (long-poll implementation) — PR #81 merged
- [x] Discord channel (polling implementation) — PR #81 merged
- [x] Webhook HTTP server (axum) — PR #93 merged
- [x] Wire webhook server into engine — PR #123 merged
- [x] Mention detection via webhooks (#112) — webhook handler + polling fallback
- [x] Polling fallback when webhooks not configured — PR #131 merged
- [x] Wire channels into engine event loop — PR #81 merged
- [x] **Bidirectional channel wiring** — `src/engine/mod.rs` `handle_channel_message()`: `TaskSession` → tmux send-keys or slash command; `Command` → `/status` + owner commands; `NewTask` → create internal task + bind thread + start fanout
- [x] **Output fanout streaming** — `src/channels/stream.rs` `fanout_output()`: per-channel rate limiting + message splitting + final-chunk flush

### Phase 4: CLI & User-Facing Commands

**Goal:** Replace justfile → bash script routing with native `orch` CLI.

- [x] `orch serve` — start engine
- [x] `orch config <key>` — read config
- [x] `orch sidecar get/set` — task metadata
- [x] `orch parse <path>` — parse agent response
- [x] `orch cron <expr>` — cron matching
- [x] `orch template <path>` — render templates
- [x] `orch stream <id>` — live output streaming
- [x] `orch task list/add/get/publish` — task CRUD
- [x] `orch task status` — task status overview
- [x] `orch task route <id>` — route task to agent
- [x] `orch task run <id>` — execute task
- [x] `orch task retry <id>` — retry failed task
- [x] `orch task attach <id>` — attach to tmux session
- [x] `orch task kill <id>` — kill agent session
- [x] `orch task live` — list active sessions
- [x] `orch task unblock <id|all>` — unblock tasks
- [x] `orch job list/add/remove/enable/disable/tick` — job management
- [x] `orch init` — project initialization
- [x] `orch agents` — list available agent CLIs
- [x] `orch version` — show version info
- [x] `orch log` — tail logs
- [x] `orch service start/stop/restart/status` — service management
- [x] `orch completions <shell>` — shell completions
- [x] `orch board list/link/sync/info` — GitHub Projects V2 board management
- [x] `orch project add/remove/list` — multi-project management (see Phase 6)
- [x] Rename binary from `orch-core` to `orch`
- [x] Absorb justfile routing into native CLI (justfile deleted)

### Phase 5: Polish & Migration

- [x] Rename `~/.orchestrator/` → `~/.orch/` (with backward compat) — PR #83
- [x] Rename `.orchestrator.yml` → `.orch.yml` — issue #74
- [x] Update brew formula (from `orchestrator` to `orch`) — `Formula/orch.rb`
- [x] Update AGENTS.md with Rust engine docs
- [x] Jobs config consolidated into `.orchestrator.yml` (no separate `jobs.yml`)
- [x] Cross-compile CI pipeline (macOS arm64 + x86_64) — PR #92 merged
- [x] Metrics / observability (tracing, prometheus) — PR #95 merged
- [x] Unified notification system (events → all channels) — PR #124 merged
- [x] Per-agent runner trait (AgentRunner, AgentError, per-agent parsers) — PR #116 merged
- [x] Agent memory (persist learnings across retries) — PR #122 merged
- [x] PR review integration (parse review comments → follow-up tasks) — PR #125 merged
- [x] Self-improvement loop (auto-create issues from metrics) — PR #120 merged
- [x] Polling fallback for webhooks (health check, fallback mode) — PR #131 merged
- [x] Updated default model map to current identifiers — PR #130 merged
- [x] Auto-clone GitHub repos during `orch init` — PR #141 merged
- [x] Separate `~/.orch/` from `~/.orchestrator/` (real directory, no symlink)
- [x] Project-aware tmux session naming (`orch-{project}-{id}`)
- [x] Simplified service management (pure `brew services` wrapper)
- [x] Permission rules per-agent translation (PermissionRules → native CLI flags)
- [x] Global `allowed_tools` config with per-agent translation — PR #163 merged
- [x] Migrate GitHub API from `gh` CLI to native reqwest HTTP client — PR #203 merged
- [x] Add safe `gh` CLI wrapper with fallback paths for launchd environments — PR #374 merged
- [x] System prompt file passed to Codex and OpenCode agents — PR #201 merged
- [x] Dead code cleanup: removed legacy `RunResult`/`collect_response` from response.rs

### Phase 6: Remaining Gaps

**Goal:** Close remaining feature gaps with bash orchestrator.

- [x] `orch project add/remove/list` — multi-project management CLI
- [x] Wire Telegram/Discord channels into engine event loop
- [x] Mention detection via webhooks (#112)
- [x] Owner commands (feedback via issue comments: `/retry`, `/reroute`) — see `src/engine/commands.rs`
- [x] Merge detection (auto-close after PR merge) — see `check_merged_prs()` in `src/engine/cleanup.rs:229`
- [x] Dashboard/reporting CLI command — see `src/cli/dashboard.rs`
- [x] Graceful shutdown with session handoff — see `src/engine/mod.rs` serve() loop (signal handlers at line 770+)
- [x] Slack channel integration — `src/channels/slack.rs` (polling via `conversations.history`, `chat.postMessage`, `auth.test` health check; wired in `src/engine/mod.rs:311-323`)
- [x] Context file per issue (persistent context accumulation) — see `src/engine/runner/context.rs:40-45`

---

## Module Structure

```
src/
├── main.rs                  # CLI entrypoint (clap) + all subcommand dispatch
├── config/
│   └── mod.rs               # Config loading (.orch.yml + ~/.orch/config.yml, multi-project)
│
├── cli/
│   ├── mod.rs               # CLI utilities (agents, init, log, version)
│   ├── task.rs              # Task subcommand handlers
│   ├── job.rs               # Job subcommand handlers
│   └── service.rs           # Service management (start/stop/restart/status)
│
├── backends/
│   ├── mod.rs               # ExternalBackend trait, ExternalTask, ExternalId, Status
│   └── github.rs            # GitHub Issues implementation (native reqwest HTTP, PR #203)
│
├── channels/
│   ├── mod.rs               # Channel trait, IncomingMessage, OutgoingMessage
│   ├── transport.rs         # Session bindings, routing, output broadcast
│   ├── capture.rs           # tmux output capture + diffing service
│   ├── notification.rs      # Unified notification dispatch (levels, formatting, broadcast)
│   ├── tmux.rs              # tmux channel (pane monitoring)
│   ├── github.rs            # GitHub channel: webhooks + polling fallback
│   ├── telegram.rs          # Telegram Bot API (HTTP long-poll)
│   ├── discord.rs           # Discord channel (HTTP polling)
│   └── slack.rs             # Slack channel (conversations.history polling + chat.postMessage)
│
├── engine/
│   ├── mod.rs               # Engine struct, config, project init, main event loop (serve)
│   ├── tick.rs              # Core tick phases: session polling, stuck recovery, routing, dispatch, unblock
│   ├── sync.rs              # Sync tick: worktree cleanup, PR review trigger, mentions, skills sync
│   ├── review.rs            # PR review pipeline: review agent, auto-merge, change handling, review_open_prs
│   ├── cleanup.rs           # Worktree cleanup, branch deletion, merged-PR detection
│   ├── tasks.rs             # TaskManager — unified internal + external CRUD
│   ├── internal_tasks.rs    # Internal task SQLite operations
│   ├── router/
│   │   ├── mod.rs           # Router struct, route() orchestration, public API (~200 lines)
│   │   ├── config.rs        # RouterConfig, DEFAULT_AGENTS, model_map loading (~250 lines)
│   │   ├── weights.rs       # RateLimitState, AgentWeights, weight decay/recovery (~175 lines)
│   │   ├── selection.rs     # Hash utilities: simple_hash_fraction_for, simple_hash_index_for (~60 lines)
│   │   ├── strategies.rs    # route_via_round_robin, route_via_weighted, route_via_fallback (~200 lines)
│   │   └── llm.rs           # LlmRouter — prompt building, LLM call, response parsing, skills catalog (~511 lines)
│   ├── jobs.rs              # Cron scheduler + self-review job (metrics → improvement issues)
│   ├── commands.rs          # Owner /slash command scanning (/retry, /reroute, /block, etc.)
│   └── runner/
│       ├── mod.rs           # TaskRunner — orchestrates full task lifecycle
│       ├── context.rs       # Prompt context building (project instructions, repo tree, etc.)
│       ├── worktree.rs      # Git worktree creation, branch naming, reuse
│       ├── agent.rs         # Agent invocation + prompt building
│       ├── agents/
│       │   ├── mod.rs       # AgentRunner trait, AgentError enum, PermissionRules, pattern detection
│       │   ├── claude.rs    # Claude/Kimi/MiniMax runner (JSON envelope parser)
│       │   ├── codex.rs     # Codex runner (NDJSON stream parser)
│       │   └── opencode.rs  # OpenCode runner (NDJSON parser + free model discovery)
│       ├── task_init.rs     # Task setup: sidecar init, guard checks, worktree bootstrap
│       ├── session.rs       # tmux session lifecycle: create, watch, kill
│       ├── response_handler.rs  # Post-run: parse agent response, status resolution, comment posting
│       ├── fallback.rs      # Failover logic: cooldowns, reroute, agent weight signals
│       ├── response.rs      # Legacy response helpers (review parsing, memory storage)
│       └── git_ops.rs       # Auto-commit, push, PR creation, PR override detection
│
├── github/
│   ├── mod.rs               # GitHub helpers (shared by backend + channel)
│   ├── http.rs              # Native reqwest HTTP client (connection pooling, rate-limit backoff)
│   ├── backoff.rs           # Exponential backoff state for GitHub 403 rate limits
│   ├── types.rs             # GitHubIssue, GitHubComment, GitHubLabel, etc.
│   └── projects.rs          # GitHub Projects V2 GraphQL operations
│
├── db.rs                    # SQLite for internal tasks (schema + migrations)
├── sidecar.rs               # JSON sidecar file I/O + agent memory persistence
├── parser.rs                # Agent response normalization (JSON → AgentResponse)
├── template.rs              # Template rendering (env var substitution)
├── tmux.rs                  # TmuxManager (session create/kill/list/capture)
├── security.rs              # Secret scanning + redaction
├── home.rs                  # Home directory resolution (~/.orch/)
└── cron.rs                  # Native cron expression matching
```

---

## Feature Documentation

### Agent Memory System

**Location:** `src/sidecar.rs` (memory storage), `src/engine/runner/response.rs` (write), `src/engine/runner/context.rs` (read)

Agents persist learnings across retries so subsequent attempts don't repeat the same mistakes. Memory entries are stored in the task's sidecar JSON file (`~/.orch/state/{task_id}.json`) under the `"memory"` key.

**`MemoryEntry` structure:**

| Field | Type | Description |
|-------|------|-------------|
| `attempt` | `u32` | 1-indexed attempt number |
| `agent` | `String` | Agent that made the attempt |
| `model` | `Option<String>` | Model used |
| `learnings` | `Vec<String>` | Key learnings from the attempt |
| `error` | `Option<String>` | Error message if failed |
| `files_modified` | `Vec<String>` | Files changed |
| `approach` | `String` | Approach taken (from summary) |
| `timestamp` | `String` | ISO 8601 formatted |

**How it works:**

1. After each attempt, `store_memory()` or `store_failure_memory()` writes a `MemoryEntry` to the sidecar.
2. On retry, `build_memory_context()` in `context.rs` fetches the most recent entries and formats them as a markdown section in the agent prompt.
3. Max **3 entries** per task (`MAX_MEMORY_ENTRIES = 3`) to prevent context overflow.

### Notification System

**Location:** `src/channels/notification.rs`

Unified notification dispatch that broadcasts task completion events to all configured channels (Telegram, Discord). Notifications are filtered by a configurable level.

**Configuration:**

| Key | Values | Default | Description |
|-----|--------|---------|-------------|
| `notifications.level` | `all`, `errors_only`, `none` | `all` | Controls which events trigger notifications |

**`NotificationLevel` behavior:**

| Level | Notifies on |
|-------|------------|
| `all` | All task completions (done, in_review, needs_review, blocked, failed) |
| `errors_only` | Only `needs_review`, `blocked`, `failed` |
| `none` | Disabled entirely |

**`TaskNotification` fields:** `task_id`, `title`, `status`, `summary`, `agent`, `duration_seconds`

**Formatting:** Channel-specific formatters (`format_telegram()`, `format_discord()`) with status emoji mapping (✅ done, ⚠️ needs_review, 🚫 blocked, ❌ failed, etc.).

**Integration:** The notification dispatcher is spawned in the engine loop. After task completion, a notification is pushed, filtered by level, formatted per channel, and broadcast via the transport layer. GitHub notifications are handled separately (comments), and tmux is skipped.

### PR Review Integration

**Location:** `src/engine/review.rs` (`review_open_prs()`)

Automatically creates follow-up tasks when a PR receives a `CHANGES_REQUESTED` review. This closes the feedback loop between code review and agent execution.

**How it works:**

1. During the 120s sync tick, `review_open_prs()` lists all tasks with `Status::InReview`.
2. For each task, fetches PR reviews via the GitHub CLI.
3. Filters for `CHANGES_REQUESTED` reviews with actionable comments (non-empty, non-reply).
4. Creates internal follow-up tasks with these labels:
   - `pr-review-followup`
   - `status:new`
   - `agent:{original_agent}` — routes to the same agent that created the PR
5. Each follow-up task's sidecar stores: `pr_number`, `branch`, `reviewer`, `file_path`, `parent_task_id`.

**Deduplication:** Uses `review_comment_{pr_number}_{comment_id}` as a key to prevent duplicate follow-up tasks for the same review comment.

### Self-Review Job

**Location:** `src/engine/jobs.rs` (`run_self_review()`)

A scheduled job that analyzes task metrics and auto-creates GitHub issues for orchestrator improvements. Detects patterns of failure, recurring errors, and slow execution.

**Job type:** `self-review`

**Detection patterns:**

| Pattern | Function | What it detects |
|---------|----------|----------------|
| High failure rate agents | `detect_high_failure_agent()` | Agents with disproportionately high failure rates |
| Recurring errors | `detect_common_errors()` | Error patterns appearing across multiple tasks |
| Slow tasks | `detect_slow_tasks()` | Tasks taking significantly longer than average |

**Rate limiting:** Max **3 self-improvement issues per 7-day** rolling window (`MAX_SELF_IMPROVEMENT_ISSUES_PER_WEEK = 3`). Checked via `db.count_self_improvement_issues_7d()` before creating new issues.

**Metrics sources:**

| Source | Window | Purpose |
|--------|--------|---------|
| `get_metrics_summary_24h()` | 24 hours | Current performance snapshot |
| `get_slow_tasks_7d()` | 7 days | Historical execution time analysis |
| `get_error_distribution_7d()` | 7 days | Error pattern detection |

**Output:** Creates GitHub issues via `create_self_improvement_issue()` with detailed analysis and suggested fixes. Sets `job.last_task_status` to `"done"`, `"no_issues"`, or `"failed"`.

---

## Data Flow Examples

### User creates a task via Telegram

```
Telegram msg: "add a login page with OAuth"
  → TelegramChannel receives IncomingMessage
  → Transport.route() → MessageRoute::NewTask
  → Engine.create_task(title, body, type=github)
  → GitHub Issues API: POST /repos/owner/repo/issues
  → Engine.route_task() → agent=claude, complexity=medium
  → Engine.run_with_context() → Rust runner (worktree → agent → tmux session orch-myproject-42)
  → Transport.bind("42", "orch-myproject-42", "telegram", chat_id)
  → tmux bridge captures output → Transport.push_output()
  → Telegram channel streams output to chat
  → Agent finishes → runner posts result comment → all channels notified
```

### User replies to agent in Discord

```
Discord msg in task thread: "use shadcn instead of material ui"
  → DiscordChannel receives IncomingMessage
  → Transport.route() → MessageRoute::TaskSession { task_id: "42" }
  → Transport.tmux_session_for("42") → "orch-myproject-42"
  → tmux.send_keys("orch-myproject-42", "use shadcn instead of material ui\n")
  → Agent receives input, continues working
  → Output streams back to Discord thread
```

### Cron job fires (internal task)

```
Scheduler: job "morning-review" matches 08:00 UTC
  → Engine.create_task(title, body, type=internal)  ← SQLite, not GitHub
  → Engine.route_task() → agent=claude
  → Engine.run_with_context() → Rust runner (worktree → agent → tmux session orch-int-5)
  → No branch, no PR, no GitHub issue
  → Agent finishes → result stored in SQLite
  → Summary posted to Telegram (if configured)
```

### GitHub webhook: new issue

```
GitHub webhook: issues.opened
  → GitHubChannel receives IncomingMessage
  → Transport.route() → MessageRoute::NewTask
  → Engine.create_task() (already exists in GitHub, just set status:new)
  → Engine.route_task() → picks agent
  → Normal task flow (with branch + PR)
```

---

## Brew Upgrade Path

### Formula Structure (current)

```ruby
class Orch < Formula
  desc "The Agent Orchestrator"
  homepage "https://github.com/gabrielkoerich/orch"

  # Prebuilt Rust binary (cross-compiled in CI)
  if Hardware::CPU.arm?
    url "https://github.com/.../orch-aarch64-apple-darwin.tar.gz"
  else
    url "https://github.com/.../orch-x86_64-apple-darwin.tar.gz"
  end

  # No jq/yq/python3 dependencies — everything is native Rust
  depends_on "gh"  # GitHub CLI for API calls

  def install
    bin.install "orch"
    libexec.install Dir["prompts/*"]
  end

  service do
    run [opt_bin/"orch", "serve"]
    keep_alive true
    log_path var/"log/orch.log"
    error_log_path var/"log/orch.error.log"
  end
end
```

### Release Flow

```
push to main
  → CI: cargo test + cargo clippy + cargo fmt --check
  → CI: cargo build --release (macOS arm64 + x86_64)
  → CI: create universal binary (lipo)
  → CI: auto-tag (semver from conventional commits)
  → CI: GitHub release with orch binary + prompts tarball
  → CI: update homebrew-tap formula
  → brew upgrade orch
```

### Rollback

If a release has issues: `brew switch orch 0.x.y`. The binary is self-contained — no external scripts required.

---

## Name Change

| v0 | v1 |
|----|-----|
| `orchestrator` | `orch` |
| `gabrielkoerich/orchestrator` | `gabrielkoerich/orch` |
| `brew install orchestrator` | `brew install orch` |
| `orchestrator serve` | `orch serve` |
| `orchestrator task add` | `orch task add` |
| `~/.orchestrator/` | `~/.orch/` |
| `ORCH_HOME` | `ORCH_HOME` (unchanged) |
| `.orchestrator.yml` | `.orch.yml` (with backward compat) |

All renames are complete. Binary (`orch-core` → `orch`), directory (`~/.orchestrator/` → `~/.orch/` as separate real directory), and config (`.orchestrator.yml` → `.orch.yml`) are all done. The two tools (`orchestrator` and `orch`) run fully independently with separate home dirs, tmux sessions, and config.

---

## Config Architecture

### Design

Two layers: **global defaults** + **per-project overrides**.

**Global config: `~/.orch/config.yml`** — shared defaults and project registry.

```yaml
# Project registry — list of local paths
# Each path must contain a .orch.yml with gh.repo
projects:
  - /Users/gb/Projects/orch
  - /Users/gb/Projects/my-other-project

# Shared defaults (apply to all projects unless overridden)
workflow:
  auto_close: true
  review_owner: "@owner"
  max_attempts: 10
  timeout_seconds: 1800

router:
  mode: "round_robin"
  timeout_seconds: 60
  fallback_executor: "minimax"
  allowed_tools: [yq, jq, bash, just, git, rg, ...]

model_map:
  simple: { claude: haiku, codex: gpt-5.1-codex-mini }
  medium: { claude: opus, codex: gpt-5.2 }
  complex: { claude: opus, codex: gpt-5.3-codex }

agents:
  claude: { allowed_tools: [...] }
  opencode: { permission: {...}, models: [...] }

git:
  name: "orch[bot]"
  email: "orch@orch.bot"
```

**Per-project config: `{project_path}/.orch.yml`** — project-specific settings.

```yaml
# REQUIRED — identifies this project on GitHub
gh:
  repo: "owner/repo"
  project_id: "PVT_..."           # optional: GitHub Projects V2
  project_status_field_id: "..."   # optional
  project_status_map: { ... }      # optional

# Optional overrides (merge on top of global defaults)
workflow:
  auto_close: false

router:
  fallback_executor: "codex"

required_tools:
  - cargo

# Per-project scheduled jobs
jobs:
  - id: code-review
    schedule: "0 4,17 * * *"
    task: { title: "Code review", body: "...", labels: [review] }
    enabled: true
```

### Resolution order

1. Read `projects:` from `~/.orch/config.yml` → list of paths
2. For each path, read `{path}/.orch.yml` → get `gh.repo`, overrides, jobs
3. Per-project values override global defaults for the same key
4. CLI commands resolve project from CWD (find nearest `.orch.yml` walking up)

### What was removed

| Old key | Where it was | Replacement |
|---------|-------------|-------------|
| `project_dir` (global) | `~/.orch/config.yml` | `projects:` list (paths) |
| `gh.repo` (global) | `~/.orch/config.yml` | Per-project `.orch.yml` |
| `projects.yml` | `~/.orch/projects.yml` | `projects:` in `config.yml` |
| `repo` (top-level) | `~/.orch/config.yml` | Per-project `gh.repo` |

### CLI naming

| Command | Purpose |
|---------|---------|
| `orch project add <path>` | Register a project path in global config |
| `orch project remove <path>` | Unregister a project from global config |
| `orch project list` | List all registered projects with repo + status |
| `orch board list/link/sync/info` | GitHub Projects V2 board management |
| `orch init` | Initialize `.orch.yml` in current directory |

Note: `orch board` manages GitHub Projects V2 boards. `orch project` manages the multi-project registry in global config.

---

## Parity Audit — Feature Gaps

Last updated: 2026-03-03 (366 tests, ~98% parity)

### Completed

| Feature | Module | PR |
|---------|--------|----|
| GitHub Projects V2 integration | `src/github/projects.rs` | — |
| Per-task artifact folders (per-repo, per-attempt) | `src/home.rs`, `src/engine/runner/` | — |
| Per-repo state isolation | `~/.orch/state/{owner}/{repo}/tasks/{id}/` | — |
| Config architecture (multi-project) | `src/config/mod.rs` | PR #82, #78 |
| `orch board list/link/sync/info` CLI | `src/cli/mod.rs` | — |
| Project board auto-sync on status change | `src/backends/github.rs` | — |
| Auto-clone repos during `orch init` | `src/cli/mod.rs` | PR #141 |
| Separate `~/.orch/` from `~/.orchestrator/` | `src/home.rs` | — |
| Project-aware tmux naming (`orch-{project}-{id}`) | `src/tmux.rs`, `src/engine/mod.rs` | — |
| Simplified service management (brew wrapper) | `src/cli/service.rs` | — |
| Permission rules per-agent translation | `src/engine/runner/agents/` | — |
| PR review integration | `src/engine/review.rs` | PR #125 |
| Agent memory across retries | `src/sidecar.rs` | PR #122 |
| Self-improvement loop | `src/engine/jobs.rs` | PR #120 |
| Polling fallback for webhooks | `src/channels/github.rs` | PR #131 |

### Remaining Gaps

| Feature | Status | Priority | Notes |
|---------|--------|----------|-------|
| `orch project add/remove/list` CLI | Implemented | Done | See `src/cli/mod.rs:484-710` |
| Wire Telegram/Discord into engine loop | Implemented | Done | See `src/channels/telegram.rs`, `src/channels/discord.rs`, `src/engine/mod.rs:251-296` |
| Mention detection via webhooks | Implemented | Done | Polling works, webhook receives events via `start_webhook_server()` in `src/channels/github.rs` |
| Review Agent + Auto-Merge | Implemented | Done | See `src/engine/review.rs` `review_and_merge()` function |
| PR Review Comments → Fix Dispatch | Implemented | Done | See `src/engine/review.rs` `review_open_prs()` function |
| Dashboard CLI | Implemented | Done | See `src/cli/dashboard.rs` - `orch dashboard` command |
| Task Tree CLI | Implemented | Done | See `src/cli/tree.rs` - `orch task tree` command |
| Owner commands (issue comment commands) | Implemented | Done | Issue #179 - see `src/engine/commands.rs` for `/retry`, `/reroute`, `/block` |
| Child task delegation (auto-spawn subtasks) | Implemented | Done | Issue #178 - see `src/engine/runner/mod.rs:1003-1070` |
| Skills Sync (auto-clone skill repos) | Implemented | Done | See `skills_sync()` in `src/engine/sync.rs:263` (PR #158) |
| Merge detection (auto-close after PR merge) | Implemented | Done | See `check_merged_prs()` in `src/engine/cleanup.rs` |
| Graceful shutdown with session handoff | Implemented | Done | See `src/engine/mod.rs` serve() loop |
| Slack channel integration | Implemented | Done | `src/channels/slack.rs` — polling + `chat.postMessage`, wired in `src/engine/mod.rs:311-323` |
| Context file per issue | Implemented | Done | See `src/engine/runner/context.rs:40-45` `load_task_context()` |

### Config Architecture

Redesigned from single-global to multi-project:
- Global `~/.orch/config.yml`: shared defaults + `projects:` list of local paths
- Per-project `.orch.yml`: `gh.repo`, jobs, project-specific overrides
- No more `project_dir`, `gh.repo`, or `projects.yml` at global level
- CLI resolves project context from CWD

### GitHub Projects V2

The old orchestrator had 4 dedicated scripts for project board management. `orch` now has:
- `ProjectSync` struct with GraphQL operations (discover fields, add items, update status)
- Automatic project board column sync when task status changes (non-fatal)
- CLI: `orch board list`, `orch board link <id>`, `orch board sync`, `orch board info`
- Config stored in per-project `.orch.yml` under `gh.project_id`, `gh.project_status_field_id`, `gh.project_status_map`

### Per-Task Artifact Folders

Old layout (flat): `~/.orch/state/prompt-42-sys.txt`, `runner-42.sh`, etc.

New layout (per-repo, per-task, per-attempt):
```
~/.orch/state/{owner}/{repo}/tasks/{id}/
  sidecar.json
  attempts/
    1/
      prompt-sys.md
      prompt-msg.md
      exit.txt
      stderr.txt
      output.json
    2/  (retry)
      ...
```

PTY runner removes `runner.sh` from the attempt folder; legacy tmux command mode still uses an in-memory shell command but does not write scripts to disk.

Benefits: per-repo isolation (no issue number collisions), per-attempt separation (retries don't overwrite), easy cleanup. Legacy flat paths still work as fallback for reads.

---

## Key Dependencies

| Crate | Purpose | Status |
|-------|---------|--------|
| `tokio` | Async runtime | In use |
| ~~`portable-pty`~~ | ~~PTY spawning for agent runner~~ | **Removed** — tmux IS the PTY (PR #420); do not reintroduce |
| `serde` / `serde_json` / `serde_yml` | Serialization | In use |
| `clap` / `clap_complete` | CLI parsing + shell completions | In use |
| `chrono` | Timestamps | In use |
| `tracing` / `tracing-subscriber` | Structured logging | In use |
| `anyhow` / `thiserror` | Error handling | In use |
| `sha2` | Hashing (dedup) | In use |
| `cron` | Cron expressions | In use |
| `rusqlite` | Internal task SQLite DB | In use |
| `reqwest` | HTTP client | In use |
| `regex` | Secret/leak detection | In use |
| `notify` | Config file watching | In use |
| `which` | Agent CLI discovery | In use |
| `futures` | Async combinators | In use |
| `dirs` | XDG directory helpers | In use |
| `async-trait` | Async trait support | In use |
| `urlencoding` | URL encoding for labels | In use |
| `axum` | Webhook HTTP server | In use |
| `teloxide` | Telegram bot | Not used — implemented via raw HTTP polling in `src/channels/telegram.rs` |
| `serenity` | Discord bot | Not used — implemented via raw HTTP polling in `src/channels/discord.rs` |
| `cargo-llvm-cov` | Coverage tracking in CI | CI tooling |

---

## TODO — Remaining Work

*No open items — all planned features implemented.*

### Recently Closed

| Issue | Title | Description |
|-------|-------|-------------|
| #361 | PR coverage comments | `romeovs/lcov-reporter-action@v0.4.0` comments coverage % on each PR — `.github/workflows/release.yml:52-57` |
| #230 | Break `tick()` into named phases | Extracted 4-5 phases of the `tick()` function into independent methods. |
| #144 | Cost Tracking CLI and Budget Enforcement | `orch cost` command in `src/cli/cost.rs` — per-task and aggregate cost reporting |
| #145 | Webhook Server Production Hardening | Graceful shutdown, webhook event handling in `src/channels/github.rs` |
| #316 | `orch stream` live output never arrives | `stream_task()` now starts its own `CaptureService` to poll tmux — `src/cli/mod.rs:304-316` |
| #317 | Webhook deduplication | In-memory `x-github-delivery` dedup in `src/channels/github.rs` — 2h eviction window, silently ACKs duplicate deliveries |
| #112 | Wire webhook server into engine | Webhook server integrated via `start_webhook_server()` in `src/channels/github.rs` |
| #178 | Task delegation processing | `src/engine/runner/mod.rs:1003-1070` — spawns child tasks from `delegations` field |
| #179 | Owner slash commands | `src/engine/commands.rs` — `/retry`, `/reroute [agent]`, `/block [reason]`, `/unblock`, `/close`, `/review` |
| #143 | PR Review Integration | `src/engine/review.rs` — processes `changes_requested` reviews and re-dispatches tasks |
| - | Review Agent + Auto-Merge | `review_and_merge()` in `src/engine/review.rs` |
| - | Merge Detection | `check_merged_prs()` in `src/engine/cleanup.rs` |
| #479 | Worktree janitor: reliably clean internal task worktrees | `JanitorOptions` (TTL, dry-run) + tmux session guard + fallback path fix in `src/engine/cleanup.rs` — janitor now reads `workflow.worktree_janitor_ttl_hours` / `workflow.worktree_janitor_dry_run` from config, checks filesystem mtime, and skips any worktree referenced by an active tmux pane |
| - | Dashboard CLI | `orch dashboard` in `src/cli/dashboard.rs` |
| - | Task Tree CLI | `orch task tree` in `src/cli/tree.rs` |
| - | Graceful Shutdown | SIGTERM/SIGINT handlers in `src/engine/mod.rs` (serve loop) |

### Code Quality

- [x] `src/engine/mod.rs` — decomposed into tick.rs, sync.rs, review.rs, cleanup.rs (#283)
- [x] `src/engine/router.rs` — extracted LLM routing into `router/llm.rs` (`LlmRouter` struct) (#257)
- [x] `src/engine/runner/mod.rs` — decomposed into task_init.rs, session.rs, response_handler.rs, fallback.rs (#295)
- [x] `cargo-llvm-cov` added to CI (#258) — runs on every push via `release.yml`, uploads LCOV artifact; `romeovs/lcov-reporter-action` posts coverage % on PRs

### Feature Gaps (Low Priority)

- [x] Skills sync from config (auto-clone skill repos) — see `src/engine/sync.rs:263` (PR #158)
- [x] Slack channel integration — `src/channels/slack.rs` implemented and wired into engine

