use std::time::Duration;

use super::*;
use serial_test::serial;
use tokio::time;

async fn test_store() -> Arc<TaskStore> {
    Arc::new(TaskStore::open_memory().await.expect("in-memory store"))
}

fn sqlite_datetime_offset(secs: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::seconds(secs))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

#[serial(cooldown_state)]
#[tokio::test]
async fn init_loads_unexpired_cooldowns_from_kv() {
    super::reset_global_state().await;
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
        let mut map = cooldown_lock();
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

#[serial(cooldown_state)]
#[tokio::test]
async fn credit_exhaustion_inside_active_cooldown_keeps_counter_at_one() {
    super::reset_global_state().await;
    let store = test_store().await;
    *cooldown_store().lock().await = Some(Arc::clone(&store));

    let msg = "API error (status 402 Payment Required): usage balance exhausted, out of credits";
    assert!(record_credit_exhaustion_from_message("credit_dup_agent", Some("m"), msg).await);
    assert!(record_credit_exhaustion_from_message("credit_dup_agent", Some("m"), msg).await);

    assert!(is_agent_in_cooldown("credit_dup_agent"));
    let count = store
        .kv_get(&format!("{CREDIT_FAILURE_COUNT_PREFIX}credit_dup_agent"))
        .await
        .unwrap();
    assert_eq!(count.as_deref(), Some("1"));
}

#[serial(cooldown_state)]
#[tokio::test]
async fn non_credit_message_is_not_recorded_as_credit_exhaustion() {
    super::reset_global_state().await;
    assert!(!record_credit_exhaustion_from_message("x_agent", Some("m"), "boom").await);
    assert!(!is_agent_in_cooldown("x_agent"));
}

#[serial(cooldown_state)]
#[tokio::test]
async fn record_model_failure_writes_to_kv() {
    super::reset_global_state().await;
    let store = test_store().await;

    *cooldown_store().lock().await = Some(Arc::clone(&store));

    record_model_failure("testagent_persist", "testmodel_persist").await;

    // Give the async KV write a moment to settle before querying.
    time::sleep(Duration::from_millis(50)).await;

    let kv_key = format!("{KV_PREFIX}testagent_persist:testmodel_persist");
    let stored = store.kv_get(&kv_key).await.unwrap();
    assert!(stored.is_some(), "model failure should be persisted to KV");
    let (ts, reason) = parse_cooldown_value(&stored.unwrap()).expect("cooldown value");
    let now = chrono::Utc::now().timestamp();
    // Should be in the future (cooldown_until, not failed_at)
    assert!(ts > now, "persisted timestamp should be in the future");
    assert_eq!(reason, "model_error");
}

#[serial(cooldown_state)]
#[tokio::test]
async fn record_transient_model_failure_backs_off_5_then_15_minutes() {
    super::reset_global_state().await;
    let store = test_store().await;
    *cooldown_store().lock().await = Some(Arc::clone(&store));

    let agent = "testagent_transient";
    let model = "testmodel_transient";
    let key = format!("{agent}:{model}");

    record_transient_model_failure(agent, model).await;
    let first = cooldown_until(&key)
        .expect("first cooldown should exist")
        .saturating_sub(chrono::Utc::now().timestamp());
    assert!(
        (TRANSIENT_MODEL_BACKOFF_BASE_SECS - 5..=TRANSIENT_MODEL_BACKOFF_BASE_SECS + 5)
            .contains(&first),
        "first transient cooldown should be ~5m, got {first}s"
    );

    record_transient_model_failure(agent, model).await;
    let second = cooldown_until(&key)
        .expect("second cooldown should exist")
        .saturating_sub(chrono::Utc::now().timestamp());
    assert!(
        (TRANSIENT_MODEL_BACKOFF_MAX_SECS - 5..=TRANSIENT_MODEL_BACKOFF_MAX_SECS + 5)
            .contains(&second),
        "second transient cooldown should be capped at ~15m, got {second}s"
    );

    let (_, reason) = parse_cooldown_value(
        &store
            .kv_get(&format!("{KV_PREFIX}{key}"))
            .await
            .unwrap()
            .expect("cooldown persisted"),
    )
    .expect("cooldown value");
    assert_eq!(reason, "transient_model_error");

    let count = store
        .kv_get(&format!("{FAILURE_COUNT_PREFIX}{key}"))
        .await
        .unwrap();
    assert_eq!(count.as_deref(), Some("2"));
}

#[serial(cooldown_state)]
#[tokio::test]
async fn record_model_failure_and_persistent_failure_use_separate_counters() {
    super::reset_global_state().await;
    let store = test_store().await;
    *cooldown_store().lock().await = Some(Arc::clone(&store));

    let agent = "testagent_counters";
    let model = "testmodel_counters";
    let key = format!("{agent}:{model}");

    record_model_failure(agent, model).await;
    record_persistent_model_failure(agent, model).await;
    record_persistent_model_failure(agent, model).await;

    let generic = store
        .kv_get(&format!("{FAILURE_COUNT_PREFIX}{key}"))
        .await
        .unwrap();
    let persistent = store
        .kv_get(&format!("{PERSISTENT_FAILURE_COUNT_PREFIX}{key}"))
        .await
        .unwrap();
    assert_eq!(generic.as_deref(), Some("1"));
    assert_eq!(persistent.as_deref(), Some("2"));

    let (_, reason) = parse_cooldown_value(
        &store
            .kv_get(&format!("{KV_PREFIX}{key}"))
            .await
            .unwrap()
            .expect("cooldown persisted"),
    )
    .expect("cooldown value");
    assert_eq!(reason, "persistent_model_error");
}

#[serial(cooldown_state)]
#[tokio::test]
async fn cooldown_reason_survives_restart_via_kv() {
    super::reset_global_state().await;
    let store = test_store().await;
    let agent = "test_reason_restart";
    let future = chrono::Utc::now().timestamp() + 600;

    init_cooldown_store(Arc::clone(&store)).await;
    set_cooldown_async(agent, future, "billing_cycle_exhausted").await;

    // Simulate a service restart: drop the in-memory map, reload from KV.
    {
        let mut map = cooldown_lock();
        map.clear();
    }
    init_cooldown_store(store).await;

    let entries = list_all_cooldowns();
    let entry = entries.iter().find(|(key, ..)| key == agent);
    assert_eq!(
        entry.map(|(_, _, reason)| reason.as_str()),
        Some("billing_cycle_exhausted"),
        "cooldown reason must survive a restart, not collapse to 'persisted'"
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn record_persistent_model_failure_escalates_and_increments_failure_count() {
    super::reset_global_state().await;
    let store = test_store().await;
    *cooldown_store().lock().await = Some(Arc::clone(&store));

    let agent = "testagent_persistent";
    let model = "testmodel_persistent";
    let key = format!("{agent}:{model}");

    // First failure -> base 4h.
    record_persistent_model_failure(agent, model).await;
    let now = chrono::Utc::now().timestamp();
    let first_until = cooldown_until(&key).expect("first cooldown should exist");
    let first_remaining = first_until.saturating_sub(now);
    assert!(
        first_remaining >= PERSISTENT_MODEL_BACKOFF_BASE_SECS - 5,
        "first persistent model cooldown should be ~4h, got {first_remaining}s"
    );

    // Second failure -> 12h (3x growth).
    record_persistent_model_failure(agent, model).await;
    let now2 = chrono::Utc::now().timestamp();
    let second_until = cooldown_until(&key).expect("second cooldown should exist");
    let second_remaining = second_until.saturating_sub(now2);
    assert!(
        second_remaining >= (PERSISTENT_MODEL_BACKOFF_BASE_SECS * 3) - 5,
        "second persistent model cooldown should be ~12h, got {second_remaining}s"
    );

    let count_key = format!("{PERSISTENT_FAILURE_COUNT_PREFIX}{agent}:{model}");
    let stored_count = store
        .kv_get(&count_key)
        .await
        .expect("kv read should succeed")
        .expect("failure count should be written");
    assert_eq!(stored_count, "2");
}

#[serial(cooldown_state)]
#[tokio::test]
async fn record_agent_success_does_not_reset_persistent_failure_count() {
    super::reset_global_state().await;
    let store = test_store().await;
    *cooldown_store().lock().await = Some(Arc::clone(&store));

    let agent = "testagent_persistent_success";
    let model = "testmodel_persistent_success";
    let key = format!("{agent}:{model}");

    // Two persistent failures escalate the counter to 2 (~12h cooldown).
    record_persistent_model_failure(agent, model).await;
    record_persistent_model_failure(agent, model).await;

    let count_key = format!("{PERSISTENT_FAILURE_COUNT_PREFIX}{agent}:{model}");
    let stored_count = store
        .kv_get(&count_key)
        .await
        .expect("kv read should succeed")
        .expect("failure count should be written");
    assert_eq!(stored_count, "2");

    // A successful run resets the generic failure_count but must NOT touch
    // the persistent counter — otherwise an intervening success would erase
    // escalation progress for a structurally unreliable model.
    record_agent_success(agent, model).await;

    let stored_count_after_success = store
        .kv_get(&count_key)
        .await
        .expect("kv read should succeed")
        .expect("persistent failure count should still exist");
    assert_eq!(
        stored_count_after_success, "2",
        "persistent failure count must survive an intervening success"
    );

    // The next persistent failure should escalate from 3 (~36h), not reset to base (4h).
    record_persistent_model_failure(agent, model).await;
    let now = chrono::Utc::now().timestamp();
    let third_until = cooldown_until(&key).expect("third cooldown should exist");
    let third_remaining = third_until.saturating_sub(now);
    assert!(
        third_remaining > PERSISTENT_MODEL_BACKOFF_BASE_SECS,
        "third persistent model failure should escalate past the 4h base, got {third_remaining}s"
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn record_persistent_model_failure_stays_capped_at_7_days() {
    super::reset_global_state().await;
    let store = test_store().await;
    *cooldown_store().lock().await = Some(Arc::clone(&store));

    let agent = "testagent_persistent_capped";
    let model = "testmodel_persistent_capped";
    let key = format!("{agent}:{model}");

    for _ in 0..9 {
        record_persistent_model_failure(agent, model).await;
    }

    let remaining = cooldown_until(&key)
        .expect("cooldown should exist")
        .saturating_sub(chrono::Utc::now().timestamp());
    assert!(
        remaining >= PERSISTENT_MODEL_BACKOFF_MAX_SECS - 5,
        "persistent model cooldown should stay capped at 7 days, got {remaining}s"
    );
    assert!(
        remaining <= PERSISTENT_MODEL_BACKOFF_MAX_SECS + 5,
        "persistent model cooldown should not exceed 7 days, got {remaining}s"
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn record_rate_limit_uses_model_specific_window_cooldown() {
    super::reset_global_state().await;
    let store = test_store().await;
    *cooldown_store().lock().await = Some(Arc::clone(&store));

    let agent = "test_record_rl_window_agent";
    let model = "test_record_rl_window_model";
    let msg = "You've reached your 5-hour usage limit. Your quota will reset when the current 5-hour window expires.";

    record_rate_limit(agent, Some(model), msg).await;

    assert!(
        is_model_in_cooldown(agent, model),
        "model should be cooled for the relative usage window"
    );
    assert!(
        !is_agent_in_cooldown(agent),
        "agent should not be cooled for a model-scoped quota"
    );

    let key = format!("{agent}:{model}");
    let remaining = cooldown_until(&key)
        .expect("model cooldown should exist")
        .saturating_sub(chrono::Utc::now().timestamp());
    assert!(
        remaining >= 5 * 3600 - 5,
        "model cooldown should be at least ~5 hours, got {remaining}s"
    );
    assert_eq!(
        cooldown_reason(&key).as_deref(),
        Some("rate_limit_usage_window"),
        "relative usage-window cooldown must be labeled as a rate limit, not silence"
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn record_rate_limit_without_window_falls_back_to_agent_and_model() {
    super::reset_global_state().await;
    let store = test_store().await;
    *cooldown_store().lock().await = Some(Arc::clone(&store));

    let agent = "test_record_rl_fallback_agent";
    let model = "test_record_rl_fallback_model";
    let msg = "429 Too Many Requests";

    record_rate_limit(agent, Some(model), msg).await;

    assert!(
        is_agent_in_cooldown(agent),
        "agent should be cooled when no relative window is detected"
    );
    assert!(
        is_model_in_cooldown(agent, model),
        "model should be cooled when no relative window is detected"
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn agent_cooldown_persists_to_kv() {
    super::reset_global_state().await;
    // Agent-level cooldowns persist to KV via set_cooldown_async
    let agent = "test_agent_persist_check";
    record_agent_failure_with_message(agent, "").await;
    assert!(is_agent_in_cooldown(agent));
}

#[serial(cooldown_state)]
#[tokio::test]
async fn cooldown_never_shortens() {
    super::reset_global_state().await;
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

#[serial(cooldown_state)]
#[tokio::test]
async fn silence_agent_cooldown_is_short_lived() {
    super::reset_global_state().await;
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
        remaining <= i64::try_from(SILENCE_AGENT_COOLDOWN_SECS).unwrap(),
        "silence agent cooldown should be <= {SILENCE_AGENT_COOLDOWN_SECS}s, got {remaining}s"
    );
    assert!(remaining > 0, "cooldown should still be active");
}

#[serial(cooldown_state)]
#[tokio::test]
async fn record_silence_detection_applies_extended_cooldown() {
    super::reset_global_state().await;
    let store = test_store().await;
    *cooldown_store().lock().await = Some(Arc::clone(&store));

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
        remaining >= i64::try_from(SILENCE_EXTENDED_COOLDOWN_SECS).unwrap() - 5,
        "extended cooldown should be applied, got {remaining}s"
    );

    let kv_key = format!("{SILENCE_COUNT_PREFIX}{agent}:{model}");
    let stored = store.kv_get(&kv_key).await.unwrap().unwrap();
    assert_eq!(stored, "[]");
}

#[serial(cooldown_state)]
#[tokio::test]
async fn record_credit_exhaustion_applies_agent_cooldown() {
    super::reset_global_state().await;
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

#[serial(cooldown_state)]
#[tokio::test]
async fn record_credit_exhaustion_org_level_applies_longer_cooldown() {
    super::reset_global_state().await;
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

#[serial(cooldown_state)]
#[tokio::test]
async fn record_credit_exhaustion_billing_cycle_applies_24h() {
    super::reset_global_state().await;
    let store = test_store().await;
    *cooldown_store().lock().await = Some(Arc::clone(&store));

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

#[serial(cooldown_state)]
#[tokio::test]
async fn record_credit_exhaustion_billing_cycle_escalates() {
    super::reset_global_state().await;
    let store = test_store().await;
    *cooldown_store().lock().await = Some(Arc::clone(&store));

    let agent = "test_billing_cycle_escalation";

    // First failure: 24h
    record_credit_exhaustion(agent, CreditExhaustionReason::BillingCycleExhausted).await;
    let remaining_1 = {
        let map = cooldowns().lock().unwrap();
        let entry = map.get(agent).expect("cooldown entry should exist");
        entry.cooldown_until - chrono::Utc::now().timestamp()
    };
    assert!(
        (BILLING_CYCLE_COOLDOWN_SECS - 5..=BILLING_CYCLE_COOLDOWN_SECS + 5).contains(&remaining_1),
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

#[serial(cooldown_state)]
#[tokio::test]
async fn record_credit_exhaustion_billing_cycle_stays_capped_after_repeats() {
    super::reset_global_state().await;
    let store = test_store().await;
    *cooldown_store().lock().await = Some(Arc::clone(&store));

    let agent = "test_billing_cycle_agent_capped";

    for i in 0..5 {
        if i > 0 {
            // Each repeat is a new exhaustion after the previous cooldown ended
            cooldowns().lock().unwrap().remove(agent);
        }
        record_credit_exhaustion(agent, CreditExhaustionReason::BillingCycleExhausted).await;
    }

    let remaining = {
        let map = cooldowns().lock().unwrap();
        let entry = map.get(agent).expect("cooldown entry should exist");
        entry.cooldown_until - chrono::Utc::now().timestamp()
    };
    assert!(
        remaining >= BILLING_CYCLE_MAX_SECS - 5,
        "billing cycle cooldown should stay capped at 7 days, got {remaining}s"
    );
    assert!(
        remaining <= BILLING_CYCLE_MAX_SECS + 5,
        "billing cycle cooldown should not exceed 7 days, got {remaining}s"
    );
}

#[serial(cooldown_state)]
#[test]
fn credit_exhaustion_backoff_constants() {
    // OutOfCredits first failure: 1h base
    assert_eq!(
        compute_backoff(1, CREDIT_BACKOFF_BASE_SECS, CREDIT_BACKOFF_MAX_SECS, true),
        CREDIT_BACKOFF_BASE_SECS
    );
    // OrgLevelDisabled first failure: 2h base
    assert_eq!(
        compute_backoff(1, ORG_BACKOFF_BASE_SECS, CREDIT_BACKOFF_MAX_SECS, true),
        ORG_BACKOFF_BASE_SECS
    );
    // BillingCycleExhausted: 24h base, 7d cap
    assert_eq!(BILLING_CYCLE_COOLDOWN_SECS, 24 * 60 * 60);
    assert_eq!(BILLING_CYCLE_MAX_SECS, 7 * 24 * 60 * 60);
}

#[serial(cooldown_state)]
#[test]
fn compute_backoff_long_cap_without_extended_stays_capped() {
    assert_eq!(
        compute_backoff(
            3,
            BILLING_CYCLE_COOLDOWN_SECS,
            BILLING_CYCLE_MAX_SECS,
            false
        ),
        BILLING_CYCLE_MAX_SECS,
        "7-day capped paths should stop at the documented max"
    );
    assert_eq!(
        compute_backoff(
            9,
            PERSISTENT_MODEL_BACKOFF_BASE_SECS,
            PERSISTENT_MODEL_BACKOFF_MAX_SECS,
            false,
        ),
        PERSISTENT_MODEL_BACKOFF_MAX_SECS,
        "persistent model failures should not extend past 7 days"
    );
}

// ---- Degraded-agent tracking tests ----

#[serial(cooldown_state)]
#[test]
fn mark_and_check_agent_degraded() {
    let agent = "test_degraded_mark";
    assert!(!is_agent_degraded(agent));
    mark_agent_degraded(agent);
    assert!(is_agent_degraded(agent));
    clear_agent_degraded(agent);
    assert!(!is_agent_degraded(agent));
}

#[serial(cooldown_state)]
#[tokio::test]
async fn sync_from_kv_drops_in_memory_entry_cleared_externally() {
    super::reset_global_state().await;
    let store = test_store().await;
    let agent = "test_sync_external_clear";
    let future = chrono::Utc::now().timestamp() + 600;

    // Register the store and put a cooldown in both memory and KV.
    init_cooldown_store(Arc::clone(&store)).await;
    set_cooldown_async(agent, future, "test").await;
    assert!(is_agent_in_cooldown(agent));

    // Simulate `orch cooldown clear --all` from a separate CLI process: it
    // can only write KV (to "0"); the running engine's in-memory map is
    // untouched.
    store
        .kv_set(&format!("{KV_PREFIX}{agent}"), "0")
        .await
        .expect("kv_set tombstone");
    assert!(
        is_agent_in_cooldown(agent),
        "in-memory cooldown should still be present before sync"
    );

    sync_from_kv(&store).await;

    assert!(
        !is_agent_in_cooldown(agent),
        "sync_from_kv must drop in-memory entries the KV tombstoned"
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn sync_from_kv_closes_github_circuit_when_row_cleared() {
    super::reset_global_state().await;
    let store = test_store().await;
    {
        *github_5xx_circuit_open_lock() = Some(chrono::Utc::now().timestamp() + 600);
        github_5xx_timestamps_lock().push(chrono::Utc::now().timestamp());
    }
    assert!(is_github_circuit_open());

    store
        .kv_set("cooldown:github:5xx", "0")
        .await
        .expect("kv_set tombstone");
    sync_from_kv(&store).await;

    assert!(!is_github_circuit_open());
    assert!(github_5xx_timestamps_lock().is_empty());
}

#[serial(cooldown_state)]
#[tokio::test]
async fn sync_from_kv_keeps_5xx_window_when_expired_row_and_breaker_closed() {
    super::reset_global_state().await;
    let store = test_store().await;
    let now = chrono::Utc::now().timestamp();
    {
        github_5xx_timestamps_lock().push(now);
        github_5xx_timestamps_lock().push(now);
    }

    store
        .kv_set("cooldown:github:5xx", &(now - 600).to_string())
        .await
        .expect("kv_set past");
    sync_from_kv(&store).await;

    assert!(!is_github_circuit_open());
    assert_eq!(github_5xx_timestamps_lock().len(), 2);

    github_5xx_timestamps_lock().clear();
}

#[serial(cooldown_state)]
#[tokio::test]
async fn sync_from_kv_keeps_github_circuit_open_for_future_row() {
    super::reset_global_state().await;
    let store = test_store().await;
    let future = chrono::Utc::now().timestamp() + 600;
    *github_5xx_circuit_open_lock() = Some(future);

    store
        .kv_set("cooldown:github:5xx", &future.to_string())
        .await
        .expect("kv_set future");
    sync_from_kv(&store).await;

    assert!(is_github_circuit_open());

    *github_5xx_circuit_open_lock() = None;
}

#[serial(cooldown_state)]
#[tokio::test]
async fn sync_from_kv_keeps_in_memory_entry_when_kv_row_missing() {
    // Race window: set_cooldown_async writes memory first, then KV. If a
    // sync runs between those two writes, the KV row is missing — we must
    // leave the in-memory entry alone, not drop it.
    super::reset_global_state().await;
    let store = test_store().await;
    let agent = "test_sync_race_window";
    let future = chrono::Utc::now().timestamp() + 600;

    // Simulate the race: memory has the entry, KV does not.
    set_cooldown_in_memory(agent, future, "raced");
    assert!(is_agent_in_cooldown(agent));

    sync_from_kv(&store).await;

    assert!(
        is_agent_in_cooldown(agent),
        "sync_from_kv must NOT drop entries whose KV row is missing — that is the set_cooldown_async race window"
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn sync_from_kv_adopts_kv_timestamp() {
    // KV is the source of truth: if KV has a different active timestamp,
    // memory adopts it (covers external manual KV edits).
    super::reset_global_state().await;
    let store = test_store().await;
    let agent = "test_sync_adopt_kv";
    let original = chrono::Utc::now().timestamp() + 600;
    let updated = chrono::Utc::now().timestamp() + 1200;

    init_cooldown_store(Arc::clone(&store)).await;
    set_cooldown_async(agent, original, "initial").await;

    store
        .kv_set(&format!("{KV_PREFIX}{agent}"), &updated.to_string())
        .await
        .expect("kv_set updated");

    sync_from_kv(&store).await;

    let until = cooldown_until(agent).expect("cooldown still active");
    assert_eq!(
        until, updated,
        "sync_from_kv must adopt KV's authoritative timestamp"
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn refresh_degraded_agents_marks_cooled_agent() {
    super::reset_global_state().await;
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

#[serial(cooldown_state)]
#[tokio::test]
async fn refresh_degraded_agents_marks_no_models_agent() {
    super::reset_global_state().await;
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

#[serial(cooldown_state)]
#[tokio::test]
async fn refresh_degraded_agents_marks_rate_limited_agent() {
    super::reset_global_state().await;
    let store = test_store().await;
    let agent = "test_refresh_rate_limited";

    // Insert rate limit events exceeding threshold
    for _ in 0..4 {
        store.record_rate_limit(agent, "rate", None).await.unwrap();
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

#[serial(cooldown_state)]
#[tokio::test]
async fn refresh_degraded_agents_clears_healthy_agent() {
    super::reset_global_state().await;
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

#[serial(cooldown_state)]
#[tokio::test]
async fn refresh_degraded_agents_ignores_expired_cooldowns() {
    super::reset_global_state().await;
    let store = test_store().await;
    let agent = "test_refresh_expired_cooldown";

    // Manually insert an already-expired cooldown entry directly into the map
    // (simulating a stale entry that wasn't cleaned up)
    let expired_ts = chrono::Utc::now().timestamp() - 100; // 100 seconds ago
    {
        let mut map = cooldowns().lock().unwrap();
        map.insert(
            agent.to_string(),
            CooldownEntry {
                cooldown_until: expired_ts,
                reason: "expired_test".to_string(),
            },
        );
    }

    // Verify the expired entry is in the map but should not cause degraded status
    assert!(
        !is_agent_in_cooldown(agent),
        "expired cooldown should not be active"
    );

    // Pre-mark as degraded to verify it gets cleared
    mark_agent_degraded(agent);
    assert!(is_agent_degraded(agent));

    // Run refresh — should clear degraded because cooldown is expired
    let agents = vec![agent.to_string()];
    refresh_degraded_agents(&store, &agents, &|_| true, 6, 3).await;

    assert!(
        !is_agent_degraded(agent),
        "agent with only expired cooldown should NOT be marked degraded"
    );

    // Cleanup
    clear_agent_degraded(agent);
    {
        let mut map = cooldowns().lock().unwrap();
        map.remove(agent);
    }
}

#[serial(cooldown_state)]
#[test]
fn compute_backoff_grows_exponentially() {
    // count=0 or 1: base
    assert_eq!(compute_backoff(0, 300, 14400, true), 300);
    assert_eq!(compute_backoff(1, 300, 14400, true), 300);
    // count=2: base * 3
    assert_eq!(compute_backoff(2, 300, 14400, true), 900);
    // count=3: base * 9
    assert_eq!(compute_backoff(3, 300, 14400, true), 2700);
    // count=4: base * 27 = 8100 < 14400
    assert_eq!(compute_backoff(4, 300, 14400, true), 8100);
    // count=5: base * 81 = 24300 → capped at 14400
    assert_eq!(compute_backoff(5, 300, 14400, true), 14400);
}

#[serial(cooldown_state)]
#[test]
fn compute_backoff_extended_tier_1_applies_24h() {
    // With base=300, max=14400:
    // count=4: 8100s (not yet at max)
    // count=5: 14400s (first time at max, times_at_max=0, no extension)
    // count=6: 14400s (times_at_max=1, no extension yet, need >=2)
    // count=7: 86400s (times_at_max=2, extended tier 1: 6x = 24h)
    assert_eq!(
        compute_backoff(5, 300, 14400, true),
        14400,
        "first at max, not extended"
    );
    assert_eq!(
        compute_backoff(6, 300, 14400, true),
        14400,
        "times_at_max=1, not extended"
    );
    assert_eq!(
        compute_backoff(7, 300, 14400, true),
        86400,
        "times_at_max=2, extended 6x"
    );
    assert_eq!(
        compute_backoff(8, 300, 14400, true),
        86400,
        "times_at_max=3, still extended 6x"
    );
}

#[serial(cooldown_state)]
#[test]
fn compute_backoff_extended_tier_2_applies_48h() {
    // With base=300, max=14400:
    // count=10: times_at_max=5 (first_max=5, count=10), extended tier 2: 12x = 48h
    assert_eq!(
        compute_backoff(9, 300, 14400, true),
        86400,
        "times_at_max=4, still tier 1"
    );
    assert_eq!(
        compute_backoff(10, 300, 14400, true),
        172_800,
        "times_at_max=5, extended 12x"
    );
    assert_eq!(
        compute_backoff(15, 300, 14400, true),
        172_800,
        "times_at_max=10, still tier 2"
    );
    assert_eq!(
        compute_backoff(20, 300, 14400, true),
        172_800,
        "times_at_max=15, still tier 2"
    );
}

#[serial(cooldown_state)]
#[test]
fn compute_backoff_extended_tier_with_different_base_max() {
    // With base=60, max=3600:
    // count=3: 60*9=540 < 3600
    // count=4: 60*27=1620 < 3600
    // count=5: 60*81=4860 → capped at 3600
    // count=6: times_at_max=1, still 3600
    // count=7: times_at_max=2, extended 6x = 21600 (6h)
    assert_eq!(compute_backoff(5, 60, 3600, true), 3600, "first at max");
    assert_eq!(compute_backoff(6, 60, 3600, true), 3600, "times_at_max=1");
    assert_eq!(
        compute_backoff(7, 60, 3600, true),
        21600,
        "times_at_max=2, extended 6x"
    );
}

#[serial(cooldown_state)]
#[test]
fn compute_backoff_extended_tier_persistent_fails_example() {
    // Real-world example from issue: glm:haiku with 21 failures
    // With base=300, max=14400:
    // count=21 → times_at_max=16 (first_max=5), extended tier 2: 12x = 48h
    assert_eq!(
        compute_backoff(21, 300, 14400, true),
        172_800,
        "21 failures should get 48h extended cooldown"
    );

    // kimi:haiku with 36 failures
    assert_eq!(
        compute_backoff(36, 300, 14400, true),
        172_800,
        "36 failures should also get 48h extended cooldown"
    );
}

#[serial(cooldown_state)]
#[test]
fn compute_backoff_extended_tier_zero_base() {
    // Zero base should still work without panicking
    assert_eq!(compute_backoff(1, 0, 14400, true), 0);
    assert_eq!(compute_backoff(5, 0, 14400, true), 0);
    assert_eq!(compute_backoff(10, 0, 14400, true), 0);
}

#[serial(cooldown_state)]
#[tokio::test]
async fn refresh_degraded_agents_below_threshold_not_degraded() {
    super::reset_global_state().await;
    let store = test_store().await;
    let agent = "test_refresh_below_threshold";

    // Insert rate limit events below threshold
    for _ in 0..2 {
        store.record_rate_limit(agent, "rate", None).await.unwrap();
    }

    let agents = vec![agent.to_string()];
    // threshold=3, only 2 events — should NOT be degraded
    refresh_degraded_agents(&store, &agents, &|_| true, 6, 3).await;

    assert!(
        !is_agent_degraded(agent),
        "agent with rate limit count < threshold should not be degraded"
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn refresh_degraded_agents_ignores_error_limit_type() {
    super::reset_global_state().await;
    let store = test_store().await;
    let agent = "test_refresh_error_rows";

    for _ in 0..3 {
        store.record_rate_limit(agent, "error", None).await.unwrap();
    }

    let agents = vec![agent.to_string()];
    refresh_degraded_agents(&store, &agents, &|_| true, 6, 3).await;

    assert!(
        !is_agent_degraded(agent),
        "three error rows in 6 hours must not mark an agent degraded"
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn refresh_degraded_agents_marks_three_rate_rows() {
    super::reset_global_state().await;
    let store = test_store().await;
    let agent = "test_refresh_three_rate_rows";

    for _ in 0..3 {
        store.record_rate_limit(agent, "rate", None).await.unwrap();
    }

    let agents = vec![agent.to_string()];
    refresh_degraded_agents(&store, &agents, &|_| true, 6, 3).await;

    assert!(
        is_agent_degraded(agent),
        "three rate rows in 6 hours must mark an agent degraded"
    );
    assert_eq!(
        agent_degraded_reason(agent).as_deref(),
        Some("rate limit threshold exceeded")
    );
    let listed = list_degraded_agents();
    assert!(
        listed
            .iter()
            .any(|(a, r)| a == agent && r == "rate limit threshold exceeded"),
        "orch cooldown list must show the degraded reason, got {listed:?}"
    );

    clear_agent_degraded(agent);
}

#[serial(cooldown_state)]
#[tokio::test]
async fn refresh_degraded_agents_logs_reason_change() {
    super::reset_global_state().await;
    let store = test_store().await;
    let agent = "test_refresh_reason_change";
    *cooldown_store().lock().await = Some(Arc::clone(&store));

    set_agent_cooldown(agent, 600).await;
    let agents = vec![agent.to_string()];
    refresh_degraded_agents(&store, &agents, &|_| true, 6, 3).await;
    assert_eq!(
        agent_degraded_reason(agent).as_deref(),
        Some("agent in cooldown")
    );

    clear_cooldown(agent, &store).await;
    let after_clear = sqlite_datetime_offset(60);
    for _ in 0..3 {
        store
            .record_rate_limit_at(agent, "rate", None, &after_clear)
            .await
            .unwrap();
    }
    refresh_degraded_agents(&store, &agents, &|_| true, 6, 3).await;
    assert_eq!(
        agent_degraded_reason(agent).as_deref(),
        Some("rate limit threshold exceeded"),
        "degraded reason must update when the hold changes"
    );

    clear_agent_degraded(agent);
}

#[serial(cooldown_state)]
#[tokio::test]
async fn clear_un_degrades_on_next_health_check() {
    super::reset_global_state().await;
    let store = test_store().await;
    let agent = "test_clear_un_degrades";
    let before = sqlite_datetime_offset(-30);
    let after = sqlite_datetime_offset(60);

    for _ in 0..3 {
        store
            .record_rate_limit_at(agent, "rate", None, &before)
            .await
            .unwrap();
    }

    let agents = vec![agent.to_string()];
    refresh_degraded_agents(&store, &agents, &|_| true, 6, 3).await;
    assert!(
        is_agent_degraded(agent),
        "three rate rows must mark an agent degraded"
    );

    clear_cooldown(agent, &store).await;
    refresh_degraded_agents(&store, &agents, &|_| true, 6, 3).await;
    assert!(
        !is_agent_degraded(agent),
        "clear must un-degrade on the next health check"
    );

    store
        .record_rate_limit_at(agent, "rate", None, &after)
        .await
        .unwrap();
    refresh_degraded_agents(&store, &agents, &|_| true, 6, 3).await;
    assert!(
        !is_agent_degraded(agent),
        "one rate row after the mark must not degrade again"
    );

    for _ in 0..2 {
        store
            .record_rate_limit_at(agent, "rate", None, &after)
            .await
            .unwrap();
    }
    refresh_degraded_agents(&store, &agents, &|_| true, 6, 3).await;
    assert!(
        is_agent_degraded(agent),
        "three rate rows after the mark must degrade again"
    );

    clear_agent_degraded(agent);
}

#[serial(cooldown_state)]
#[tokio::test]
async fn clear_all_un_degrades_rate_limited_agent() {
    super::reset_global_state().await;
    let store = test_store().await;
    let agent = "test_clear_all_un_degrades";
    let before = sqlite_datetime_offset(-30);

    for _ in 0..3 {
        store
            .record_rate_limit_at(agent, "rate", None, &before)
            .await
            .unwrap();
    }

    let agents = vec![agent.to_string()];
    refresh_degraded_agents(&store, &agents, &|_| true, 6, 3).await;
    assert!(is_agent_degraded(agent));

    clear_cooldown("*", &store).await;
    refresh_degraded_agents(&store, &agents, &|_| true, 6, 3).await;
    assert!(
        !is_agent_degraded(agent),
        "clear --all must un-degrade an agent held only by rate rows"
    );
}

// ---- Exponential backoff integration tests ----

#[serial(cooldown_state)]
#[tokio::test]
async fn repeated_agent_failures_escalate_backoff() {
    super::reset_global_state().await;
    let store = test_store().await;
    *cooldown_store().lock().await = Some(Arc::clone(&store));

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

#[serial(cooldown_state)]
#[tokio::test]
async fn vendor_past_retry_falls_back_to_backoff() {
    super::reset_global_state().await;
    // If a vendor-provided "try again at" date is in the past, the
    // code must NOT early-return and must instead apply the normal
    // exponential backoff (increment failure count and set a future cooldown).
    let store = test_store().await;
    *cooldown_store().lock().await = Some(Arc::clone(&store));

    let agent = "test_vendor_past_retry";

    // Ensure no pre-existing cooldown
    {
        let mut map = cooldowns().lock().unwrap();
        map.remove(agent);
    }

    // A clearly past date that parse_retry_at can parse
    let past_msg = "You've hit the limit; try again at Jan 01, 2000 01:00 AM.";

    record_agent_failure_with_message(agent, past_msg).await;

    // After handling a past vendor timestamp, we should have applied
    // exponential backoff (base = BACKOFF_BASE_SECS) and a persisted
    // failure count of 1.
    assert!(
        is_agent_in_cooldown(agent),
        "agent should be in cooldown after failure"
    );

    let remaining = {
        let map = cooldowns().lock().unwrap();
        let entry = map.get(agent).expect("cooldown entry should exist");
        entry.cooldown_until - chrono::Utc::now().timestamp()
    };

    assert!(
        remaining >= BACKOFF_BASE_SECS - 5,
        "expected backoff of ~{BACKOFF_BASE_SECS}s when vendor timestamp is past, got {remaining}s"
    );

    // Verify the failure count was incremented and persisted to KV
    let kv_key = format!("{FAILURE_COUNT_PREFIX}{agent}");
    let stored = store
        .kv_get(&kv_key)
        .await
        .unwrap()
        .expect("failure count should be stored");
    assert_eq!(stored, "1");
}

fn window(status: &str, resets_at: Option<i64>) -> RateLimitWindow {
    RateLimitWindow {
        status: status.to_string(),
        window_type: "five_hour".to_string(),
        utilization: Some(1.0),
        resets_at,
    }
}

#[serial(cooldown_state)]
#[tokio::test]
async fn rejected_window_cools_agent_until_reset() {
    super::reset_global_state().await;
    let store = test_store().await;
    *cooldown_store().lock().await = Some(Arc::clone(&store));

    let agent = "test_window_rejected_agent";
    let reset = chrono::Utc::now().timestamp() + 7200;
    record_rate_limit_window(agent, &window("rejected", Some(reset))).await;

    assert_eq!(cooldown_until(agent), Some(reset));
    let stored = list_rate_limit_windows(&store).await;
    assert_eq!(
        stored,
        vec![(agent.to_string(), window("rejected", Some(reset)))]
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn allowed_window_changes_no_cooldown() {
    super::reset_global_state().await;
    let store = test_store().await;
    *cooldown_store().lock().await = Some(Arc::clone(&store));

    let agent = "test_window_allowed_agent";
    let reset = chrono::Utc::now().timestamp() + 7200;
    record_rate_limit_window(agent, &window("allowed", Some(reset))).await;
    record_rate_limit_window(agent, &window("allowed_warning", Some(reset))).await;

    assert_eq!(cooldown_until(agent), None);
    assert_eq!(list_rate_limit_windows(&store).await.len(), 1);
}

#[serial(cooldown_state)]
#[tokio::test]
async fn record_agent_success_resets_backoff() {
    super::reset_global_state().await;
    let store = test_store().await;
    *cooldown_store().lock().await = Some(Arc::clone(&store));

    let agent = "test_success_reset_agent";
    let model = "test_model";

    // Accumulate 3 failures to escalate the backoff
    for _ in 0..3 {
        let _ = read_and_increment_failure_count(Some(&store), agent).await;
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

#[serial(cooldown_state)]
#[tokio::test]
async fn record_agent_success_without_model_skips_empty_model_key() {
    super::reset_global_state().await;
    let store = test_store().await;
    *cooldown_store().lock().await = Some(Arc::clone(&store));

    let agent = "test_success_without_model";

    record_agent_success(agent, "").await;

    let agent_kv_key = format!("{FAILURE_COUNT_PREFIX}{agent}");
    let agent_count = store
        .kv_get(&agent_kv_key)
        .await
        .unwrap()
        .expect("agent failure count should be stored");
    assert_eq!(agent_count, "0");

    let empty_model_kv_key = format!("{FAILURE_COUNT_PREFIX}{agent}:");
    let empty_model_count = store.kv_get(&empty_model_kv_key).await.unwrap();
    assert!(
        empty_model_count.is_none(),
        "success without model should not create an empty model failure count key"
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn clear_cooldown_resets_failure_counts() {
    super::reset_global_state().await;
    let store = test_store().await;
    *cooldown_store().lock().await = Some(Arc::clone(&store));

    let agent = "test_clear_fc_agent";

    // Accumulate failures
    for _ in 0..5 {
        let _ = read_and_increment_failure_count(Some(&store), agent).await;
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

#[serial(cooldown_state)]
#[tokio::test]
async fn clear_all_cooldowns_resets_all_failure_counts() {
    super::reset_global_state().await;
    let store = test_store().await;
    *cooldown_store().lock().await = Some(Arc::clone(&store));

    let agents = ["test_clear_all_a", "test_clear_all_b"];
    for agent in &agents {
        let _ = read_and_increment_failure_count(Some(&store), agent).await;
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

#[serial(cooldown_state)]
#[test]
fn compute_backoff_saturates_on_large_counts() {
    // Very large count should not overflow — extended tier applies 48h
    assert_eq!(
        compute_backoff(100, BACKOFF_BASE_SECS, BACKOFF_MAX_SECS, true),
        BACKOFF_MAX_SECS * 12
    );
    assert_eq!(
        compute_backoff(u64::from(u32::MAX), 300, 14400, true),
        172_800
    );
    assert_eq!(compute_backoff(u64::MAX, 300, 14400, true), 172_800);
}

#[serial(cooldown_state)]
#[test]
fn compute_backoff_zero_base_returns_zero() {
    assert_eq!(compute_backoff(1, 0, 14400, true), 0);
    assert_eq!(compute_backoff(5, 0, 14400, true), 0);
}

// ---- GitHub 5xx circuit breaker tests ----

#[serial(cooldown_state)]
#[tokio::test]
async fn github_circuit_stays_closed_below_threshold() {
    super::reset_global_state().await;
    // Reset circuit breaker state
    {
        let mut open = github_5xx_circuit_open().lock().unwrap();
        *open = None;
        let mut ts = github_5xx_timestamps().lock().unwrap();
        ts.clear();
        // Also clear any generic cooldown map entry for the github 5xx
        // circuit so tests are isolated from other tests that may have
        // set the generic cooldown via set_agent_cooldown("github:5xx", ...).
        let mut map = cooldown_lock();
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

#[serial(cooldown_state)]
#[tokio::test]
async fn github_circuit_trips_at_threshold() {
    super::reset_global_state().await;
    // Reset circuit breaker state
    {
        let mut open = github_5xx_circuit_open_lock();
        *open = None;
        let mut ts = github_5xx_timestamps_lock();
        ts.clear();
        let mut map = cooldown_lock();
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

#[serial(cooldown_state)]
#[tokio::test]
async fn github_circuit_cannot_double_trip() {
    super::reset_global_state().await;
    // Reset circuit breaker state
    {
        let mut open = github_5xx_circuit_open_lock();
        *open = None;
        let mut ts = github_5xx_timestamps_lock();
        ts.clear();
        let mut map = cooldown_lock();
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

#[serial(cooldown_state)]
#[tokio::test]
async fn github_circuit_recovers_after_cooldown() {
    super::reset_global_state().await;
    // Reset circuit breaker state
    {
        let mut open = github_5xx_circuit_open_lock();
        *open = None;
        let mut ts = github_5xx_timestamps_lock();
        ts.clear();
        let mut map = cooldown_lock();
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

#[serial(cooldown_state)]
#[tokio::test]
async fn github_circuit_sliding_window_ages_out() {
    super::reset_global_state().await;
    // Reset circuit breaker state
    {
        let mut open = github_5xx_circuit_open_lock();
        *open = None;
        let mut ts = github_5xx_timestamps_lock();
        ts.clear();
        let mut map = cooldown_lock();
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
