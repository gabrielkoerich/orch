//! RAII guard for the per-task dispatching [`DashMap`].
//!
//! [`DispatchGuard`] removes a key from the shared `dispatching` map when it is
//! dropped — whether the owning async task completes normally **or panics**.
//!
//! ## Problem it solves
//!
//! Without this guard, the cleanup code (`map.remove(&key)`) only runs when the
//! spawned async block reaches the end of its normal execution path.  If the block
//! panics (e.g. from an `unwrap()` inside `review_and_merge`), Tokio catches the
//! panic and terminates the task, but the manual remove never executes.  The key
//! leaks in the map permanently: subsequent attempts to review the task are skipped
//! by the guard check, and stuck-task recovery keeps resetting the task to
//! `NeedsReview` in an infinite loop until the service is restarted.
//!
//! ## Why DashMap instead of Mutex\<HashMap\>
//!
//! The dispatching map is accessed from both async contexts (tick, sync, subscribers)
//! and synchronous `Drop` implementations.  `DashMap` is a lock-free concurrent map
//! that works in both contexts without risk of blocking Tokio worker threads — unlike
//! `std::sync::Mutex` which blocks the OS thread, or `tokio::sync::Mutex` which
//! cannot be used in `Drop`.
//!
//! ## Why DashMap instead of DashSet
//!
//! `DashSet::insert` returns `false` in two distinct cases:
//! 1. The same task is already being dispatched (intended skip).
//! 2. A different task happens to produce the same key (should never happen, but
//!    silently skips a valid dispatch if it does).
//!
//! By storing the task-id as the map value, the caller can distinguish these cases:
//! a matching task-id means an expected duplicate; a mismatched task-id is an
//! unexpected collision that warrants a warning log.
//!
//! ## Usage
//!
//! ```rust,ignore
//! use dashmap::mapref::entry::Entry;
//!
//! // 1. Attempt to claim the dispatch slot atomically.
//! match dispatching.entry(dispatch_key.clone()) {
//!     Entry::Occupied(existing) => {
//!         let existing_id = existing.get().clone();
//!         drop(existing); // release shard lock before logging
//!         if existing_id == task.id.0 {
//!             tracing::debug!(task_id = %task.id.0, "task already dispatching, skipping duplicate");
//!         } else {
//!             tracing::warn!(
//!                 task_id = %task.id.0,
//!                 existing_task_id = %existing_id,
//!                 dispatch_key,
//!                 "dispatch key collision: unexpected task already holds this key"
//!             );
//!         }
//!         continue;
//!     }
//!     Entry::Vacant(slot) => {
//!         slot.insert(task.id.0.clone());
//!     }
//! }
//!
//! // 2. Create a guard that owns the removal obligation.
//! let guard = DispatchGuard::new(Arc::clone(&dispatching), dispatch_key.clone());
//!
//! // 3. Move the guard into the spawned task.
//! tokio::spawn(async move {
//!     let _guard = guard; // key removed on drop, even on panic
//!     // ... do work ...
//! });
//! ```

use dashmap::DashMap;
use std::sync::Arc;

/// Removes a key from a shared [`DashMap<String, String>`] when dropped.
///
/// Create this guard (step 2) only after the key has already been inserted into
/// the map (step 1) and before calling `tokio::spawn` (step 3).  Moving the guard
/// into the spawned async block ensures the key is removed regardless of whether
/// the block completes normally or unwinds via a panic.
pub struct DispatchGuard {
    map: Arc<DashMap<String, String>>,
    key: String,
}

impl DispatchGuard {
    /// Creates a guard that will remove `key` from `map` on drop.
    ///
    /// The caller must have already inserted `key` into `map`.
    pub fn new(map: Arc<DashMap<String, String>>, key: String) -> Self {
        Self { map, key }
    }
}

impl Drop for DispatchGuard {
    fn drop(&mut self) {
        self.map.remove(&self.key);
        // Deliberately logged at drop time (not just claim time): this is the only
        // signal that lets a future stuck-task-reclaim race be diagnosed from logs
        // by comparing this timestamp against the reclaim check's own log line for
        // the same key, instead of relying on static reasoning about ordering.
        tracing::debug!(dispatch_key = %self.key, "dispatch guard released");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn make_map(entries: &[(&str, &str)]) -> Arc<DashMap<String, String>> {
        let map = DashMap::new();
        for (k, v) in entries {
            map.insert(k.to_string(), v.to_string());
        }
        Arc::new(map)
    }

    #[test]
    fn drop_removes_key() {
        let map = make_map(&[("owner/repo/1", "1"), ("owner/repo/2", "2")]);
        let guard = DispatchGuard::new(Arc::clone(&map), "owner/repo/1".to_string());
        drop(guard);
        assert!(
            !map.contains_key("owner/repo/1"),
            "key must be removed on drop"
        );
        assert!(
            map.contains_key("owner/repo/2"),
            "unrelated key must be untouched"
        );
    }

    #[test]
    fn drop_is_idempotent_when_key_absent() {
        let map = make_map(&[]);
        // Key not present — should not panic
        let guard = DispatchGuard::new(Arc::clone(&map), "missing".to_string());
        drop(guard);
        assert!(map.is_empty());
    }

    #[tokio::test]
    async fn drop_runs_on_task_panic() {
        let map = make_map(&[("owner/repo/42", "42")]);
        let key = "owner/repo/42".to_string();
        let guard = DispatchGuard::new(Arc::clone(&map), key.clone());

        let handle = tokio::spawn(async move {
            let _guard = guard;
            panic!("simulated panic");
        });
        let _ = handle.await; // absorb JoinError

        assert!(
            !map.contains_key(&key),
            "key must be removed even when spawned task panics"
        );
    }
}
