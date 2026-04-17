# Changes Made

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