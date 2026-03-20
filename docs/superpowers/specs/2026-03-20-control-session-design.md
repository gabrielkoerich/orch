# Control Session — Design Spec

## Goal

A persistent conversational control plane for the orchestrator. Users talk to it from any channel (CLI, Telegram, Discord) and it knows everything that's running, can create tasks, check status, unblock things — like talking to an ops assistant that never loses context.

## How It Works

Each message triggers a **one-shot agent invocation** (`claude -p`, `codex -q`, etc.) with context assembled from SQLite. No long-running session — stateless process, stateful database.

```
message arrives
  → store user message in SQLite
  → assemble context: live state + memories + recent message summaries
  → resolve model/agent from control_state table
  → invoke agent one-shot with assembled context
  → parse response
  → store assistant message + summary in SQLite
  → send response back to channel
```

### Why One-Shot

- No crash recovery or session babysitting needed
- No tmux session management
- SQLite is the single source of truth
- Each invocation is independent — easy to switch models between calls
- Concurrency: SQLite handles concurrent access natively

## Storage (SQLite)

All state lives in `~/.orch/orch.db` alongside existing tables.

### Schema

```sql
-- Full conversation history (every message preserved, searchable)
CREATE TABLE control_messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    role TEXT NOT NULL,            -- 'user', 'assistant'
    channel TEXT NOT NULL,         -- 'cli', 'telegram', 'discord'
    channel_thread TEXT,           -- thread/topic ID for reply routing
    content TEXT NOT NULL,         -- full message text
    summary TEXT,                  -- one-line summary for context assembly
    model TEXT,                    -- which model responded (NULL for user messages)
    agent TEXT,                    -- which agent CLI was used
    tokens_used INTEGER,
    cost_usd REAL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

-- Persistent key-value state (model preference, memories, etc.)
CREATE TABLE control_state (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
```

### Key-Value State

| Key | Example Value | Purpose |
|-----|--------------|---------|
| `model` | `sonnet` | Current sticky model |
| `agent` | `claude` | Derived from model or explicit override |
| `memory:bean_pref` | `User prefers bean trading tasks to use codex` | Persistent preference |
| `memory:schedule` | `Morning reviews run at 7am BRT` | Persistent fact |

### Context Assembly

On each invocation, context is built dynamically:

```
1. Live state: run `orch task list`, `orch stats` (always fresh)
2. Model/agent: read from control_state
3. Memories: SELECT * FROM control_state WHERE key LIKE 'memory:%'
4. Recent conversation: SELECT summary FROM control_messages ORDER BY created_at DESC LIMIT 20
5. Compose system prompt → invoke agent
```

The full message history stays in SQLite forever. Only summaries go into the context window.

### History Search

The agent can search past conversations:

```sql
-- "What did I say about bean auth last week?"
SELECT content, created_at FROM control_messages
WHERE content LIKE '%bean%auth%'
AND created_at > datetime('now', '-7 days')
ORDER BY created_at DESC;
```

Exposed via: `orch chat history --search "bean auth" --since 7d`

Or the agent can query it directly via bash tool during invocation.

## Model & Agent Selection

- **Sticky model**: persisted in `control_state` table (`key='model'`)
- **Switch via command**: `/model opus` → updates `control_state`, next invocation uses it
- **Agent derived from model**: sonnet/opus/haiku → claude, gpt-* → codex — same `model_map` logic from config
- **Or explicit**: `/agent codex` overrides, picks default model for that agent
- **Initial default**: from config (`control.model`) or `sonnet` if unset

## One-Shot Runner

Extract from existing runner a simpler invocation path:

```rust
pub struct OneShotInvocation {
    pub agent: String,         // "claude", "codex", "opencode"
    pub model: String,         // "sonnet", "opus", etc.
    pub system_prompt: String, // assembled context
    pub user_message: String,  // the incoming message
    pub allowed_tools: Vec<String>,
    pub cwd: Option<String>,   // working directory (project dir or ~/.orch)
}

pub struct OneShotResult {
    pub response: String,
    pub summary: Option<String>,  // extracted from <summary> tag
    pub tokens_used: Option<u64>,
    pub cost: Option<f64>,
}
```

Agent CLI mapping:

