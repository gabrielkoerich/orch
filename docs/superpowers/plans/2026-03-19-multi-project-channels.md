# Multi-Project Channel Routing — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Route Telegram forum topics and Discord channels to specific projects, with interactive project picker, per-project notifications, `/stats` command, and `orch stats` CLI.

**Architecture:** Per-project channel config in `.orch.yml` maps topic/channel IDs to repos. Engine builds reverse lookup at startup. Channels gain topic-aware send/receive. Notifications route to dedicated channels automatically plus subscribed channels. New migration adds `repo` to `task_metrics` and `channel_subscriptions` table.

**Tech Stack:** Rust, SQLite (sqlx), Telegram Bot API (forum topics, inline keyboards, callback queries), Discord Gateway (multi-channel, interaction buttons)

**Spec:** `docs/superpowers/specs/2026-03-19-multi-project-channels-design.md`

---

### Task 1: Migration — add `repo` to `task_metrics` + `channel_subscriptions` table

**Files:**
- Create: `migrations/005_channel_routing.sql`

- [ ] **Step 1: Write migration**

```sql
-- Add repo column to task_metrics for per-project stats
ALTER TABLE task_metrics ADD COLUMN repo TEXT DEFAULT '';

-- Channel notification subscriptions
CREATE TABLE IF NOT EXISTS channel_subscriptions (
    channel   TEXT NOT NULL,   -- "telegram", "discord"
    thread_id TEXT NOT NULL,   -- topic_id or channel_id
    repo      TEXT NOT NULL,
    PRIMARY KEY (channel, thread_id, repo)
);
```

- [ ] **Step 2: Verify migration runs**

Run: `cargo nextest run store`
Expected: All store tests pass (migration runs on open_memory)

- [ ] **Step 3: Commit**

```bash
git add migrations/005_channel_routing.sql
git commit -m "feat: add migration for channel routing (repo on task_metrics, channel_subscriptions)"
```

---

### Task 2: Store methods — subscription CRUD + per-repo metrics

**Files:**
- Modify: `src/store.rs`

- [ ] **Step 1: Write tests for subscription CRUD**

Add to `src/store.rs` test module:

```rust
#[tokio::test]
async fn subscribe_and_list_channel_subscriptions() {
    let store = TaskStore::open_memory().await.unwrap();
    store.subscribe_channel("telegram", "42", "owner/orch").await.unwrap();
    store.subscribe_channel("telegram", "42", "owner/bean").await.unwrap();
    let subs = store.list_channel_subscriptions("telegram", "42").await.unwrap();
    assert_eq!(subs.len(), 2);
}

#[tokio::test]
async fn unsubscribe_channel() {
    let store = TaskStore::open_memory().await.unwrap();
    store.subscribe_channel("telegram", "42", "owner/orch").await.unwrap();
    store.unsubscribe_channel("telegram", "42", "owner/orch").await.unwrap();
    let subs = store.list_channel_subscriptions("telegram", "42").await.unwrap();
    assert_eq!(subs.len(), 0);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run subscribe`
Expected: FAIL — methods don't exist

- [ ] **Step 3: Implement subscription methods**

Add to `impl TaskStore`:

```rust
pub async fn subscribe_channel(&self, channel: &str, thread_id: &str, repo: &str) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO channel_subscriptions (channel, thread_id, repo) VALUES (?, ?, ?)"
    )
    .bind(channel).bind(thread_id).bind(repo)
    .execute(&self.pool).await?;
    Ok(())
}

pub async fn unsubscribe_channel(&self, channel: &str, thread_id: &str, repo: &str) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM channel_subscriptions WHERE channel = ? AND thread_id = ? AND repo = ?")
    .bind(channel).bind(thread_id).bind(repo)
    .execute(&self.pool).await?;
    Ok(())
}

pub async fn list_channel_subscriptions(&self, channel: &str, thread_id: &str) -> anyhow::Result<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT repo FROM channel_subscriptions WHERE channel = ? AND thread_id = ?"
    )
    .bind(channel).bind(thread_id)
    .fetch_all(&self.pool).await?;
    Ok(rows.into_iter().map(|r| r.0).collect())
}

pub async fn list_subscribers_for_repo(&self, repo: &str) -> anyhow::Result<Vec<(String, String)>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT channel, thread_id FROM channel_subscriptions WHERE repo = ?"
    )
    .bind(repo)
    .fetch_all(&self.pool).await?;
    Ok(rows)
}
```

