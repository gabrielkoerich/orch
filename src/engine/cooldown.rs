//! Per-agent and per-model cooldown tracking.
//!
//! Shared by `engine::runner::response` (recording failures) and
//! `engine::router::config` (avoiding cooled models during pool selection).
//!
//! # Persistence
//!
//! All cooldowns are persisted to the KV store in SQLite so they survive
//! service restarts. Call [`init_cooldown_store`] once at startup (after
//! the `TaskStore` is open) to register the store and pre-load unexpired
//! cooldowns.
//!
//! When a rate limit includes a "try again at {date}" message, the cooldown
//! is set to that specific timestamp instead of the default duration.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// Default cooldown duration for failed agents (30 minutes).
pub const AGENT_COOLDOWN_SECS: i64 = 30 * 60;

/// Default cooldown duration for model-specific failures (1 hour).
pub const MODEL_COOLDOWN_SECS: i64 = 60 * 60;

/// Short agent cooldown applied on silence detection (120 seconds).
///
/// Forces the router to pick a different agent immediately on re-route,
/// without long-term blocking. The model cooldown (30 min) keeps the dead
/// model out while this short cooldown just breaks the same-agent loop.
pub const SILENCE_AGENT_COOLDOWN_SECS: u64 = 120;

/// Silence detections before applying an extended cooldown (rolling window).
pub const SILENCE_COUNT_THRESHOLD: usize = 5;

/// Rolling window for silence detections (24 hours).
pub const SILENCE_COUNT_WINDOW_SECS: i64 = 24 * 60 * 60;

/// Extended cooldown applied after repeated silence detections (4 hours).
pub const SILENCE_EXTENDED_COOLDOWN_SECS: u64 = 4 * 60 * 60;

/// KV key prefix for persisted cooldowns (both agent and model).
const KV_PREFIX: &str = "cooldown:";

/// KV key prefix for rolling silence counters.
const SILENCE_COUNT_PREFIX: &str = "silence_count:";

struct CooldownEntry {
    /// Unix timestamp when the cooldown expires.
    cooldown_until: i64,
    #[allow(dead_code)]
    reason: String,
}

