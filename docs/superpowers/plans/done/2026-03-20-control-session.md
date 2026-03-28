# Control Session (`orch chat`) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `orch chat` — a conversational control plane backed by SQLite message history and one-shot agent invocations.

**Architecture:** Each message triggers a standalone agent process (`claude -p`, `codex -q`, etc.) with context assembled from SQLite (recent summaries + memories + live state). Full conversation history is stored and searchable. Model selection is sticky via KV store.

**Tech Stack:** Rust, sqlx (SQLite), tokio, clap, existing agent runner patterns

---

## File Structure

| File | Responsibility |
|------|---------------|
| `migrations/006_control_session.sql` | New tables: `control_messages`, `control_state` |
| `src/store.rs` | CRUD methods for control_messages and control_state |
| `src/control.rs` (new) | Context assembly + one-shot agent invocation + response handling |
| `src/cli/chat.rs` (new) | CLI handlers: single message, REPL, history |
| `src/cli/mod.rs` | Add `pub mod chat;` |
| `src/main.rs` | Add `Chat` command variant + dispatch |
| `prompts/control_system.md` (new) | Static system prompt template for control session |

---

### Task 1: SQLite Migration

**Files:**
- Create: `migrations/006_control_session.sql`

- [ ] **Step 1: Write the migration file**

```sql
-- Control session: full conversation history
CREATE TABLE IF NOT EXISTS control_messages (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    role            TEXT NOT NULL,       -- 'user', 'assistant'
    channel         TEXT NOT NULL,       -- 'cli', 'telegram', 'discord'
    channel_thread  TEXT,                -- thread/topic ID for reply routing
    content         TEXT NOT NULL,       -- full message text
    summary         TEXT,                -- one-line summary for context assembly
    model           TEXT,                -- which model responded (NULL for user)
    agent           TEXT,                -- which agent CLI was used
    tokens_used     INTEGER,
    cost_usd        REAL,
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_control_messages_created
    ON control_messages(created_at);
CREATE INDEX IF NOT EXISTS idx_control_messages_role
    ON control_messages(role, created_at);
```

- [ ] **Step 2: Verify migration runs**

Run: `cargo build 2>&1 | tail -5`
Expected: compiles (sqlx picks up new migration at compile time)

- [ ] **Step 3: Commit**

```bash
git add migrations/006_control_session.sql
git commit -m "feat(db): add control_messages table for chat history"
```

---

### Task 2: Store Methods for Control Messages

**Files:**
- Modify: `src/store.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `src/store.rs`:

```rust
#[tokio::test]
async fn control_insert_and_list() {
    let store = TaskStore::open_memory().await.unwrap();
    store
        .insert_control_message("user", "cli", None, "what's running?", None, None, None, None, None)
        .await
        .unwrap();
    store
        .insert_control_message("assistant", "cli", None, "3 tasks active", Some("listed tasks"), Some("sonnet"), Some("claude"), Some(500), Some(0.01))
        .await
        .unwrap();

    let messages = store.list_control_messages(10, 0).await.unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[1].role, "assistant");
    assert_eq!(messages[1].summary.as_deref(), Some("listed tasks"));
}

#[tokio::test]
async fn control_search_messages() {
    let store = TaskStore::open_memory().await.unwrap();
    store
        .insert_control_message("user", "cli", None, "check bean auth issue", None, None, None, None, None)
        .await
        .unwrap();
    store
        .insert_control_message("user", "cli", None, "unblock trading tasks", None, None, None, None, None)
        .await
        .unwrap();

    let results = store.search_control_messages("bean", 10).await.unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].content.contains("bean"));
}

#[tokio::test]
async fn control_recent_summaries() {
    let store = TaskStore::open_memory().await.unwrap();
    store
        .insert_control_message("assistant", "cli", None, "long response", Some("did X"), Some("sonnet"), Some("claude"), None, None)
        .await
        .unwrap();
    store
        .insert_control_message("assistant", "cli", None, "another response", Some("did Y"), Some("sonnet"), Some("claude"), None, None)
        .await
        .unwrap();

    let summaries = store.control_recent_summaries(5).await.unwrap();
    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries[0], "did X");
    assert_eq!(summaries[1], "did Y");
}