- [ ] **Step 4: Add per-repo metrics query**

```rust
pub async fn get_metrics_summary_24h_by_repo(&self, repo: &str) -> anyhow::Result<MetricsSummary> {
    // Same as get_metrics_summary_24h but with WHERE repo = ? on all queries
    // Join task_metrics with tasks table on task_id to get repo
    // OR use the new repo column on task_metrics directly
}
```

- [ ] **Step 5: Run tests**

Run: `cargo nextest run subscribe && cargo nextest run metrics`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add src/store.rs
git commit -m "feat: add channel subscription CRUD and per-repo metrics"
```

---

### Task 3: Project-channel mapping config

**Files:**
- Modify: `src/config/mod.rs` (or wherever project config is read)
- Create: `src/channels/routing.rs`

- [ ] **Step 1: Write the routing map struct and builder**

```rust
//! Channel-to-project routing map.
//!
//! Built at engine startup from per-project `.orch.yml` configs.
//! Provides reverse lookup: (channel, topic/channel_id) → repo.

use std::collections::HashMap;

/// Maps channel targets to projects.
pub struct ChannelRouter {
    /// (channel_name, topic_or_channel_id) → repo
    target_to_repo: HashMap<(String, String), String>,
    /// repo → { channel_name: topic_or_channel_id }
    repo_to_targets: HashMap<String, HashMap<String, String>>,
    /// General channel IDs per channel type
    general: HashMap<String, String>,
}
```

- [ ] **Step 2: Implement builder from project configs**

`ChannelRouter::from_projects(projects: &[(String, ProjectChannelConfig)])` reads each project's `.orch.yml` `channels:` section, builds both maps, stores general IDs from global config.

- [ ] **Step 3: Implement lookup methods**

```rust
impl ChannelRouter {
    /// Given a channel message, resolve which project it belongs to.
    pub fn resolve_project(&self, channel: &str, topic_or_channel_id: &str) -> Option<&str>;

    /// Check if a target is the General channel.
    pub fn is_general(&self, channel: &str, topic_or_channel_id: &str) -> bool;

    /// Get the target (topic/channel ID) for a project on a given channel.
    pub fn target_for_project(&self, repo: &str, channel: &str) -> Option<&str>;

    /// List all configured projects.
    pub fn projects(&self) -> Vec<&str>;
}
```

- [ ] **Step 4: Write tests**

```rust
#[test]
fn resolve_project_from_topic() {
    let router = ChannelRouter::from_projects(&[
        ("owner/orch".into(), ProjectChannelConfig { telegram_topic_id: Some("42".into()), .. }),
        ("owner/bean".into(), ProjectChannelConfig { telegram_topic_id: Some("87".into()), .. }),
    ]);
    assert_eq!(router.resolve_project("telegram", "42"), Some("owner/orch"));
    assert_eq!(router.resolve_project("telegram", "99"), None);
}
```

- [ ] **Step 5: Run tests and commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo nextest run routing
git add src/channels/routing.rs src/config/mod.rs
git commit -m "feat: add ChannelRouter for project-channel mapping"
```

---

### Task 4: Telegram forum topic support

**Files:**
- Modify: `src/channels/telegram.rs`
- Modify: `src/channels/mod.rs` (add `topic_id` to IncomingMessage/OutgoingMessage)

- [ ] **Step 1: Add `topic_id` to message types**

In `src/channels/mod.rs`, add `pub topic_id: Option<String>` to both `IncomingMessage` and `OutgoingMessage`.

- [ ] **Step 2: Update TelegramMessage struct to include `message_thread_id`**

