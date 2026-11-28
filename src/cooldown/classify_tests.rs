use super::*;
use chrono::Datelike;
use chrono::TimeZone;
use serial_test::serial;

#[serial(cooldown_state)]
#[test]
fn parse_retry_at_codex_format() {
    let msg = "You've hit your usage limit. Upgrade to Pro, visit https://chatgpt.com/codex/settings/usage to purchase more credits or try again at Mar 26th, 2026 5:55 AM.";
    let ts = parse_retry_at(msg);
    assert!(ts.is_some(), "should parse codex retry-at date");

    // The parsed timestamp must equal "Mar 26 2026 05:55 local" in UTC.
    // We reconstruct what the function should produce and compare directly.
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
    assert_eq!(dt.month(), 3);
    assert_eq!(dt.day(), 26);
}

#[serial(cooldown_state)]
#[test]
fn parse_retry_at_hour_only_and_month_day() {
    let now = chrono::Utc::now().timestamp();
    for msg in [
        "You've hit your session limit · resets 8am (America/Sao_Paulo)",
        "You've hit your session limit · resets 6pm (America/Sao_Paulo)",
        "You've hit your session limit · resets 12am (America/Sao_Paulo)",
        "You've hit your weekly limit · resets Jun 9 at 1am (America/Sao_Paulo)",
    ] {
        let ts = parse_retry_at(msg).unwrap_or_else(|| panic!("should parse: {msg}"));
        assert!(ts > now, "{msg}");
    }
}

#[serial(cooldown_state)]
#[test]
fn parse_retry_at_weekly_resets_without_year() {
    let t = chrono::Local::now() + chrono::Duration::days(2);
    let msg = format!(
        "You've hit your weekly limit · resets {} {} at 1am (America/Sao_Paulo)",
        t.format("%b"),
        t.day()
    );
    let ts = parse_retry_at(&msg).expect("should parse year-less reset date");
    let remaining = ts - chrono::Utc::now().timestamp();
    assert!(
        remaining > 3600 && remaining < 3 * 86400,
        "expected ~2d retry-at, got {remaining}s"
    );
}

#[serial(cooldown_state)]
#[test]
fn parse_retry_at_no_date() {
    assert!(parse_retry_at("generic rate limit error").is_none());
    assert!(parse_retry_at("").is_none());
}

#[serial(cooldown_state)]
#[test]
fn parse_retry_at_glm_reset_format_with_seconds() {
    let msg = "Weekly/Monthly Limit Exhausted. Your limit will reset at 2026-04-23 18:33:48";
    let ts = parse_retry_at(msg).expect("should parse GLM reset-at date");

    let naive = chrono::NaiveDateTime::new(
        chrono::NaiveDate::from_ymd_opt(2026, 4, 23).expect("valid date"),
        chrono::NaiveTime::from_hms_opt(18, 33, 48).expect("valid time"),
    );
    let expected_utc = match chrono::Local.from_local_datetime(&naive) {
        chrono::LocalResult::Single(local_dt) => local_dt.with_timezone(&chrono::Utc),
        chrono::LocalResult::Ambiguous(earliest, _) => earliest.with_timezone(&chrono::Utc),
        chrono::LocalResult::None => naive.and_utc(),
    };

    assert_eq!(
        ts,
        expected_utc.timestamp(),
        "timestamp should reflect local-timezone interpretation of vendor reset date"
    );
}

#[serial(cooldown_state)]
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

#[serial(cooldown_state)]
#[test]
fn parse_relative_usage_window_extracts_kimi_five_hour_window() {
    let msg = "Failed to authenticate. API Error: 403 You've reached your 5-hour usage limit. Your quota will reset when the current 5-hour window expires.";
    assert_eq!(
        parse_relative_usage_window(msg),
        Some(5 * 3600),
        "should extract the 5-hour window as seconds"
    );
}

#[serial(cooldown_state)]
#[test]
fn parse_relative_usage_window_handles_common_durations() {
    assert_eq!(
        parse_relative_usage_window("You've reached your hourly usage limit."),
        Some(3600)
    );
    assert_eq!(
        parse_relative_usage_window("You've reached your daily usage limit."),
        Some(86400)
    );
    assert_eq!(
        parse_relative_usage_window("You've reached your weekly limit."),
        Some(604_800)
    );
    assert_eq!(
        parse_relative_usage_window("You've reached your 30-minute usage limit."),
        Some(1800)
    );
    assert_eq!(
        parse_relative_usage_window("You've reached your 2-day quota."),
        Some(172_800)
    );
}

#[serial(cooldown_state)]
#[test]
fn parse_relative_usage_window_returns_none_without_usage_context() {
    // A bare rate-limit message with a number should not be interpreted as a
    // relative usage window unless usage/quota/window context is present.
    assert!(parse_relative_usage_window("429 Too Many Requests").is_none());
    assert!(parse_relative_usage_window("rate limit exceeded").is_none());
    assert!(parse_relative_usage_window("").is_none());
}

