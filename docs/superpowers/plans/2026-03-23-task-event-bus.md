# Task Event Bus Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace polling-based status discovery with an event-driven bus so engine-initiated transitions (needs_review → review, routed → dispatch) happen instantly.

**Architecture:** A `tokio::broadcast` channel carries `TaskEvent` structs. `TaskManager::update_task_status()` publishes events after every status write. Internal subscribers (dispatch, review, notify) react immediately. A localhost-only websocket server fans events out to external consumers (CLI).

**Tech Stack:** `tokio::broadcast` (already available), `tokio-tungstenite` (already in Cargo.toml), `futures` (already in Cargo.toml).

**Spec:** `docs/superpowers/specs/2026-03-23-task-event-bus-design.md`

---

## File Structure

| File | Responsibility |
|------|---------------|
| `src/engine/events.rs` | `TaskEvent` struct, `EventBus` (channel creation + ws server), port selection |
| `src/engine/subscribers/mod.rs` | Re-exports, shared helpers |
| `src/engine/subscribers/dispatch.rs` | Reacts to `Routed` → spawn agent immediately |
| `src/engine/subscribers/review.rs` | Reacts to `NeedsReview` → spawn review agent immediately |
| `src/engine/subscribers/notify.rs` | Reacts to all transitions → push to transport channels |
| `src/engine/tasks.rs` | Modify: add `event_tx` to `TaskManager`, publish on status change |
| `src/engine/mod.rs` | Modify: create bus at startup, wire subscribers, pass sender to TaskManager |
| `src/cli/events.rs` | `orch events` command — connect to ws, print events |
| `src/cli/mod.rs` | Modify: add `pub mod events;` |
| `src/main.rs` | Modify: add `Events` command variant + `Watch` to `TaskAction` |

---

### Task 1: TaskEvent struct and EventBus

**Files:**
- Create: `src/engine/events.rs`
- Modify: `src/engine/mod.rs` (add `pub mod events;`)

- [ ] **Step 1: Write the test for TaskEvent serialization**

In `src/engine/events.rs`, add a test that creates a `TaskEvent`, serializes it to JSON, and verifies the fields:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_event_serializes_to_json() {
        let event = TaskEvent {
            task_id: "123".to_string(),
            repo: "owner/repo".to_string(),
            old_status: "new".to_string(),
            new_status: "routed".to_string(),
            agent: Some("claude".to_string()),
            model: None,
            pr_number: None,
            branch: None,
            review_context: None,
            error: None,
            timestamp: "2026-03-23T12:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"task_id\":\"123\""));
        assert!(json.contains("\"new_status\":\"routed\""));
    }

    #[test]
    fn event_bus_send_receive() {
        let bus = EventBus::new(256);
        let mut rx = bus.subscribe();
        let event = TaskEvent {
            task_id: "1".to_string(),
            repo: "r".to_string(),
            old_status: "new".to_string(),
            new_status: "routed".to_string(),
            agent: None,
            model: None,
            pr_number: None,
            branch: None,
            review_context: None,
            error: None,
            timestamp: "2026-03-23T12:00:00Z".to_string(),
        };
        bus.publish(event.clone());
        let received = rx.try_recv().unwrap();
        assert_eq!(received.task_id, "1");
        assert_eq!(received.new_status, "routed");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run events::tests -v`
Expected: FAIL — module doesn't exist yet.

- [ ] **Step 3: Implement TaskEvent and EventBus**

```rust
//! Task event bus — broadcast channel for status transitions.

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// A task status transition event.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskEvent {
    pub task_id: String,
    pub repo: String,
    pub old_status: String,
    pub new_status: String,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub pr_number: Option<String>,
    pub branch: Option<String>,
    pub review_context: Option<String>,
    pub error: Option<String>,
    pub timestamp: String,
}

/// The event bus — wraps a tokio broadcast channel.
pub struct EventBus {
    tx: broadcast::Sender<TaskEvent>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Get a sender clone (for TaskManager, subscribers, etc.).
    pub fn sender(&self) -> broadcast::Sender<TaskEvent> {
        self.tx.clone()
    }

    /// Subscribe to events.
    pub fn subscribe(&self) -> broadcast::Receiver<TaskEvent> {
        self.tx.subscribe()
    }

