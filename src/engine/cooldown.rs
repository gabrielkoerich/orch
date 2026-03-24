//! Per-agent and per-model cooldown tracking.
//!
//! Shared by `engine::runner::response` (recording failures) and
//! `engine::router::config` (avoiding cooled models during pool selection).
//! Extracted here to avoid a circular dependency between the two.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Cooldown duration for failed agents (30 minutes).
pub const AGENT_COOLDOWN_SECS: i64 = 30 * 60;

/// Cooldown duration for model-specific failures (1 hour).
/// When a specific agent+model combo fails (e.g., model not available,
/// model-specific rate limit), we ban that combo for longer.
pub const MODEL_COOLDOWN_SECS: i64 = 60 * 60;

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

/// Record that an agent has failed and should be temporarily avoided.
pub fn record_agent_failure(agent_name: &str) {
    record_failure_with_reason(agent_name, "agent_error");
}

/// Record that a specific agent+model combo has failed.
///
/// The cooldown key is `"agent:model"` so we can track model-specific
/// failures separately (e.g., codex with o3-mini fails but gpt-4o works).
pub fn record_model_failure(agent_name: &str, model: &str) {
    let key = format!("{agent_name}:{model}");
    record_failure_with_reason(&key, "model_error");
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