| Agent | Invocation |
|-------|-----------|
| claude | `claude -p --model {model} --system-prompt {file} --allowedTools {tools} "{message}"` |
| codex | `codex --model {model} -q "{system_prompt}\n\n{message}"` |
| opencode | `opencode -p --model {model} "{system_prompt}\n\n{message}"` |

Key differences from task runner:
- No tmux session (direct stdout capture via `Command::output()`)
- No worktree (runs in project dir or `~/.orch`)
- Synchronous request-response (wait for exit, capture output)

## Tools Available to Control Session

The agent needs to run orch commands and query history:

- `bash` — `orch task list`, `orch task unblock`, `gh`, `git log`, etc.
- `read` — logs, config, project files
- `write` — only for generating reports/artifacts if needed

The system prompt tells the agent it's the orch control session and lists available commands.

## Channel Integration

### Routing

Messages that don't match a running task session go to the control session. This is the **default route** — no prefix needed.

In `Transport::route()`:

```
1. Check thread → task binding (existing behavior)
2. Check if it's an orch command like /task, /model (existing behavior)
3. → Control session (NEW — instead of NewTask)
```

Alternative: dedicated channel/topic for control (e.g., Telegram topic "Control", Discord channel `#orch`). Simpler routing, clearer separation.

### CLI

**Single message:** `orch chat "what's running?"`

**Interactive REPL:** `orch chat` opens a prompt loop:

```
$ orch chat
orch> what's running?
3 tasks active:
  internal:42 — bean trading update (in_progress, codex, 2min)
  internal:43 — morning review (in_review, claude)
  #127 — fix auth middleware (routed, waiting for dispatch)

orch> unblock all bean tasks
Done. Unblocked 2 tasks.

orch> /model opus
Switched to opus.

orch> what did we discuss yesterday about the auth issue?
[searches SQLite history]
Yesterday at 14:30 you asked about the auth middleware rewrite...
```

**History browsing:** `orch chat history --since 1d` or `orch chat history --search "bean"`

### Telegram/Discord

Messages in the control channel/topic invoke the control session. Response sent back to the same thread.

## System Prompt

Static template at `prompts/control_system.md`, composed with dynamic state:

```markdown
You are the orch control session — an ops assistant for the orchestrator.

## Available Commands (via bash)
- `orch task list [--status STATUS]` — list tasks
- `orch task create "title"` — create a new task
- `orch task unblock <id>` — unblock a task
- `orch task retry <id>` — retry a failed task
- `orch stats` — show statistics
- `orch cost` — show cost tracking
- `gh pr list` — list PRs
- `gh run list` — list CI runs
- `sqlite3 ~/.orch/orch.db "SELECT ..."` — query conversation history

## Current State
{live_state_injected_here}

## Memories
{memories_from_control_state}

## Recent Conversation
{last_20_message_summaries}

## Instructions
- After responding, output a <summary>one-line summary</summary> tag
- If the user tells you to remember something, store it as a memory
- You can search conversation history in SQLite for older context
```

## Message Queue

Multiple channels might send messages simultaneously:

- SQLite WAL mode handles concurrent reads
- Write lock during invocation (one active invocation at a time)
- If an invocation is running, queue incoming messages in `control_messages` with `role='user'`
- Process queue after current invocation completes
- Response routes back to originating channel via `channel` + `channel_thread` columns

## Configuration

```yaml
control:
  enabled: true
  model: "sonnet"           # initial default model (overridden by /model)
  context_messages: 20      # how many recent summaries to include
  allowed_tools:
    - bash
    - read
    - write
```

## What This Is NOT

- Not a task agent — doesn't write code, doesn't work in worktrees
- Not a router replacement — doesn't decide which agent handles tasks
- Not always-on — only active when someone talks to it
- Not multi-turn in the traditional sense — each message is independent, SQLite provides continuity

## Implementation Order

1. **SQLite schema** — migration V4: `control_messages` + `control_state` tables
2. **Context assembler** — build system prompt from live state + SQLite data
3. **One-shot runner** — extract from existing runner, agent-agnostic single invocation
4. **`orch chat` CLI** — single message mode + interactive REPL
5. **`/model` command** — update model in `control_state`, reflect in next invocation
6. **History commands** — `orch chat history --search/--since`
7. **Channel routing** — wire Telegram/Discord unmatched messages to control session