    /// Publish an event. Returns number of receivers that got it.
    pub fn publish(&self, event: TaskEvent) -> usize {
        self.tx.send(event).unwrap_or(0)
    }
}
```

- [ ] **Step 4: Add module declaration**

In `src/engine/mod.rs`, add `pub mod events;` alongside the other module declarations.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo nextest run events::tests -v`
Expected: PASS (both tests).

- [ ] **Step 6: Run full checks**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo nextest run`

- [ ] **Step 7: Commit**

```bash
git add src/engine/events.rs src/engine/mod.rs
git commit -m "feat: add TaskEvent struct and EventBus (broadcast channel)"
```

---

### Task 2: Wire EventBus into TaskManager

**Files:**
- Modify: `src/engine/tasks.rs` (add sender field, publish events)
- Modify: `src/engine/mod.rs` (create bus, pass sender to TaskManager)

- [ ] **Step 1: Write failing test for event emission**

Add to `src/engine/tasks.rs` tests:

```rust
#[tokio::test]
async fn update_task_status_publishes_event() {
    let backend = Arc::new(MockBackend::new());
    let (tx, mut rx) = tokio::sync::broadcast::channel::<crate::engine::events::TaskEvent>(16);
    let tm = TaskManager {
        backend,
        store: None,
        repo: "owner/repo".to_string(),
        event_tx: Some(tx),
    };
    // External task — backend mock accepts any status update
    let id = ExternalId("42".to_string());
    tm.update_task_status(&id, Status::Routed).await.unwrap();
    let event = rx.try_recv().unwrap();
    assert_eq!(event.task_id, "42");
    assert_eq!(event.new_status, "routed");
    assert_eq!(event.repo, "owner/repo");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run update_task_status_publishes_event -v`
Expected: FAIL — `event_tx` field doesn't exist.

- [ ] **Step 3: Add event_tx field to TaskManager**

In `src/engine/tasks.rs`, add the field to `TaskManager`:

```rust
pub struct TaskManager {
    backend: Arc<dyn ExternalBackend>,
    store: Option<Arc<TaskStore>>,
    repo: String,
    /// Event bus sender — publishes TaskEvent on every status change.
    event_tx: Option<tokio::sync::broadcast::Sender<crate::engine::events::TaskEvent>>,
}
```

Update the `Clone` impl, `new()`, and `with_store()` to include `event_tx: None`.

Add a new constructor:

```rust
pub fn with_events(
    backend: Arc<dyn ExternalBackend>,
    store: Arc<TaskStore>,
    repo: String,
    event_tx: tokio::sync::broadcast::Sender<crate::engine::events::TaskEvent>,
) -> Self {
    Self {
        backend,
        store: Some(store),
        repo,
        event_tx: Some(event_tx),
    }
}
```

- [ ] **Step 4: Publish event in update_task_status**

At the end of `update_task_status()`, before the final `Ok(())`, add:

```rust
// Publish event to bus
if let Some(ref tx) = self.event_tx {
    let event = crate::engine::events::TaskEvent {
        task_id: id.0.clone(),
        repo: self.repo.clone(),
        old_status: String::new(), // caller doesn't know old status yet
        new_status: status.as_label().trim_start_matches("status:").to_string(),
        agent: None,
        model: None,
        pr_number: None,
        branch: None,
        review_context: None,
        error: None,
        timestamp: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    };
    let _ = tx.send(event);
}
```

Note: `old_status` is empty for now — we'll enrich it in Task 3 by reading from the store before updating.

- [ ] **Step 5: Wire EventBus in engine init**

In `src/engine/mod.rs`, in `init_project_engines()` or `serve()`:

1. Create the `EventBus` once before the project engine loop.
2. Pass `bus.sender()` to each `TaskManager::with_events()` call.
3. Store the `EventBus` alongside the project engines for later use by subscribers and ws server.

Find where `TaskManager::with_store()` is called in `init_project_engines()` and change to `TaskManager::with_events()`, passing the sender.

- [ ] **Step 6: Run tests**

Run: `cargo nextest run update_task_status_publishes_event -v`
Expected: PASS.

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo nextest run`
Expected: All pass. Some tests using `TaskManager::new()` or `with_store()` still work since `event_tx` defaults to `None`.

- [ ] **Step 7: Commit**

```bash
git add src/engine/tasks.rs src/engine/mod.rs
git commit -m "feat: TaskManager publishes TaskEvent on every status change"
```

---

### Task 3: Enrich events with old_status and store context

**Files:**
- Modify: `src/engine/tasks.rs` (read old status before update, enrich event with store fields)

- [ ] **Step 1: Write failing test**

```rust
#[tokio::test]
async fn event_includes_old_status() {
    // Setup with a real in-memory store, create a task, then update status
    // Assert the event has both old_status and new_status populated
}
```

This test needs a real store. If existing tests already create in-memory stores, follow that pattern. The test should:
1. Create a task with status `New`
2. Update to `Routed`
3. Assert event has `old_status: "new"`, `new_status: "routed"`

- [ ] **Step 2: Implement old_status lookup**

In `update_task_status()`, before the status write, read the current status from the store:

```rust
let old_status_str = if let Some(ref store) = self.store {
    if let Ok(Some(store_id)) = store.resolve_task_id(&self.repo, &id.0).await {
        if let Ok(task) = store.get(store_id).await {
            Some(format!("{:?}", task.status).to_lowercase())
        } else {
            None
        }
    } else {
        None
    }
} else {
    None
};
```

Then use `old_status_str.unwrap_or_default()` when constructing the event.

Also enrich with store fields (agent, model, branch, pr_number, error) by reading from the store's KV data if available.

- [ ] **Step 3: Run tests**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo nextest run`

- [ ] **Step 4: Commit**

```bash
git add src/engine/tasks.ts
git commit -m "feat: enrich TaskEvent with old_status and store context"
```

---

### Task 4: Websocket server

**Files:**
- Modify: `src/engine/events.rs` (add ws server, port selection, port file management)

- [ ] **Step 1: Write test for port selection**

```rust
#[test]
fn select_port_finds_available_port() {
    let port = select_available_port().unwrap();
    assert!(port >= 49152);
    assert!(port <= 65535);
    // Verify the port is actually available by binding to it
    let listener = std::net::TcpListener::bind(("127.0.0.1", port));
    assert!(listener.is_ok());
}
```

- [ ] **Step 2: Implement port selection**

```rust
/// Find an available port in the ephemeral range.
/// Starts from a deterministic offset based on hostname hash, then increments.
pub fn select_available_port() -> anyhow::Result<u16> {
    let hostname = hostname::get()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let hash = hostname.bytes().fold(0u32, |acc, b| acc.wrapping_add(b as u32));
    let start = 49152 + (hash % (65535 - 49152)) as u16;

    for offset in 0..1000 {
        let port = start.wrapping_add(offset);
        if port < 49152 {
            continue;
        }
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    anyhow::bail!("no available port found in range 49152-65535")
}
```

Note: Check if `hostname` crate is needed or use `gethostname` from libc. If not available, use a simpler seed.

- [ ] **Step 3: Implement websocket server**

Add to `src/engine/events.rs`:

```rust
use tokio::net::TcpListener;
use tokio_tungstenite::accept_async;
use futures::{SinkExt, StreamExt};

impl EventBus {
    /// Start the websocket server. Returns the bound port.
    /// Spawns a background task — does not block.
    pub async fn start_ws_server(&self) -> anyhow::Result<u16> {
        let port = select_available_port()?;
        let listener = TcpListener::bind(("127.0.0.1", port)).await?;

        // Write port file
        let state_dir = crate::home::state_dir()?;
        std::fs::write(state_dir.join("ws.port"), port.to_string())?;

        let tx = self.tx.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _addr)) => {
                        let mut rx = tx.subscribe();
                        tokio::spawn(async move {
                            let Ok(ws) = accept_async(stream).await else {
                                return;
                            };
                            let (mut write, mut read) = ws.split();
                            loop {
                                tokio::select! {
                                    event = rx.recv() => {
                                        match event {
                                            Ok(e) => {
                                                let json = serde_json::to_string(&e).unwrap_or_default();
                                                if write.send(tokio_tungstenite::tungstenite::Message::Text(json)).await.is_err() {
                                                    break;
                                                }
                                            }
                                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                                            Err(_) => break,
                                        }
                                    }
                                    msg = read.next() => {
                                        // Client disconnected or sent close
                                        if msg.is_none() {
                                            break;
                                        }
                                    }
                                }
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(?e, "ws accept failed");
                    }
                }
            }
        });

        tracing::info!(port, "event bus websocket server started on 127.0.0.1");
        Ok(port)
    }
}