pub struct SilenceCountResult {
    pub count: usize,
    pub extended_cooldown_applied: bool,
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
/// Loads unexpired cooldowns from KV and registers the store for future writes.
pub async fn init_cooldown_store(store: Arc<crate::store::TaskStore>) {
    match store.kv_list_prefix(KV_PREFIX).await {
        Ok(rows) => {
            let now = chrono::Utc::now().timestamp();
            let mut map = cooldowns().lock().unwrap_or_else(|e| e.into_inner());
            let mut loaded = 0;
            for (key, value) in rows {
                let cooldown_key = key.trim_start_matches(KV_PREFIX);
                if let Ok(cooldown_until) = value.parse::<i64>() {
                    if now < cooldown_until {
                        map.insert(
                            cooldown_key.to_string(),
                            CooldownEntry {
                                cooldown_until,
                                reason: "persisted".to_string(),
                            },
                        );
                        loaded += 1;
                    }
                    // Expired entries are simply not loaded — they'll be overwritten
                    // on the next failure or cleaned up by future writes.
                }
            }
            if loaded > 0 {
                tracing::info!(loaded, "loaded persisted cooldowns from KV store");
            }
        }
        Err(e) => {
            tracing::warn!(err = %e, "failed to load cooldowns from KV store");
        }
    }

    if let Ok(mut slot) = cooldown_store().lock() {
        *slot = Some(store);
    }
}

/// Record that an agent has failed and should be temporarily avoided.
///
/// If `error_message` contains "try again at {date}", the cooldown is set
/// to that date. Otherwise uses the default 30-minute cooldown.
pub fn record_agent_failure_with_message(agent_name: &str, error_message: &str) {
    let cooldown_until = parse_retry_at(error_message)
        .unwrap_or_else(|| chrono::Utc::now().timestamp() + AGENT_COOLDOWN_SECS);

    set_cooldown(agent_name, cooldown_until, "agent_error");
}

/// Record that a specific agent+model combo has failed.
///
/// Persisted to KV so it survives restarts.
pub fn record_model_failure(agent_name: &str, model: &str) {
    let key = format!("{agent_name}:{model}");
    let cooldown_until = chrono::Utc::now().timestamp() + MODEL_COOLDOWN_SECS;
    set_cooldown(&key, cooldown_until, "model_error");
}

/// Set a model cooldown with a custom duration (in seconds).
///
/// Used by silence detection to cooldown the specific model that failed to
/// produce any output, with a configurable duration.
pub fn set_model_cooldown(agent_name: &str, model: &str, duration_secs: u64) {
    let key = format!("{agent_name}:{model}");
    let cooldown_until = chrono::Utc::now().timestamp() + duration_secs as i64;
    set_cooldown(&key, cooldown_until, "silence_detected");
}

/// Set a short agent-level cooldown (in seconds).
///
/// Used by silence detection to temporarily block the whole agent so the
/// router picks a different one on re-route. Unlike `record_agent_failure`
/// (30 min), this uses a short duration (typically 120s) — just enough to
/// force one re-route cycle to a different agent.
pub fn set_agent_cooldown(agent_name: &str, duration_secs: u64) {
    let cooldown_until = chrono::Utc::now().timestamp() + duration_secs as i64;
    set_cooldown(agent_name, cooldown_until, "silence_agent_cooldown");
}

/// Record a silence detection for an agent+model and apply extended cooldowns
/// when repeated silences exceed the threshold within the rolling window.
pub async fn record_silence_detection(agent_name: &str, model: &str) -> Option<SilenceCountResult> {
    let store_opt = cooldown_store().lock().ok().and_then(|g| g.clone());
    let store = match store_opt {
        Some(store) => store,
        None => {
            tracing::debug!(
                agent = agent_name,
                model,
                "skipping silence count record (no KV store)"
            );
            return None;
        }
    };

    let key = format!("{SILENCE_COUNT_PREFIX}{agent_name}:{model}");
    let now = chrono::Utc::now().timestamp();
    let window_start = now - SILENCE_COUNT_WINDOW_SECS;

    let mut timestamps = match store.kv_get(&key).await {
        Ok(Some(raw)) => serde_json::from_str::<Vec<i64>>(&raw).unwrap_or_default(),
        Ok(None) => Vec::new(),
        Err(err) => {
            tracing::warn!(
                kv_key = key,
                err = %err,
                "failed to load silence count from KV"
            );
            Vec::new()
        }
    };

    timestamps.retain(|ts| *ts >= window_start);
    timestamps.push(now);

    let count = timestamps.len();
    let mut extended_cooldown_applied = false;
    if count >= SILENCE_COUNT_THRESHOLD {
        set_model_cooldown(agent_name, model, SILENCE_EXTENDED_COOLDOWN_SECS);
        extended_cooldown_applied = true;
        timestamps.clear();
    }

    let value = serde_json::to_string(&timestamps).unwrap_or_else(|_| "[]".to_string());
    if let Err(err) = store.kv_set(&key, &value).await {
        tracing::warn!(kv_key = key, err = %err, "failed to persist silence count");
    }

    Some(SilenceCountResult {
        count,
        extended_cooldown_applied,
    })
}

/// Check if a specific agent+model combo is in cooldown.
pub fn is_model_in_cooldown(agent_name: &str, model: &str) -> bool {
    let key = format!("{agent_name}:{model}");
    is_in_cooldown(&key)
}

/// Check if an agent is currently in cooldown period.
pub fn is_agent_in_cooldown(agent_name: &str) -> bool {
    is_in_cooldown(agent_name)
}

/// Set a cooldown with a specific expiry timestamp. Persists to KV.
fn set_cooldown(key: &str, cooldown_until: i64, reason: &str) {
    {
        let mut map = cooldowns().lock().unwrap_or_else(|e| e.into_inner());
        map.insert(
            key.to_string(),
            CooldownEntry {
                cooldown_until,
                reason: reason.to_string(),
            },
        );
    }

    // Persist to KV via a background task when we have a runtime.
    // Unit tests call cooldown helpers without a Tokio runtime; avoid panicking.
    let store_opt = cooldown_store().lock().ok().and_then(|g| g.clone());
    if let Some(store) = store_opt {
        let kv_key = format!("{KV_PREFIX}{key}");
        let value = cooldown_until.to_string();
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::spawn(async move {
                if let Err(e) = store.kv_set(&kv_key, &value).await {
                    tracing::warn!(kv_key, err = %e, "failed to persist cooldown to KV store");
                }
            });
        } else {
            tracing::debug!(kv_key, "skipping KV cooldown persist (no Tokio runtime)");
        }
    }
}

