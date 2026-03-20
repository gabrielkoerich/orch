# Extract Channel Message Handler Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move all channel message handler functions from `src/engine/mod.rs` into a new `src/engine/channel_handler.rs`, shrinking mod.rs from ~1915 to ~1300 lines.

**Architecture:** Pure structural refactor — no behaviour changes. Extract 8 functions (lines 1193–1783) into a dedicated module. `EngineRef` type alias stays in `mod.rs` and is imported by `channel_handler.rs` via `use super::EngineRef`. The call site in `mod.rs` is updated to `channel_handler::handle_channel_message(...)`.

**Tech Stack:** Rust, Tokio, existing crate types (`Transport`, `ChannelRegistry`, `ChannelRouter`, `TmuxManager`, `CaptureService`).

---

## File Structure

| Action | File | Responsibility |
|--------|------|----------------|
| Create | `src/engine/channel_handler.rs` | All 8 handler functions extracted from mod.rs |
| Modify | `src/engine/mod.rs` | Add `pub mod channel_handler;`, remove extracted functions, update call site |

---

### Task 1: Create `src/engine/channel_handler.rs`

**Files:**
- Create: `src/engine/channel_handler.rs`

This task is a pure move — no new logic. The functions being moved are:
- `send_channel_reply` (mod.rs line 1198)
- `forward_to_tmux` (mod.rs line 1232)
- `handle_channel_message` (mod.rs line 1261) — main public entry point
- `handle_status_command` (mod.rs line 1584)
- `handle_stats_command` (mod.rs line 1614)
- `handle_subscribe_command` (mod.rs line 1685)
- `handle_unsubscribe_command` (mod.rs line 1731)
- `handle_stream_command` (mod.rs line 1760)

- [ ] **Step 1: Create `src/engine/channel_handler.rs` with imports and all 8 functions**

