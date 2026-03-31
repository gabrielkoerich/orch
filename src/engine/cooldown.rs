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

/// Default fallback cooldown for agents when no store is available: 30 minutes.
///
/// Used in contexts that need a concrete duration and cannot compute exponential
/// backoff (e.g. computing how long to wait before retrying when a cooldown
/// expiry timestamp is unknown). New code should use [`BACKOFF_BASE_SECS`] instead.
pub const AGENT_COOLDOWN_SECS: i64 = 30 * 60;

/// Default fallback cooldown for models when no store is available: 1 hour.
///
/// Used in contexts that need a concrete duration. New code should use
/// [`BACKOFF_BASE_SECS`] instead.
pub const MODEL_COOLDOWN_SECS: i64 = 60 * 60;

/// Base backoff for generic agent/model failures: 5 minutes.
pub const BACKOFF_BASE_SECS: i64 = 5 * 60;

/// Maximum backoff for generic agent/model failures: 4 hours.
pub const BACKOFF_MAX_SECS: i64 = 4 * 60 * 60;

/// Base backoff for credit exhaustion (out_of_credits): 1 hour.
pub const CREDIT_BACKOFF_BASE_SECS: i64 = 60 * 60;

/// Maximum backoff for credit exhaustion and org-level disabling: 8 hours.
pub const CREDIT_BACKOFF_MAX_SECS: i64 = 8 * 60 * 60;

/// Flat cooldown for billing cycle exhaustion: 24 hours (calendar event, no backoff).
pub const BILLING_CYCLE_COOLDOWN_SECS: i64 = 24 * 60 * 60;

/// Credit exhaustion reason detected from error message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreditExhaustionReason {
    /// Per-model credit exhaustion (out_of_credits).
    OutOfCredits,
    /// Organization-level disabling (org_level_disabled).
    OrgLevelDisabled,
    /// Monthly billing cycle exhaustion — 24h flat cooldown (no backoff).
    BillingCycleExhausted,
}

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

/// KV key prefix for per-agent and per-agent:model failure counts (drives exponential backoff).
const FAILURE_COUNT_PREFIX: &str = "failure_count:";

