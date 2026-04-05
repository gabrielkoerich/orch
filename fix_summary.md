# Fix Summary: Bug #1889 - Cleanup.rs branch_delete duplicate events

## Problem
In `src/engine/cleanup.rs`, the `cleanup_task_worktree_with_opts` function was logging the `branch_delete` activity **before** calling `mark_cleaned`. If `mark_cleaned` failed transiently (e.g., brief SQLite lock), the task wouldn't be marked as cleaned, causing the janitor to retry cleanup on the next tick. On retry, since the worktree directory was already gone, the code would fall into the branch-only path and call `store_log_activity` again, producing duplicate `branch_delete` events.

## Location
- File: `src/engine/cleanup.rs`
- Function: `cleanup_task_worktree_with_opts`
- Lines: 531-558 (before fix)

## Solution
Swapped the order of operations:
1. First call `store.mark_cleaned()` to mark the task as cleaned in the store
2. Only if that succeeds, call `store_log_activity()` to log the branch_delete event
3. If `mark_cleaned` fails, return early without logging the activity (to be retried on next tick)

## Code Changes
```rust
// Before (problematic):
if did_clean {
    store_log_activity(...).await;  // ← Logged FIRST
    if let Ok(Some(store_id)) = store.resolve_task_id(repo, task_id).await {
        if let Err(e) = store.mark_cleaned(store_id).await {  // ← Then marked
            tracing::warn!(task_id, err = %e, "failed to mark worktree cleaned in store — will retry");
        }
    }
}

// After (fixed):
if did_clean {
    // Mark as cleaned in store FIRST
    if let Ok(Some(store_id)) = store.resolve_task_id(repo, task_id).await {
        if let Err(e) = store.mark_cleaned(store_id).await {
            tracing::warn!(task_id, err = %e, "failed to mark worktree cleaned — will retry");
            // Don't log activity; retry next tick will do the actual cleanup
            return Ok(did_clean);
        }
    }
    // Only log activity AFTER successful mark
    store_log_activity(...).await;
}
```

## Impact
- Eliminates duplicate `branch_delete` events in task timelines
- Prevents unnecessary branch deletion attempts on retry (though git branch delete is idempotent)
- Maintains proper cleanup semantics: activity logged only when task is actually marked as cleaned
- Preserves existing error handling and retry logic for transient failures

## Testing
- Verified the fix compiles successfully with `cargo check`
- The change maintains backward compatibility
- No changes to function signatures or public APIs