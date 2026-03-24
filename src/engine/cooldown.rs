//! Per-agent and per-model cooldown tracking.
//!
//! Shared by `engine::runner::response` (recording failures) and
//! `engine::router::config` (avoiding cooled models during pool selection).
//! Extracted here to avoid a circular dependency between the two.
//!
//! # Persistence
//!
//! Model-level cooldowns are persisted to the KV store in SQLite so they
//! survive service restarts.  Call [`init_cooldown_store`] once at startup
//! (after the `TaskStore` is open) to:
//!
//! 1. Register the store for background writes, and
//! 2. Pre-load any unexpired cooldowns that were recorded before the last
//!    restart.
//!
//! The in-memory map is the read-fast path; SQLite is the durable write path.
//! Agent-level cooldowns are not persisted (they are short-lived and rare).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// Cooldown duration for failed agents (30 minutes).
pub const AGENT_COOLDOWN_SECS: i64 = 30 * 60;

/// Cooldown duration for model-specific failures (1 hour).
/// When a specific agent+model combo fails (e.g., model not available,
/// model-specific rate limit), we ban that combo for longer.
pub const MODEL_COOLDOWN_SECS: i64 = 60 * 60;

/// KV key prefix for persisted model cooldowns.
const KV_PREFIX: &str = "cooldown:model:";

struct CooldownEntry {
    failed_at: i64,
    #[allow(dead_code)]
    reason: String,
}

/// Global in-memory cooldown map, protected by a Mutex.
fn cooldowns() -> &'static Mutex<HashMap<String, CooldownEntry>> {
    static COOLDOWNS: OnceLock<Mutex<HashMap<String, CooldownEntry>>> = OnceLock::new();
    COOLDOWNS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Global store reference used for background KV writes.
fn cooldown_store() -> &'static Mutex<Option<Arc<crate::store::TaskStore>>> {
    static STORE: OnceLock<Mutex<Option<Arc<crate::store::TaskStore>>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(None))
}

/// Initialise persistent cooldowns.
///
/// Must be called once at engine startup, after the `TaskStore` is opened.
/// This registers the store for background KV writes and pre-loads any
/// unexpired model cooldowns that were recorded before the last restart.
pub async fn init_cooldown_store(store: Arc<crate::store::TaskStore>) {
    // Load unexpired model cooldowns from KV into the in-memory map.
    match store.kv_list_prefix(KV_PREFIX).await {
        Ok(rows) => {
            let now = chrono::Utc::now().timestamp();
            let mut map = cooldowns().lock().unwrap_or_else(|e| e.into_inner());
            for (key, value) in rows {
                // key = "cooldown:model:{agent}:{model}"
                // value = failed_at unix timestamp as string
                let model_key = key.trim_start_matches(KV_PREFIX);
                if let Ok(failed_at) = value.parse::<i64>() {
                    if (now - failed_at) < MODEL_COOLDOWN_SECS {
                        map.insert(
                            model_key.to_string(),
                            CooldownEntry {
                                failed_at,
                                reason: "persisted".to_string(),
                            },
                        );
                    }
                }
            }
            tracing::debug!(loaded = map.len(), "loaded model cooldowns from KV store");
        }
        Err(e) => {
            tracing::warn!(err = %e, "failed to load cooldowns from KV store");
        }
    }

    // Register the store for background writes on future failures.
    if let Ok(mut slot) = cooldown_store().lock() {
        *slot = Some(store);
    }
}

/// Record that an agent has failed and should be temporarily avoided.
pub fn record_agent_failure(agent_name: &str) {
    record_failure_with_reason(agent_name, "agent_error");
}

/// Record that a specific agent+model combo has failed.
///
/// The cooldown key is `"agent:model"` so we can track model-specific
/// failures separately (e.g., codex with o3-mini fails but gpt-4o works).
/// The failure is also persisted to the KV store via a background task.
pub fn record_model_failure(agent_name: &str, model: &str) {
    let key = format!("{agent_name}:{model}");
    record_failure_with_reason(&key, "model_error");

    // Persist to KV via a background Tokio task so restarts don't lose state.
    let store_opt = cooldown_store().lock().ok().and_then(|g| g.clone());
    if let Some(store) = store_opt {
        let kv_key = format!("{KV_PREFIX}{key}");
        let failed_at = chrono::Utc::now().timestamp().to_string();
        tokio::spawn(async move {
            if let Err(e) = store.kv_set(&kv_key, &failed_at).await {
                tracing::warn!(
                    kv_key,
                    err = %e,
                    "failed to persist model cooldown to KV store"
                );
            }
        });
    }
}