```rust
//! Channel message handler — routes incoming channel messages to the
//! appropriate action (task session, command, control session, or new task).
//!
//! This module is called from the engine message-dispatch loop in
//! [`super::serve`] and has no dependency on the engine tick loop.

use std::sync::Arc;

use crate::backends::Status;
use crate::channels::capture::CaptureService;
use crate::channels::routing::ChannelRouter;
use crate::channels::transport::Transport;
use crate::channels::{ChannelRegistry, IncomingMessage, OutgoingMessage};
use crate::github::http::GhHttp;
use crate::tmux::TmuxManager;

use super::EngineRef;

/// Send a reply message to a specific channel thread.
///
/// Iterates the channel registry and finds the channel by name,
/// then sends the message to the given thread ID.
/// Optionally sends to a specific topic (e.g. Telegram forum topic).
pub(super) async fn send_channel_reply(
    channels: &Arc<ChannelRegistry>,
    channel_name: &str,
    thread_id: &str,
    body: String,
    topic_id: Option<&str>,
) {
    for ch in channels.iter() {
        if ch.name() == channel_name {
            let msg = OutgoingMessage {
                thread_id: thread_id.to_string(),
                body,
                reply_to: None,
                metadata: serde_json::json!({}),
                topic_id: topic_id.map(String::from),
            };
            if let Err(e) = ch.send(&msg).await {
                tracing::warn!(
                    channel = channel_name,
                    thread_id,
                    ?e,
                    "failed to send channel reply"
                );
            }
            return;
        }
    }
    tracing::debug!(
        channel = channel_name,
        "channel not found in registry for reply"
    );
}

/// Forward a text message to an agent's tmux session via send-keys.
pub(super) async fn forward_to_tmux(transport: &Arc<Transport>, task_id: &str, text: &str) {
    if let Some(binding) = transport.get_binding(task_id).await {
        if let Err(e) = crate::channels::tmux::send_keys(&binding.tmux_session, text).await {
            tracing::warn!(
                task_id,
                session = %binding.tmux_session,
                ?e,
                "failed to forward message to tmux session"
            );
        } else {
            tracing::debug!(
                task_id,
                session = %binding.tmux_session,
                "forwarded message to tmux"
            );
        }
    } else {
        tracing::warn!(task_id, "no tmux binding found, cannot forward message");
    }
}

/// Handle an incoming channel message by routing it to the appropriate action.
///
/// - `TaskSession`: slash command → execute on task; otherwise → forward to tmux
/// - `Command`: global commands like `/status`, `/stats`, `/subscribe`, `/unsubscribe`, `/stream`
/// - `NewTask`: create an internal task, bind thread, start output fanout
///
/// The `channel_router` resolves which project a message targets based on
/// its topic/channel ID. This enables per-project task creation and status views.
pub(super) async fn handle_channel_message(
    msg: IncomingMessage,
    transport: &Arc<Transport>,
    _tmux: &Arc<TmuxManager>,
    capture: &Arc<CaptureService>,
    channels: &Arc<ChannelRegistry>,
    engine_refs: &[EngineRef],
    channel_router: &Arc<ChannelRouter>,
) {
    use crate::backends::ExternalId;
    use crate::channels::stream::fanout_output;
    use crate::channels::transport::MessageRoute;
    use crate::engine::commands::{execute_command, parse_command};
    use crate::engine::tasks::{CreateTaskRequest, TaskType};

    // Resolve project from incoming message topic/channel
    let topic_id = msg.topic_id.as_deref().unwrap_or(&msg.thread_id);
    let resolved_repo = channel_router.resolve_project(&msg.channel, topic_id);
    let is_general = channel_router.is_general(&msg.channel, topic_id);
    let is_control = channel_router.is_control_channel(&msg.channel, topic_id);
    let msg_topic_id = msg.topic_id.clone();

    // Check control channel BEFORE task bindings so a control channel message
    // never accidentally falls through to task session routing.
    let route = if is_control {
        MessageRoute::ControlSession
    } else {
        transport.route(&msg).await
    };

    match route {
        MessageRoute::TaskSession { task_id } => {
            let body = msg.body.trim().to_string();
            let channel = msg.channel.clone();
            let thread_id = msg.thread_id.clone();

            if body.starts_with('/') {
                // Parse slash command and execute it on the bound task
                if let Some(cmd) = parse_command(&body) {
                    if let Some((repo, backend, task_manager, store)) = engine_refs.first() {
                        let gh = match GhHttp::new() {
                            Ok(gh) => gh,
                            Err(e) => {
                                tracing::warn!(error = %e, "failed to build HTTP client for command execution");
                                send_channel_reply(
                                    channels,
                                    &channel,
                                    &thread_id,
                                    format!("Command `{cmd}` failed: {e}"),
                                    msg_topic_id.as_deref(),
                                )
                                .await;
                                return;
                            }
                        };
                        let ext_id = ExternalId(task_id.clone());
                        let result =
                            execute_command(backend, &gh, repo, &ext_id, &cmd, store, task_manager)
                                .await;
                        let reply = match result {
                            Ok(r) => r,
                            Err(e) => format!("Command `{cmd}` failed: {e}"),
                        };
                        send_channel_reply(
                            channels,
                            &channel,
                            &thread_id,
                            reply,
                            msg_topic_id.as_deref(),
                        )
                        .await;
                    }
                } else {
                    // Unknown command — forward to agent as-is
                    forward_to_tmux(transport, &task_id, &body).await;
                }
            } else {
                // Regular message — forward to the agent's tmux session
                forward_to_tmux(transport, &task_id, &body).await;
            }
        }

        MessageRoute::Command { raw } => {
            let cmd_str = raw.trim().to_string();
            let channel = msg.channel.clone();
            let thread_id = msg.thread_id.clone();

            if cmd_str == "/status" || cmd_str.starts_with("/status ") {
                // /status — project-aware: show tasks for resolved project or all
                let reply = handle_status_command(engine_refs, resolved_repo).await;
                send_channel_reply(
                    channels,
                    &channel,
                    &thread_id,
                    reply,
                    msg_topic_id.as_deref(),
                )
                .await;
            } else if cmd_str == "/stats" || cmd_str.starts_with("/stats ") {
                let reply = handle_stats_command(engine_refs, resolved_repo, is_general).await;
                send_channel_reply(
                    channels,
                    &channel,
                    &thread_id,
                    reply,
                    msg_topic_id.as_deref(),
                )
                .await;
            } else if cmd_str.starts_with("/subscribe") {
                let reply = handle_subscribe_command(
                    &cmd_str,
                    &channel,
                    &thread_id,
                    engine_refs,
                    channel_router,
                )
                .await;
                send_channel_reply(
                    channels,
                    &channel,
                    &thread_id,
                    reply,
                    msg_topic_id.as_deref(),
                )
                .await;
            } else if cmd_str.starts_with("/unsubscribe") {
                let reply = handle_unsubscribe_command(
                    &cmd_str,
                    &channel,
                    &thread_id,
                    engine_refs,
                    channel_router,
                )
                .await;
                send_channel_reply(
                    channels,
                    &channel,
                    &thread_id,
                    reply,
                    msg_topic_id.as_deref(),
                )
                .await;
            } else if cmd_str.starts_with("/stream") {
                let reply = handle_stream_command(&cmd_str, &channel, &thread_id, transport).await;
                send_channel_reply(
                    channels,
                    &channel,
                    &thread_id,
                    reply,
                    msg_topic_id.as_deref(),
                )
                .await;
            } else if let Some(cmd) = parse_command(&cmd_str) {
                let reply = format!(
                    "Command `{cmd}` requires a task context. \
                     Send it in a thread that is bound to a running task."
                );
                send_channel_reply(
                    channels,
                    &channel,
                    &thread_id,
                    reply,
                    msg_topic_id.as_deref(),
                )
                .await;
            } else {
                let reply = "Available commands: /status, /stats, /subscribe, /unsubscribe, \
                             /stream, /retry, /close, /block, /unblock, /review — \
                             send task-specific commands in a task thread."
                    .to_string();
                send_channel_reply(
                    channels,
                    &channel,
                    &thread_id,
                    reply,
                    msg_topic_id.as_deref(),
                )
                .await;
            }
        }

        MessageRoute::ControlSession => {
            let channel = msg.channel.clone();
            let thread_id = msg.thread_id.clone();
            let body = msg.body.clone();

            // Session ID is per channel+topic so Telegram and Discord are isolated.
            let session_id = format!("{}:{}", channel, topic_id);

            // Find a store from the first available engine reference.
            let store = engine_refs
                .iter()
                .find_map(|(_, _, _, s)| s.as_ref())
                .cloned();

            if let Some(store) = store {
                match crate::control::send_message(
                    &store,
                    &session_id,
                    &channel,
                    Some(&thread_id),
                    &body,
                )
                .await
                {
                    Ok(reply) => {
                        send_channel_reply(
                            channels,
                            &channel,
                            &thread_id,
                            reply,
                            msg_topic_id.as_deref(),
                        )
                        .await;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "control session agent invocation failed");
                        send_channel_reply(
                            channels,
                            &channel,
                            &thread_id,
                            format!("Control session error: {e}"),
                            msg_topic_id.as_deref(),
                        )
                        .await;
                    }
                }
            } else {
                tracing::warn!("no store available, cannot handle control session message");
            }
        }

        MessageRoute::NewTask => {
            let channel = msg.channel.clone();
            let thread_id = msg.thread_id.clone();

            // Resolve target project: resolved_repo → specific project, else first
            let target_engine_ref = if let Some(repo) = resolved_repo {
                engine_refs.iter().find(|(r, _, _, _)| r == repo)
            } else {
                engine_refs.first()
            };

            if let Some((repo, _, task_manager, _)) = target_engine_ref {
                let title = if msg.body.chars().count() > 80 {
                    let truncated: String = msg.body.chars().take(80).collect();
                    format!("{}…", truncated)
                } else {
                    msg.body.clone()
                };
                let req = CreateTaskRequest {
                    title,
                    body: msg.body.clone(),
                    task_type: TaskType::Internal,
                    labels: vec!["channel-created".to_string()],
                    source: channel.clone(),
                    source_id: thread_id.clone(),
                };
                match task_manager.create_task(req).await {
                    Ok(task) => {
                        use crate::engine::tasks::Task;
                        let task_id = match &task {
                            Task::Internal(t) => format!("internal:{}", t.id),
                            Task::External(t) => t.id.0.clone(),
                        };
                        // Bind the thread to the new task
                        transport
                            .bind(
                                &task_id,
                                &format!("orch-{repo}-{task_id}"),
                                &channel,
                                &thread_id,
                            )
                            .await;
                        // Register the session with CaptureService (graceful if no session yet)
                        capture
                            .register_session(&task_id, &format!("orch-{repo}-{task_id}"))
                            .await;
                        // Spawn output fanout for this task
                        let transport_clone = transport.clone();
                        let channels_clone = channels.clone();
                        let task_id_clone = task_id.clone();
                        tokio::spawn(async move {
                            fanout_output(task_id_clone, transport_clone, channels_clone).await;
                        });
                        let project_label = if resolved_repo.is_some() {
                            String::new()
                        } else {
                            format!(" in [{repo}]")
                        };
                        let reply = format!(
                            "Task created: `{task_id}`{project_label} — I'll start working on it now."
                        );
                        send_channel_reply(
                            channels,
                            &channel,
                            &thread_id,
                            reply,
                            msg_topic_id.as_deref(),
                        )
                        .await;
                    }
                    Err(e) => {
                        tracing::warn!(repo, err = %e, "failed to create task from channel message");
                        let reply = format!("Failed to create task: {e}");
                        send_channel_reply(
                            channels,
                            &channel,
                            &thread_id,
                            reply,
                            msg_topic_id.as_deref(),
                        )
                        .await;
                    }
                }
            } else {
                tracing::warn!("no project configured, cannot create task from channel message");
            }
        }
    }
}

/// Handle `/status` command with project-aware filtering.
pub(super) async fn handle_status_command(
    engine_refs: &[EngineRef],
    resolved_repo: Option<&str>,
) -> String {
    let mut lines = vec!["**Active tasks:**".to_string()];
    for (repo, _, task_manager, _) in engine_refs {
        // If a specific project was resolved, only show that project
        if let Some(target) = resolved_repo {
            if repo != target {
                continue;
            }
        }
        match task_manager
            .list_external_by_status(Status::InProgress)
            .await
        {
            Ok(tasks) => {
                for t in &tasks {
                    lines.push(format!("- #{} [{}]: {}", t.id.0, repo, t.title));
                }
            }
            Err(e) => {
                tracing::warn!(repo, err = %e, "failed to list in-progress tasks");
            }
        }
    }
    if lines.len() == 1 {
        lines.push("No tasks currently in progress.".to_string());
    }
    lines.join("\n")
}

/// Handle `/stats` command — show 24h metrics, optionally per-project.
pub(super) async fn handle_stats_command(
    engine_refs: &[EngineRef],
    resolved_repo: Option<&str>,
    is_general: bool,
) -> String {
    let mut lines = vec!["**Stats (24h)**".to_string(), String::new()];

    // Determine which repos to query
    let repos_to_query: Vec<&str> = if let Some(repo) = resolved_repo {
        vec![repo]
    } else {
        engine_refs.iter().map(|(r, _, _, _)| r.as_str()).collect()
    };

    for repo in &repos_to_query {
        // Find the store for this repo
        let store = engine_refs
            .iter()
            .find(|(r, _, _, _)| r == repo)
            .and_then(|(_, _, _, s)| s.as_ref());

        let store = match store {
            Some(s) => s,
            None => continue,
        };

        match store.get_metrics_summary_24h_by_repo(repo).await {
            Ok(summary) => {
                let total = summary.tasks_completed_24h + summary.tasks_failed_24h;
                let rate = if total > 0 {
                    (summary.tasks_completed_24h as f64 / total as f64) * 100.0
                } else {
                    0.0
                };
                // Show repo name only if showing multiple (general or no resolution)
                if is_general || resolved_repo.is_none() {
                    lines.push(format!(
                        "**{}**: {} done, {} failed ({:.1}%)",
                        repo, summary.tasks_completed_24h, summary.tasks_failed_24h, rate
                    ));
                } else {
                    lines.push(format!(
                        "{} done, {} failed ({:.1}%)",
                        summary.tasks_completed_24h, summary.tasks_failed_24h, rate
                    ));
                }
                // Per-agent breakdown
                for agent in &summary.agent_stats {
                    lines.push(format!(
                        "  {}: {} runs ({:.0}%)",
                        agent.agent, agent.total_runs, agent.success_rate
                    ));
                }
                if !summary.agent_stats.is_empty() {
                    lines.push(String::new());
                }
            }
            Err(e) => {
                tracing::warn!(repo, err = %e, "failed to query metrics");
                lines.push(format!("**{}**: error fetching metrics", repo));
            }
        }
    }

    if lines.len() <= 2 {
        lines.push("No metrics data available.".to_string());
    }
    lines.join("\n")
}

/// Handle `/subscribe <project>` command.
pub(super) async fn handle_subscribe_command(
    cmd_str: &str,
    channel: &str,
    thread_id: &str,
    engine_refs: &[EngineRef],
    channel_router: &Arc<ChannelRouter>,
) -> String {
    let parts: Vec<&str> = cmd_str.splitn(2, ' ').collect();
    if parts.len() < 2 || parts[1].trim().is_empty() {
        let projects: Vec<&str> = channel_router
            .projects()
            .iter()
            .map(|s| s.as_str())
            .collect();
        return format!(
            "Usage: `/subscribe <project>`\nAvailable projects: {}",
            projects.join(", ")
        );
    }
    let project = parts[1].trim();

    // Validate project exists
    if !channel_router.projects().iter().any(|p| p == project) {
        return format!(
            "Unknown project `{project}`. Available: {}",
            channel_router
                .projects()
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // Find a store to use for subscription
    if let Some((_, _, _, Some(store))) = engine_refs.first() {
        match store.subscribe_channel(channel, thread_id, project).await {
            Ok(()) => format!("Subscribed to notifications from `{project}`."),
            Err(e) => format!("Failed to subscribe: {e}"),
        }
    } else {
        "No store available for subscription management.".to_string()
    }
}

/// Handle `/unsubscribe <project>` command.
pub(super) async fn handle_unsubscribe_command(
    cmd_str: &str,
    channel: &str,
    thread_id: &str,
    engine_refs: &[EngineRef],
    channel_router: &Arc<ChannelRouter>,
) -> String {
    let parts: Vec<&str> = cmd_str.splitn(2, ' ').collect();
    if parts.len() < 2 || parts[1].trim().is_empty() {
        return "Usage: `/unsubscribe <project>`".to_string();
    }
    let project = parts[1].trim();

    // Validate project exists
    if !channel_router.projects().iter().any(|p| p == project) {
        return format!("Unknown project `{project}`.");
    }

    if let Some((_, _, _, Some(store))) = engine_refs.first() {
        match store.unsubscribe_channel(channel, thread_id, project).await {
            Ok(()) => format!("Unsubscribed from `{project}` notifications."),
            Err(e) => format!("Failed to unsubscribe: {e}"),
        }
    } else {
        "No store available for subscription management.".to_string()
    }
}

/// Handle `/stream <task_id>` command — bind channel to task output stream.
pub(super) async fn handle_stream_command(
    cmd_str: &str,
    channel: &str,
    thread_id: &str,
    transport: &Arc<Transport>,
) -> String {
    let parts: Vec<&str> = cmd_str.splitn(2, ' ').collect();
    if parts.len() < 2 || parts[1].trim().is_empty() {
        return "Usage: `/stream <task_id>`".to_string();
    }
    let task_id = parts[1].trim();

    // Check if the task has an active binding (i.e. is running)
    if let Some(binding) = transport.get_binding(task_id).await {
        // Bind this channel/thread as an additional output target
        // The session name is retrieved from the existing binding
        transport
            .bind(task_id, &binding.tmux_session, channel, thread_id)
            .await;
        format!("Streaming output from task `{task_id}` to this channel.")
    } else {
        format!("Task `{task_id}` is not currently running or has no active session.")
    }
}
```