struct CooldownEntry {
    /// Unix timestamp when the cooldown expires.
    cooldown_until: i64,
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

/// Compute exponential backoff duration using base-3 growth.
///
/// Returns `min(base * 3^(count-1), max)` for count >= 1.
/// For count == 0 or 1 (first failure), returns `base`.
///
/// | count | base=300s | result |
/// |-------|-----------|--------|
/// | 0     | 300       | 300    |
/// | 1     | 300       | 300    |
/// | 2     | 300       | 900    |
/// | 3     | 300       | 2700   |
/// | 4     | 300       | 8100 → capped |
pub fn compute_backoff(count: u32, base: i64, max: i64) -> i64 {
    if count <= 1 {
        return base;
    }
    let factor = 3_i64.saturating_pow(count.saturating_sub(1));
    base.saturating_mul(factor).min(max)
}

/// Read the failure count for a key from KV, increment it, write it back, and return the new count.
///
/// Returns 1 when the store is unavailable (unit-test contexts without a store).
async fn read_and_increment_failure_count(
    store_opt: &Option<Arc<crate::store::TaskStore>>,
    key: &str,
) -> u32 {
    let kv_key = format!("{FAILURE_COUNT_PREFIX}{key}");
    if let Some(store) = store_opt {
        let count: u32 = store
            .kv_get(&kv_key)
            .await
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let new_count = count.saturating_add(1);
        let _ = store.kv_set(&kv_key, &new_count.to_string()).await;
        new_count
    } else {
        1
    }
}

/// Reset failure counts for an agent and agent:model combo after a successful run.
///
/// Call this from the runner's success path so that the next failure starts
/// backoff from the base duration again, not from wherever it left off.
pub async fn record_agent_success(agent_name: &str, model: &str) {
    let store_opt = cooldown_store().lock().ok().and_then(|g| g.clone());
    if let Some(store) = store_opt {
        let agent_key = format!("{FAILURE_COUNT_PREFIX}{agent_name}");
        let model_key = format!("{FAILURE_COUNT_PREFIX}{agent_name}:{model}");
        let _ = store.kv_set(&agent_key, "0").await;
        let _ = store.kv_set(&model_key, "0").await;
    }
}

/// Record that an agent has failed and should be temporarily avoided.
///
/// Applies exponential backoff based on the agent's failure count in KV.
/// If `error_message` contains "try again at {date}", that vendor date is used
/// instead of the computed backoff (vendor dates are always authoritative).
pub async fn record_agent_failure_with_message(agent_name: &str, error_message: &str) {
    // Vendor-specified retry date takes priority over backoff.
    if let Some(cooldown_until) = parse_retry_at(error_message) {
        set_cooldown(agent_name, cooldown_until, "agent_error");
        return;
    }

    let store_opt = cooldown_store().lock().ok().and_then(|g| g.clone());
    let count = read_and_increment_failure_count(&store_opt, agent_name).await;
    let cooldown_until = chrono::Utc::now().timestamp()
        + compute_backoff(count, BACKOFF_BASE_SECS, BACKOFF_MAX_SECS);
    set_cooldown(agent_name, cooldown_until, "agent_error");
}

/// Detect credit exhaustion reasons from error message.
///
/// Returns `Some(CreditExhaustionReason)` if the error indicates
/// out_of_credits or org_level_disabled, which require longer agent-wide cooldowns.
pub fn detect_credit_exhaustion(error_message: &str) -> Option<CreditExhaustionReason> {
    let lower = error_message.to_lowercase();

    let overage_patterns = [
        "overagedisabledreason",
        "overage_disabled_reason",
        "overage disabled reason",
    ];

    if overage_patterns.iter().any(|p| lower.contains(p)) {
        if lower.contains("out_of_credits") || lower.contains("outofcredits") {
            return Some(CreditExhaustionReason::OutOfCredits);
        }
        if lower.contains("org_level_disabled") || lower.contains("orgdisabled") {
            return Some(CreditExhaustionReason::OrgLevelDisabled);
        }
    }

    let billing_patterns = [
        "credit balance too low",
        "credit balance insufficient",
        "insufficient credit balance",
        "billing quota exceeded",
        "billing limit exceeded",
        "organization has been disabled",
        "org disabled",
    ];

    if billing_patterns.iter().any(|p| lower.contains(p)) {
        return Some(CreditExhaustionReason::OrgLevelDisabled);
    }

    // Billing cycle exhaustion: monthly quota reset — check before generic quota
    // patterns because "monthly quota exceeded" would otherwise match "quota exceeded".
    let billing_cycle_patterns = [
        "billing cycle",
        "quota refreshed next cycle",
        "monthly quota",
        "quota will be refreshed",
        "next billing cycle",
        "refreshed in the next cycle",
    ];

    if billing_cycle_patterns.iter().any(|p| lower.contains(p)) {
        return Some(CreditExhaustionReason::BillingCycleExhausted);
    }

    if lower.contains("out of credits")
        || lower.contains("outofcredits")
        || lower.contains("insufficient funds")
        || lower.contains("no credits remaining")
        || lower.contains("insufficient_quota")
        || lower.contains("quota exceeded")
    {
        return Some(CreditExhaustionReason::OutOfCredits);
    }

    None
}

/// Record an agent-level cooldown for credit exhaustion.
///
/// Applies exponential backoff based on the agent's failure count.
/// `BillingCycleExhausted` is a calendar event — flat 24h cooldown, no escalation.
/// For other reasons, the backoff starts at 1h (out_of_credits) or 2h (org_level_disabled)
/// and caps at 8h to prevent multi-day lockouts when credits can be refilled at any time.
pub async fn record_credit_exhaustion(agent_name: &str, reason: CreditExhaustionReason) {
    let reason_str = match reason {
        CreditExhaustionReason::OutOfCredits => "credit_exhaustion_out_of_credits",
        CreditExhaustionReason::OrgLevelDisabled => "credit_exhaustion_org_level_disabled",
        CreditExhaustionReason::BillingCycleExhausted => "billing_cycle_exhausted",
    };

    // Billing cycle exhaustion is a calendar event — flat 24h, backoff is meaningless.
    if reason == CreditExhaustionReason::BillingCycleExhausted {
        let cooldown_until = chrono::Utc::now().timestamp() + BILLING_CYCLE_COOLDOWN_SECS;
        set_cooldown(agent_name, cooldown_until, reason_str);
        tracing::warn!(
            agent = agent_name,
            reason = reason_str,
            cooldown_secs = BILLING_CYCLE_COOLDOWN_SECS,
            "billing cycle exhausted: applying 24h flat cooldown"
        );
        return;
    }

    let store_opt = cooldown_store().lock().ok().and_then(|g| g.clone());
    let count = read_and_increment_failure_count(&store_opt, agent_name).await;

    let (base, max) = match reason {
        CreditExhaustionReason::OutOfCredits => (CREDIT_BACKOFF_BASE_SECS, CREDIT_BACKOFF_MAX_SECS),
        CreditExhaustionReason::OrgLevelDisabled => {
            (CREDIT_BACKOFF_BASE_SECS * 2, CREDIT_BACKOFF_MAX_SECS)
        }
        CreditExhaustionReason::BillingCycleExhausted => unreachable!(),
    };
    let cooldown_secs = compute_backoff(count, base, max);
    let cooldown_until = chrono::Utc::now().timestamp() + cooldown_secs;
    set_cooldown(agent_name, cooldown_until, reason_str);
    tracing::warn!(
        agent = agent_name,
        reason = reason_str,
        attempt = count,
        cooldown_secs,
        "credit exhaustion detected: applying exponential agent-wide cooldown"
    );
}

/// Record that a specific agent+model combo has failed.
///
/// Applies exponential backoff based on the model's failure count in KV.
pub async fn record_model_failure(agent_name: &str, model: &str) {
    let key = format!("{agent_name}:{model}");
    let store_opt = cooldown_store().lock().ok().and_then(|g| g.clone());
    let count = read_and_increment_failure_count(&store_opt, &key).await;
    let cooldown_until = chrono::Utc::now().timestamp()
        + compute_backoff(count, BACKOFF_BASE_SECS, BACKOFF_MAX_SECS);
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

/// Return the cooldown expiry timestamp for a key (agent or agent:model), if active.
pub fn cooldown_until(key: &str) -> Option<i64> {
    let map = cooldowns().lock().unwrap_or_else(|e| e.into_inner());
    map.get(key).and_then(|entry| {
        let now = chrono::Utc::now().timestamp();
        if now < entry.cooldown_until {
            Some(entry.cooldown_until)
        } else {
            None
        }
    })
}

/// Return the reason string recorded when a cooldown was set, if the cooldown is still active.
pub fn cooldown_reason(key: &str) -> Option<String> {
    let map = cooldowns().lock().unwrap_or_else(|e| e.into_inner());
    map.get(key).and_then(|entry| {
        let now = chrono::Utc::now().timestamp();
        if now < entry.cooldown_until {
            Some(entry.reason.clone())
        } else {
            None
        }
    })
}

/// Return all currently active cooldowns as `(key, cooldown_until_unix, reason)`.
///
/// Reads from the in-memory map only — always fast, no async needed.
/// Expired entries are filtered out. Used by `orch cooldown list`.
pub fn list_all_cooldowns() -> Vec<(String, i64, String)> {
    let now = chrono::Utc::now().timestamp();
    let map = cooldowns().lock().unwrap_or_else(|e| e.into_inner());
    let mut result: Vec<(String, i64, String)> = map
        .iter()
        .filter(|(_, entry)| now < entry.cooldown_until)
        .map(|(key, entry)| (key.clone(), entry.cooldown_until, entry.reason.clone()))
        .collect();
    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

/// Clear a specific cooldown by key, or all cooldowns when `key == "*"`.
///
/// Removes from the in-memory map and writes a past timestamp to KV so the
/// entry is not reloaded on the next service restart.
pub async fn clear_cooldown(key: &str, store: &Arc<crate::store::TaskStore>) {
    if key == "*" {
        let keys: Vec<String> = {
            let mut map = cooldowns().lock().unwrap_or_else(|e| e.into_inner());
            let keys: Vec<String> = map.keys().cloned().collect();
            map.clear();
            keys
        };
        for k in &keys {
            let kv_key = format!("{KV_PREFIX}{k}");
            let _ = store.kv_set(&kv_key, "0").await;
        }
        tracing::info!(count = keys.len(), "cleared all cooldowns");
    } else {
        {
            let mut map = cooldowns().lock().unwrap_or_else(|e| e.into_inner());
            map.remove(key);
        }
        let kv_key = format!("{KV_PREFIX}{key}");
        let _ = store.kv_set(&kv_key, "0").await;
        tracing::info!(key, "cleared cooldown");
    }
}

/// Set a cooldown with a specific expiry timestamp. Persists to KV.
fn set_cooldown(key: &str, cooldown_until: i64, reason: &str) {
    {
        let mut map = cooldowns().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = map.get(key) {
            // Never shorten an existing cooldown. This prevents a short
            // retry window (e.g., generic rate limit) from overriding a longer
            // billing-cycle cooldown.
            if existing.cooldown_until >= cooldown_until {
                return;
            }
        }
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

// ---------------------------------------------------------------------------
// Degraded-agent tracking (pre-emptive health check)
// ---------------------------------------------------------------------------

/// Default health check window for recent rate-limit events (hours).
pub const DEFAULT_HEALTH_CHECK_WINDOW_HOURS: u32 = 6;

/// Default threshold: minimum rate-limit events within the window to consider
/// an agent degraded.  A single transient 429 shouldn't mark an agent; we need
/// a pattern of repeated failures.
pub const DEFAULT_DEGRADED_RATE_LIMIT_THRESHOLD: i64 = 3;

/// In-memory set of agents currently flagged as degraded.
fn degraded_agents() -> &'static Mutex<std::collections::HashSet<String>> {
    static DEGRADED: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    DEGRADED.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// Check if an agent is currently flagged as degraded by the health check.
///
/// Degraded means: all configured models are cooled **or** the agent has
/// exceeded the rate-limit/out_of_credits threshold within the configured
/// lookback window.
pub fn is_agent_degraded(agent: &str) -> bool {
    let set = degraded_agents().lock().unwrap_or_else(|e| e.into_inner());
    set.contains(agent)
}

/// Mark an agent as degraded (called by [`refresh_degraded_agents`]).
pub fn mark_agent_degraded(agent: &str) {
    let mut set = degraded_agents().lock().unwrap_or_else(|e| e.into_inner());
    set.insert(agent.to_string());
}

/// Clear the degraded flag for an agent.
pub fn clear_agent_degraded(agent: &str) {
    let mut set = degraded_agents().lock().unwrap_or_else(|e| e.into_inner());
    set.remove(agent);
}

/// Refresh the degraded-agent set from the rate_limits table and cooldown state.
///
/// An agent is marked degraded when:
/// 1. It is in agent-level cooldown, **or**
/// 2. All its configured models are in cooldown, **or**
/// 3. It has >= `threshold` rate_limit/out_of_credits events within `window_hours`.
///
/// Agents that no longer meet these criteria are cleared.
pub async fn refresh_degraded_agents(
    store: &Arc<crate::store::TaskStore>,
    available_agents: &[String],
    model_checker: &dyn Fn(&str) -> bool,
    window_hours: u32,
    threshold: i64,
) {
    let counts = match store.recent_rate_limit_counts(window_hours).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(err = %e, "failed to query recent rate limit counts for health check");
            return;
        }
    };

    for agent in available_agents {
        let in_cooldown = is_agent_in_cooldown(agent);
        let no_models = !model_checker(agent);
        let rate_limit_count = counts.get(agent.as_str()).copied().unwrap_or(0);
        let over_threshold = rate_limit_count >= threshold;

        let degraded = in_cooldown || no_models || over_threshold;

        if degraded {
            if !is_agent_degraded(agent) {
                let reason = if in_cooldown {
                    "agent in cooldown"
                } else if no_models {
                    "all models cooled"
                } else {
                    "rate limit threshold exceeded"
                };
                tracing::warn!(
                    agent,
                    reason,
                    rate_limit_count,
                    window_hours,
                    threshold,
                    "pre-emptive health check: marking agent as degraded"
                );
            }
            mark_agent_degraded(agent);
        } else {
            if is_agent_degraded(agent) {
                tracing::info!(
                    agent,
                    rate_limit_count,
                    "pre-emptive health check: agent recovered, clearing degraded flag"
                );
            }
            clear_agent_degraded(agent);
        }
    }
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
        let date_str = &lower[idx + retry_marker.len()..];
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

    // Without a specific "try again at" date, return None and let the caller
    // use its default cooldown (30 min agent / 1 hour model).  Only codex
    // provides exact retry dates in its rate-limit messages; other agents
    // (claude, opencode, kimi, minimax) have temporary limits that clear
    // within minutes.  A blanket 24 h fallback here caused false billing-
    // cycle cooldowns on agents that don't have billing cycles (#1292).
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

        record_model_failure("testagent_persist", "testmodel_persist").await;

        // set_cooldown persists via tokio::spawn — yield to let the task complete.
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
    fn parse_retry_at_billing_cycle_without_date_returns_none() {
        // Without a specific "try again at" date, parse_retry_at should return
        // None so callers use their default cooldown (30m agent / 1h model).
        let msg = "You've reached your usage limit for this billing cycle. Your quota will be refreshed in the next cycle. Upgrade to get more.";
        assert!(
            parse_retry_at(msg).is_none(),
            "billing cycle without date should return None"
        );
    }

    #[tokio::test]
    async fn agent_cooldown_persists_to_kv() {
        // Agent-level cooldowns should also go through set_cooldown which persists
        let agent = "test_agent_persist_check";
        record_agent_failure_with_message(agent, "").await;
        assert!(is_agent_in_cooldown(agent));
    }

    #[tokio::test]
    async fn cooldown_never_shortens() {
        let agent = "test_agent_no_shorten";
        set_agent_cooldown(agent, 24 * 60 * 60);
        let initial = {
            let map = cooldowns().lock().unwrap();
            map.get(agent)
                .expect("cooldown entry should exist")
                .cooldown_until
        };

        // Shorter cooldown attempt should not override the existing one.
        record_agent_failure_with_message(agent, "rate limit").await;
        let after = {
            let map = cooldowns().lock().unwrap();
            map.get(agent)
                .expect("cooldown entry should exist")
                .cooldown_until
        };
        assert_eq!(
            initial, after,
            "cooldown should not be shortened by later failures"
        );
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

    #[test]
    fn detect_credit_exhaustion_out_of_credits() {
        let msg = "Error: API error - {\"error\":{\"message\":\"You exceeded your current quota\",\"type\":\"insufficient_quota\",\"param\":null,\"code\":\"insufficient_quota\"}}";
        assert_eq!(
            detect_credit_exhaustion(msg),
            Some(CreditExhaustionReason::OutOfCredits)
        );
    }

    #[test]
    fn detect_credit_exhaustion_out_of_credits_variations() {
        assert_eq!(
            detect_credit_exhaustion("out of credits"),
            Some(CreditExhaustionReason::OutOfCredits)
        );
        assert_eq!(
            detect_credit_exhaustion("outofcredits"),
            Some(CreditExhaustionReason::OutOfCredits)
        );
        assert_eq!(
            detect_credit_exhaustion("no credits remaining"),
            Some(CreditExhaustionReason::OutOfCredits)
        );
        assert_eq!(
            detect_credit_exhaustion("insufficient funds"),
            Some(CreditExhaustionReason::OutOfCredits)
        );
    }

    #[test]
    fn detect_credit_exhaustion_org_level_disabled() {
        let msg = "Error: API error - {\"error\":{\"message\":\"Organization has been disabled\",\"type\":\"org_disabled\",\"code\":\"overageDisabledReason:org_level_disabled\"}}";
        assert_eq!(
            detect_credit_exhaustion(msg),
            Some(CreditExhaustionReason::OrgLevelDisabled)
        );
    }

    #[test]
    fn detect_credit_exhaustion_org_level_disabled_variations() {
        assert_eq!(
            detect_credit_exhaustion("organization has been disabled"),
            Some(CreditExhaustionReason::OrgLevelDisabled)
        );
        assert_eq!(
            detect_credit_exhaustion("org disabled"),
            Some(CreditExhaustionReason::OrgLevelDisabled)
        );
        assert_eq!(
            detect_credit_exhaustion("billing quota exceeded"),
            Some(CreditExhaustionReason::OrgLevelDisabled)
        );
        assert_eq!(
            detect_credit_exhaustion("credit balance too low"),
            Some(CreditExhaustionReason::OrgLevelDisabled)
        );
    }

    #[test]
    fn detect_credit_exhaustion_overage_disabled_reason_parsing() {
        let msg = "overageDisabledReason: out_of_credits";
        assert_eq!(
            detect_credit_exhaustion(msg),
            Some(CreditExhaustionReason::OutOfCredits)
        );

        let msg = "overageDisabledReason: org_level_disabled";
        assert_eq!(
            detect_credit_exhaustion(msg),
            Some(CreditExhaustionReason::OrgLevelDisabled)
        );
    }

    #[test]
    fn detect_credit_exhaustion_none_for_regular_rate_limit() {
        assert!(detect_credit_exhaustion("rate limit exceeded").is_none());
        assert!(detect_credit_exhaustion("too many requests").is_none());
        assert!(detect_credit_exhaustion("429 Too Many Requests").is_none());
        assert!(detect_credit_exhaustion("you've hit your usage limit").is_none());
    }

    #[tokio::test]
    async fn record_credit_exhaustion_applies_agent_cooldown() {
        let agent = "test_credit_exhaust_agent";
        assert!(!is_agent_in_cooldown(agent));

        record_credit_exhaustion(agent, CreditExhaustionReason::OutOfCredits).await;
        assert!(is_agent_in_cooldown(agent));

        let remaining = {
            let map = cooldowns().lock().unwrap();
            let entry = map.get(agent).expect("cooldown entry should exist");
            entry.cooldown_until - chrono::Utc::now().timestamp()
        };
        // First failure: base = 1h
        assert!(
            remaining >= CREDIT_BACKOFF_BASE_SECS - 5,
            "first out_of_credits cooldown should be ~1 hour, got {remaining}s"
        );
        assert!(
            remaining <= CREDIT_BACKOFF_MAX_SECS,
            "cooldown should not exceed cap of 8h, got {remaining}s"
        );
    }

    #[tokio::test]
    async fn record_credit_exhaustion_org_level_applies_longer_cooldown() {
        let agent = "test_org_disabled_agent";
        assert!(!is_agent_in_cooldown(agent));

        record_credit_exhaustion(agent, CreditExhaustionReason::OrgLevelDisabled).await;
        assert!(is_agent_in_cooldown(agent));

        let remaining = {
            let map = cooldowns().lock().unwrap();
            let entry = map.get(agent).expect("cooldown entry should exist");
            entry.cooldown_until - chrono::Utc::now().timestamp()
        };
        // First failure: base = 2h (CREDIT_BACKOFF_BASE_SECS * 2)
        assert!(
            remaining >= CREDIT_BACKOFF_BASE_SECS * 2 - 5,
            "first org-level disabled cooldown should be ~2 hours, got {remaining}s"
        );
        assert!(
            remaining <= CREDIT_BACKOFF_MAX_SECS,
            "cooldown should not exceed cap of 8h, got {remaining}s"
        );
    }

    #[tokio::test]
    async fn record_credit_exhaustion_billing_cycle_applies_24h() {
        let agent = "test_billing_cycle_agent";
        assert!(!is_agent_in_cooldown(agent));

        record_credit_exhaustion(agent, CreditExhaustionReason::BillingCycleExhausted).await;
        assert!(is_agent_in_cooldown(agent));

        let remaining = {
            let map = cooldowns().lock().unwrap();
            let entry = map.get(agent).expect("cooldown entry should exist");
            entry.cooldown_until - chrono::Utc::now().timestamp()
        };
        assert!(
            remaining >= BILLING_CYCLE_COOLDOWN_SECS - 5,
            "billing cycle cooldown should be ~24 hours, got {remaining}s"
        );
    }

    #[test]
    fn credit_exhaustion_backoff_constants() {
        // OutOfCredits first failure: 1h base
        assert_eq!(
            compute_backoff(1, CREDIT_BACKOFF_BASE_SECS, CREDIT_BACKOFF_MAX_SECS),
            CREDIT_BACKOFF_BASE_SECS
        );
        // OrgLevelDisabled first failure: 2h base
        assert_eq!(
            compute_backoff(1, CREDIT_BACKOFF_BASE_SECS * 2, CREDIT_BACKOFF_MAX_SECS),
            CREDIT_BACKOFF_BASE_SECS * 2
        );
        // BillingCycleExhausted: flat 24h
        assert_eq!(BILLING_CYCLE_COOLDOWN_SECS, 24 * 60 * 60);
    }

    // ---- Degraded-agent tracking tests ----

    #[test]
    fn mark_and_check_agent_degraded() {
        let agent = "test_degraded_mark";
        assert!(!is_agent_degraded(agent));
        mark_agent_degraded(agent);
        assert!(is_agent_degraded(agent));
        clear_agent_degraded(agent);
        assert!(!is_agent_degraded(agent));
    }

    #[tokio::test]
    async fn refresh_degraded_agents_marks_cooled_agent() {
        let store = test_store().await;
        let agent = "test_refresh_cooled";

        // Put agent in cooldown
        record_agent_failure_with_message(agent, "").await;
        assert!(is_agent_in_cooldown(agent));

        // model_checker always returns true (agent has models)
        let agents = vec![agent.to_string()];
        refresh_degraded_agents(&store, &agents, &|_| true, 6, 3).await;

        assert!(
            is_agent_degraded(agent),
            "agent in cooldown should be marked degraded"
        );

        // Cleanup
        clear_agent_degraded(agent);
    }

    #[tokio::test]
    async fn refresh_degraded_agents_marks_no_models_agent() {
        let store = test_store().await;
        let agent = "test_refresh_no_models";

        // model_checker returns false (no available models)
        let agents = vec![agent.to_string()];
        refresh_degraded_agents(&store, &agents, &|_| false, 6, 3).await;

        assert!(
            is_agent_degraded(agent),
            "agent with no available models should be marked degraded"
        );

        // Cleanup
        clear_agent_degraded(agent);
    }

    #[tokio::test]
    async fn refresh_degraded_agents_marks_rate_limited_agent() {
        let store = test_store().await;
        let agent = "test_refresh_rate_limited";

        // Insert rate limit events exceeding threshold
        for _ in 0..4 {
            store
                .record_rate_limit(agent, "rate_limit", None)
                .await
                .unwrap();
        }

        let agents = vec![agent.to_string()];
        // threshold=3, window=6h — 4 events should trigger degraded
        refresh_degraded_agents(&store, &agents, &|_| true, 6, 3).await;

        assert!(
            is_agent_degraded(agent),
            "agent with rate limit count >= threshold should be marked degraded"
        );

        // Cleanup
        clear_agent_degraded(agent);
    }

    #[tokio::test]
    async fn refresh_degraded_agents_clears_healthy_agent() {
        let store = test_store().await;
        let agent = "test_refresh_healthy";

        // Pre-mark as degraded
        mark_agent_degraded(agent);
        assert!(is_agent_degraded(agent));

        // No cooldown, has models, no rate limit events
        let agents = vec![agent.to_string()];
        refresh_degraded_agents(&store, &agents, &|_| true, 6, 3).await;

        assert!(
            !is_agent_degraded(agent),
            "healthy agent should have degraded flag cleared"
        );
    }

    #[test]
    fn compute_backoff_grows_exponentially() {
        // count=0 or 1: base
        assert_eq!(compute_backoff(0, 300, 14400), 300);
        assert_eq!(compute_backoff(1, 300, 14400), 300);
        // count=2: base * 3
        assert_eq!(compute_backoff(2, 300, 14400), 900);
        // count=3: base * 9
        assert_eq!(compute_backoff(3, 300, 14400), 2700);
        // count=4: base * 27 = 8100 < 14400
        assert_eq!(compute_backoff(4, 300, 14400), 8100);
        // count=5: base * 81 = 24300 → capped at 14400
        assert_eq!(compute_backoff(5, 300, 14400), 14400);
    }

    #[test]
    fn detect_credit_exhaustion_billing_cycle() {
        assert_eq!(
            detect_credit_exhaustion("billing cycle"),
            Some(CreditExhaustionReason::BillingCycleExhausted)
        );
        assert_eq!(
            detect_credit_exhaustion("quota refreshed next cycle"),
            Some(CreditExhaustionReason::BillingCycleExhausted)
        );
        assert_eq!(
            detect_credit_exhaustion(
                "You've reached your usage limit for this billing cycle. Your quota will be refreshed in the next cycle."
            ),
            Some(CreditExhaustionReason::BillingCycleExhausted)
        );
        assert_eq!(
            detect_credit_exhaustion("monthly quota exceeded"),
            Some(CreditExhaustionReason::BillingCycleExhausted)
        );
    }

    #[test]
    fn detect_credit_exhaustion_overage_takes_priority_over_billing_cycle() {
        // A message containing both overage and billing cycle language should resolve
        // to OrgLevelDisabled/OutOfCredits (detected first) not BillingCycleExhausted.
        let msg = "overageDisabledReason: org_level_disabled for billing cycle";
        assert_eq!(
            detect_credit_exhaustion(msg),
            Some(CreditExhaustionReason::OrgLevelDisabled)
        );
    }

    #[tokio::test]
    async fn refresh_degraded_agents_below_threshold_not_degraded() {
        let store = test_store().await;
        let agent = "test_refresh_below_threshold";

        // Insert rate limit events below threshold
        for _ in 0..2 {
            store
                .record_rate_limit(agent, "rate_limit", None)
                .await
                .unwrap();
        }

        let agents = vec![agent.to_string()];
        // threshold=3, only 2 events — should NOT be degraded
        refresh_degraded_agents(&store, &agents, &|_| true, 6, 3).await;

        assert!(
            !is_agent_degraded(agent),
            "agent with rate limit count < threshold should not be degraded"
        );
    }
}