/// Remove the ws.port file on shutdown.
pub fn cleanup_port_file() {
    if let Ok(state_dir) = crate::home::state_dir() {
        let _ = std::fs::remove_file(state_dir.join("ws.port"));
    }
}
```

- [ ] **Step 4: Start ws server in engine serve()**

In `src/engine/mod.rs`, after creating the `EventBus`, call:

```rust
if let Err(e) = event_bus.start_ws_server().await {
    tracing::warn!(?e, "failed to start event websocket server, continuing without it");
}
```

Add `events::cleanup_port_file()` to the shutdown handler (near the SIGTERM handler).

- [ ] **Step 5: Run tests**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo nextest run`

- [ ] **Step 6: Commit**

```bash
git add src/engine/events.rs src/engine/mod.rs
git commit -m "feat: localhost websocket server for event bus"
```

---

### Task 5: Dispatch subscriber (Routed → immediate dispatch)

**Files:**
- Create: `src/engine/subscribers/mod.rs`
- Create: `src/engine/subscribers/dispatch.rs`
- Modify: `src/engine/mod.rs` (add `pub mod subscribers;`, spawn subscriber task)

- [ ] **Step 1: Create subscriber module**

`src/engine/subscribers/mod.rs`:
```rust
pub mod dispatch;
pub mod review;
pub mod notify;
```