- [ ] **Step 2: Verify it compiles (will fail until mod.rs is updated)**

Run: `cargo check 2>&1 | head -30`
Expected: duplicate function errors (OK — mod.rs still has old versions)

---

### Task 2: Update `src/engine/mod.rs`

**Files:**
- Modify: `src/engine/mod.rs`

Three changes:
1. Add `pub mod channel_handler;` to the module declarations (after line 23)
2. Remove the 8 extracted functions (lines 1193–1783)
3. Update the call site at line 518 to `channel_handler::handle_channel_message(...)`

- [ ] **Step 1: Add `pub mod channel_handler;` to mod declarations**

In `src/engine/mod.rs`, add after the existing `pub mod tick;` line:
```rust
pub mod channel_handler;
```

- [ ] **Step 2: Update call site (line ~518)**

Change:
```rust
                handle_channel_message(
                    msg,
                    &transport,
                    &tmux,
                    &capture,
                    &channels,
                    &engine_refs,
                    &ch_router,
                )
                .await;
```
To:
```rust
                channel_handler::handle_channel_message(
                    msg,
                    &transport,
                    &tmux,
                    &capture,
                    &channels,
                    &engine_refs,
                    &ch_router,
                )
                .await;
```

- [ ] **Step 3: Remove the 8 extracted functions from mod.rs**

