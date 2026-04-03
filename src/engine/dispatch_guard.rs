//! RAII guard for the per-task dispatching [`DashSet`].
//!
//! [`DispatchGuard`] removes a key from the shared `dispatching` set when it is
//! dropped — whether the owning async task completes normally **or panics**.
//!
//! ## Problem it solves
//!
//! Without this guard, the cleanup code (`set.remove(&key)`) only runs when the
//! spawned async block reaches the end of its normal execution path.  If the block
//! panics (e.g. from an `unwrap()` inside `review_and_merge`), Tokio catches the
//! panic and terminates the task, but the manual remove never executes.  The key
//! leaks in the set permanently: subsequent attempts to review the task are skipped
//! by the guard check, and stuck-task recovery keeps resetting the task to
//! `NeedsReview` in an infinite loop until the service is restarted.
//!
//! ## Why DashSet instead of Mutex\<HashSet\>
//!
//! The dispatching set is accessed from both async contexts (tick, sync, subscribers)
//! and synchronous `Drop` implementations.  `DashSet` is a lock-free concurrent set
//! that works in both contexts without risk of blocking Tokio worker threads — unlike
//! `std::sync::Mutex` which blocks the OS thread, or `tokio::sync::Mutex` which
//! cannot be used in `Drop`.
//!
//! ## Usage
//!
//! ```rust,ignore
//! // 1. Insert the key BEFORE the spawn.
//! if !dispatching.insert(dispatch_key.clone()) {
//!     continue; // already dispatching
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

use dashmap::DashSet;
use std::sync::Arc;

/// Removes a key from a shared [`DashSet<String>`] when dropped.
///
/// Create this guard (step 2) only after the key has already been inserted into
/// the set (step 1) and before calling `tokio::spawn` (step 3).  Moving the guard
/// into the spawned async block ensures the key is removed regardless of whether
/// the block completes normally or unwinds via a panic.
pub struct DispatchGuard {
    set: Arc<DashSet<String>>,
    key: String,
}

impl DispatchGuard {
    /// Creates a guard that will remove `key` from `set` on drop.
    ///
    /// The caller must have already inserted `key` into `set`.
    pub fn new(set: Arc<DashSet<String>>, key: String) -> Self {
        Self { set, key }
    }
}

impl Drop for DispatchGuard {
    fn drop(&mut self) {
        self.set.remove(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn make_set(keys: &[&str]) -> Arc<DashSet<String>> {
        let set = DashSet::new();
        for key in keys {
            set.insert(key.to_string());
        }
        Arc::new(set)
    }

    #[test]
    fn drop_removes_key() {
        let set = make_set(&["a", "b"]);
        let guard = DispatchGuard::new(Arc::clone(&set), "a".to_string());
        drop(guard);
        assert!(!set.contains("a"), "key must be removed on drop");
        assert!(set.contains("b"), "unrelated key must be untouched");
    }

    #[test]
    fn drop_is_idempotent_when_key_absent() {
        let set = make_set(&[]);
        // Key not present — should not panic
        let guard = DispatchGuard::new(Arc::clone(&set), "missing".to_string());
        drop(guard);
        assert!(set.is_empty());
    }

    #[tokio::test]
    async fn drop_runs_on_task_panic() {
        let set = make_set(&["owner/repo/42"]);
        let key = "owner/repo/42".to_string();
        let guard = DispatchGuard::new(Arc::clone(&set), key.clone());

        let handle = tokio::spawn(async move {
            let _guard = guard;
            panic!("simulated panic");
        });
        let _ = handle.await; // absorb JoinError

        assert!(
            !set.contains(&key),
            "key must be removed even when spawned task panics"
        );
    }
}
