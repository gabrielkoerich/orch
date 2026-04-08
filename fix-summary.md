# Fix for block_reason race condition in auto_merge.rs and subscribers/review.rs

## Problem
The same race condition fixed in commit 61b8c0f2 (#2181) still existed in 2 locations:
1. `src/engine/auto_merge.rs` line ~1233 in handle_review_changes function
2. `src/engine/subscribers/review.rs` in ReviewOutcome::Block arm

In both locations, `update_task_status(Status::Blocked)` was called before persisting `block_reason`. Between these operations, the auto-unblock job in `sync.rs:336` would check `task.block_reason.is_some()` and auto-unblock tasks without a block_reason, creating an infinite block/unblock loop.

## Solution
Applied the same fix pattern from commit 61b8c0f2:
1. Persist `block_reason` using `store_set_result` BEFORE calling `update_task_status(Status::Blocked)`
2. If the block_reason write fails, log error and skip blocking to avoid silent auto-unblock loop
3. Only call `update_task_status(Status::Blocked)` after block_reason is persisted

## Changes Made

### src/engine/auto_merge.rs
Added block_reason persistence before Blocked transition in handle_review_changes function:
```rust
// Persist block_reason BEFORE transitioning to Blocked to avoid
// a race where auto_unblock sees a blocked task without a reason
// and immediately unblocks it.
let fields = [(
    "block_reason",
    serde_json::json!(format!("max review cycles ({}) reached", max_cycles)),
)];
if let Err(e) = store_set_result(&Some(Arc::clone(store)), repo, &task.id.0, &fields).await
{
    tracing::error!(task_id = task.id.0, err = %e, "failed to write block_reason — skipping block to avoid silent auto-unblock loop");
    return Ok(());
}
// A transient store failure here must not be counted as a review-agent
// crash.  Log and return Ok — the next tick will re-check the task and
// can retry the status transition.
if let Err(e) = task_manager
    .update_task_status(&task.id, Status::Blocked)
    .await
{
    tracing::warn!(
        task_id = task.id.0,
        err = %e,
        "failed to set Blocked after max review cycles — will retry on next tick"
    );
    return Ok(());
}
```

### src/engine/subscribers/review.rs
Fixed ReviewOutcome::Block arm to use store_set_result and persist block_reason before blocking:
```rust
// Persist block_reason BEFORE transitioning to Blocked to avoid
// a race where auto_unblock sees a blocked task without a reason
// and immediately unblocks it.
let fields = [
    (
        "block_reason",
        serde_json::json!("review agent blocked — exceeded failure threshold"),
    ),
    ("last_error", serde_json::json!(reason)),
];
if let Err(e) = store_set_result(
    &Some(store_c.clone()),
    &repo_s,
    &tid,
    &fields,
)
.await
{
    tracing::error!(task_id = %tid, err = %e, "failed to write block_reason — skipping block to avoid silent auto-unblock loop");
} else {
    if let Err(e) = task_manager_c
        .update_task_status(
            &ExternalId(tid.clone()),
            Status::Blocked,
        )
        .await
    {
        tracing::error!(task_id = %tid, err = %e, "update_task_status(Blocked) failed — task may be stuck in InReview");
    }
}
```

## Verification
- Code compiles successfully with `cargo check`
- Code formatting passes with `cargo fmt -- --check`
- All changes follow the established pattern from commit 61b8c0f2