- [ ] **Step 2: Implement dispatch subscriber**

`src/engine/subscribers/dispatch.rs`:

```rust
//! Reacts to Routed events — dispatches agent immediately.

use crate::engine::events::TaskEvent;
use tokio::sync::broadcast;

/// Spawn a task that listens for Routed events and dispatches agents.
///
/// This mirrors the logic in `tick_dispatch_tasks` but triggers instantly
/// instead of waiting for the next tick. The dispatching set prevents
/// double-dispatch if the tick loop picks up the same task.
pub fn spawn(
    mut rx: broadcast::Receiver<TaskEvent>,
    // Same args that tick_dispatch_tasks needs — passed from engine setup
    backend: std::sync::Arc<dyn crate::backends::ExternalBackend>,
    tmux: std::sync::Arc<crate::tmux::TmuxManager>,
    runner: std::sync::Arc<crate::engine::runner::TaskRunner>,
    capture: std::sync::Arc<crate::channels::capture::CaptureService>,
    semaphore: std::sync::Arc<tokio::sync::Semaphore>,
    task_manager: std::sync::Arc<crate::engine::tasks::TaskManager>,
    weight_tx: tokio::sync::mpsc::Sender<crate::engine::runner::WeightSignal>,
    transport: std::sync::Arc<crate::channels::transport::Transport>,
    router_arc: std::sync::Arc<tokio::sync::RwLock<crate::engine::router::Router>>,
    dispatching: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    store: std::sync::Arc<crate::store::TaskStore>,
    repo: String,
) {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) if event.new_status == "routed" && event.repo == repo => {
                    tracing::info!(
                        task_id = event.task_id,
                        "event-driven dispatch triggered"
                    );
                    // Delegate to existing dispatch logic.
                    // The dispatching set guard inside tick_dispatch_tasks
                    // prevents double-dispatch if the tick loop also picks this up.
                    // We call the same function — it's idempotent due to the guard.
                    if let Err(e) = crate::engine::tick::tick_dispatch_tasks(
                        &backend,
                        &tmux,
                        &repo,
                        &runner,
                        &capture,
                        &semaphore,
                        &task_manager,
                        &weight_tx,
                        &transport,
                        &router_arc,
                        &dispatching,
                        &store,
                    ).await {
                        tracing::warn!(?e, "event-driven dispatch failed (tick will retry)");
                    }
                }
                Ok(_) => {} // Not a routed event or different repo
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!(n, "dispatch subscriber lagged, tick will catch up");
                }
                Err(_) => break, // Channel closed
            }
        }
    });
}
```

