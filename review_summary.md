# Review of .await Usage While Holding Mutexes

## Summary

After reviewing the codebase for instances where `.lock().await` is used followed by await operations, I found that all mutex usage follows proper scoping patterns. Locks are held only for brief critical sections and are released before any await points that could block or yield control.

## Detailed Findings

### src/engine/cooldown.rs
All instances of `cooldown_store().lock().await` are followed by:
- Simple assignment (`= Some(store)`)
- Cloning operations (`.clone()`)
- These operations are synchronous and complete before any await points

Examples:
- Line 191: `*cooldown_store().lock().await = Some(store);` - Lock held only for assignment
- Line 259: `let store_opt = cooldown_store().lock().await.clone();` - Lock held only for clone, released before `read_and_increment_failure_count().await`
- Similar patterns on lines 288, 381, 419, 434, 469, 698, 957, etc.

### src/engine/sync.rs
Mutex usage appears only in test functions:
- Line 2456: `let comments = backend.comments.lock().await;` - Lock held briefly for length check
- Line 2460: `let status_updates = backend.status_updates.lock().await;` - Same pattern
These are in test contexts and properly scoped.

### src/engine/router/llm.rs
- Line 681: `let mut cache = self.skills_catalog.lock().await;` - Lock held for cache operations, then released
- Line 690: `let cache = self.skills_catalog.lock().await;` - Lock held briefly for cache check
- Line 735: `let mut cache = self.skills_catalog.lock().await;` - Lock held briefly for cache update
All followed by synchronous operations before any await points.

### src/control.rs
- Line 675: `let _guard = session_lock.lock().await;` - Creates a guard for synchronous operations, no await points within the guarded section.

## Conclusion

All mutex lock usage in the codebase properly follows the pattern of:
1. Acquiring lock with `.lock().await`
2. Performing only synchronous, quick operations while lock is held
3. Releasing lock (by dropping guard) before any await points

No instances were found where locks are held across await points that could lead to deadlocks or lock contention issues. The code correctly avoids holding mutexes during potentially blocking operations.

## Recommendations

No changes are required. The current implementation correctly handles mutex locking without holding locks across await points.