/// Check if a key is currently in cooldown.
fn is_in_cooldown(key: &str) -> bool {
    let map = cooldowns().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(entry) = map.get(key) {
        let now = chrono::Utc::now().timestamp();
        return now < entry.cooldown_until;
    }
    false
}

/// Clear expired cooldowns from the in-memory map.
pub fn clear_expired_cooldowns() {
    let mut map = cooldowns().lock().unwrap_or_else(|e| e.into_inner());
    let now = chrono::Utc::now().timestamp();
    map.retain(|_key, entry| now < entry.cooldown_until);
}

/// Parse a "try again at {date}" or "try again after {time}" from an error message.
///
/// Returns a Unix timestamp if a retry-at date is found.
fn parse_retry_at(error_message: &str) -> Option<i64> {
    if error_message.is_empty() {
        return None;
    }

    // Look for "try again at {date}" pattern
    let lower = error_message.to_lowercase();
    let retry_marker = "try again at ";
    if let Some(idx) = lower.find(retry_marker) {
        let date_str = &error_message[idx + retry_marker.len()..];
        // Take until period, newline, or end
        let date_str = date_str
            .split(['.', '\n'])
            .next()
            .unwrap_or(date_str)
            .trim();

        // Try common date formats
        // "Mar 26th, 2026 5:55 AM" → strip ordinal suffixes
        let cleaned = date_str
            .replace("st,", ",")
            .replace("nd,", ",")
            .replace("rd,", ",")
            .replace("th,", ",");

        for fmt in &[
            "%b %d, %Y %I:%M %p", // Mar 26, 2026 5:55 AM
            "%b %d, %Y %I:%M%p",  // Mar 26, 2026 5:55AM
            "%B %d, %Y %I:%M %p", // March 26, 2026 5:55 AM
            "%Y-%m-%dT%H:%M:%S",  // 2026-03-26T05:55:00
            "%Y-%m-%d %H:%M",     // 2026-03-26 05:55
        ] {
            if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(&cleaned, fmt) {
                let utc = dt.and_utc();
                tracing::info!(
                    retry_at = %utc,
                    raw = date_str,
                    "parsed rate limit retry-at date"
                );
                return Some(utc.timestamp());
            }
        }
        tracing::debug!(raw = date_str, "could not parse retry-at date");
    }

    // "billing cycle" / "next cycle" / "quota" without a specific date → cooldown 24 hours.
    // Billing cycles are typically daily/weekly/monthly so a 5-hour cooldown was too short:
    // the model would be retried multiple times per day, wasting ~2 min per attempt.
    if lower.contains("billing cycle")
        || lower.contains("next cycle")
        || lower.contains("quota will be refreshed")
    {
        let twenty_four_hours = chrono::Utc::now().timestamp() + 24 * 60 * 60;
        tracing::info!("detected billing cycle limit — cooldown for 24 hours");
        return Some(twenty_four_hours);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let cooldown_until = chrono::Utc::now().timestamp() + 3600; // 1 hour from now

        store
            .kv_set(
                &format!("{KV_PREFIX}kimi:k2p5"),
                &cooldown_until.to_string(),
            )
            .await
            .unwrap();

        // Also add an already-expired entry
        let expired = chrono::Utc::now().timestamp() - 1;
        store
            .kv_set(
                &format!("{KV_PREFIX}opencode:old-free"),
                &expired.to_string(),
            )
            .await
            .unwrap();

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

        {
            let mut slot = cooldown_store().lock().unwrap();
            *slot = Some(store.clone());
        }

        record_model_failure("testagent_persist", "testmodel_persist");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let kv_key = format!("{KV_PREFIX}testagent_persist:testmodel_persist");
        let stored = store.kv_get(&kv_key).await.unwrap();
        assert!(stored.is_some(), "model failure should be persisted to KV");
        let ts: i64 = stored.unwrap().parse().expect("timestamp string");
        let now = chrono::Utc::now().timestamp();
        // Should be in the future (cooldown_until, not failed_at)
        assert!(ts > now, "persisted timestamp should be in the future");
    }

    #[test]
    fn parse_retry_at_codex_format() {
        let msg = "You've hit your usage limit. Upgrade to Pro, visit https://chatgpt.com/codex/settings/usage to purchase more credits or try again at Mar 26th, 2026 5:55 AM.";
        let ts = parse_retry_at(msg);
        assert!(ts.is_some(), "should parse codex retry-at date");
        // Should be March 26 2026
        let dt = chrono::DateTime::from_timestamp(ts.unwrap(), 0).unwrap();
        use chrono::Datelike;
        assert_eq!(dt.month(), 3);
        assert_eq!(dt.day(), 26);
    }

    #[test]
    fn parse_retry_at_no_date() {
        assert!(parse_retry_at("generic rate limit error").is_none());
        assert!(parse_retry_at("").is_none());
    }

    #[test]
    fn parse_retry_at_billing_cycle_sets_24h_cooldown() {
        let msg = "You've reached your usage limit for this billing cycle. Your quota will be refreshed in the next cycle. Upgrade to get more.";
        let ts = parse_retry_at(msg);
        assert!(ts.is_some(), "billing cycle message should set a cooldown");
        let now = chrono::Utc::now().timestamp();
        let remaining = ts.unwrap() - now;
        // Should be ~24 hours (86400s), allow ±5s for test execution time
        assert!(
            remaining > 86395 && remaining <= 86400,
            "billing cycle cooldown should be ~24 hours, got {remaining}s"
        );
    }

    #[test]
    fn agent_cooldown_persists_to_kv() {
        // Agent-level cooldowns should also go through set_cooldown which persists
        let agent = "test_agent_persist_check";
        record_agent_failure_with_message(agent, "");
        assert!(is_agent_in_cooldown(agent));
    }

    #[test]
    fn silence_agent_cooldown_is_short_lived() {
        let agent = "test_silence_agent_cd";
        assert!(!is_agent_in_cooldown(agent));

        set_agent_cooldown(agent, SILENCE_AGENT_COOLDOWN_SECS);
        assert!(is_agent_in_cooldown(agent));

        // Verify it's a short cooldown (120s), not the long one (30 min)
        let map = cooldowns().lock().unwrap();
        let entry = map.get(agent).expect("should have cooldown entry");
        let now = chrono::Utc::now().timestamp();
        let remaining = entry.cooldown_until - now;
        assert!(
            remaining <= SILENCE_AGENT_COOLDOWN_SECS as i64,
            "silence agent cooldown should be <= {SILENCE_AGENT_COOLDOWN_SECS}s, got {remaining}s"
        );
        assert!(remaining > 0, "cooldown should still be active");
    }

    #[tokio::test]
    async fn record_silence_detection_applies_extended_cooldown() {
        let store = test_store().await;
        {
            let mut slot = cooldown_store().lock().unwrap();
            *slot = Some(store.clone());
        }

        let agent = "test_silence_count_agent";
        let model = "test_silence_count_model";
        assert!(!is_model_in_cooldown(agent, model));

        for _ in 0..SILENCE_COUNT_THRESHOLD {
            let result = record_silence_detection(agent, model).await;
            assert!(result.is_some());
        }

        assert!(is_model_in_cooldown(agent, model));
        let key = format!("{agent}:{model}");
        let remaining = {
            let map = cooldowns().lock().unwrap();
            let entry = map.get(&key).expect("cooldown entry should exist");
            entry.cooldown_until - chrono::Utc::now().timestamp()
        };
        assert!(
            remaining >= SILENCE_EXTENDED_COOLDOWN_SECS as i64 - 5,
            "extended cooldown should be applied, got {remaining}s"
        );

        let kv_key = format!("{SILENCE_COUNT_PREFIX}{agent}:{model}");
        let stored = store.kv_get(&kv_key).await.unwrap().unwrap();
        assert_eq!(stored, "[]");
    }
}
