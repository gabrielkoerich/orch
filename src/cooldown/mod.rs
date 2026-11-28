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
//! Relative usage windows (e.g. "5-hour usage limit") are also detected and
//! used to set a model-specific cooldown matching the advertised window.

use chrono::TimeZone;
use regex::Regex;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::LazyLock;
use std::sync::MutexGuard;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync;

use crate::config;
use crate::store::TaskStore;

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

/// Base backoff for persistent model failures (silent exit/provider errors): 4 hours.
pub const PERSISTENT_MODEL_BACKOFF_BASE_SECS: i64 = 4 * 60 * 60;

/// Maximum backoff for persistent model failures: 7 days.
pub const PERSISTENT_MODEL_BACKOFF_MAX_SECS: i64 = 7 * 24 * 60 * 60;

/// Base backoff for a one-off model failure that does not mean the model is gone: 5 minutes.
pub const TRANSIENT_MODEL_BACKOFF_BASE_SECS: i64 = 5 * 60;

/// Maximum backoff for a one-off model failure: 15 minutes.
pub const TRANSIENT_MODEL_BACKOFF_MAX_SECS: i64 = 15 * 60;

/// Base backoff for credit exhaustion (out_of_credits): 1 hour.
pub const CREDIT_BACKOFF_BASE_SECS: i64 = 60 * 60;

/// Base backoff for org-level disabling: 2 hours.
pub const ORG_BACKOFF_BASE_SECS: i64 = 2 * 60 * 60;

/// Standard cap for credit exhaustion and org-level disabling: 8 hours.
/// Repeated failures at the cap move to the extended tiers (48h, then 96h)
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

impl CreditExhaustionReason {
    /// Stable string for `task_runs.outcome`, distinct from the cooldown reason
    /// string used by `record_credit_exhaustion`. Lets callers persist the actual
    /// failure mode instead of collapsing it into a generic "rate_limit"/"failed".
    pub fn outcome_str(self) -> &'static str {
        match self {
            CreditExhaustionReason::OutOfCredits => "credit_exhausted",
            CreditExhaustionReason::OrgLevelDisabled => "org_level_disabled",
            CreditExhaustionReason::BillingCycleExhausted => "billing_cycle_exhausted",
        }
    }
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

/// KV key prefix for persistent model failure counts (separate from [`FAILURE_COUNT_PREFIX`] so
/// `record_agent_success()` resetting the generic counter on an intervening success doesn't erase
/// escalation progress for structurally unreliable models — see `record_persistent_model_failure`).
const PERSISTENT_FAILURE_COUNT_PREFIX: &str = "persistent_failure_count:";

/// KV key prefix for the unix timestamp after which `rate_limits` rows count toward degraded
const RATE_LIMITS_CLEARED_AT_PREFIX: &str = "rate_limits_cleared_at:";

/// Agent name from a cooldown key (`claude:sonnet` -> `claude`)
fn agent_from_key(key: &str) -> &str {
    key.split_once(':').map_or(key, |(agent, _)| agent)
}

struct CooldownEntry {
    /// Unix timestamp when the cooldown expires.
    cooldown_until: i64,
    reason: String,
}

#[derive(Debug)]
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
fn cooldown_store() -> &'static sync::Mutex<Option<Arc<TaskStore>>> {
    static STORE: OnceLock<sync::Mutex<Option<Arc<TaskStore>>>> = OnceLock::new();
    STORE.get_or_init(|| sync::Mutex::new(None))
}

/// Initialise persistent cooldowns.
///
/// Must be called once at engine startup, after the `TaskStore` is opened.
/// Loads unexpired cooldowns from KV and registers the store for future writes.
/* Parses the `{"until": N, "reason": "..."}` cooldown KV format, falling back
to the legacy bare-integer format (reason unknown) so existing rows keep
working across the upgrade without a migration. */
fn parse_cooldown_value(value: &str) -> Option<(i64, String)> {
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str(value) {
        let until = map.get("until")?.as_i64()?;
        let reason = map
            .get("reason")
            .and_then(|r| r.as_str())
            .unwrap_or("persisted")
            .to_string();
        return Some((until, reason));
    }
    value
        .parse::<i64>()
        .ok()
        .map(|until| (until, "persisted".to_string()))
}