**Important:** `tick_dispatch_tasks` dispatches ALL routed tasks it finds, not just one. This is fine — the dispatching set deduplicates. Alternatively, we could extract single-task dispatch from `tick_dispatch_tasks` if the full scan is too heavy. Evaluate during implementation — if `tick_dispatch_tasks` is fast (just a list query + guard check), calling it is simpler.

- [ ] **Step 3: Wire in engine startup**

In `src/engine/mod.rs`, after creating the event bus and project engines, spawn the dispatch subscriber for each project:

```rust
for engine in &project_engines {
    subscribers::dispatch::spawn(
        event_bus.subscribe(),
        engine.backend.clone(),
        tmux.clone(),
        engine.runner.clone(),
        capture.clone(),
        semaphore.clone(),
        engine.task_manager.clone(),
        weight_tx.clone(),
        transport.clone(),
        router.clone(), // the Arc<RwLock<Router>>
        dispatching.clone(),
        engine.store.clone(),
        engine.repo.clone(),
    );
}
```

- [ ] **Step 4: Run full checks**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo nextest run`

- [ ] **Step 5: Commit**

```bash
git add src/engine/subscribers/ src/engine/mod.rs
git commit -m "feat: event-driven dispatch subscriber (Routed → immediate agent spawn)"
```

---

### Task 6: Review subscriber (NeedsReview → immediate review agent)

**Files:**
- Create: `src/engine/subscribers/review.rs`
- Modify: `src/engine/mod.rs` (spawn review subscriber)

- [ ] **Step 1: Implement review subscriber**

`src/engine/subscribers/review.rs`:

```rust
//! Reacts to NeedsReview events — spawns review agent immediately.

use crate::engine::events::TaskEvent;
use tokio::sync::broadcast;

/// Spawn a task that listens for NeedsReview events and triggers review.
///
/// This replaces the sync_tick catch-up path for review agent spawning.
/// The needs_review → in_review label transition is the atomic guard
/// against duplicate review agents.
pub fn spawn(
    mut rx: broadcast::Receiver<TaskEvent>,
    backend: std::sync::Arc<dyn crate::backends::ExternalBackend>,
    tmux: std::sync::Arc<crate::tmux::TmuxManager>,
    semaphore: std::sync::Arc<tokio::sync::Semaphore>,
    task_manager: std::sync::Arc<crate::engine::tasks::TaskManager>,
    dispatching: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    store: std::sync::Arc<crate::store::TaskStore>,
    config: crate::engine::EngineConfig,
    repo: String,
) {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) if event.new_status == "needs_review" && event.repo == repo => {
                    if !config.enable_review_agent {
                        continue;
                    }
                    tracing::info!(
                        task_id = event.task_id,
                        "event-driven review triggered"
                    );
                    // Reuse the review trigger logic from sync.rs or tick.rs.
                    // The in_review status transition is the atomic guard.
                    // Extract the review spawn logic into a shared function
                    // that both the subscriber and sync tick can call.
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!(n, "review subscriber lagged, sync will catch up");
                }
                Err(_) => break,
            }
        }
    });
}
```

**Implementation note:** The review agent spawn logic currently lives inline in `tick_dispatch_tasks` (tick.rs lines 637-820) and `sync_tick` (sync.rs lines 103-189). Extract the core "spawn review for one task" logic into a shared function in `src/engine/review.rs` (or a new helper) that all three callers use. This prevents code duplication. Look at the existing `review_and_merge()` function — the spawn logic may already be partially factored out.

- [ ] **Step 2: Extract shared review spawn function**

Look at the review spawn logic in `tick.rs` and `sync.rs`. Create a shared function like:

```rust
pub async fn spawn_review_for_task(
    task_id: &str,
    repo: &str,
    backend: &Arc<dyn ExternalBackend>,
    tmux: &Arc<TmuxManager>,
    semaphore: &Arc<Semaphore>,
    task_manager: &Arc<TaskManager>,
    dispatching: &Arc<Mutex<HashSet<String>>>,
    store: &Arc<TaskStore>,
) -> anyhow::Result<()>
```

This function should:
1. Check the dispatching set (guard)
2. Transition status to InReview
3. Acquire semaphore permit
4. Spawn review agent in tmux
5. Call `review_and_merge()`

- [ ] **Step 3: Wire in engine startup**

Similar to Task 5 — spawn for each project engine.

- [ ] **Step 4: Run full checks**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo nextest run`

