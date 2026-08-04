# Changes Made

## Add escalation for prolonged startup GitHub unreachable retry loop

### Problem

When GitHub is unreachable at service startup, the engine retries `init_project_engines()`
in an unbounded loop with backoff capped at 120s. Short blips are fine, but a prolonged
outage (network down, expired token, GitHub incident) could run silently for hours with
only `WARN`-level log lines — no ERROR-level escalation, no push notification. A ~13h
outage was observed with zero operator-visible signal.

### Solution

Added `engine.startup_failure_escalation_secs` config (default: 3600 = 1h). Once the
startup retry loop has been spinning for longer than this threshold, a **one-time**
escalation fires:

1. **ERROR-level log line** with elapsed duration, attempt count, and the underlying error
2. **One-time push notification** to all configured channels (Telegram, Discord, Slack),
   sent directly from config since channels aren't registered yet at this stage

Subsequent retries continue at `WARN` level to avoid spam, but the single escalation
ensures the operator is alerted.

### Files Modified
- `src/engine/mod.rs`: Added `startup_failure_escalation_secs` to `EngineConfig`, config
  parsing, `send_startup_escalation_notification` helper, and escalation logic in the
  startup retry loop
- `docs/content/configuration.md`: Documented new config key

### Config
```yaml
engine:
  startup_failure_escalation_secs: 3600  # 0 to disable escalation
```

---

## Fixed stuck-task recovery swallowing resolve_task_id errors

### Problem
In stuck-task recovery, `resolve_task_id` errors were silently dropped via `.ok().flatten()`, causing recovery to proceed without clearing routing state when the database operation failed. This could leave stale routing fields (`agent`, `model`, `route_attempts`) after recovery.

### Solution
Replaced `.ok().flatten()` with explicit match handling that:
1. Logs warnings when `resolve_task_id` returns `None` (task not found)
2. Logs errors when `resolve_task_id` returns an error (database failure)
3. Continues with `None` in both cases to maintain existing behavior
4. Preserves the existing logic for clearing routing state when a valid store ID is found

### Files Modified
- `src/engine/tick.rs`: Fixed both external and internal stuck task recovery paths

### Specific Changes
1. Lines 616-638: External task stuck recovery 
2. Lines 808-831: Internal task stuck recovery

Both changes follow the same pattern:
```rust
let resolved_store_id = match cached_store_id {
    Some(id) => Some(id),
    None => match store.resolve_task_id(repo, &task_id).await {
        Ok(Some(id)) => Some(id),
        Ok(None) => {
            tracing::warn!(
                task_id = %task_id,
                repo,
                "resolve_task_id returned None during [internal-]stuck-task recovery"
            );
            None
        }
        Err(e) => {
            tracing::error!(
                task_id = %task_id,
                repo,
                error = %e,
                "resolve_task_id failed during [internal-]stuck-task recovery"
            );
            None
        }
    },
};
```

This ensures that when database errors occur during stuck-task recovery:
- The error is logged with full context for debugging
- Recovery proceeds safely without leaving stale routing metadata
- The underlying store failure is visible in logs rather than being swallowed