/// Check if a specific agent+model combo is in cooldown.
pub fn is_model_in_cooldown(agent_name: &str, model: &str) -> bool {
    let key = format!("{agent_name}:{model}");
    is_key_in_cooldown(&key, MODEL_COOLDOWN_SECS)
}

/// Check if an agent is currently in cooldown period.
pub fn is_agent_in_cooldown(agent_name: &str) -> bool {
    is_key_in_cooldown(agent_name, AGENT_COOLDOWN_SECS)
}

pub fn record_failure_with_reason(key: &str, reason: &str) {
    let mut map = cooldowns().lock().unwrap_or_else(|e| e.into_inner());
    map.insert(
        key.to_string(),
        CooldownEntry {
            failed_at: chrono::Utc::now().timestamp(),
            reason: reason.to_string(),
        },
    );
}

pub fn is_key_in_cooldown(key: &str, max_age_secs: i64) -> bool {
    let map = cooldowns().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(entry) = map.get(key) {
        let now = chrono::Utc::now().timestamp();
        return (now - entry.failed_at) < max_age_secs;
    }
    false
}

/// Clear expired cooldowns from the in-memory map.
pub fn clear_expired_cooldowns() {
    let mut map = cooldowns().lock().unwrap_or_else(|e| e.into_inner());
    let now = chrono::Utc::now().timestamp();
    map.retain(|key, entry| {
        let timeout = if key.contains(':') {
            MODEL_COOLDOWN_SECS
        } else {
            AGENT_COOLDOWN_SECS
        };
        (now - entry.failed_at) < timeout
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh in-memory store for testing KV persistence.
    async fn test_store() -> Arc<crate::store::TaskStore> {
        Arc::new(
            crate::store::TaskStore::open_memory()
                .await
                .expect("in-memory store"),
        )
    }

    #[tokio::test]
    async fn init_loads_unexpired_cooldowns_from_kv() {
        let store = test_store().await;
        let failed_at = chrono::Utc::now().timestamp();

        // Pre-populate KV with a valid cooldown entry.
        store
            .kv_set(&format!("{KV_PREFIX}kimi:k2p5"), &failed_at.to_string())
            .await
            .unwrap();

        // Also add an already-expired entry (failed 2 hours ago).
        let expired_at = failed_at - (MODEL_COOLDOWN_SECS + 1);
        store
            .kv_set(
                &format!("{KV_PREFIX}opencode:old-free"),
                &expired_at.to_string(),
            )
            .await
            .unwrap();

        // Clear in-memory state before calling init.
        {
            let mut map = cooldowns().lock().unwrap();
            map.clear();
        }

        init_cooldown_store(store).await;

        assert!(
            is_model_in_cooldown("kimi", "k2p5"),
            "unexpired cooldown should be loaded"
        );
        assert!(
            !is_model_in_cooldown("opencode", "old-free"),
            "expired cooldown should not be loaded"
        );
    }

    #[tokio::test]
    async fn record_model_failure_writes_to_kv() {
        let store = test_store().await;

        // Reset global store slot to this test store.
        {
            let mut slot = cooldown_store().lock().unwrap();
            *slot = Some(store.clone());
        }

        record_model_failure("testagent", "testmodel");

        // Give the background task time to complete.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let kv_key = format!("{KV_PREFIX}testagent:testmodel");
        let stored = store.kv_get(&kv_key).await.unwrap();
        assert!(stored.is_some(), "model failure should be persisted to KV");
        let ts: i64 = stored.unwrap().parse().expect("timestamp string");
        let now = chrono::Utc::now().timestamp();
        assert!((now - ts).abs() < 5, "persisted timestamp should be recent");
    }
}