- [ ] **Step 5: Commit**

```bash
git add src/engine/subscribers/review.rs src/engine/review.rs src/engine/tick.rs src/engine/sync.rs src/engine/mod.rs
git commit -m "feat: event-driven review subscriber (NeedsReview → immediate review agent)"
```

---

### Task 7: Notification subscriber (all transitions → channels)

**Files:**
- Create: `src/engine/subscribers/notify.rs`
- Modify: `src/engine/mod.rs` (spawn notify subscriber)

- [ ] **Step 1: Implement notify subscriber**

`src/engine/subscribers/notify.rs`:

```rust
//! Reacts to all status transitions — pushes notifications to channels.

use crate::engine::events::TaskEvent;
use tokio::sync::broadcast;

/// Spawn a task that listens for all events and pushes to transport.
pub fn spawn(
    mut rx: broadcast::Receiver<TaskEvent>,
    transport: std::sync::Arc<crate::channels::transport::Transport>,
) {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    // Convert TaskEvent to TaskNotification and push to transport.
                    // This replaces the inline notification pushes scattered
                    // through tick.rs and sync.rs.
                    let notification = crate::channels::transport::TaskNotification {
                        task_id: event.task_id.clone(),
                        status: event.new_status.clone(),
                        repo: Some(event.repo.clone()),
                        title: None,
                        url: None,
                        agent: event.agent.clone(),
                        pr_number: event.pr_number.clone(),
                    };
                    transport.push_notification(notification);
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::debug!(n, "notify subscriber lagged");
                }
                Err(_) => break,
            }
        }
    });
}
```

**Note:** Check the actual `TaskNotification` struct fields in `transport.rs` and adjust accordingly.

- [ ] **Step 2: Wire in engine startup**

Spawn once (not per-project — it handles all repos):

```rust
subscribers::notify::spawn(event_bus.subscribe(), transport.clone());
```

- [ ] **Step 3: Run full checks**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo nextest run`

- [ ] **Step 4: Commit**

```bash
git add src/engine/subscribers/notify.rs src/engine/mod.rs
git commit -m "feat: notification subscriber pushes all transitions to channels"
```

---

### Task 8: CLI — `orch events` and `orch task watch`

**Files:**
- Create: `src/cli/events.rs`
- Modify: `src/cli/mod.rs` (add `pub mod events;`)
- Modify: `src/main.rs` (add `Events` command, add `Watch` to `TaskAction`)

- [ ] **Step 1: Add CLI module and command variants**

In `src/cli/mod.rs`, add:
```rust
pub mod events;
```

In `src/main.rs`, add to `Commands` enum:
```rust
/// Stream task events in real-time
Events {
    /// Filter by repo (substring match)
    #[arg(long)]
    repo: Option<String>,
    /// Filter by task ID
    #[arg(long)]
    task: Option<String>,
},
```

Add to `TaskAction` enum:
```rust
/// Watch a task's status changes in real-time
Watch {
    /// Task ID
    id: String,
},
```

- [ ] **Step 2: Implement CLI handler**

`src/cli/events.rs`:

```rust
use anyhow::Context;
use futures::StreamExt;
use tokio_tungstenite::connect_async;

/// Read the websocket port from ~/.orch/state/ws.port.
fn read_ws_port() -> anyhow::Result<u16> {
    let state_dir = crate::home::state_dir()?;
    let port_str = std::fs::read_to_string(state_dir.join("ws.port"))
        .context("event bus not running — is the service started?")?;
    port_str.trim().parse().context("invalid port in ws.port")
}

