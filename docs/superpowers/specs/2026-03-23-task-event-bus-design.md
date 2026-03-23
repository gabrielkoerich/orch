# Task Event Bus — Design Spec

**Date:** 2026-03-23
**Status:** Approved

## Problem

The engine polls itself on 10s (tick) and 45s (sync) intervals to discover status changes it made. This adds unnecessary latency to transitions the engine already knows about — e.g., `needs_review` waits up to 45s for the review agent to start, `routed` waits up to 10s for dispatch.

## Solution

A `tokio::broadcast` event bus for internal zero-latency reactions, plus a local websocket server for external consumers (CLI, debugging tools).

## Event Structure

```rust
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
    pub timestamp: String,  // ISO 8601
}
```

## Internal Bus

- `tokio::broadcast::channel::<TaskEvent>(256)` created at engine startup.
- `TaskManager` holds a `broadcast::Sender<TaskEvent>`.
- Every call to `update_task_status()` publishes a `TaskEvent` after the status write succeeds.
- The sender is cloned into components that need to publish events outside of `TaskManager` (e.g., runner completion, review outcome).

### Internal Subscribers

Each subscriber calls `sender.subscribe()` to get its own `Receiver`. They run as independent tokio tasks spawned at engine startup.

| Subscriber | Listens for | Action |
|-----------|-------------|--------|
| Review launcher | `new_status == NeedsReview` | Spawn review agent immediately (bypass sync tick) |
| Dispatcher | `new_status == Routed` | Dispatch agent immediately (bypass tick Phase 3b) |
| Notification router | All transitions | Push to Telegram/Discord/Slack channels |

### Deduplication

Event-driven subscribers act immediately. The tick loop still polls for the same statuses as a fallback. To prevent double-dispatch:

- The existing `dispatching: HashSet<String>` (in-progress guard) already prevents duplicate agent spawns. The event subscriber and tick loop both check it before spawning.
- Review agent spawn already uses the `needs_review → in_review` label transition as an atomic guard.
- No new deduplication mechanism needed — existing guards cover it.

## External Bus (Local Websocket)

### Server

- Starts at engine startup alongside the main loop.
- Binds to `127.0.0.1` (localhost only, no external access).
- Port selection: try ports in range `49152-65535` starting from a hash of the machine hostname (deterministic first attempt). On conflict, increment until a free port is found.
- Write the bound port to `~/.orch/state/ws.port` (overwritten each startup).
- On shutdown, remove `ws.port` file.

### Protocol

- Connection: `ws://127.0.0.1:{port}/events`
- Server subscribes to the broadcast channel and fans out JSON-serialized `TaskEvent` to all connected websocket clients.
- Optional query parameter for filtering: `?task_id=123` or `?repo=owner/repo`.
- No authentication (localhost-only, single-user tool).

### Backpressure

- If a websocket client can't keep up, buffer up to 64 messages per client. If exceeded, drop oldest events for that client. The client can re-query the store to catch up.

## CLI Integration

### `orch events`

Stream all events across all projects:

```bash
orch events                    # all events, all projects
orch events --repo owner/repo  # filter by project
orch events --task 123         # filter by task ID
```

Reads port from `~/.orch/state/ws.port`. Connects to websocket. Prints events as formatted lines (colorized status transitions). Exits cleanly on Ctrl+C.

### `orch task watch <id>`

Watch a single task's lifecycle:

```bash
orch task watch 123
# 12:03:16 new → routed (agent: claude, complexity: medium)
# 12:03:26 routed → in_progress
# 12:08:42 in_progress → needs_review (PR: #804)
# 12:08:42 needs_review → in_review
# 12:12:18 in_review → done
```

Shorthand for `orch events --task <id>` with task-specific formatting.

## Tick Loop Changes

The tick loop remains as a catch-up/fallback mechanism. Changes:

| Phase | Before | After |
|-------|--------|-------|
| 3a. Route new tasks | Polls `list_routable()` | No change — external sources (GitHub) still need polling |
| 3b. Dispatch routed | Polls `list_by_status(Routed)` | Event subscriber dispatches immediately; tick is fallback only |
| Review trigger (sync) | Polls `needs_review` tasks every 45s | Event subscriber spawns review agent immediately; sync is fallback |
| Review outcome (sync) | Polls GitHub PR reviews every 45s | No change — GitHub reviews are an external source, must poll |
| Unblock parents | Polls blocked tasks with done children | Event subscriber on `Done` checks if parent can be unblocked |

## File Layout

```
src/engine/events.rs          # TaskEvent struct, bus creation, websocket server
src/engine/subscribers/
  mod.rs                      # Subscriber trait or common setup
  dispatch.rs                 # Reacts to Routed → spawn agent
  review.rs                   # Reacts to NeedsReview → spawn review agent
  notify.rs                   # Reacts to all → push to channels
src/cli/events.rs             # CLI: orch events, orch task watch
```

## Dependencies

- `tokio-tungstenite` for websocket server (async, tokio-native).
- No other new dependencies. `tokio::broadcast` is in `tokio::sync` (already a dependency).

## Reliability

- Broadcast channel capacity: 256. Slow internal subscribers get `RecvError::Lagged` — they skip missed events. The tick loop catches up.
- Events are ephemeral notifications, not persistent. SQLite is the source of truth.
- If the websocket server fails to bind, the engine logs a warning and continues without it. Internal bus still works.
- `ws.port` file is cleaned up on graceful shutdown. CLI checks if the port is reachable before connecting; if not, falls back to polling the store directly.

## Configuration

```yaml
# In config.yml (all optional, sane defaults)
events:
  # Websocket buffer per client (default: 64)
  ws_buffer_size: 64
  # Broadcast channel capacity (default: 256)
  channel_capacity: 256
```

No config needed for basic operation. Port is auto-selected. Websocket is always on.