pub async fn init_cooldown_store(store: Arc<TaskStore>) {
    match store.kv_list_prefix(KV_PREFIX).await {
        Ok(rows) => {
            let now = chrono::Utc::now().timestamp();
            let mut map = cooldown_lock();
            let mut loaded = 0;
            for (key, value) in rows {
                let cooldown_key = key.trim_start_matches(KV_PREFIX);
                if let Some((cooldown_until, reason)) = parse_cooldown_value(&value)
                    && now < cooldown_until
                {
                    map.insert(
                        cooldown_key.to_string(),
                        CooldownEntry {
                            cooldown_until,
                            reason,
                        },
                    );
                    loaded += 1;
                }
                // Expired entries are simply not loaded — they'll be overwritten
                // on the next failure or cleaned up by future writes.
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
    if let Ok(Some(raw)) = store.kv_get("cooldown:github:5xx").await
        && let Ok(cooldown_until) = raw.parse::<i64>()
    {
        let now = chrono::Utc::now().timestamp();
        if now < cooldown_until {
            let mut open = github_5xx_circuit_open_lock();
            *open = Some(cooldown_until);
            let remaining = cooldown_until - now;
            tracing::warn!(
                remaining_secs = remaining,
                "GitHub 5xx circuit breaker restored from KV (still open)"
            );
        }
    }

    *cooldown_store().lock().await = Some(store);
}

/// Reconcile the in-memory cooldown map with the persisted KV state.
///
/// KV is the source of truth. This is called once per tick by
/// `router.refresh_health` so the running service notices state mutated by a
/// separate process (e.g. `orch cooldown clear`). Without this sync, the CLI
/// clears KV but the engine's in-memory map keeps the stale entries until they
/// expire on their own, producing the "all agents cooled" + "No active
/// cooldowns" inconsistency.
///
/// Reconciliation rules (KV authoritative for every key it has):
/// - KV row missing for an in-memory key: leave memory alone — likely a
///   `set_cooldown_async` race where memory was written but the KV row isn't
///   yet visible. The next tick will catch up.
/// - KV row present with `cooldown_until <= now` (including the "0" tombstone
///   written by `clear_cooldown`): drop from memory.
/// - KV row present with `cooldown_until > now`: insert/update memory to match.
pub async fn sync_from_kv(store: &Arc<TaskStore>) {
    let rows = match store.kv_list_prefix(KV_PREFIX).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(err = %e, "cooldown sync_from_kv: KV list failed");
            return;
        }
    };
    let now = chrono::Utc::now().timestamp();

    // Some((ts, reason)) for active cooldowns, None for cleared/expired rows.
    // Missing key means no KV row at all, left alone below.
    let mut explicit_kv: HashMap<String, Option<(i64, String)>> =
        HashMap::with_capacity(rows.len());
    for (kv_key, value) in &rows {
        let cooldown_key = kv_key.trim_start_matches(KV_PREFIX);
        if let Some((ts, reason)) = parse_cooldown_value(value) {
            let normalized = if ts > now { Some((ts, reason)) } else { None };
            explicit_kv.insert(cooldown_key.to_string(), normalized);
        }
    }

    // The 5xx circuit breaker lives in its own statics, so mirror its KV row there
    match explicit_kv.get("github:5xx") {
        Some(None) => {
            let mut open = github_5xx_circuit_open_lock();
            if open.take().is_some() {
                tracing::info!("GitHub 5xx circuit breaker closed by KV clear");
                github_5xx_timestamps_lock().clear();
            }
        }
        Some(Some((ts, _))) => {
            *github_5xx_circuit_open_lock() = Some(*ts);
        }
        None => {}
    }

    let mut map = cooldown_lock();
    let mut dropped = 0usize;
    let mut updated = 0usize;

    // Drop in-memory entries that KV explicitly marks cleared/expired.
    map.retain(|key, _| {
        if matches!(explicit_kv.get(key), Some(None)) {
            dropped += 1;
            false
        } else {
            true
        }
    });

    // Adopt KV's authoritative timestamp for active cooldowns.
    for (key, normalized) in explicit_kv {
        if let Some((ts, reason)) = normalized {
            if let Some(entry) = map.get_mut(&key) {
                if entry.cooldown_until != ts {
                    entry.cooldown_until = ts;
                    updated += 1;
                }
            } else {
                map.insert(
                    key,
                    CooldownEntry {
                        cooldown_until: ts,
                        reason,
                    },
                );
                updated += 1;
            }
        }
    }

    if dropped > 0 || updated > 0 {
        tracing::info!(
            dropped,
            updated,
            "cooldown sync_from_kv: reconciled in-memory map with KV"
        );
    }
}

/// Failure count threshold (times at max backoff) for first extended tier (24h).
///
/// After a model has been at the max cap this many times, apply a 6x multiplier.
const EXTENDED_TIER_1_THRESHOLD: u64 = 2;

/// Multiplier for first extended tier (6x = 24h when max=4h).
const EXTENDED_TIER_1_MULTIPLIER: i64 = 6;

/// Failure count threshold (times at max backoff) for second extended tier (48h).
///
/// After a model has been at the max cap this many times, apply a 12x multiplier.
const EXTENDED_TIER_2_THRESHOLD: u64 = 5;

/// Multiplier for second extended tier (12x = 48h when max=4h).
const EXTENDED_TIER_2_MULTIPLIER: i64 = 12;

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
/// | 4     | 300       | 8100   |
/// | 5-6   | 14400     | 14400 (max, not yet extended) |
/// | 7-9   | 86400     | 86400 (24h extended tier) |
/// | 10+   | 172800    | 172800 (48h extended tier) |
///
/// The extended tier applies when the model has hit the max cap multiple times,
/// indicating a persistently broken model that won't self-resolve quickly.
pub fn compute_backoff(count: u64, base: i64, max: i64, allow_extended: bool) -> i64 {
    if count <= 1 {
        return base;
    }
    let exp = u32::try_from(count.saturating_sub(1)).unwrap_or(u32::MAX);
    let factor = 3_i64.saturating_pow(exp);
    let standard = base.saturating_mul(factor).min(max);

    // Extended tier: after N failures at max backoff, apply longer window
    // to reduce noise from persistently broken models.
    //
    // First find the count at which exponential growth first hits max:
    // solve: base * 3^(first_max-1) >= max
    // => 3^(first_max-1) >= max / base
    // => first_max-1 >= log_3(max / base)
    // => first_max >= 1 + log_3(max / base)
    let first_max = if base <= 0 {
        0
    } else {
        // Use integer approximation: count where base * 3^(count-1) would first exceed max
        let mut accumulated = base;
        let mut first_max_count = 1u64;
        while accumulated < max && first_max_count < count {
            accumulated = accumulated.saturating_mul(3);
            first_max_count += 1;
        }
        first_max_count
    };

    // Times we've been at the max cap (excluding the first time we hit it)
    let times_at_max = count.saturating_sub(first_max);

    if !allow_extended {
        return standard;
    }

    if standard == max && times_at_max >= EXTENDED_TIER_2_THRESHOLD {
        max.saturating_mul(EXTENDED_TIER_2_MULTIPLIER)
    } else if standard == max && times_at_max >= EXTENDED_TIER_1_THRESHOLD {
        max.saturating_mul(EXTENDED_TIER_1_MULTIPLIER)
    } else {
        standard
    }
}

/// Read the failure count for a key from KV, increment it, write it back, and return the new count.
///
/// Always increments — the backoff formula's `max` parameter prevents the
/// cooldown duration from growing beyond the cap, so unbounded counts are safe.
///
/// Returns 1 when no store is configured (unit-test contexts without a store).
/// Returns `u64::MAX` on store errors (e.g. lock contention) so backoff applies the cap duration
/// instead of resetting to the base, preventing rapid re-dispatch during store outages.
async fn read_and_increment_failure_count(store_opt: Option<&Arc<TaskStore>>, key: &str) -> u64 {
    read_and_increment_failure_count_with_prefix(store_opt, FAILURE_COUNT_PREFIX, key).await
}

/// Like [`read_and_increment_failure_count`] but with an explicit KV prefix.
///
/// Credit exhaustion uses [`CREDIT_FAILURE_COUNT_PREFIX`] so its backoff escalation
/// is independent of generic agent failures (which use [`FAILURE_COUNT_PREFIX`]).
async fn read_and_increment_failure_count_with_prefix(
    store_opt: Option<&Arc<TaskStore>>,
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

/// Backoff schedule for one kind of per-model failure.
struct ModelBackoff {
    prefix: &'static str,
    base: i64,
    max: i64,
    allow_extended: bool,
    reason: &'static str,
}

/// Bump the failure counter for `agent:model` and cool the model for the backoff it maps to.
async fn record_model_backoff(agent_name: &str, model: &str, schedule: &ModelBackoff) {
    let key = format!("{agent_name}:{model}");
    let store_opt = cooldown_store().lock().await.clone();
    let count =
        read_and_increment_failure_count_with_prefix(store_opt.as_ref(), schedule.prefix, &key)
            .await;
    let cooldown_secs =
        compute_backoff(count, schedule.base, schedule.max, schedule.allow_extended);
    let cooldown_until = chrono::Utc::now().timestamp() + cooldown_secs;
    set_cooldown_async(&key, cooldown_until, schedule.reason).await;
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
        let credit_key = format!("{CREDIT_FAILURE_COUNT_PREFIX}{agent_name}");
        if let Err(e) = store.kv_set(&agent_key, "0").await {
            tracing::warn!(key = agent_key, err = %e, "failed to reset failure count");
        }
        if !model.is_empty() {
            let model_key = format!("{FAILURE_COUNT_PREFIX}{agent_name}:{model}");
            if let Err(e) = store.kv_set(&model_key, "0").await {
                tracing::warn!(key = model_key, err = %e, "failed to reset failure count");
            }
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
    // Vendor-specified retry date takes priority over backoff — but only
    // if it is in the present or future. If the parsed timestamp is in the
    // past (stale vendor message), fall through to the exponential backoff
    // path instead of returning early and bypassing backoff entirely.
    if let Some(cooldown_until) = parse_retry_at(error_message) {
        let now = chrono::Utc::now().timestamp();
        if cooldown_until > now {
            set_cooldown_async(agent_name, cooldown_until, "agent_error").await;
            return;
        }
        tracing::debug!(
            agent = agent_name,
            cooldown_until,
            "vendor retry_at is in the past — using backoff instead"
        );
    }

    let store_opt = cooldown_store().lock().await.clone();
    let count = read_and_increment_failure_count(store_opt.as_ref(), agent_name).await;
    let base = get_agent_backoff_base(agent_name);
    let cooldown_until =
        chrono::Utc::now().timestamp() + compute_backoff(count, base, BACKOFF_MAX_SECS, true);
    set_cooldown_async(agent_name, cooldown_until, "agent_error").await;
}

/// Detect credit exhaustion from an error message and return the stable
/// `task_runs.outcome` string for it (see `CreditExhaustionReason::outcome_str`).
///
/// Callers that classify `AgentError::RateLimit`/`AgentError::Auth` into an
/// outcome string should consult this first — generic rate-limit patterns like
/// "usage limit" also match billing-cycle exhaustion messages (e.g. Kimi's
/// "You've reached your usage limit for this billing cycle"), which would
/// otherwise be stored as a plain "rate_limit" outcome.
pub fn detect_credit_exhaustion_outcome(error_message: &str) -> Option<&'static str> {
    detect_credit_exhaustion(error_message).map(CreditExhaustionReason::outcome_str)
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
        "weekly/monthly limit exhausted",
        "limit will reset at",
        "upgrade to pro", // codex/chatgpt plan usage cap: requires billing upgrade
        "upgrade your token plan", // minimax: Token Plan usage limit reached
        "token plan usage limit", // minimax: Token Plan usage limit reached
        "purchase credits for more", // minimax: ...purchase Credits for more usage
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
        || lower.contains("insufficient balance")
        || lower.contains("please recharge")
        || lower.contains("no resource package")
        || lower.contains("balance exhausted")
        || lower.contains("402 payment required")
    {
        return Some(CreditExhaustionReason::OutOfCredits);
    }

    None
}

/// Record an agent-level cooldown for credit exhaustion.
///
/// Applies exponential backoff based on the agent's failure count.
/// All variants use escalating backoff:
/// - `OutOfCredits`: 1h, 3h, then 8h (credits can be refilled any time), 48h from the 5th failure, 96h from the 8th
/// - `OrgLevelDisabled`: 2h, 6h, then 8h, 48h from the 5th failure, 96h from the 8th
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

    // A failure from a run that started before the cooldown is the same event
    if is_in_cooldown_with_reason(agent_name, reason_str) {
        tracing::debug!(
            agent = agent_name,
            reason = reason_str,
            "credit exhaustion already cooled, not escalating"
        );
        return;
    }

    let store_opt = cooldown_store().lock().await.clone();
    // Use a distinct failure counter (CREDIT_FAILURE_COUNT_PREFIX) so credit-
    // exhaustion backoff escalation is independent of generic agent failures.
    // Previously both paths shared FAILURE_COUNT_PREFIX, causing generic failures
    // to inflate credit-exhaustion backoff (and vice versa).
    let count = read_and_increment_failure_count_with_prefix(
        store_opt.as_ref(),
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
    let cooldown_secs = compute_backoff(
        count,
        base,
        max,
        !matches!(reason, CreditExhaustionReason::BillingCycleExhausted),
    );
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

// Returns false when the message is no credit exhaustion. Model-scoped billing cycle cools the model only
pub async fn record_credit_exhaustion_from_message(
    agent_name: &str,
    model: Option<&str>,
    message: &str,
) -> bool {
    let Some(reason) = detect_credit_exhaustion(message) else {
        return false;
    };
    match (reason, model) {
        (CreditExhaustionReason::BillingCycleExhausted, Some(model)) => {
            record_persistent_model_failure(agent_name, model).await;
        }
        _ => record_credit_exhaustion(agent_name, reason).await,
    }
    true
}

/// Record that a specific agent+model combo has failed.
///
/// Applies exponential backoff based on the model's failure count in KV.
pub async fn record_model_failure(agent_name: &str, model: &str) {
    let base = get_agent_backoff_base(agent_name);
    let schedule = ModelBackoff {
        prefix: FAILURE_COUNT_PREFIX,
        base,
        max: BACKOFF_MAX_SECS,
        allow_extended: true,
        reason: "model_error",
    };
    record_model_backoff(agent_name, model, &schedule).await;
}

/// Record a rate-limit event for an agent+model.
///
/// If the error message advertises a relative usage window (e.g.
/// "5-hour usage limit"), the cooldown is applied to the specific model for
/// that window length. The quota is model-scoped, so the whole agent is not
/// cooled. If no window is detected, fall back to the generic agent+model
/// exponential backoff path.
pub async fn record_rate_limit(agent_name: &str, model: Option<&str>, error_message: &str) {
    if let Some(window_secs) = parse_relative_usage_window(error_message)
        && let Some(m) = model
    {
        let cooldown_until = chrono::Utc::now()
            .timestamp()
            .saturating_add(i64::try_from(window_secs).unwrap_or(i64::MAX));
        set_model_cooldown(agent_name, m, window_secs, "rate_limit_usage_window").await;
        tracing::info!(
            agent = agent_name,
            model = m,
            window_secs,
            cooldown_until,
            "relative usage window detected: applying model-specific cooldown"
        );
        return;
    }

    record_agent_failure_with_message(agent_name, error_message).await;
    if let Some(m) = model {
        record_model_failure(agent_name, m).await;
    }
}

/// Record a persistent model-specific failure and apply longer exponential backoff.
///
/// Used for failure modes that are commonly broken for long periods (for example
/// silent exit-0 or opaque provider errors). Starts at 4h and escalates with
/// the generic backoff formula up to 7 days.
///
/// Uses [`PERSISTENT_FAILURE_COUNT_PREFIX`], a counter independent of the generic
/// `failure_count:` key that `record_agent_success()` resets on every successful
/// run. Otherwise a single intervening success between persistent failures would
/// reset escalation back to the 4h base, letting structurally unreliable models
/// (intermittent parse errors, truncated runs) stay in rotation indefinitely.
pub async fn record_persistent_model_failure(agent_name: &str, model: &str) {
    let schedule = ModelBackoff {
        prefix: PERSISTENT_FAILURE_COUNT_PREFIX,
        base: PERSISTENT_MODEL_BACKOFF_BASE_SECS,
        max: PERSISTENT_MODEL_BACKOFF_MAX_SECS,
        allow_extended: false,
        reason: "persistent_model_error",
    };
    record_model_backoff(agent_name, model, &schedule).await;
}

/// Record a `ModelUnavailable` failure for an agent+model.
///
/// "Not found", "not supported" and region blocks never lift on their own, so they get the
/// persistent backoff (4h base, 7d max). Any other message gets [`record_model_failure`]
pub async fn record_model_unavailable(agent_name: &str, model: &str, message: &str) {
    if is_model_permanently_gone(message) {
        record_persistent_model_failure(agent_name, model).await;
    } else {
        record_model_failure(agent_name, model).await;
    }
}

fn is_model_permanently_gone(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("not found")
        || lower.contains("not supported")
        || lower.contains("not available in your country")
        || lower.contains("not available in your region")
}

/// Record a run that failed without showing the model is gone: empty or cut off output, an
/// unparseable review, an opaque provider error.
///
/// A stopped session or one odd answer causes these, so the backoff is 5 then 15 minutes. Billing
/// exhaustion and "model not found" keep [`record_persistent_model_failure`]
pub async fn record_transient_model_failure(agent_name: &str, model: &str) {
    let schedule = ModelBackoff {
        prefix: FAILURE_COUNT_PREFIX,
        base: TRANSIENT_MODEL_BACKOFF_BASE_SECS,
        max: TRANSIENT_MODEL_BACKOFF_MAX_SECS,
        allow_extended: false,
        reason: "transient_model_error",
    };
    record_model_backoff(agent_name, model, &schedule).await;
}

/// Set a model cooldown with a custom duration (in seconds) and reason.
///
/// Generic model-cooldown setter shared by silence detection and the
/// rate-limit relative-usage-window path. The reason is persisted with
/// the cooldown (KV + in-memory entry) so operators can tell the two
/// apart in `orch cooldown list`.
pub async fn set_model_cooldown(agent_name: &str, model: &str, duration_secs: u64, reason: &str) {
    let key = format!("{agent_name}:{model}");
    let cooldown_until = chrono::Utc::now()
        .timestamp()
        .saturating_add(i64::try_from(duration_secs).unwrap_or(i64::MAX));
    set_cooldown_async(&key, cooldown_until, reason).await;
}

/// Set a model cooldown that ends at a vendor-provided Unix timestamp.
pub async fn set_model_cooldown_until(agent_name: &str, model: &str, until: i64) {
    let key = format!("{agent_name}:{model}");
    set_cooldown_async(&key, until, "vendor_reset_at").await;
}

/// Set a short agent-level cooldown (in seconds).
///
/// Used by silence detection to temporarily block the whole agent so the
/// router picks a different one on re-route. Unlike `record_agent_failure`
/// (exponential backoff), this uses a short duration (typically 120s) —
/// just enough to force one re-route cycle to a different agent.
pub async fn set_agent_cooldown(agent_name: &str, duration_secs: u64) {
    let cooldown_until = chrono::Utc::now()
        .timestamp()
        .saturating_add(i64::try_from(duration_secs).unwrap_or(i64::MAX));
    set_cooldown_async(agent_name, cooldown_until, "silence_agent_cooldown").await;
}

/// KV prefix for the latest quota window of each type an agent reported: `rate_window:{agent}:{type}`
pub const RATE_WINDOW_KV_PREFIX: &str = "rate_window:";

/// One quota window from a vendor `rate_limit_event`, with the vendor window type kept as sent
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RateLimitWindow {
    pub status: String,
    pub window_type: String,
    pub utilization: Option<f64>,
    pub resets_at: Option<i64>,
}

/// Store the latest window for an agent. A rejected window cools the agent until its reset time
pub async fn record_rate_limit_window(agent_name: &str, window: &RateLimitWindow) {
    let store_opt = cooldown_store().lock().await.clone();
    if let Some(store) = store_opt
        && let Ok(value) = serde_json::to_string(window)
        && let Err(e) = store
            .kv_set(
                &format!("{RATE_WINDOW_KV_PREFIX}{agent_name}:{}", window.window_type),
                &value,
            )
            .await
    {
        tracing::warn!(agent = agent_name, err = %e, "failed to persist rate limit window");
    }

    let now = chrono::Utc::now().timestamp();
    if window.status == "rejected"
        && let Some(reset) = window.resets_at
        && reset > now
    {
        let until = reset.min(now + PERSISTENT_MODEL_BACKOFF_MAX_SECS);
        set_cooldown_async(agent_name, until, "rate_limit_window").await;
        tracing::info!(
            agent = agent_name,
            window = %window.window_type,
            cooldown_until = until,
            "rate limit window rejected: cooling agent until vendor reset"
        );
    }
}

/// Latest stored quota window per agent, sorted by agent name
pub async fn list_rate_limit_windows(store: &Arc<TaskStore>) -> Vec<(String, RateLimitWindow)> {
    let rows = store
        .kv_list_prefix(RATE_WINDOW_KV_PREFIX)
        .await
        .unwrap_or_default();
    let mut windows: Vec<(String, RateLimitWindow)> = rows
        .into_iter()
        .filter_map(|(key, value)| {
            let (agent, _) = key.strip_prefix(RATE_WINDOW_KV_PREFIX)?.rsplit_once(':')?;
            let window = serde_json::from_str(&value).ok()?;
            Some((agent.to_string(), window))
        })
        .collect();
    windows.sort_by(|a, b| (&a.0, &a.1.window_type).cmp(&(&b.0, &b.1.window_type)));
    windows
}

/// Record a silence detection for an agent+model and apply extended cooldowns
/// when repeated silences exceed the threshold within the rolling window.
pub async fn record_silence_detection(agent_name: &str, model: &str) -> Option<SilenceCountResult> {
    let store_opt = cooldown_store().lock().await.clone();
    let Some(store) = store_opt else {
        tracing::debug!(
            agent = agent_name,
            model,
            "skipping silence count record (no KV store)"
        );
        return None;
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
        set_model_cooldown(
            agent_name,
            model,
            SILENCE_EXTENDED_COOLDOWN_SECS,
            "silence_detected",
        )
        .await;
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
    let map = cooldown_lock();
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
    let map = cooldown_lock();
    map.get(key).and_then(|entry| {
        let now = chrono::Utc::now().timestamp();
        if now < entry.cooldown_until {
            Some(entry.reason.clone())
        } else {
            None
        }
    })
}

/// Active cooldowns as `(key, cooldown_until_unix, reason)` from the store, so a process other
/// than the daemon sees the same list as `orch cooldown list`
pub async fn list_persisted_cooldowns(store: &TaskStore) -> Vec<(String, i64, String)> {
    let now = chrono::Utc::now().timestamp();
    let mut out: Vec<(String, i64, String)> = store
        .kv_list_prefix(KV_PREFIX)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(key, value)| {
            let (until, reason) = parse_cooldown_value(&value)?;
            (now < until).then(|| (key.trim_start_matches(KV_PREFIX).to_string(), until, reason))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Return all currently active cooldowns as `(key, cooldown_until_unix, reason)`.
///
/// Reads from the in-memory map only — always fast, no async needed.
/// Expired entries are filtered out. Used by `orch cooldown list`.
pub fn list_all_cooldowns() -> Vec<(String, i64, String)> {
    let now = chrono::Utc::now().timestamp();
    let map = cooldown_lock();
    let mut result: Vec<(String, i64, String)> = map
        .iter()
        .filter(|(_, entry)| now < entry.cooldown_until)
        .map(|(key, entry)| (key.clone(), entry.cooldown_until, entry.reason.clone()))
        .collect();
    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

/// Write `rate_limits_cleared_at:{agent}` = now so the next health check
/// ignores older `rate_limits` rows. Rows stay in the table for metrics
pub async fn mark_rate_limits_cleared(store: &Arc<TaskStore>, agents: &[String]) {
    let now = chrono::Utc::now().timestamp().to_string();
    let mut seen = HashSet::new();
    for key in agents {
        let agent = agent_from_key(key);
        if agent.is_empty() || !seen.insert(agent.to_string()) {
            continue;
        }
        let kv_key = format!("{RATE_LIMITS_CLEARED_AT_PREFIX}{agent}");
        if let Err(e) = store.kv_set(&kv_key, &now).await {
            tracing::warn!(agent, err = %e, "failed to mark rate limits cleared");
        }
    }
}

/// Clear a specific cooldown by key, or all cooldowns when `key == "*"`.
///
/// Removes from the in-memory map, writes a past timestamp to KV, and resets
/// the corresponding `failure_count:` entries so the next failure starts
/// backoff from the base duration. Also writes `rate_limits_cleared_at:{agent}`
/// so a degraded agent held only by rate rows recovers on the next health check
pub async fn clear_cooldown(key: &str, store: &Arc<TaskStore>) {
    let mut agents_to_mark: Vec<String> = Vec::new();
    if key == "*" {
        let keys: Vec<String> = {
            let mut map = cooldown_lock();
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
            // Also reset the persistent-model-failure counter.
            let persistent_fc_key = format!("{PERSISTENT_FAILURE_COUNT_PREFIX}{k}");
            if let Err(e) = store.kv_set(&persistent_fc_key, "0").await {
                tracing::warn!(key = persistent_fc_key, err = %e, "failed to reset persistent failure count");
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
        if let Ok(pfc_entries) = store.kv_list_prefix(PERSISTENT_FAILURE_COUNT_PREFIX).await {
            for (pfc_key, _) in pfc_entries {
                if let Err(e) = store.kv_set(&pfc_key, "0").await {
                    tracing::warn!(key = pfc_key, err = %e, "failed to reset persistent failure count");
                }
            }
        }
        tracing::info!(
            count = keys.len(),
            "cleared all cooldowns and failure counts"
        );
        agents_to_mark.extend(keys.iter().map(|k| agent_from_key(k).to_string()));
        match store.rate_limit_agents().await {
            Ok(rl_agents) => agents_to_mark.extend(rl_agents),
            Err(e) => tracing::warn!(err = %e, "failed to list rate_limits agents while clearing"),
        }
        match store.kv_list_prefix(RATE_LIMITS_CLEARED_AT_PREFIX).await {
            Ok(marks) => {
                for (k, _) in marks {
                    let agent = k.trim_start_matches(RATE_LIMITS_CLEARED_AT_PREFIX);
                    if !agent.is_empty() {
                        agents_to_mark.push(agent.to_string());
                    }
                }
            }
            Err(e) => tracing::warn!(err = %e, "failed to list rate_limits_cleared_at keys"),
        }
    } else {
        {
            let mut map = cooldown_lock();
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
        // Also reset the persistent-model-failure counter.
        let persistent_fc_key = format!("{PERSISTENT_FAILURE_COUNT_PREFIX}{key}");
        if let Err(e) = store.kv_set(&persistent_fc_key, "0").await {
            tracing::warn!(key = persistent_fc_key, err = %e, "failed to reset persistent failure count");
        }
        tracing::info!(key, "cleared cooldown and failure count");
        agents_to_mark.push(agent_from_key(key).to_string());
    }
    mark_rate_limits_cleared(store, &agents_to_mark).await;
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
    let mut map = cooldown_lock();
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
        let value = serde_json::json!({"until": cooldown_until, "reason": reason}).to_string();
        if let Err(e) = store.kv_set(&kv_key, &value).await {
            tracing::warn!(
                kv_key,
                err = %e,
                "failed to persist cooldown to KV store — rolling back in-memory entry"
            );
            let mut map = cooldown_lock();
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

/// Helper to acquire the cooldown map lock, logging and recovering on poison.
fn cooldown_lock() -> MutexGuard<'static, HashMap<String, CooldownEntry>> {
    cooldowns().lock().unwrap_or_else(|e| {
        tracing::error!("cooldown map lock poisoned — recovering with partial state; backoff and degraded-agent decisions may be incorrect");
        e.into_inner()
    })
}

/// Helper to acquire the degraded agents lock, logging and recovering on poison.
fn degraded_agents_lock() -> MutexGuard<'static, HashMap<String, String>> {
    degraded_agents().lock().unwrap_or_else(|e| {
        tracing::error!("degraded agents lock poisoned — recovering with partial state; health check decisions may be incorrect");
        e.into_inner()
    })
}

/// Helper to acquire the GitHub 5xx timestamps lock, logging and recovering on poison.
fn github_5xx_timestamps_lock() -> MutexGuard<'static, Vec<i64>> {
    github_5xx_timestamps().lock().unwrap_or_else(|e| {
        tracing::error!("github 5xx timestamps lock poisoned — recovering with partial state; circuit breaker may operate incorrectly");
        e.into_inner()
    })
}

/// Helper to acquire the GitHub 5xx circuit open lock, logging and recovering on poison.
fn github_5xx_circuit_open_lock() -> MutexGuard<'static, Option<i64>> {
    github_5xx_circuit_open().lock().unwrap_or_else(|e| {
        tracing::error!("github 5xx circuit open lock poisoned — recovering with partial state; circuit breaker may operate incorrectly");
        e.into_inner()
    })
}

/// Check if a key is in cooldown with the given reason.
fn is_in_cooldown_with_reason(key: &str, reason: &str) -> bool {
    let map = cooldown_lock();
    map.get(key).is_some_and(|entry| {
        entry.reason == reason && chrono::Utc::now().timestamp() < entry.cooldown_until
    })
}

/// Check if a key is currently in cooldown.
fn is_in_cooldown(key: &str) -> bool {
    let map = cooldown_lock();
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

/// In-memory map of agents currently flagged as degraded, keyed by agent
/// with the reason that holds now
fn degraded_agents() -> &'static Mutex<HashMap<String, String>> {
    static DEGRADED: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    DEGRADED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Check if an agent is currently flagged as degraded by the health check.
///
/// Degraded means: all configured models are cooled **or** the agent has
/// exceeded the rate-limit threshold within the configured lookback window.
pub fn is_agent_degraded(agent: &str) -> bool {
    let set = degraded_agents_lock();
    set.contains_key(agent)
}

/// Reason the agent is currently degraded, if it is
pub fn agent_degraded_reason(agent: &str) -> Option<String> {
    degraded_agents_lock().get(agent).cloned()
}

/// Active degraded agents as `(agent, reason)`, sorted by agent name
pub fn list_degraded_agents() -> Vec<(String, String)> {
    let mut entries: Vec<(String, String)> = degraded_agents_lock()
        .iter()
        .map(|(agent, reason)| (agent.clone(), reason.clone()))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

/// Mark an agent as degraded (called by [`refresh_degraded_agents`]).
pub fn mark_agent_degraded(agent: &str) {
    mark_agent_degraded_with_reason(agent, "degraded");
}

fn mark_agent_degraded_with_reason(agent: &str, reason: &str) {
    let mut set = degraded_agents_lock();
    set.insert(agent.to_string(), reason.to_string());
}

/// Clear the degraded flag for an agent.
pub fn clear_agent_degraded(agent: &str) {
    let mut set = degraded_agents_lock();
    set.remove(agent);
}

/// Refresh the degraded-agent set from the rate_limits table and cooldown state.
///
/// An agent is marked degraded when:
/// 1. It is in agent-level cooldown (explicit cooldown KV key exists and hasn't expired), **or**
/// 2. All its configured models are in cooldown, **or**
/// 3. It has >= `threshold` `limit_type = 'rate'` events within `window_hours`.
///
/// Failures keep their own cooldowns and do not count toward the threshold
/// Agents that no longer meet these criteria are cleared.
///
/// Note: This function calls `clear_expired_cooldowns()` first to ensure the
/// in-memory cooldown map reflects current state. An agent should NOT be marked
/// degraded simply because an expired cooldown entry hasn't been cleaned up yet.
pub async fn refresh_degraded_agents(
    store: &Arc<TaskStore>,
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
            let reason = if in_cooldown {
                "agent in cooldown"
            } else if no_models {
                "all models cooled"
            } else {
                "rate limit threshold exceeded"
            };
            let previous = agent_degraded_reason(agent);
            match previous.as_deref() {
                None => tracing::warn!(
                    agent,
                    reason,
                    rate_limit_count,
                    window_hours,
                    threshold,
                    "pre-emptive health check: marking agent as degraded"
                ),
                Some(prev) if prev != reason => tracing::warn!(
                    agent,
                    reason,
                    previous = prev,
                    rate_limit_count,
                    window_hours,
                    threshold,
                    "pre-emptive health check: degraded reason changed"
                ),
                Some(_) => {}
            }
            mark_agent_degraded_with_reason(agent, reason);
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
    let mut map = cooldown_lock();
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
        let mut ts = github_5xx_timestamps_lock();
        ts.retain(|t| *t >= window_start);
        ts.push(now);
        ts.len() >= GITHUB_5XX_THRESHOLD
    };

    if should_trip {
        let cooldown_until = {
            let mut open = github_5xx_circuit_open_lock();
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
        let mut ts = github_5xx_timestamps_lock();
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
        let mut open = github_5xx_circuit_open_lock();
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
        let mut open = github_5xx_circuit_open_lock();
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
        let open = github_5xx_circuit_open_lock();
        if let Some(until) = *open {
            let now = chrono::Utc::now().timestamp();
            if now < until {
                return u64::try_from(until - now).unwrap_or(0);
            }
            return 0;
        }
    }

    // Fall back to the generic cooldown map if the dedicated flag isn't set.
    if let Some(until) = cooldown_until("github:5xx") {
        let now = chrono::Utc::now().timestamp();
        if now < until {
            return u64::try_from(until - now).unwrap_or(0);
        }
    }
    0
}

/// Parse a "try again at {date}" or "reset at {date}" from an error message.
///
/// Returns a Unix timestamp if a retry-at date is found.
pub(crate) fn parse_retry_at(error_message: &str) -> Option<i64> {
    if error_message.is_empty() {
        return None;
    }

    let lower = error_message.to_lowercase();
    // Accept a wider set of vendor-provided markers. Claude commonly uses
    // variations like "resets 10:30am" (no explicit "at"), so include
    // both "resets" and the older "reset at"/"try again at" markers.
    for retry_marker in [
        "try again at ",
        "reset at ",
        "resets at ",
        "resets ",
        "reset ",
    ] {
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
                "%Y-%m-%d %H:%M:%S",  // 2026-03-26 05:55:00
                "%Y-%m-%d %H:%M",     // 2026-03-26 05:55
            ] {
                if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(&cleaned, fmt) {
                    // Vendor messages (e.g. Codex/GLM) emit times in the machine's
                    // local timezone, not UTC. Interpret naive datetime as local time.
                    let utc = match chrono::Local.from_local_datetime(&dt) {
                        chrono::LocalResult::Single(local_dt) => {
                            local_dt.with_timezone(&chrono::Utc)
                        }
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
                        marker = retry_marker,
                        "parsed rate limit retry-at date"
                    );
                    return Some(utc.timestamp());
                }
            }
            tracing::debug!(
                raw = date_str,
                marker = retry_marker,
                "could not parse retry-at date"
            );

            // If the vendor provided only a time (e.g. "resets 10:30am"),
            // attempt to parse a time-only and interpret it as the next
            // occurrence of that time in the local timezone.
            // Strip parenthesised timezone hints like "(America/Sao_Paulo)".
            let time_only = date_str.split_once('(').map_or(date_str, |(t, _)| t).trim();
            // Hour-only values like "8am" have no minute field, which chrono needs, so insert ":00"
            let mut compact = time_only.replace(" am", "am").replace(" pm", "pm");
            let split = compact.rfind(' ').map_or(0, |i| i + 1);
            let last = &compact[split..];
            if !last.contains(':')
                && (last.ends_with("am") || last.ends_with("pm"))
                && last.len() > 2
            {
                let at = split + last.len() - 2;
                compact.insert_str(at, ":00");
            }
            // "Jun 9 at 1am" has no year, so take the next future occurrence
            let md = compact.replace(" at ", " ");
            let now_local = chrono::Local::now();
            let this_year = chrono::Datelike::year(&now_local);
            for year in [this_year, this_year + 1] {
                let with_year = format!("{year} {md}");
                if let Ok(dt) =
                    chrono::NaiveDateTime::parse_from_str(&with_year, "%Y %b %d %I:%M%p")
                    && let Some(local_dt) = chrono::Local.from_local_datetime(&dt).earliest()
                    && local_dt > now_local
                {
                    return Some(local_dt.timestamp());
                }
            }
            let time_only = compact.as_str();
            for tf in &["%I:%M %p", "%I:%M%p", "%I %p", "%H:%M"] {
                if let Ok(t) = chrono::NaiveTime::parse_from_str(time_only, tf) {
                    // Build a NaiveDateTime for today at that time, then choose
                    // today or tomorrow depending on whether the time has passed.
                    let local_now = chrono::Local::now();
                    let today = local_now.date_naive();
                    let candidate = chrono::NaiveDateTime::new(today, t);
                    // Use the TimeZone trait methods; annotate types so the
                    // compiler can infer the concrete DateTime types.
                    let candidate_local: chrono::DateTime<chrono::Local> =
                        match chrono::Local.from_local_datetime(&candidate) {
                            chrono::LocalResult::Single(dt) => dt,
                            chrono::LocalResult::Ambiguous(earliest, _) => earliest,
                            chrono::LocalResult::None => {
                                // DST gap — fall back to UTC interpretation
                                let utc_dt = candidate.and_utc();
                                return Some(utc_dt.timestamp());
                            }
                        };
                    let chosen = if candidate_local <= local_now {
                        // Use tomorrow
                        let tomorrow = today.succ_opt().unwrap_or(today);
                        let ndt = chrono::NaiveDateTime::new(tomorrow, t);
                        match chrono::Local.from_local_datetime(&ndt) {
                            chrono::LocalResult::Single(dt) => dt.with_timezone(&chrono::Utc),
                            chrono::LocalResult::Ambiguous(earliest, _) => {
                                earliest.with_timezone(&chrono::Utc)
                            }
                            chrono::LocalResult::None => ndt.and_utc(),
                        }
                    } else {
                        candidate_local.with_timezone(&chrono::Utc)
                    };
                    tracing::info!(
                        retry_at = %chosen,
                        raw = date_str,
                        marker = retry_marker,
                        "parsed rate limit retry-at time-only; interpreted as next occurrence"
                    );
                    return Some(chosen.timestamp());
                }
            }
        }
    }

    // Without a specific retry/reset date, return None and let the caller use
    // its default cooldown (exponential backoff). A blanket long fallback here
    // would cause false billing-cycle cooldowns on agents without those limits.
    None
}

/// Parse a relative usage window (e.g. "5-hour usage limit") from an error
/// message.
///
/// Returns the window length in seconds. Only matches in usage/quota/limit
/// contexts ("usage limit", "quota", "window", "limit") to avoid interpreting
/// unrelated numbers as cooldown durations.
///
/// `pub(crate)` so `engine::runner::fallback::handle_error()` can apply the
/// same window-detection the `needs_review` call site
/// (`record_rate_limit()`, below) already uses — see issue #3580.
pub(crate) fn parse_relative_usage_window(error_message: &str) -> Option<u64> {
    static WINDOW_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)(?:^|\s|[\(\)\.,;:!?])(?:(\d+)\s*-?\s*(hour|hours|hr|hrs|minute|minutes|min|mins|day|days)|(hourly|daily|weekly|monthly))(?:$|\s|[\(\)\.,;:!?])"
        )
        .expect("usage window regex is valid")
    });

    if error_message.is_empty() {
        return None;
    }

    let lower = error_message.to_lowercase();
    let has_context = lower.contains("usage limit")
        || lower.contains("quota")
        || lower.contains("window")
        || lower.contains("limit");
    if !has_context {
        return None;
    }

    let mut max_secs: Option<u64> = None;
    for caps in WINDOW_RE.captures_iter(&lower) {
        let secs = if let Some(num) = caps.get(1) {
            let n: u64 = num.as_str().parse().ok()?;
            let unit = caps.get(2)?.as_str().to_lowercase();
            let multiplier = match unit.as_str() {
                "hour" | "hours" | "hr" | "hrs" => 3600,
                "minute" | "minutes" | "min" | "mins" => 60,
                "day" | "days" => 86400,
                _ => return None,
            };
            n.checked_mul(multiplier)?
        } else if let Some(word) = caps.get(3) {
            match word.as_str().to_lowercase().as_str() {
                "hourly" => 3600,
                "daily" => 86400,
                "weekly" => 604_800,
                "monthly" => 2_592_000,
                _ => return None,
            }
        } else {
            continue;
        };
        max_secs = Some(max_secs.map_or(secs, |m| m.max(secs)));
    }

    max_secs
}

/// Return the configured base backoff (seconds) for an agent.
///
/// Returns the value of `router.backoff_base.{agent}` if set, otherwise
/// the default BACKOFF_BASE_SECS (5 minutes).
///
/// This lets opencode and other higher-failure-rate agents use a longer
/// initial backoff to reduce wasted retry attempts against consistently
/// failing models.
pub fn get_agent_backoff_base(agent: &str) -> i64 {
    let key = format!("router.backoff_base.{agent}");
    config::get(&key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(BACKOFF_BASE_SECS)
}

#[cfg(test)]
pub(crate) async fn reset_global_state() {
    cooldowns().lock().unwrap().clear();
    *cooldown_store().lock().await = None;
    github_5xx_timestamps().lock().unwrap().clear();
    *github_5xx_circuit_open().lock().unwrap() = None;
    degraded_agents().lock().unwrap().clear();
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod classify_tests;