/// Stream events, optionally filtered.
pub async fn stream(repo: Option<&str>, task: Option<&str>) -> anyhow::Result<()> {
    let port = read_ws_port()?;
    let url = format!("ws://127.0.0.1:{}/events", port);

    let (ws, _) = connect_async(&url)
        .await
        .context("failed to connect to event bus — is the service running?")?;

    let (_, mut read) = ws.split();

    println!("Connected to event bus (port {}). Streaming events...\n", port);

    while let Some(msg) = read.next().await {
        let msg = msg?;
        if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
            if let Ok(event) = serde_json::from_str::<crate::engine::events::TaskEvent>(&text) {
                // Apply filters
                if let Some(repo_filter) = repo {
                    if !event.repo.contains(repo_filter) {
                        continue;
                    }
                }
                if let Some(task_filter) = task {
                    if event.task_id != task_filter {
                        continue;
                    }
                }

                // Format output
                let agent_str = event.agent.as_deref().unwrap_or("");
                let pr_str = event.pr_number.as_ref().map(|p| format!(" PR: #{p}")).unwrap_or_default();
                let time = &event.timestamp[11..19]; // HH:MM:SS

                println!(
                    "{} {} {} → {} {}{}",
                    time,
                    event.task_id,
                    event.old_status,
                    event.new_status,
                    agent_str,
                    pr_str,
                );
            }
        }
    }

    Ok(())
}
```

- [ ] **Step 3: Wire commands in main.rs**

```rust
Commands::Events { repo, task } => {
    cli::events::stream(repo.as_deref(), task.as_deref()).await?;
}
// In TaskAction match:
TaskAction::Watch { id } => {
    cli::events::stream(None, Some(&id)).await?;
}
```

- [ ] **Step 4: Run full checks**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo nextest run`

- [ ] **Step 5: Commit**

```bash
git add src/cli/events.rs src/cli/mod.rs src/main.rs
git commit -m "feat: orch events and orch task watch CLI commands"
```

---

### Task 9: Integration test — end-to-end event flow

**Files:**
- Modify: `src/engine/events.rs` (add integration test)

- [ ] **Step 1: Write integration test**

```rust
#[tokio::test]
async fn ws_server_broadcasts_events() {
    let bus = EventBus::new(256);
    let port = bus.start_ws_server().await.unwrap();

    // Connect a websocket client
    let url = format!("ws://127.0.0.1:{}/events", port);
    let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let (_, mut read) = futures::StreamExt::split(ws);

    // Give the server a moment to register the client
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Publish an event
    let event = TaskEvent {
        task_id: "test-1".to_string(),
        repo: "owner/repo".to_string(),
        old_status: "new".to_string(),
        new_status: "routed".to_string(),
        agent: Some("claude".to_string()),
        model: None,
        pr_number: None,
        branch: None,
        review_context: None,
        error: None,
        timestamp: "2026-03-23T12:00:00Z".to_string(),
    };
    bus.publish(event);

    // Read from websocket
    let msg = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        futures::StreamExt::next(&mut read),
    ).await.unwrap().unwrap().unwrap();

    if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
        let received: TaskEvent = serde_json::from_str(&text).unwrap();
        assert_eq!(received.task_id, "test-1");
        assert_eq!(received.new_status, "routed");
    } else {
        panic!("expected text message");
    }

    // Cleanup
    crate::engine::events::cleanup_port_file();
}
```

- [ ] **Step 2: Run the integration test**

Run: `cargo nextest run ws_server_broadcasts_events -v`
Expected: PASS.

- [ ] **Step 3: Run full checks**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo nextest run`

- [ ] **Step 4: Commit**

```bash
git add src/engine/events.rs
git commit -m "test: integration test for websocket event broadcast"
```

---

### Task 10: Cleanup and port file management

**Files:**
- Modify: `src/engine/mod.rs` (cleanup on shutdown)
- Modify: `src/engine/events.rs` (port file helpers)

- [ ] **Step 1: Add cleanup to shutdown handler**

In `src/engine/mod.rs`, find the SIGTERM/shutdown handler and add:

```rust
crate::engine::events::cleanup_port_file();
```

- [ ] **Step 2: Verify port file lifecycle**

Manually test:
1. `cargo run -- serve` — check `~/.orch/state/ws.port` exists with a valid port
2. Ctrl+C — check `ws.port` is removed
3. `orch events` — connects and streams (when service is running)
4. `orch events` when service is stopped — shows clear error message

- [ ] **Step 3: Run full checks**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo nextest run`

- [ ] **Step 4: Commit**

```bash
git add src/engine/mod.rs src/engine/events.rs
git commit -m "fix: clean up ws.port on shutdown, verify port file lifecycle"
```