#[serial(cooldown_state)]
#[test]
fn detect_credit_exhaustion_out_of_credits() {
    let msg = "Error: API error - {\"error\":{\"message\":\"You exceeded your current quota\",\"type\":\"insufficient_quota\",\"param\":null,\"code\":\"insufficient_quota\"}}";
    assert_eq!(
        detect_credit_exhaustion(msg),
        Some(CreditExhaustionReason::OutOfCredits)
    );
}

#[serial(cooldown_state)]
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

#[serial(cooldown_state)]
#[test]
fn detect_credit_exhaustion_org_level_disabled() {
    let msg = "Error: API error - {\"error\":{\"message\":\"Organization has been disabled\",\"type\":\"org_disabled\",\"code\":\"overageDisabledReason:org_level_disabled\"}}";
    assert_eq!(
        detect_credit_exhaustion(msg),
        Some(CreditExhaustionReason::OrgLevelDisabled)
    );
}

#[serial(cooldown_state)]
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

#[serial(cooldown_state)]
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

#[serial(cooldown_state)]
#[test]
fn detect_credit_exhaustion_glm_insufficient_balance() {
    // GLM/ZhipuAI 429: "Insufficient balance or no resource package. Please recharge."
    assert_eq!(
        detect_credit_exhaustion(
            "API Error: Request rejected (429) · Insufficient balance or no resource package. Please recharge."
        ),
        Some(CreditExhaustionReason::OutOfCredits)
    );
    assert_eq!(
        detect_credit_exhaustion("Insufficient balance"),
        Some(CreditExhaustionReason::OutOfCredits)
    );
    assert_eq!(
        detect_credit_exhaustion("please recharge your account"),
        Some(CreditExhaustionReason::OutOfCredits)
    );
    assert_eq!(
        detect_credit_exhaustion("no resource package available"),
        Some(CreditExhaustionReason::OutOfCredits)
    );
}

#[serial(cooldown_state)]
#[test]
fn detect_credit_exhaustion_none_for_regular_rate_limit() {
    assert!(detect_credit_exhaustion("rate limit exceeded").is_none());
    assert!(detect_credit_exhaustion("too many requests").is_none());
    assert!(detect_credit_exhaustion("429 Too Many Requests").is_none());
    assert!(detect_credit_exhaustion("you've hit your usage limit").is_none());
    assert!(
        detect_credit_exhaustion("You've hit your usage limit, try again in a few minutes.")
            .is_none()
    );
}

#[serial(cooldown_state)]
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
    assert_eq!(
        detect_credit_exhaustion(
            "Weekly/Monthly Limit Exhausted. Your limit will reset at 2026-04-23 18:33:48"
        ),
        Some(CreditExhaustionReason::BillingCycleExhausted)
    );
}

#[serial(cooldown_state)]
#[test]
fn detect_credit_exhaustion_billing_cycle_minimax() {
    assert_eq!(
        detect_credit_exhaustion(
            "minimax rate limit: API Error: Request rejected (429) · Token Plan usage limit reached: Upgrade your Token Plan or purchase Credits for more usage."
        ),
        Some(CreditExhaustionReason::BillingCycleExhausted)
    );
    assert_eq!(
        detect_credit_exhaustion("Upgrade your Token Plan"),
        Some(CreditExhaustionReason::BillingCycleExhausted)
    );
    assert_eq!(
        detect_credit_exhaustion("Token Plan usage limit reached"),
        Some(CreditExhaustionReason::BillingCycleExhausted)
    );
    assert_eq!(
        detect_credit_exhaustion("purchase Credits for more usage"),
        Some(CreditExhaustionReason::BillingCycleExhausted)
    );
    assert_eq!(
        detect_credit_exhaustion(
            "You've hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), visit https://chatgpt.com/codex/settings/usage to purchase more credits"
        ),
        Some(CreditExhaustionReason::BillingCycleExhausted)
    );
}

#[serial(cooldown_state)]
#[test]
fn detect_credit_exhaustion_outcome_kimi_billing_cycle() {
    // Regression test for issue #3529: the exact Kimi billing-cycle exhaustion
    // message from task_runs (tasks 156734/156678) must resolve to the
    // "billing_cycle_exhausted" outcome string, not fall through to a generic
    // rate-limit/auth classification.
    let msg = "Failed to authenticate. API Error: 403 You've reached your usage limit for \
                    this billing cycle. Your quota will be refreshed soon.";
    assert_eq!(
        detect_credit_exhaustion_outcome(msg),
        Some("billing_cycle_exhausted")
    );
}

#[serial(cooldown_state)]
#[test]
fn detect_credit_exhaustion_outcome_maps_all_reasons() {
    assert_eq!(
        detect_credit_exhaustion_outcome("out of credits"),
        Some("credit_exhausted")
    );
    assert_eq!(
        detect_credit_exhaustion_outcome("organization has been disabled"),
        Some("org_level_disabled")
    );
    assert_eq!(
        detect_credit_exhaustion_outcome("monthly quota exceeded"),
        Some("billing_cycle_exhausted")
    );
}

#[test]
fn detect_credit_exhaustion_outcome_returns_none_for_unrelated_errors() {
    assert_eq!(
        detect_credit_exhaustion_outcome("429 Too Many Requests"),
        None
    );
    assert_eq!(detect_credit_exhaustion_outcome("connection reset"), None);
}

#[serial(cooldown_state)]
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
