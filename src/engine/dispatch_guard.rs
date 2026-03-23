//! RAII guard for the per-task dispatching HashSet.
//!
//! [`DispatchGuard`] removes a key from the shared `dispatching` set when it is
//! dropped — whether the owning async task completes normally **or panics**.
//!
//! ## Problem it solves
//!
//! Without this guard, the cleanup code (`guard.remove(&key)`) only runs when the
//! spawned async block reaches the end of its normal execution path.  If the block
//! panics (e.g. from an `unwrap()` inside `review_and_merge`), Tokio catches the
//! panic and terminates the task, but the manual remove never executes.  The key
//! leaks in the set permanently: subsequent attempts to review the task are skipped
//! by the guard check, and stuck-task recovery keeps resetting the task to
//! `NeedsReview` in an infinite loop until the service is restarted.
//!
//! ## Usage
//!
//! ```rust,ignore
//! // 1. Insert the key BEFORE the spawn (maintains the a6d8b9a invariant).
//! {
//!     let mut g = dispatching.lock().unwrap_or_else(|e| e.into_inner());
//!     g.insert(dispatch_key.clone());
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

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// Removes a key from a shared `HashSet<String>` when dropped.
///
/// Create this guard (step 2) only after the key has already been inserted into
/// the set (step 1) and before calling `tokio::spawn` (step 3).  Moving the guard
/// into the spawned async block ensures the key is removed regardless of whether
/// the block completes normally or unwinds via a panic.
pub struct DispatchGuard {
    set: Arc<Mutex<HashSet<String>>>,
    key: String,
}

impl DispatchGuard {
    /// Creates a guard that will remove `key` from `set` on drop.
    ///
    /// The caller must have already inserted `key` into `set`.
    pub fn new(set: Arc<Mutex<HashSet<String>>>, key: String) -> Self {
        Self { set, key }
    }
}

impl Drop for DispatchGuard {
    fn drop(&mut self) {
        // Use `lock()` (not `unwrap_or_else`) so we silently no-op if the mutex
        // is poisoned — poisoning means another thread already panicked while
        // holding the lock, and the key may or may not be present.  In that
        // scenario skipping the remove is the safest option; the service will
        // recover on restart.
        if let Ok(mut g) = self.set.lock() {
            g.remove(&self.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn make_set(keys: &[&str]) -> Arc<Mutex<HashSet<String>>> {
        let set = HashSet::from_iter(keys.iter().map(|s| s.to_string()));
        Arc::new(Mutex::new(set))
    }

    #[test]
    fn drop_removes_key() {
        let set = make_set(&["a", "b"]);
        let guard = DispatchGuard::new(Arc::clone(&set), "a".to_string());
        drop(guard);
        let g = set.lock().unwrap();
        assert!(!g.contains("a"), "key must be removed on drop");
        assert!(g.contains("b"), "unrelated key must be untouched");
    }

    #[test]
    fn drop_is_idempotent_when_key_absent() {
        let set = make_set(&[]);
        // Key not present — should not panic
        let guard = DispatchGuard::new(Arc::clone(&set), "missing".to_string());
        drop(guard);
        assert!(set.lock().unwrap().is_empty());
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
            !set.lock().unwrap().contains(&key),
            "key must be removed even when spawned task panics"
        );
    }
}