```rust
#[derive(Deserialize)]
struct TelegramMessage {
    message_id: i64,
    from: Option<TelegramUser>,
    chat: TelegramChat,
    text: Option<String>,
    date: i64,
    #[serde(default)]
    message_thread_id: Option<i64>,  // Forum topic ID
}
```

- [ ] **Step 3: Pass `message_thread_id` as `topic_id` in IncomingMessage**

In the polling loop, set `topic_id: msg.message_thread_id.map(|id| id.to_string())`.

- [ ] **Step 4: Update `send_message` to support `message_thread_id`**

```rust
async fn send_message(&self, chat_id: i64, text: &str, topic_id: Option<i64>) -> anyhow::Result<()> {
    let mut params = serde_json::json!({
        "chat_id": chat_id,
        "text": text,
        "parse_mode": "Markdown"
    });
    if let Some(tid) = topic_id {
        params["message_thread_id"] = serde_json::json!(tid);
    }
    // ... rest unchanged
}
```

- [ ] **Step 5: Update `send()` trait impl to use `msg.topic_id`**

- [ ] **Step 6: Add `callback_query` to `allowed_updates` and handle it**

Update `get_updates` params:
```rust
"allowed_updates": ["message", "callback_query"]
```

Add `CallbackQuery` struct and handle in polling loop — produce `IncomingMessage` with `metadata: { "callback_query_id": "...", "callback_data": "..." }`.

- [ ] **Step 7: Add `send_inline_keyboard` method for project picker**

```rust
async fn send_inline_keyboard(&self, chat_id: i64, topic_id: Option<i64>, text: &str, buttons: &[(String, String)]) -> anyhow::Result<i64> {
    // Returns message_id for tracking
}
```

- [ ] **Step 8: Add `answer_callback_query` method**

```rust
async fn answer_callback_query(&self, callback_query_id: &str, text: &str) -> anyhow::Result<()> {}
```

- [ ] **Step 9: Update all callers of `send_message` for new signature**

- [ ] **Step 10: Run full test suite and commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo nextest run
git add src/channels/mod.rs src/channels/telegram.rs
git commit -m "feat: telegram forum topic support with inline keyboards"
```

---

### Task 5: Discord multi-channel support

**Files:**
- Modify: `src/channels/discord_ws.rs`

- [ ] **Step 1: Remove single `channel_id` filter**

Change `channel_id: Option<String>` to accept messages from any channel in the guild. The `ChannelRouter` handles project resolution instead of filtering at the gateway level.

- [ ] **Step 2: Set `channel_id` as `topic_id` on IncomingMessage**

The Discord message's `channel_id` becomes `topic_id` (for consistency with the routing model).

- [ ] **Step 3: Update `send()` to target specific channel**

Use `msg.topic_id` (or `msg.thread_id`) as the Discord channel ID for the REST POST.

- [ ] **Step 4: Add button support for project picker**

Add `send_with_components` method that includes Discord action row buttons. Handle `INTERACTION_CREATE` gateway events for button clicks.

- [ ] **Step 5: Run full test suite and commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo nextest run
git add src/channels/discord_ws.rs
git commit -m "feat: discord multi-channel support with button interactions"
```

---

### Task 6: Engine message routing with ChannelRouter

**Files:**
- Modify: `src/engine/mod.rs`

- [ ] **Step 1: Build ChannelRouter at engine startup**

After reading project configs, construct `ChannelRouter` from all projects' `.orch.yml` channel sections. Pass it to `handle_channel_message`.

- [ ] **Step 2: Resolve project from incoming message**

In `handle_channel_message`, use `router.resolve_project(msg.channel, msg.topic_id)` to determine which project the message targets. Pass `repo` to task creation and command handling.

- [ ] **Step 3: Update NewTask handling**

- If project is resolved → create task in that project
- If General channel → show project picker (inline buttons), wait for callback, then create task

- [ ] **Step 4: Update Command handling**

- `/status` in dedicated channel → filter by that project's repo
- `/status` in General → show all projects grouped
- `/subscribe <project>` → validate project exists, insert subscription
- `/unsubscribe <project>` → remove subscription

- [ ] **Step 5: Add `/stats` command**

