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

/// Base backoff for generic agent/model failures: 5 minutes.
pub const BACKOFF_BASE_SECS: i64 = 5 * 60;

/// Maximum backoff for generic agent/model failures: 4 hours.
pub const BACKOFF_MAX_SECS: i64 = 4 * 60 * 60;

/// Base backoff for credit exhaustion (out_of_credits): 1 hour.
pub const CREDIT_BACKOFF_BASE_SECS: i64 = 60 * 60;

/// Base backoff for org-level disabling: 2 hours.
pub const ORG_BACKOFF_BASE_SECS: i64 = 2 * 60 * 60;

/// Maximum backoff for credit exhaustion and org-level disabling: 8 hours.
pub const CREDIT_BACKOFF_MAX_SECS: i64 = 8 * 60 * 60;

/// Base cooldown for billing cycle exhaustion: 24 hours.
///
/// Unlike per-request rate limits, billing cycle exhaustion is a calendar event
/// that won't self-resolve for days or weeks. The first failure starts at 24h;
/// subsequent failures escalate via `compute_backoff()` up to
/// `BILLING_CYCLE_MAX_SECS` (7 days). This prevents daily retry-and-fail cycles
/// when a monthly billing quota is exhausted.
pub const BILLING_CYCLE_COOLDOWN_SECS: i64 = 24 * 60 * 60;

/// Maximum cooldown for billing cycle exhaustion: 7 days.
///
/// Monthly billing cycles typically reset on a fixed date. 7 days is a
/// reasonable cap — the cooldown will expire before the next monthly reset
/// in most cases, allowing a single probe to re-check availability.
pub const BILLING_CYCLE_MAX_SECS: i64 = 7 * 24 * 60 * 60;

/// Credit exhaustion reason detected from error message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreditExhaustionReason {
    /// Per-model credit exhaustion (out_of_credits).
    OutOfCredits,
    /// Organization-level disabling (org_level_disabled).
    OrgLevelDisabled,
    /// Monthly billing cycle exhaustion — escalating cooldown (24h base, 7d cap).
    BillingCycleExhausted,
}

/// Short agent cooldown applied on silence detection (120 seconds).
///
/// Forces the router to pick a different agent immediately on re-route,
/// without long-term blocking. The model cooldown (exponential, starting at
/// 5 min) keeps the dead model out while this short cooldown just breaks
/// the same-agent loop.
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

/// KV key prefix for credit-exhaustion failure counts (separate from generic failures to prevent
/// cross-contamination of backoff escalation — credit exhaustion uses different base/max durations).
const CREDIT_FAILURE_COUNT_PREFIX: &str = "credit_failure_count:";

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
///
/// Uses `std::sync::Mutex` because this map is also read from sync helper
/// functions (`is_in_cooldown`, `is_model_in_cooldown`, etc.).  All lock
/// holders perform only in-memory HashMap operations — **no `.await` is ever
/// called while a guard is held**.  This invariant must be preserved: if a
/// future caller needs to await while holding this lock, switch to
/// `tokio::sync::Mutex` and make the helpers async.
fn cooldowns() -> &'static Mutex<HashMap<String, CooldownEntry>> {
    static COOLDOWNS: OnceLock<Mutex<HashMap<String, CooldownEntry>>> = OnceLock::new();
    COOLDOWNS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Global store reference used for background KV writes.
///
/// Uses `tokio::sync::Mutex` because this value is only ever accessed from
/// async functions. A `tokio::sync::Mutex` yields the Tokio task instead of
/// blocking the worker thread if the lock is contended.
fn cooldown_store() -> &'static tokio::sync::Mutex<Option<Arc<crate::store::TaskStore>>> {
    static STORE: OnceLock<tokio::sync::Mutex<Option<Arc<crate::store::TaskStore>>>> =
        OnceLock::new();
    STORE.get_or_init(|| tokio::sync::Mutex::new(None))
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

    // Load GitHub 5xx circuit breaker state if it was persisted before restart.
    if let Ok(Some(raw)) = store.kv_get("cooldown:github:5xx").await {
        if let Ok(cooldown_until) = raw.parse::<i64>() {
            let now = chrono::Utc::now().timestamp();
            if now < cooldown_until {
                if let Ok(mut open) = github_5xx_circuit_open().lock() {
                    *open = Some(cooldown_until);
                    let remaining = cooldown_until - now;
                    tracing::warn!(
                        remaining_secs = remaining,
                        "GitHub 5xx circuit breaker restored from KV (still open)"
                    );
                }
            }
        }
    }

    *cooldown_store().lock().await = Some(store);
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
pub fn compute_backoff(count: u64, base: i64, max: i64) -> i64 {
    if count <= 1 {
        return base;
    }
    let exp = count.saturating_sub(1).min(u32::MAX as u64) as u32;
    let factor = 3_i64.saturating_pow(exp);
    base.saturating_mul(factor).min(max)
}

/// Read the failure count for a key from KV, increment it, write it back, and return the new count.
///
/// Always increments — the backoff formula's `max` parameter prevents the
/// cooldown duration from growing beyond the cap, so unbounded counts are safe.
///
/// Returns 1 when no store is configured (unit-test contexts without a store).
/// Returns `u64::MAX` on store errors (e.g. lock contention) so backoff applies the cap duration
/// instead of resetting to the base, preventing rapid re-dispatch during store outages.
async fn read_and_increment_failure_count(
    store_opt: &Option<Arc<crate::store::TaskStore>>,
    key: &str,
) -> u64 {
    read_and_increment_failure_count_with_prefix(store_opt, FAILURE_COUNT_PREFIX, key).await
}