Delete from `src/engine/mod.rs`:
- The comment + `send_channel_reply` function (lines ~1193–1229)
- The comment + `forward_to_tmux` function (lines ~1231–1251)
- The comment + `handle_channel_message` function (lines ~1253–1581)
- `handle_status_command` function (lines ~1583–1611)
- `handle_stats_command` function (lines ~1613–1682)
- `handle_subscribe_command` function (lines ~1684–1728)
- `handle_unsubscribe_command` function (lines ~1730–1757)
- `handle_stream_command` function (lines ~1759–1783)

- [ ] **Step 4: Verify it compiles**

Run: `cargo check 2>&1 | head -30`
Expected: clean (no errors)

---

### Task 3: Run CI checks and commit

**Files:** none new

- [ ] **Step 1: Run full CI gate**

```bash
cargo fmt -- --check && cargo clippy --all-targets -- -D warnings && cargo nextest run
```
Expected: all pass, 692+ tests pass

- [ ] **Step 2: Fix any fmt/clippy issues**

If `cargo fmt -- --check` fails, run `cargo fmt` then re-check.
If clippy warns about unused imports in mod.rs (any imports only used by the removed functions), remove them.

- [ ] **Step 3: Verify line counts**

```bash
wc -l src/engine/mod.rs src/engine/channel_handler.rs
```
Expected: mod.rs ~1300 lines, channel_handler.rs ~600 lines

- [ ] **Step 4: Commit**

```bash
git add src/engine/mod.rs src/engine/channel_handler.rs
git commit -m "refactor: extract channel message handlers into engine/channel_handler.rs

Moves handle_channel_message + 7 related handler functions (send_channel_reply,
forward_to_tmux, handle_status_command, handle_stats_command,
handle_subscribe_command, handle_unsubscribe_command, handle_stream_command)
from src/engine/mod.rs into a new src/engine/channel_handler.rs.

engine/mod.rs shrinks from ~1915 to ~1300 lines. No behaviour changes.

Closes #738"
```