#[tokio::test]
async fn control_model_state_via_kv() {
    let store = TaskStore::open_memory().await.unwrap();
    // Default: no model set
    assert_eq!(store.kv_get("control:model").await.unwrap(), None);

    // Set model
    store.kv_set("control:model", "sonnet").await.unwrap();
    assert_eq!(store.kv_get("control:model").await.unwrap(), Some("sonnet".to_string()));

    // Switch model
    store.kv_set("control:model", "opus").await.unwrap();
    assert_eq!(store.kv_get("control:model").await.unwrap(), Some("opus".to_string()));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run control_insert 2>&1 | tail -5`
Expected: FAIL — `insert_control_message` method not found

- [ ] **Step 3: Add ControlMessage struct and store methods**

Add to `src/store.rs` (after the Job State section):

```rust
// ---------------------------------------------------------------
// Control Session
// ---------------------------------------------------------------

/// A message in the control session conversation history.
#[derive(Debug, Clone)]
pub struct ControlMessage {
    pub id: i64,
    pub role: String,
    pub channel: String,
    pub channel_thread: Option<String>,
    pub content: String,
    pub summary: Option<String>,
    pub model: Option<String>,
    pub agent: Option<String>,
    pub tokens_used: Option<i64>,
    pub cost_usd: Option<f64>,
    pub created_at: String,
}

impl TaskStore {
    /// Insert a control session message.
    pub async fn insert_control_message(
        &self,
        role: &str,
        channel: &str,
        channel_thread: Option<&str>,
        content: &str,
        summary: Option<&str>,
        model: Option<&str>,
        agent: Option<&str>,
        tokens_used: Option<i64>,
        cost_usd: Option<f64>,
    ) -> anyhow::Result<i64> {
        let row = sqlx::query(
            "INSERT INTO control_messages (role, channel, channel_thread, content, summary, model, agent, tokens_used, cost_usd)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(role)
        .bind(channel)
        .bind(channel_thread)
        .bind(content)
        .bind(summary)
        .bind(model)
        .bind(agent)
        .bind(tokens_used)
        .bind(cost_usd)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get("id"))
    }

    /// List recent control messages (newest last).
    pub async fn list_control_messages(
        &self,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<ControlMessage>> {
        let rows = sqlx::query(
            "SELECT * FROM control_messages ORDER BY created_at ASC LIMIT ? OFFSET ?",
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(Self::row_to_control_message).collect())
    }

    /// Search control messages by content (LIKE match).
    pub async fn search_control_messages(
        &self,
        query: &str,
        limit: i64,
    ) -> anyhow::Result<Vec<ControlMessage>> {
        let pattern = format!("%{query}%");
        let rows = sqlx::query(
            "SELECT * FROM control_messages WHERE content LIKE ? ORDER BY created_at DESC LIMIT ?",
        )
        .bind(&pattern)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(Self::row_to_control_message).collect())
    }

    /// Get recent assistant message summaries for context assembly.
    pub async fn control_recent_summaries(&self, limit: i64) -> anyhow::Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT summary FROM control_messages WHERE summary IS NOT NULL ORDER BY created_at ASC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| r.get::<String, _>("summary")).collect())
    }

    fn row_to_control_message(row: &sqlx::sqlite::SqliteRow) -> ControlMessage {
        use sqlx::Row;
        ControlMessage {
            id: row.get("id"),
            role: row.get("role"),
            channel: row.get("channel"),
            channel_thread: row.get("channel_thread"),
            content: row.get("content"),
            summary: row.get("summary"),
            model: row.get("model"),
            agent: row.get("agent"),
            tokens_used: row.get("tokens_used"),
            cost_usd: row.get("cost_usd"),
            created_at: row.get("created_at"),
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo nextest run control_ 2>&1 | tail -10`
Expected: all 4 control tests PASS

- [ ] **Step 5: Commit**

```bash
git add src/store.rs
git commit -m "feat(store): add control_messages CRUD methods"
```

---

### Task 3: System Prompt Template

**Files:**
- Create: `prompts/control_system.md`

- [ ] **Step 1: Write the prompt template**

```markdown
You are the orch control session — an interactive ops assistant for orch.

You can run commands to manage tasks, check status, and take actions. Use bash for all commands.

## Available Commands

- `orch task list [--status STATUS]` — list tasks (statuses: new, routed, in_progress, needs_review, in_review, done, blocked)
- `orch task list --source internal` — list internal (cron-created) tasks
- `orch task add "title" --body "description"` — create a new task
- `orch task unblock <id>` — unblock a stuck task
- `orch task unblock all` — unblock all blocked tasks
- `orch task retry <id>` — retry a failed task
- `orch task get <id>` — get task details
- `orch stats` — show task metrics and statistics
- `orch cost` — show cost tracking and token usage
- `orch stream <task_id>` — stream live output from a running task
- `orch dashboard` — combined dashboard: tasks, sessions, recent activity
- `gh pr list` — list open pull requests
- `gh run list` — list CI workflow runs
- `gh issue list` — list open issues

## Searching Conversation History

You can search past conversations in SQLite:
```bash
sqlite3 ~/.orch/orch.db "SELECT created_at, role, content FROM control_messages WHERE content LIKE '%search_term%' ORDER BY created_at DESC LIMIT 10"
```

## Current State
{current_state}

## Memories
{memories}

## Recent Conversation
{recent_summaries}

## Response Format

Respond naturally and concisely. After your response, output a summary tag on its own line:

<summary>one-line summary of what happened in this exchange</summary>

If the user tells you to remember something, acknowledge it (the system will store it).
```

- [ ] **Step 2: Commit**

```bash
git add prompts/control_system.md
git commit -m "feat: add control session system prompt template"
```

---

### Task 4: Control Module — Context Assembly & One-Shot Invocation

**Files:**
- Create: `src/control.rs`
- Modify: `src/main.rs` (add `mod control;`)

- [ ] **Step 1: Write the test for context assembly**

Create `src/control.rs` with a test at the bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::TaskStore;

    #[tokio::test]
    async fn assemble_context_includes_memories_and_summaries() {
        let store = TaskStore::open_memory().await.unwrap();
        store.kv_set("control:memory:tz", "User is in BRT timezone").await.unwrap();
        store
            .insert_control_message("assistant", "cli", None, "response", Some("checked status"), Some("sonnet"), Some("claude"), None, None)
            .await
            .unwrap();

        let ctx = assemble_context(&store).await.unwrap();
        assert!(ctx.contains("BRT timezone"), "should include memories");
        assert!(ctx.contains("checked status"), "should include summaries");
    }

    #[test]
    fn extract_summary_from_response() {
        let response = "Here are your tasks.\n\n<summary>listed 3 active tasks</summary>";
        let (clean, summary) = parse_response(response);
        assert_eq!(summary.as_deref(), Some("listed 3 active tasks"));
        assert!(!clean.contains("<summary>"));
    }

    #[test]
    fn extract_summary_missing() {
        let response = "No tasks found.";
        let (clean, summary) = parse_response(response);
        assert_eq!(summary, None);
        assert_eq!(clean, "No tasks found.");
    }

    #[test]
    fn resolve_agent_from_model() {
        assert_eq!(agent_for_model("sonnet"), "claude");
        assert_eq!(agent_for_model("opus"), "claude");
        assert_eq!(agent_for_model("haiku"), "claude");
        assert_eq!(agent_for_model("gpt-4o"), "codex");
        assert_eq!(agent_for_model("o3"), "codex");
        assert_eq!(agent_for_model("deepseek-r1"), "opencode");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Add `mod control;` to `src/main.rs` (after `mod config;`).

Run: `cargo nextest run assemble_context 2>&1 | tail -5`
Expected: FAIL — functions not defined

- [ ] **Step 3: Implement the control module**

Write the full `src/control.rs`:

```rust
//! Control session — one-shot agent invocations with SQLite-backed context.

use crate::store::TaskStore;
use anyhow::Context;
use std::path::PathBuf;

/// Default model when none is set.
const DEFAULT_MODEL: &str = "sonnet";

/// Max recent summaries included in context.
const MAX_SUMMARIES: i64 = 20;

/// Resolve which agent CLI to use for a given model name.
pub fn agent_for_model(model: &str) -> &'static str {
    let m = model.to_lowercase();
    if m.contains("gpt") || m.contains("o1") || m.contains("o3") || m.contains("o4") || m.contains("codex") {
        "codex"
    } else if m.contains("deepseek") || m.contains("qwen") {
        "opencode"
    } else {
        // claude models: sonnet, opus, haiku, or any unknown → default to claude
        "claude"
    }
}

/// Assemble the system prompt from template + live state + memories + recent summaries.
pub async fn assemble_context(store: &TaskStore) -> anyhow::Result<String> {
    // Read the template
    let template = include_str!("../prompts/control_system.md");

    // Gather memories from KV (keys starting with "control:memory:")
    let memories = gather_memories(store).await?;

    // Gather recent summaries
    let summaries = store.control_recent_summaries(MAX_SUMMARIES).await?;
    let summaries_text = if summaries.is_empty() {
        "(no recent conversation)".to_string()
    } else {
        summaries.join("\n")
    };

    // Gather live state (best-effort, don't fail if orch commands aren't available)
    let current_state = gather_live_state().await;

    let context = template
        .replace("{current_state}", &current_state)
        .replace("{memories}", &memories)
        .replace("{recent_summaries}", &summaries_text);

    Ok(context)
}

/// Gather persistent memories from KV store.
async fn gather_memories(store: &TaskStore) -> anyhow::Result<String> {
    // Query all control:memory:* keys
    let rows = sqlx::query("SELECT key, value FROM kv WHERE key LIKE 'control:memory:%' ORDER BY key")
        .fetch_all(store.pool())
        .await?;

    if rows.is_empty() {
        return Ok("(no memories stored)".to_string());
    }

    use sqlx::Row;
    let entries: Vec<String> = rows
        .iter()
        .map(|r| {
            let value: String = r.get("value");
            format!("- {value}")
        })
        .collect();

    Ok(entries.join("\n"))
}

/// Gather live state by running orch commands.
async fn gather_live_state() -> String {
    let output = tokio::process::Command::new("orch")
        .args(["task", "list"])
        .output()
        .await;

    match output {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            if text.trim().is_empty() || text.contains("No tasks found") {
                "No active tasks.".to_string()
            } else {
                format!("```\n{}\n```", text.trim())
            }
        }
        _ => "(could not fetch live state)".to_string(),
    }
}