Query `get_metrics_summary_24h_by_repo()` and format as chat message. In dedicated channel → single project. In General → all projects.

- [ ] **Step 6: Add `/stream <task_id>` command**

Bind the current channel/topic to the task's output stream.

- [ ] **Step 7: Run full test suite and commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo nextest run
git add src/engine/mod.rs
git commit -m "feat: engine message routing with ChannelRouter and new commands"
```

---

### Task 7: Notification routing

**Files:**
- Modify: `src/engine/mod.rs` (notification broadcast section)
- Modify: `src/channels/notification.rs`

- [ ] **Step 1: Add project prefix to notification format**

Add `repo: Option<String>` to `TaskNotification`. When set, `format_*` methods prepend `[project-name]`.

- [ ] **Step 2: Route notifications to dedicated channels**

When a task completes, look up its repo in `ChannelRouter`, get the target topic/channel, send notification there (no prefix needed).

- [ ] **Step 3: Route notifications to subscribed channels**

Query `list_subscribers_for_repo(repo)` from store, send notification to each (with project prefix).

- [ ] **Step 4: General channel gets notifications only for subscribed projects**

- [ ] **Step 5: Run full test suite and commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo nextest run
git add src/engine/mod.rs src/channels/notification.rs
git commit -m "feat: per-project notification routing with subscriptions"
```

---

### Task 8: `orch stats` CLI command

**Files:**
- Create: `src/cli/stats.rs`
- Modify: `src/main.rs` (add Stats command)
- Modify: `src/cli/mod.rs` (expose stats module)

- [ ] **Step 1: Write the stats display function**

```rust
pub async fn stats(all: bool) -> anyhow::Result<()> {
    let store = Arc::new(crate::cli::init_store().await?);

    if all {
        // Aggregate across all repos
        let summary = store.get_metrics_summary_24h().await?;
        print_summary_table("All Projects", &summary);
    } else {
        // Per-project tables
        let repos = get_configured_repos()?;
        for repo in &repos {
            let summary = store.get_metrics_summary_24h_by_repo(repo).await?;
            print_summary_table(repo, &summary);
        }
    }
    Ok(())
}
```

- [ ] **Step 2: Add Stats to CLI commands**

In `src/main.rs`, add:
```rust
/// Show task metrics and statistics
Stats {
    /// Aggregate all projects into one table
    #[arg(long)]
    all: bool,
},
```

And in match:
```rust
Commands::Stats { all } => {
    cli::stats::stats(all).await?;
}
```

- [ ] **Step 3: Test manually**

Run: `orch stats` and `orch stats --all`

- [ ] **Step 4: Run full test suite and commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo nextest run
git add src/cli/stats.rs src/cli/mod.rs src/main.rs
git commit -m "feat: add orch stats CLI command with per-project breakdown"
```

---

### Task 9: Backfill `repo` on `task_metrics` + record repo on insert

**Files:**
- Modify: `src/store.rs` (`insert_task_metric`)

- [ ] **Step 1: Add `repo` field to `InsertTaskMetric`**

```rust
pub struct InsertTaskMetric<'a> {
    pub repo: &'a str,  // NEW
    pub task_id: &'a str,
    // ... rest unchanged
}
```

- [ ] **Step 2: Update insert query to include `repo`**

- [ ] **Step 3: Update all callers to pass `repo`**

Search for `insert_task_metric` calls and add the repo parameter.

- [ ] **Step 4: Run full test suite and commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo nextest run
git add src/store.rs src/engine/
git commit -m "feat: record repo on task_metrics for per-project stats"
```

---

### Task 10: Integration test — end-to-end channel routing

**Files:**
- Create: `tests/channel_routing.rs` (or add to existing integration tests)

- [ ] **Step 1: Write test for ChannelRouter build + resolve**

- [ ] **Step 2: Write test for subscription persistence**

- [ ] **Step 3: Write test for per-repo metrics query**

- [ ] **Step 4: Run all tests and commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo nextest run
git add tests/
git commit -m "test: integration tests for multi-project channel routing"
```