/// Like [`read_and_increment_failure_count`] but with an explicit KV prefix.
///
/// Credit exhaustion uses [`CREDIT_FAILURE_COUNT_PREFIX`] so its backoff escalation
/// is independent of generic agent failures (which use [`FAILURE_COUNT_PREFIX`]).
async fn read_and_increment_failure_count_with_prefix(
    store_opt: &Option<Arc<crate::store::TaskStore>>,
    prefix: &str,
    key: &str,
) -> u64 {
    let kv_key = format!("{prefix}{key}");
    if let Some(store) = store_opt {
        match store.kv_increment(&kv_key).await {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(key = %kv_key, err = %e, "failed to increment failure count — treating as high count for safe backoff");
                u64::MAX
            }
        }
    } else {
        1
    }
}

/// Reset failure counts for an agent and agent:model combo after a successful run.
///
/// Call this from the runner's success path so that the next failure starts
/// backoff from the base duration again, not from wherever it left off.
/// Resets both generic and credit-exhaustion failure counters.
pub async fn record_agent_success(agent_name: &str, model: &str) {
    let store_opt = cooldown_store().lock().await.clone();
    if let Some(store) = store_opt {
        let agent_key = format!("{FAILURE_COUNT_PREFIX}{agent_name}");
        let model_key = format!("{FAILURE_COUNT_PREFIX}{agent_name}:{model}");
        let credit_key = format!("{CREDIT_FAILURE_COUNT_PREFIX}{agent_name}");
        if let Err(e) = store.kv_set(&agent_key, "0").await {
            tracing::warn!(key = agent_key, err = %e, "failed to reset failure count");
        }
        if let Err(e) = store.kv_set(&model_key, "0").await {
            tracing::warn!(key = model_key, err = %e, "failed to reset failure count");
        }
        if let Err(e) = store.kv_set(&credit_key, "0").await {
            tracing::warn!(key = credit_key, err = %e, "failed to reset failure count");
        }
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
        set_cooldown_async(agent_name, cooldown_until, "agent_error").await;
        return;
    }

    let store_opt = cooldown_store().lock().await.clone();
    let count = read_and_increment_failure_count(&store_opt, agent_name).await;
    let base = crate::engine::router::config::get_agent_backoff_base(agent_name);
    let cooldown_until =
        chrono::Utc::now().timestamp() + compute_backoff(count, base, BACKOFF_MAX_SECS);
    set_cooldown_async(agent_name, cooldown_until, "agent_error").await;
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
/// All variants use escalating backoff:
/// - `OutOfCredits`: starts at 1h, caps at 8h (credits can be refilled any time)
/// - `OrgLevelDisabled`: starts at 2h, caps at 8h
/// - `BillingCycleExhausted`: starts at 24h, caps at 7 days (monthly event)
///
/// Billing cycle exhaustion was previously a flat 24h, but this caused daily
/// retry-and-fail cycles when a monthly quota was exhausted (the 24h cooldown
/// would expire, kimi would be retried, fail immediately, and get another 24h).
/// Now each recurrence escalates: 24h → 72h → 7d (capped).
pub async fn record_credit_exhaustion(agent_name: &str, reason: CreditExhaustionReason) {
    let reason_str = match reason {
        CreditExhaustionReason::OutOfCredits => "credit_exhaustion_out_of_credits",
        CreditExhaustionReason::OrgLevelDisabled => "credit_exhaustion_org_level_disabled",
        CreditExhaustionReason::BillingCycleExhausted => "billing_cycle_exhausted",
    };

    let store_opt = cooldown_store().lock().await.clone();
    // Use a distinct failure counter (CREDIT_FAILURE_COUNT_PREFIX) so credit-
    // exhaustion backoff escalation is independent of generic agent failures.
    // Previously both paths shared FAILURE_COUNT_PREFIX, causing generic failures
    // to inflate credit-exhaustion backoff (and vice versa).
    let count = read_and_increment_failure_count_with_prefix(
        &store_opt,
        CREDIT_FAILURE_COUNT_PREFIX,
        agent_name,
    )
    .await;

    let (base, max) = match reason {
        CreditExhaustionReason::OutOfCredits => (CREDIT_BACKOFF_BASE_SECS, CREDIT_BACKOFF_MAX_SECS),
        CreditExhaustionReason::OrgLevelDisabled => {
            (ORG_BACKOFF_BASE_SECS, CREDIT_BACKOFF_MAX_SECS)
        }
        CreditExhaustionReason::BillingCycleExhausted => {
            (BILLING_CYCLE_COOLDOWN_SECS, BILLING_CYCLE_MAX_SECS)
        }
    };
    let cooldown_secs = compute_backoff(count, base, max);
    let cooldown_until = chrono::Utc::now().timestamp() + cooldown_secs;
    set_cooldown_async(agent_name, cooldown_until, reason_str).await;
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
    let store_opt = cooldown_store().lock().await.clone();
    let count = read_and_increment_failure_count(&store_opt, &key).await;
    let base = crate::engine::router::config::get_agent_backoff_base(agent_name);
    let cooldown_until =
        chrono::Utc::now().timestamp() + compute_backoff(count, base, BACKOFF_MAX_SECS);
    set_cooldown_async(&key, cooldown_until, "model_error").await;
}

/// Set a model cooldown with a custom duration (in seconds).
///
/// Used by silence detection to cooldown the specific model that failed to
/// produce any output, with a configurable duration.
pub async fn set_model_cooldown(agent_name: &str, model: &str, duration_secs: u64) {
    let key = format!("{agent_name}:{model}");
    let cooldown_until = chrono::Utc::now().timestamp() + duration_secs as i64;
    set_cooldown_async(&key, cooldown_until, "silence_detected").await;
}

/// Set a short agent-level cooldown (in seconds).
///
/// Used by silence detection to temporarily block the whole agent so the
/// router picks a different one on re-route. Unlike `record_agent_failure`
/// (exponential backoff), this uses a short duration (typically 120s) —
/// just enough to force one re-route cycle to a different agent.
pub async fn set_agent_cooldown(agent_name: &str, duration_secs: u64) {
    let cooldown_until = chrono::Utc::now().timestamp() + duration_secs as i64;
    set_cooldown_async(agent_name, cooldown_until, "silence_agent_cooldown").await;
}

/// Record a silence detection for an agent+model and apply extended cooldowns
/// when repeated silences exceed the threshold within the rolling window.
pub async fn record_silence_detection(agent_name: &str, model: &str) -> Option<SilenceCountResult> {
    let store_opt = cooldown_store().lock().await.clone();
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
        Ok(Some(raw)) => match serde_json::from_str::<Vec<i64>>(&raw) {
            Ok(ts) => ts,
            Err(e) => {
                tracing::warn!(
                    kv_key = %key,
                    err = %e,
                    "failed to parse silence timestamps from KV — resetting window"
                );
                Vec::new()
            }
        },
        Ok(None) => Vec::new(),
        Err(err) => {
            tracing::warn!(
                kv_key = %key,
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
        set_model_cooldown(agent_name, model, SILENCE_EXTENDED_COOLDOWN_SECS).await;
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
/// Removes from the in-memory map, writes a past timestamp to KV, and resets
/// the corresponding `failure_count:` entries so the next failure starts
/// backoff from the base duration.
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
            if let Err(e) = store.kv_set(&kv_key, "0").await {
                tracing::warn!(key = kv_key, err = %e, "failed to clear cooldown");
            }
            // Reset failure count so backoff restarts from base.
            let fc_key = format!("{FAILURE_COUNT_PREFIX}{k}");
            if let Err(e) = store.kv_set(&fc_key, "0").await {
                tracing::warn!(key = fc_key, err = %e, "failed to reset failure count");
            }
            // Also reset credit-specific failure count.
            let credit_fc_key = format!("{CREDIT_FAILURE_COUNT_PREFIX}{k}");
            if let Err(e) = store.kv_set(&credit_fc_key, "0").await {
                tracing::warn!(key = credit_fc_key, err = %e, "failed to reset credit failure count");
            }
        }
        // Also reset any persisted failure_count keys that are not in the
        // in-memory map (e.g. survived a restart or were set without a
        // corresponding in-memory cooldown entry).
        if let Ok(fc_entries) = store.kv_list_prefix(FAILURE_COUNT_PREFIX).await {
            for (fc_key, _) in fc_entries {
                if let Err(e) = store.kv_set(&fc_key, "0").await {
                    tracing::warn!(key = fc_key, err = %e, "failed to reset failure count");
                }
            }
        }
        if let Ok(cfc_entries) = store.kv_list_prefix(CREDIT_FAILURE_COUNT_PREFIX).await {
            for (cfc_key, _) in cfc_entries {
                if let Err(e) = store.kv_set(&cfc_key, "0").await {
                    tracing::warn!(key = cfc_key, err = %e, "failed to reset credit failure count");
                }
            }
        }
        tracing::info!(
            count = keys.len(),
            "cleared all cooldowns and failure counts"
        );
    } else {
        {
            let mut map = cooldowns().lock().unwrap_or_else(|e| e.into_inner());
            map.remove(key);
        }
        let kv_key = format!("{KV_PREFIX}{key}");
        if let Err(e) = store.kv_set(&kv_key, "0").await {
            tracing::warn!(key = kv_key, err = %e, "failed to clear cooldown");
        }
        // Reset failure count so backoff restarts from base.
        let fc_key = format!("{FAILURE_COUNT_PREFIX}{key}");
        if let Err(e) = store.kv_set(&fc_key, "0").await {
            tracing::warn!(key = fc_key, err = %e, "failed to reset failure count");
        }
        // Also reset credit-specific failure count.
        let credit_fc_key = format!("{CREDIT_FAILURE_COUNT_PREFIX}{key}");
        if let Err(e) = store.kv_set(&credit_fc_key, "0").await {
            tracing::warn!(key = credit_fc_key, err = %e, "failed to reset credit failure count");
        }
        tracing::info!(key, "cleared cooldown and failure count");
    }
}

/// Set a cooldown with a specific expiry timestamp. Updates in-memory state only.
///
/// Returns `true` if the cooldown was applied, `false` if an existing longer
/// cooldown prevented the update (never-shorten rule).
///
/// **Callers are responsible for persisting to KV.** Use [`set_cooldown_async`]
/// for critical cooldowns that must survive a crash, or this function combined
/// with a fire-and-forget `tokio::spawn` for non-critical short cooldowns.
fn set_cooldown_in_memory(key: &str, cooldown_until: i64, reason: &str) -> bool {
    let mut map = cooldowns().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = map.get(key) {
        // Never shorten an existing cooldown. This prevents a short
        // retry window (e.g., generic rate limit) from overriding a longer
        // billing-cycle cooldown.
        if existing.cooldown_until >= cooldown_until {
            return false;
        }
    }
    map.insert(
        key.to_string(),
        CooldownEntry {
            cooldown_until,
            reason: reason.to_string(),
        },
    );
    true
}

/// Set a cooldown with a specific expiry timestamp and persist to KV inline.
///
/// This is the preferred function for critical cooldowns (credit exhaustion,
/// billing cycle, agent/model failures) that must survive a service crash.
/// The KV write is awaited before returning, so the caller can be confident
/// the cooldown is durable once this function returns `true`.
///
/// Returns `true` if the cooldown was applied, `false` if an existing longer
/// cooldown prevented the update (never-shorten rule).
async fn set_cooldown_async(key: &str, cooldown_until: i64, reason: &str) -> bool {
    if !set_cooldown_in_memory(key, cooldown_until, reason) {
        return false;
    }
    let store_opt = cooldown_store().lock().await.clone();
    if let Some(store) = store_opt {
        let kv_key = format!("{KV_PREFIX}{key}");
        let value = cooldown_until.to_string();
        if let Err(e) = store.kv_set(&kv_key, &value).await {
            tracing::warn!(
                kv_key,
                err = %e,
                "failed to persist cooldown to KV store — rolling back in-memory entry"
            );
            let mut map = cooldowns().lock().unwrap_or_else(|e| e.into_inner());
            // Only roll back if nothing has superseded our write.
            // A racing thread may have set a longer cooldown in the meantime;
            // removing unconditionally would erase that valid entry.
            if map.get(key).map(|e| e.cooldown_until) == Some(cooldown_until) {
                map.remove(key);
            }
            return false;
        }
    }
    true
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
/// 1. It is in agent-level cooldown (explicit cooldown KV key exists and hasn't expired), **or**
/// 2. All its configured models are in cooldown, **or**
/// 3. It has >= `threshold` rate_limit/out_of_credits events within `window_hours`.
///
/// Agents that no longer meet these criteria are cleared.
///
/// Note: This function calls `clear_expired_cooldowns()` first to ensure the
/// in-memory cooldown map reflects current state. An agent should NOT be marked
/// degraded simply because an expired cooldown entry hasn't been cleaned up yet.
pub async fn refresh_degraded_agents(
    store: &Arc<crate::store::TaskStore>,
    available_agents: &[String],
    model_checker: &(dyn Fn(&str) -> bool + Send + Sync),
    window_hours: u32,
    threshold: i64,
) {
    // Clear expired cooldowns from in-memory map before evaluating agent state.
    // This ensures we don't incorrectly mark agents as degraded due to stale
    // in-memory entries that should have expired but weren't cleaned up.
    clear_expired_cooldowns();

    // Time the DB query to surface slow health-checks that can dominate the
    // engine tick latency. If the query fails, log and return early.
    let start = chrono::Utc::now();
    let counts = match store.recent_rate_limit_counts(window_hours).await {
        Ok(c) => {
            let dur_ms = (chrono::Utc::now() - start).num_milliseconds();
            tracing::info!(
                duration_ms = dur_ms,
                window_hours,
                "recent_rate_limit_counts query completed"
            );
            c
        }
        Err(e) => {
            let dur_ms = (chrono::Utc::now() - start).num_milliseconds();
            tracing::warn!(
                err = %e,
                duration_ms = dur_ms,
                "failed to query recent rate limit counts for health check"
            );
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

// ---------------------------------------------------------------------------
// GitHub 5xx circuit breaker
// ---------------------------------------------------------------------------

/// Sliding window for counting GitHub 5xx errors (seconds).
const GITHUB_5XX_WINDOW_SECS: i64 = 120;

/// Number of 5xx errors within the window to trip the circuit breaker.
const GITHUB_5XX_THRESHOLD: usize = 5;

/// Duration the circuit breaker stays open after tripping (seconds).
const GITHUB_5XX_COOLDOWN_SECS: i64 = 180;

/// Global sliding window of 5xx error timestamps.
fn github_5xx_timestamps() -> &'static Mutex<Vec<i64>> {
    static TS: OnceLock<Mutex<Vec<i64>>> = OnceLock::new();
    TS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Whether the GitHub 5xx circuit breaker is currently open (tripped).
fn github_5xx_circuit_open() -> &'static Mutex<Option<i64>> {
    static OPEN: OnceLock<Mutex<Option<i64>>> = OnceLock::new();
    OPEN.get_or_init(|| Mutex::new(None))
}

/// Record a GitHub 5xx server error and trip the circuit breaker if the
/// sliding window threshold is exceeded.
///
/// Emits a single high-level log entry when the circuit transitions to open.
pub async fn record_github_5xx() {
    let now = chrono::Utc::now().timestamp();
    let window_start = now - GITHUB_5XX_WINDOW_SECS;

    let should_trip = {
        let mut ts = github_5xx_timestamps()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        ts.retain(|t| *t >= window_start);
        ts.push(now);
        ts.len() >= GITHUB_5XX_THRESHOLD
    };

    if should_trip {
        let cooldown_until = {
            let mut open = github_5xx_circuit_open()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if open.is_none() {
                let cd = now + GITHUB_5XX_COOLDOWN_SECS;
                *open = Some(cd);
                tracing::warn!(
                    errors_in_window = GITHUB_5XX_THRESHOLD,
                    window_secs = GITHUB_5XX_WINDOW_SECS,
                    cooldown_secs = GITHUB_5XX_COOLDOWN_SECS,
                    "GitHub 5xx circuit breaker OPEN — throttling non-critical work"
                );
                Some(cd)
            } else {
                None
            }
        };
        // Persist to KV (outside the mutex lock so the future is Send).
        if let Some(cd) = cooldown_until {
            let store_opt = cooldown_store().lock().await.clone();
            if let Some(store) = store_opt {
                let _ = store.kv_set("cooldown:github:5xx", &cd.to_string()).await;
            }
            // Also set the generic cooldown so is_agent_in_cooldown("github:5xx")
            // returns true immediately, avoiding a race window where concurrent
            // requests bypass the circuit breaker.
            set_agent_cooldown("github:5xx", GITHUB_5XX_COOLDOWN_SECS as u64).await;
        }
        // Clear the sliding window after tripping so we don't re-trip immediately
        let mut ts = github_5xx_timestamps()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        ts.clear();
    }
}

/// Check if the GitHub 5xx circuit breaker is currently open.
///
/// When open, non-critical background work (cleanup, polling, mention scans)
/// should be skipped. Critical operations (PR creation) should still attempt
/// with their existing exponential backoff.
///
/// Automatically recovers (logs a recovery message) when the cooldown expires.
pub fn is_github_circuit_open() -> bool {
    // Check the dedicated in-memory circuit flag first. If present, honour it
    // and perform auto-recovery when it expires (logging once on close).
    {
        let mut open = github_5xx_circuit_open()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(until) = *open {
            let now = chrono::Utc::now().timestamp();
            if now >= until {
                *open = None;
                tracing::info!("GitHub 5xx circuit breaker CLOSED — resuming normal operations");
                return false;
            }
            return true;
        }
    }

    // If the dedicated flag is not set, consult the generic cooldown map so
    // callers see a consistent view regardless of which code path set the
    // persisted cooldown (e.g. send_with_retries -> set_agent_cooldown).
    if let Some(until) = cooldown_until("github:5xx") {
        // Populate the dedicated in-memory flag so future checks and the
        // auto-recovery path go through the same code and logging.
        let mut open = github_5xx_circuit_open()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if open.is_none() {
            *open = Some(until);
            let now = chrono::Utc::now().timestamp();
            if now < until {
                let remaining = until - now;
                tracing::warn!(
                    remaining_secs = remaining,
                    "GitHub 5xx circuit breaker OPEN (discovered via generic cooldown) — throttling non-critical work"
                );
            }
        }
        return true;
    }

    false
}

/// Returns the remaining seconds of the GitHub 5xx circuit breaker cooldown,
/// or 0 if the circuit is closed.
pub fn github_circuit_remaining_secs() -> u64 {
    // Prefer the dedicated in-memory value when present.
    {
        let open = github_5xx_circuit_open()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(until) = *open {
            let now = chrono::Utc::now().timestamp();
            if now < until {
                return (until - now) as u64;
            }
            return 0;
        }
    }

    // Fall back to the generic cooldown map if the dedicated flag isn't set.
    if let Some(until) = cooldown_until("github:5xx") {
        let now = chrono::Utc::now().timestamp();
        if now < until {
            return (until - now) as u64;
        }
    }
    0
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
                // Vendor messages (e.g. Codex) emit times in the machine's local
                // timezone, not UTC.  Interpret the naive datetime as local time
                // and convert to UTC so the cooldown expires at the right moment.
                use chrono::TimeZone;
                let utc = match chrono::Local.from_local_datetime(&dt) {
                    chrono::LocalResult::Single(local_dt) => local_dt.with_timezone(&chrono::Utc),
                    chrono::LocalResult::Ambiguous(earliest, _) => {
                        // DST fold: pick the earlier (conservative) interpretation
                        earliest.with_timezone(&chrono::Utc)
                    }
                    chrono::LocalResult::None => {
                        // DST gap: fall back to treating as UTC to avoid panic
                        dt.and_utc()
                    }
                };
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
    // use its default cooldown (exponential backoff).  Only codex
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

        *cooldown_store().lock().await = Some(store.clone());

        record_model_failure("testagent_persist", "testmodel_persist").await;

        // Give the async KV write a moment to settle before querying.
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

        // The parsed timestamp must equal "Mar 26 2026 05:55 local" in UTC.
        // We reconstruct what the function should produce and compare directly.
        use chrono::TimeZone;
        let naive = chrono::NaiveDateTime::new(
            chrono::NaiveDate::from_ymd_opt(2026, 3, 26).unwrap(),
            chrono::NaiveTime::from_hms_opt(5, 55, 0).unwrap(),
        );
        let expected_utc = match chrono::Local.from_local_datetime(&naive) {
            chrono::LocalResult::Single(local_dt) => local_dt.with_timezone(&chrono::Utc),
            chrono::LocalResult::Ambiguous(earliest, _) => earliest.with_timezone(&chrono::Utc),
            chrono::LocalResult::None => naive.and_utc(),
        };
        assert_eq!(
            ts.unwrap(),
            expected_utc.timestamp(),
            "timestamp should reflect local-timezone interpretation of the vendor date"
        );

        // Sanity: the UTC date must still be March 26 (true for all timezones within ±14h)
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
        // Agent-level cooldowns persist to KV via set_cooldown_async
        let agent = "test_agent_persist_check";
        record_agent_failure_with_message(agent, "").await;
        assert!(is_agent_in_cooldown(agent));
    }

    #[tokio::test]
    async fn cooldown_never_shortens() {
        let agent = "test_agent_no_shorten";
        set_agent_cooldown(agent, 24 * 60 * 60).await;
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

    #[tokio::test]
    async fn silence_agent_cooldown_is_short_lived() {
        let agent = "test_silence_agent_cd";
        assert!(!is_agent_in_cooldown(agent));

        set_agent_cooldown(agent, SILENCE_AGENT_COOLDOWN_SECS).await;
        assert!(is_agent_in_cooldown(agent));

        // Verify it's a short cooldown (120s), not the exponential backoff one
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
        *cooldown_store().lock().await = Some(store.clone());

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
        // First failure: base = 2h (ORG_BACKOFF_BASE_SECS)
        assert!(
            remaining >= ORG_BACKOFF_BASE_SECS - 5,
            "first org-level disabled cooldown should be ~2 hours, got {remaining}s"
        );
        assert!(
            remaining <= CREDIT_BACKOFF_MAX_SECS,
            "cooldown should not exceed cap of 8h, got {remaining}s"
        );
    }

    #[tokio::test]
    async fn record_credit_exhaustion_billing_cycle_applies_24h() {
        let store = test_store().await;
        *cooldown_store().lock().await = Some(store.clone());

        let agent = "test_billing_cycle_agent";
        assert!(!is_agent_in_cooldown(agent));

        record_credit_exhaustion(agent, CreditExhaustionReason::BillingCycleExhausted).await;
        assert!(is_agent_in_cooldown(agent));

        let remaining = {
            let map = cooldowns().lock().unwrap();
            let entry = map.get(agent).expect("cooldown entry should exist");
            entry.cooldown_until - chrono::Utc::now().timestamp()
        };
        // First failure: base = 24h
        assert!(
            remaining >= BILLING_CYCLE_COOLDOWN_SECS - 5,
            "first billing cycle cooldown should be ~24 hours, got {remaining}s"
        );
        assert!(
            remaining <= BILLING_CYCLE_MAX_SECS,
            "cooldown should not exceed cap of 7 days, got {remaining}s"
        );
    }

    #[tokio::test]
    async fn record_credit_exhaustion_billing_cycle_escalates() {
        let store = test_store().await;
        *cooldown_store().lock().await = Some(store.clone());

        let agent = "test_billing_cycle_escalation";

        // First failure: 24h
        record_credit_exhaustion(agent, CreditExhaustionReason::BillingCycleExhausted).await;
        let remaining_1 = {
            let map = cooldowns().lock().unwrap();
            let entry = map.get(agent).expect("cooldown entry should exist");
            entry.cooldown_until - chrono::Utc::now().timestamp()
        };
        assert!(
            (BILLING_CYCLE_COOLDOWN_SECS - 5..=BILLING_CYCLE_COOLDOWN_SECS + 5)
                .contains(&remaining_1),
            "first billing cycle cooldown should be ~24h, got {remaining_1}s"
        );

        // Clear in-memory cooldown (but NOT failure count) to allow next set_cooldown_async
        {
            let mut map = cooldowns().lock().unwrap();
            map.remove(agent);
        }

        // Second failure: 24h * 3 = 72h (3 days)
        record_credit_exhaustion(agent, CreditExhaustionReason::BillingCycleExhausted).await;
        let remaining_2 = {
            let map = cooldowns().lock().unwrap();
            let entry = map.get(agent).expect("cooldown entry should exist");
            entry.cooldown_until - chrono::Utc::now().timestamp()
        };
        let expected_2 = BILLING_CYCLE_COOLDOWN_SECS * 3; // 72h
        assert!(
            remaining_2 >= expected_2 - 5,
            "second billing cycle cooldown should be ~72h, got {remaining_2}s"
        );

        // Clear in-memory cooldown again
        {
            let mut map = cooldowns().lock().unwrap();
            map.remove(agent);
        }

        // Third failure: 24h * 9 = 216h → capped at 7 days (168h)
        record_credit_exhaustion(agent, CreditExhaustionReason::BillingCycleExhausted).await;
        let remaining_3 = {
            let map = cooldowns().lock().unwrap();
            let entry = map.get(agent).expect("cooldown entry should exist");
            entry.cooldown_until - chrono::Utc::now().timestamp()
        };
        assert!(
            remaining_3 >= BILLING_CYCLE_MAX_SECS - 5,
            "third billing cycle cooldown should be capped at 7 days, got {remaining_3}s"
        );
        assert!(
            remaining_3 <= BILLING_CYCLE_MAX_SECS + 5,
            "third billing cycle cooldown should not exceed 7 days, got {remaining_3}s"
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
            compute_backoff(1, ORG_BACKOFF_BASE_SECS, CREDIT_BACKOFF_MAX_SECS),
            ORG_BACKOFF_BASE_SECS
        );
        // BillingCycleExhausted: 24h base, 7d cap
        assert_eq!(BILLING_CYCLE_COOLDOWN_SECS, 24 * 60 * 60);
        assert_eq!(BILLING_CYCLE_MAX_SECS, 7 * 24 * 60 * 60);
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

    // ---- Exponential backoff integration tests ----

    #[tokio::test]
    async fn repeated_agent_failures_escalate_backoff() {
        let store = test_store().await;
        *cooldown_store().lock().await = Some(store.clone());

        let agent = "test_escalation_agent";

        // First failure: should be ~5 min (BACKOFF_BASE_SECS)
        record_agent_failure_with_message(agent, "").await;
        let remaining_1 = {
            let map = cooldowns().lock().unwrap();
            let entry = map.get(agent).expect("should have cooldown");
            entry.cooldown_until - chrono::Utc::now().timestamp()
        };
        assert!(
            (BACKOFF_BASE_SECS - 5..=BACKOFF_BASE_SECS + 5).contains(&remaining_1),
            "first failure should be ~5 min, got {remaining_1}s"
        );

        // Clear the cooldown (but NOT failure count) to allow the next set_cooldown_async to apply
        {
            let mut map = cooldowns().lock().unwrap();
            map.remove(agent);
        }

        // Second failure: should be ~15 min (5 * 3)
        record_agent_failure_with_message(agent, "").await;
        let remaining_2 = {
            let map = cooldowns().lock().unwrap();
            let entry = map.get(agent).expect("should have cooldown");
            entry.cooldown_until - chrono::Utc::now().timestamp()
        };
        assert!(
            remaining_2 >= BACKOFF_BASE_SECS * 3 - 5,
            "second failure should be ~15 min, got {remaining_2}s"
        );
    }

    #[tokio::test]
    async fn record_agent_success_resets_backoff() {
        let store = test_store().await;
        *cooldown_store().lock().await = Some(store.clone());

        let agent = "test_success_reset_agent";
        let model = "test_model";

        // Accumulate 3 failures to escalate the backoff
        for _ in 0..3 {
            let _ = read_and_increment_failure_count(&Some(store.clone()), agent).await;
        }
        // Verify count is 3
        let kv_key = format!("{FAILURE_COUNT_PREFIX}{agent}");
        let count: u32 = store
            .kv_get(&kv_key)
            .await
            .unwrap()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(count, 3);

        // Success should reset to 0
        record_agent_success(agent, model).await;

        let count_after: u32 = store
            .kv_get(&kv_key)
            .await
            .unwrap()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(count_after, 0, "success should reset failure count to 0");

        // Model-specific count should also be reset
        let model_kv_key = format!("{FAILURE_COUNT_PREFIX}{agent}:{model}");
        let model_count: u32 = store
            .kv_get(&model_kv_key)
            .await
            .unwrap()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            model_count, 0,
            "success should reset model failure count to 0"
        );
    }

    #[tokio::test]
    async fn clear_cooldown_resets_failure_counts() {
        let store = test_store().await;
        *cooldown_store().lock().await = Some(store.clone());

        let agent = "test_clear_fc_agent";

        // Accumulate failures
        for _ in 0..5 {
            let _ = read_and_increment_failure_count(&Some(store.clone()), agent).await;
        }
        // Set a cooldown so clear_cooldown has something to clear
        set_agent_cooldown(agent, 3600).await;

        // Verify failure count is 5
        let kv_key = format!("{FAILURE_COUNT_PREFIX}{agent}");
        let count: u32 = store
            .kv_get(&kv_key)
            .await
            .unwrap()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(count, 5);

        // Clear the cooldown
        clear_cooldown(agent, &store).await;

        // Failure count should be reset to 0
        let count_after: u32 = store
            .kv_get(&kv_key)
            .await
            .unwrap()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            count_after, 0,
            "clear_cooldown should reset failure count to 0"
        );

        // Cooldown should be removed
        assert!(
            !is_agent_in_cooldown(agent),
            "agent should not be in cooldown after clear"
        );
    }

    #[tokio::test]
    async fn clear_all_cooldowns_resets_all_failure_counts() {
        let store = test_store().await;
        *cooldown_store().lock().await = Some(store.clone());

        let agents = ["test_clear_all_a", "test_clear_all_b"];
        for agent in &agents {
            let _ = read_and_increment_failure_count(&Some(store.clone()), agent).await;
            set_agent_cooldown(agent, 3600).await;
        }

        clear_cooldown("*", &store).await;

        for agent in &agents {
            assert!(!is_agent_in_cooldown(agent));
            let kv_key = format!("{FAILURE_COUNT_PREFIX}{agent}");
            let count: u32 = store
                .kv_get(&kv_key)
                .await
                .unwrap()
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(
                count, 0,
                "clear --all should reset failure count for {agent}"
            );
        }
    }

    #[test]
    fn compute_backoff_saturates_on_large_counts() {
        // Very large count should not overflow, just hit the cap
        assert_eq!(
            compute_backoff(100, BACKOFF_BASE_SECS, BACKOFF_MAX_SECS),
            BACKOFF_MAX_SECS
        );
        assert_eq!(compute_backoff(u32::MAX as u64, 300, 14400), 14400);
        assert_eq!(compute_backoff(u64::MAX, 300, 14400), 14400);
    }

    #[test]
    fn compute_backoff_zero_base_returns_zero() {
        assert_eq!(compute_backoff(1, 0, 14400), 0);
        assert_eq!(compute_backoff(5, 0, 14400), 0);
    }

    // ---- GitHub 5xx circuit breaker tests ----

    #[tokio::test]
    async fn github_circuit_stays_closed_below_threshold() {
        // Reset circuit breaker state
        {
            let mut open = github_5xx_circuit_open().lock().unwrap();
            *open = None;
            let mut ts = github_5xx_timestamps().lock().unwrap();
            ts.clear();
            // Also clear any generic cooldown map entry for the github 5xx
            // circuit so tests are isolated from other tests that may have
            // set the generic cooldown via set_agent_cooldown("github:5xx", ...).
            let mut map = cooldowns().lock().unwrap_or_else(|e| e.into_inner());
            map.remove("github:5xx");
        }

        // Record fewer errors than threshold
        for _ in 0..GITHUB_5XX_THRESHOLD - 1 {
            record_github_5xx().await;
        }

        assert!(
            !is_github_circuit_open(),
            "circuit should stay closed below threshold"
        );
        assert_eq!(github_circuit_remaining_secs(), 0);
    }

    #[tokio::test]
    async fn github_circuit_trips_at_threshold() {
        // Reset circuit breaker state
        {
            let mut open = github_5xx_circuit_open().lock().unwrap();
            *open = None;
            let mut ts = github_5xx_timestamps().lock().unwrap();
            ts.clear();
            let mut map = cooldowns().lock().unwrap_or_else(|e| e.into_inner());
            map.remove("github:5xx");
        }

        // Record exactly threshold errors
        for _ in 0..GITHUB_5XX_THRESHOLD {
            record_github_5xx().await;
        }

        assert!(is_github_circuit_open(), "circuit should trip at threshold");
        assert!(
            github_circuit_remaining_secs() > 0,
            "remaining should be > 0 when open"
        );
        assert!(
            github_circuit_remaining_secs() <= GITHUB_5XX_COOLDOWN_SECS as u64,
            "remaining should not exceed cooldown duration"
        );

        // Clean up
        {
            let mut open = github_5xx_circuit_open().lock().unwrap();
            *open = None;
        }
    }

    #[tokio::test]
    async fn github_circuit_cannot_double_trip() {
        // Reset circuit breaker state
        {
            let mut open = github_5xx_circuit_open().lock().unwrap();
            *open = None;
            let mut ts = github_5xx_timestamps().lock().unwrap();
            ts.clear();
            let mut map = cooldowns().lock().unwrap_or_else(|e| e.into_inner());
            map.remove("github:5xx");
        }

        // Trip the circuit
        for _ in 0..GITHUB_5XX_THRESHOLD {
            record_github_5xx().await;
        }
        assert!(is_github_circuit_open());

        let first_remaining = github_circuit_remaining_secs();

        // More 5xx errors should NOT reset the cooldown (never-shorten)
        for _ in 0..GITHUB_5XX_THRESHOLD {
            record_github_5xx().await;
        }

        let second_remaining = github_circuit_remaining_secs();
        assert!(
            second_remaining <= first_remaining,
            "additional errors should not extend cooldown"
        );

        // Clean up
        {
            let mut open = github_5xx_circuit_open().lock().unwrap();
            *open = None;
        }
    }

    #[tokio::test]
    async fn github_circuit_recovers_after_cooldown() {
        // Reset circuit breaker state
        {
            let mut open = github_5xx_circuit_open().lock().unwrap();
            *open = None;
            let mut ts = github_5xx_timestamps().lock().unwrap();
            ts.clear();
            let mut map = cooldowns().lock().unwrap_or_else(|e| e.into_inner());
            map.remove("github:5xx");
        }

        // Set the circuit to an expired cooldown
        {
            let mut open = github_5xx_circuit_open().lock().unwrap();
            *open = Some(chrono::Utc::now().timestamp() - 1);
        }

        // Should be closed now (auto-recovery)
        assert!(
            !is_github_circuit_open(),
            "circuit should auto-recover after cooldown"
        );
    }

    #[tokio::test]
    async fn github_circuit_sliding_window_ages_out() {
        // Reset circuit breaker state
        {
            let mut open = github_5xx_circuit_open().lock().unwrap();
            *open = None;
            let mut ts = github_5xx_timestamps().lock().unwrap();
            ts.clear();
            let mut map = cooldowns().lock().unwrap_or_else(|e| e.into_inner());
            map.remove("github:5xx");
        }

        // Record errors that are outside the window
        {
            let mut ts = github_5xx_timestamps().lock().unwrap();
            let old = chrono::Utc::now().timestamp() - GITHUB_5XX_WINDOW_SECS - 10;
            for _ in 0..GITHUB_5XX_THRESHOLD {
                ts.push(old);
            }
        }

        // A new error should clear old ones and not trip the circuit
        record_github_5xx().await;

        assert!(
            !is_github_circuit_open(),
            "old errors outside window should not trip circuit"
        );
    }
}