/// Get the current model from KV, falling back to default.
pub async fn get_model(store: &TaskStore) -> String {
    store
        .kv_get("control:model")
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| DEFAULT_MODEL.to_string())
}

/// Set the current model in KV.
pub async fn set_model(store: &TaskStore, model: &str) -> anyhow::Result<()> {
    store.kv_set("control:model", model).await
}

/// Parse the agent response to extract the clean text and optional summary.
pub fn parse_response(raw: &str) -> (String, Option<String>) {
    // Look for <summary>...</summary> tag
    if let Some(start) = raw.find("<summary>") {
        if let Some(end) = raw.find("</summary>") {
            let summary = raw[start + 9..end].trim().to_string();
            let clean = format!(
                "{}{}",
                raw[..start].trim_end(),
                raw[end + 10..].trim_start()
            )
            .trim()
            .to_string();
            return (clean, Some(summary));
        }
    }
    (raw.trim().to_string(), None)
}

/// Invoke an agent one-shot and return the raw response text.
pub async fn invoke_agent(
    agent: &str,
    model: &str,
    context: &str,
    message: &str,
) -> anyhow::Result<String> {
    // Write context and message to temp files
    let tmp_dir = std::env::temp_dir().join("orch-control");
    tokio::fs::create_dir_all(&tmp_dir).await?;
    let sys_file = tmp_dir.join("system.md");
    let msg_file = tmp_dir.join("message.md");
    tokio::fs::write(&sys_file, context).await?;
    tokio::fs::write(&msg_file, message).await?;

    let output = match agent {
        "claude" => {
            tokio::process::Command::new("claude")
                .args([
                    "-p",
                    "--model", model,
                    "--permission-mode", "bypassPermissions",
                    "--output-format", "text",
                    "--append-system-prompt", sys_file.to_str().unwrap(),
                ])
                .stdin(std::process::Stdio::from(
                    std::fs::File::open(&msg_file)
                        .context("open message file")?,
                ))
                .output()
                .await
                .context("failed to invoke claude")?
        }
        "codex" => {
            // codex reads from stdin: system prompt + message concatenated
            let combined = format!("{context}\n\n---\n\n{message}");
            tokio::fs::write(&msg_file, &combined).await?;
            tokio::process::Command::new("codex")
                .args(["--model", model, "--full-auto", "-q"])
                .stdin(std::process::Stdio::from(
                    std::fs::File::open(&msg_file)
                        .context("open message file")?,
                ))
                .output()
                .await
                .context("failed to invoke codex")?
        }
        "opencode" => {
            let combined = format!("{context}\n\n---\n\n{message}");
            tokio::fs::write(&msg_file, &combined).await?;
            tokio::process::Command::new("opencode")
                .args(["run", "--format", "text", "-m", model, "-"])
                .stdin(std::process::Stdio::from(
                    std::fs::File::open(&msg_file)
                        .context("open message file")?,
                ))
                .output()
                .await
                .context("failed to invoke opencode")?
        }
        _ => anyhow::bail!("unknown agent: {agent}"),
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("agent exited with {}: {stderr}", output.status);
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// High-level: send a message to the control session and get a response.
///
/// 1. Store user message
/// 2. Assemble context
/// 3. Resolve model/agent
/// 4. Invoke agent
/// 5. Parse response (extract summary)
/// 6. Store assistant message
/// 7. Return clean response text
pub async fn send_message(
    store: &TaskStore,
    channel: &str,
    channel_thread: Option<&str>,
    message: &str,
) -> anyhow::Result<String> {
    // Handle /model command
    if let Some(new_model) = message.strip_prefix("/model ").map(str::trim) {
        if new_model.is_empty() {
            let current = get_model(store).await;
            return Ok(format!("Current model: {current}"));
        }
        set_model(store, new_model).await?;
        let agent = agent_for_model(new_model);
        return Ok(format!("Switched to {new_model} ({agent})"));
    }

    // Store user message
    store
        .insert_control_message("user", channel, channel_thread, message, None, None, None, None, None)
        .await?;

    // Assemble context
    let context = assemble_context(store).await?;

    // Resolve model/agent
    let model = get_model(store).await;
    let agent = agent_for_model(&model);

    // Invoke agent
    let raw_response = invoke_agent(agent, &model, &context, message).await?;

    // Parse response
    let (clean_response, summary) = parse_response(&raw_response);

    // Store assistant message
    store
        .insert_control_message(
            "assistant",
            channel,
            channel_thread,
            &clean_response,
            summary.as_deref(),
            Some(&model),
            Some(agent),
            None, // tokens — TODO: parse from agent output
            None, // cost — TODO: parse from agent output
        )
        .await?;

    Ok(clean_response)
}
```

- [ ] **Step 4: Add `mod control;` to main.rs**

In `src/main.rs`, add after `mod config;`:
```rust
mod control;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo nextest run -E 'test(control)' 2>&1 | tail -10`
Expected: all control module tests PASS

- [ ] **Step 6: Run full test suite**

Run: `cargo nextest run 2>&1 | tail -5`
Expected: all tests PASS

- [ ] **Step 7: Commit**

```bash
git add src/control.rs src/main.rs
git commit -m "feat: control session module — context assembly, agent invocation, response parsing"
```

---

### Task 5: CLI — `orch chat` Command

**Files:**
- Create: `src/cli/chat.rs`
- Modify: `src/cli/mod.rs` (add `pub mod chat;`)
- Modify: `src/main.rs` (add `Chat` variant to Commands enum + dispatch)

- [ ] **Step 1: Add the Chat command to the Commands enum**

In `src/main.rs`, add the `Chat` variant after the `Stream` variant:

```rust
    /// Chat with orch control session
    Chat {
        #[command(subcommand)]
        action: Option<ChatAction>,
    },
```

Add the ChatAction enum (next to the other action enums):

```rust
#[derive(Subcommand)]
enum ChatAction {
    /// Search conversation history
    History {
        /// Search term
        #[arg(long)]
        search: Option<String>,
        /// Show messages since (e.g., "1d", "7d", "2026-03-20")
        #[arg(long)]
        since: Option<String>,
        /// Max results
        #[arg(long, default_value = "20")]
        limit: i64,
    },
}
```

Add dispatch in the match block:

```rust
Commands::Chat { action } => match action {
    Some(ChatAction::History { search, since, limit }) => {
        cli::chat::history(search, since, limit).await?;
    }
    None => {
        cli::chat::interactive().await?;
    }
},
```

- [ ] **Step 2: Add `pub mod chat;` to cli/mod.rs**

In `src/cli/mod.rs`, add after existing module declarations:

```rust
pub mod chat;
```

- [ ] **Step 3: Create the chat CLI handler**

Create `src/cli/chat.rs`:

```rust
//! CLI handlers for `orch chat` — control session interaction.

use crate::control;
use crate::store::TaskStore;
use std::io::{self, BufRead, Write};

/// Interactive REPL mode — reads from stdin, sends to control session.
pub async fn interactive() -> anyhow::Result<()> {
    let store = crate::cli::init_store().await?;
    let model = control::get_model(&store).await;
    let agent = control::agent_for_model(&model);

    println!("orch control session ({agent}/{model})");
    println!("Type /model <name> to switch models, Ctrl+C to exit");
    println!("---");

    let stdin = io::stdin();
    let mut stdout = io::stdout();

    loop {
        print!("orch> ");
        stdout.flush()?;

        let mut line = String::new();
        let bytes = stdin.lock().read_line(&mut line)?;
        if bytes == 0 {
            // EOF
            break;
        }

        let message = line.trim();
        if message.is_empty() {
            continue;
        }
        if message == "exit" || message == "quit" {
            break;
        }

        match control::send_message(&store, "cli", None, message).await {
            Ok(response) => {
                println!("{response}");
                println!();
            }
            Err(e) => {
                eprintln!("error: {e}");
            }
        }
    }

    Ok(())
}

/// Show conversation history.
pub async fn history(
    search: Option<String>,
    _since: Option<String>,
    limit: i64,
) -> anyhow::Result<()> {
    let store = crate::cli::init_store().await?;

    let messages = if let Some(query) = search {
        store.search_control_messages(&query, limit).await?
    } else {
        store.list_control_messages(limit, 0).await?
    };

    if messages.is_empty() {
        println!("No messages found.");
        return Ok(());
    }

    for msg in &messages {
        let role_label = match msg.role.as_str() {
            "user" => "you",
            "assistant" => msg
                .agent
                .as_deref()
                .unwrap_or("assistant"),
            _ => &msg.role,
        };
        let model_info = msg
            .model
            .as_deref()
            .map(|m| format!(" ({m})"))
            .unwrap_or_default();

        println!("[{}] {}{}", msg.created_at, role_label, model_info);
        // Indent content for readability
        for line in msg.content.lines() {
            println!("  {line}");
        }
        println!();
    }

    Ok(())
}
```

- [ ] **Step 4: Build and verify**

Run: `cargo build 2>&1 | tail -5`
Expected: compiles

- [ ] **Step 5: Check help output**

Run: `cargo run -- chat --help 2>&1`
Expected: shows Chat subcommand with History action

- [ ] **Step 6: Run clippy**

Run: `cargo clippy --all-targets -- -D warnings 2>&1 | tail -5`
Expected: no warnings

- [ ] **Step 7: Run full test suite**

Run: `cargo nextest run 2>&1 | tail -5`
Expected: all tests PASS

- [ ] **Step 8: Commit**

```bash
git add src/cli/chat.rs src/cli/mod.rs src/main.rs
git commit -m "feat: orch chat CLI — interactive REPL and history browsing"
```

---

### Task 6: Single-Message Mode

**Files:**
- Modify: `src/cli/chat.rs`
- Modify: `src/main.rs` (update Chat variant to accept optional message)

- [ ] **Step 1: Update the Chat command to accept a positional message**

In `src/main.rs`, update the `Chat` variant:

```rust
    /// Chat with orch control session
    Chat {
        /// Send a single message (omit for interactive mode)
        message: Vec<String>,
        #[command(subcommand)]
        action: Option<ChatAction>,
    },
```

Update dispatch:

```rust
Commands::Chat { action, message } => match action {
    Some(ChatAction::History { search, since, limit }) => {
        cli::chat::history(search, since, limit).await?;
    }
    None if !message.is_empty() => {
        cli::chat::single_message(&message.join(" ")).await?;
    }
    None => {
        cli::chat::interactive().await?;
    }
},
```

- [ ] **Step 2: Add single_message handler to chat.rs**

```rust
/// Single message mode — send one message, print response, exit.
pub async fn single_message(message: &str) -> anyhow::Result<()> {
    let store = crate::cli::init_store().await?;

    let response = control::send_message(&store, "cli", None, message).await?;
    println!("{response}");

    Ok(())
}
```

- [ ] **Step 3: Verify it compiles and help looks right**

Run: `cargo run -- chat --help 2>&1`
Expected: shows `orch chat [MESSAGE]...` with optional message args

- [ ] **Step 4: Run full checks**

Run: `cargo fmt -- --check && cargo clippy --all-targets -- -D warnings && cargo nextest run 2>&1 | tail -5`
Expected: all pass

- [ ] **Step 5: Commit**

```bash
git add src/cli/chat.rs src/main.rs
git commit -m "feat: orch chat single-message mode — orch chat 'what is running?'"
```

---

### Task 7: Integration Test — End to End

**Files:**
- Modify: `src/control.rs` (add integration test)

- [ ] **Step 1: Write an integration test for send_message with /model**

Add to `src/control.rs` tests:

```rust
#[tokio::test]
async fn send_message_model_switch() {
    let store = TaskStore::open_memory().await.unwrap();

    // Default model
    let model = get_model(&store).await;
    assert_eq!(model, "sonnet");

    // Switch model via /model command
    let response = send_message(&store, "cli", None, "/model opus").await.unwrap();
    assert!(response.contains("opus"));
    assert_eq!(get_model(&store).await, "opus");

    // Show current model
    let response = send_message(&store, "cli", None, "/model ").await.unwrap();
    assert!(response.contains("opus"));
}

#[tokio::test]
async fn send_message_stores_user_message() {
    let store = TaskStore::open_memory().await.unwrap();

    // /model doesn't store messages (it's a command, not a conversation)
    let _ = send_message(&store, "cli", None, "/model haiku").await.unwrap();
    let messages = store.list_control_messages(10, 0).await.unwrap();
    assert_eq!(messages.len(), 0);
}
```

- [ ] **Step 2: Run tests**

Run: `cargo nextest run send_message 2>&1 | tail -10`
Expected: PASS

- [ ] **Step 3: Run full test suite and clippy**

Run: `cargo fmt -- --check && cargo clippy --all-targets -- -D warnings && cargo nextest run 2>&1 | tail -5`
Expected: all pass

- [ ] **Step 4: Commit**

```bash
git add src/control.rs
git commit -m "test: integration tests for control session model switching"
```

---

## Summary

| Task | What | Commit |
|------|------|--------|
| 1 | SQLite migration — `control_messages` table | `feat(db): add control_messages table` |
| 2 | Store CRUD methods + tests | `feat(store): add control_messages CRUD` |
| 3 | System prompt template | `feat: control session system prompt` |
| 4 | Control module — context, invocation, parsing | `feat: control session module` |
| 5 | `orch chat` CLI — REPL + history | `feat: orch chat CLI` |
| 6 | Single-message mode | `feat: orch chat single-message mode` |
| 7 | Integration tests | `test: control session integration` |

Channel integration (Telegram/Discord routing) is Phase 2 — documented in spec but not implemented here.
