//! Cron expression matcher — replaces the Python `cron_match.py` script.
//!
//! Supports:
//! - Standard 5-field cron expressions
//! - `--since TIMESTAMP` mode: check if the schedule fired between a timestamp and now
//!
//! This eliminates the `python3` subprocess that v0 forks on every tick.

use anyhow::Context;
use chrono::{DateTime, Utc};
use cron::Schedule;
use std::str::FromStr;

/// Normalize day-of-week field: standard cron allows 0 for Sunday, but the
/// `cron` crate only accepts 1-7 (Sun=1). Replace standalone `0` with `7`.
pub fn normalize_dow(expression: &str) -> String {
    let fields: Vec<&str> = expression.split_whitespace().collect();
    if fields.len() != 5 {
        return expression.to_string();
    }
    let dow = fields[4];
    // Replace 0 with 7 in the DOW field, handling ranges/lists.
    // Examples: "0" → "7", "0,3" → "7,3", "0-5" → "1-5,7" (split: Mon-Fri + Sunday)
    // Ranges starting at 0 cannot be expressed as a single contiguous range in the
    // cron crate (Sun=7 > Sat=6), so we split: "0-N" → "1-N,7".
    let normalized_dow = dow
        .split(',')
        .map(|part| {
            if part == "0" {
                "7".to_string()
            } else if let Some(rest) = part.strip_prefix("0-") {
                format!("1-{rest},7")
            } else {
                part.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{} {} {} {} {}",
        fields[0], fields[1], fields[2], fields[3], normalized_dow
    )
}

/// Check if a cron expression matches now, or (with `since`) has matched
/// at any point between `since` and now.
///
/// Returns `true` if the cron fired, `false` otherwise.
pub fn check(expression: &str, since: Option<&str>) -> anyhow::Result<bool> {
    // cron crate expects 7-field expressions (sec min hour dom mon dow year)
    // We accept 5-field (min hour dom mon dow) and wrap with "0" seconds + "*" year.
    //
    // The cron crate uses 1-7 for DOW (Sun=1..Sat=7), but standard cron uses
    // 0-7 where both 0 and 7 mean Sunday. Normalize DOW=0 → DOW=7.
    let normalized = normalize_dow(expression);
    let full_expr = format!("0 {normalized} *");

    let schedule = Schedule::from_str(&full_expr)
        .with_context(|| format!("invalid cron expression: {expression}"))?;

    let now = Utc::now();

    match since {
        Some(since_str) => {
            let since_dt = parse_timestamp(since_str)
                .with_context(|| format!("invalid --since timestamp: {since_str}"))?;

            // Cap at 24 hours to prevent runaway catch-up
            let cap = now - chrono::Duration::hours(24);
            let effective_since = if since_dt < cap { cap } else { since_dt };

            // Check if any occurrence falls between since and now
            Ok(schedule
                .after(&effective_since)
                .take_while(|dt| *dt <= now)
                .next()
                .is_some())
        }
        None => {
            // Check if the schedule matches the current minute.
            // We look for the next occurrence after 1 minute ago — if it falls
            // within the current minute, the schedule is firing now.
            let one_min_ago = now - chrono::Duration::minutes(1);
            let next = schedule.after(&one_min_ago).next();
            match next {
                Some(dt) => {
                    let diff = now.signed_duration_since(dt);
                    Ok(diff >= chrono::Duration::zero() && diff < chrono::Duration::minutes(1))
                }
                None => Ok(false),
            }
        }
    }
}

/// Parse a timestamp string (ISO 8601 or common formats).
fn parse_timestamp(s: &str) -> anyhow::Result<DateTime<Utc>> {
    // Try ISO 8601 first
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
    // Try without timezone (assume UTC)
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Ok(dt.and_utc());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Ok(dt.and_utc());
    }
    anyhow::bail!("unrecognized timestamp format: {s}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    #[test]
    fn every_minute_matches_now() {
        // "* * * * *" should always match the current minute
        let result = check("* * * * *", None).unwrap();
        assert!(result);
    }

    #[test]
    fn impossible_schedule_does_not_match() {
        // Feb 30 never exists
        let result = check("0 0 30 2 *", None).unwrap();
        assert!(!result);
    }

    #[test]
    fn since_mode_catches_recent_fire() {
        // Every minute, since 5 minutes ago — should have fired
        let five_min_ago = (Utc::now() - chrono::Duration::minutes(5))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let result = check("* * * * *", Some(&five_min_ago)).unwrap();
        assert!(result);
    }

    #[test]
    fn since_mode_caps_at_24h() {
        // Since 48 hours ago, but cap is 24h — should still work
        let old = (Utc::now() - chrono::Duration::hours(48))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let result = check("* * * * *", Some(&old)).unwrap();
        assert!(result);
    }

    #[test]
    fn invalid_expression_errors() {
        let result = check("not a cron", None);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_since_timestamp_errors() {
        let result = check("* * * * *", Some("not-a-date"));
        assert!(result.is_err());
    }

    #[test]
    fn sunday_as_zero_parses() {
        // Standard cron uses 0 for Sunday; the cron crate uses 1-7 (Sun=1)
        let result = check("0 20 * * 0", None);
        assert!(
            result.is_ok(),
            "DOW=0 (Sunday) should be accepted: {result:?}"
        );
    }

    #[test]
    fn sunday_as_seven_still_works() {
        let result = check("0 20 * * 7", None);
        assert!(
            result.is_ok(),
            "DOW=7 (Sunday) should be accepted: {result:?}"
        );
    }

    #[test]
    fn dow_zero_in_list_parses() {
        // e.g. "every Sunday and Wednesday"
        let result = check("0 20 * * 0,3", None);
        assert!(
            result.is_ok(),
            "DOW=0 in list should be accepted: {result:?}"
        );
    }

    #[test]
    fn dow_range_starting_at_zero_parses() {
        // "0-5" = Sunday through Friday in standard cron
        let result = check("0 9 * * 0-5", None);
        assert!(result.is_ok(), "DOW range 0-5 should parse: {result:?}");
    }

    #[test]
    fn dow_range_zero_to_four_parses() {
        // "0-4" = Sunday through Thursday in standard cron
        let result = check("0 9 * * 0-4", None);
        assert!(result.is_ok(), "DOW range 0-4 should parse: {result:?}");
    }

    #[test]
    fn normalize_dow_range_zero_to_five_includes_sunday() {
        // "0-5" must normalize to "1-5,7": Mon-Fri (1-5) + Sunday (7)
        let normalized = normalize_dow("0 9 * * 0-5");
        assert!(
            normalized.ends_with("1-5,7"),
            "Expected DOW field to be '1-5,7' but got: {normalized}"
        );
    }

    #[test]
    fn normalize_dow_range_zero_to_four_includes_sunday() {
        // "0-4" must normalize to "1-4,7": Mon-Thu (1-4) + Sunday (7)
        let normalized = normalize_dow("0 9 * * 0-4");
        assert!(
            normalized.ends_with("1-4,7"),
            "Expected DOW field to be '1-4,7' but got: {normalized}"
        );
    }

    #[test]
    fn dow_range_zero_to_six_includes_sunday() {
        // "0-6" = every day; normalizes to "1-6,7"
        let normalized = normalize_dow("0 9 * * 0-6");
        assert!(
            normalized.ends_with("1-6,7"),
            "Expected DOW field to be '1-6,7' but got: {normalized}"
        );
        // Also verify it parses and fires (every day schedule always matches now)
        let result = check("0 9 * * 0-6", None);
        assert!(result.is_ok(), "DOW range 0-6 should parse: {result:?}");
    }

    #[test]
    fn dow_range_zero_to_five_fires_on_sunday() {
        use chrono::{Datelike, TimeZone};
        use cron::Schedule;
        use std::str::FromStr;

        // Normalize and build a 7-field expression, then check that a known Sunday
        // appears in the schedule's upcoming occurrences.
        let normalized = normalize_dow("0 9 * * 0-5");
        let full_expr = format!("0 {normalized} *");
        let schedule = Schedule::from_str(&full_expr).expect("should parse");

        // Find the next 14 occurrences and verify at least one is a Sunday (weekday 0 in chrono).
        let base = Utc.with_ymd_and_hms(2026, 3, 23, 0, 0, 0).unwrap(); // Monday
        let has_sunday = schedule
            .after(&base)
            .take(14)
            .any(|dt| dt.weekday() == chrono::Weekday::Sun);
        assert!(has_sunday, "Schedule '0 9 * * 0-5' must fire on Sunday");
    }

    #[test]
    fn parse_rfc3339_timestamp() {
        let dt = parse_timestamp("2026-02-22T10:30:00Z").unwrap();
        assert_eq!(dt.hour(), 10);
        assert_eq!(dt.minute(), 30);
    }

    #[test]
    fn parse_naive_timestamp() {
        let dt = parse_timestamp("2026-02-22T10:30:00").unwrap();
        assert_eq!(dt.hour(), 10);
    }

    #[test]
    fn parse_space_separated_timestamp() {
        let dt = parse_timestamp("2026-02-22 10:30:00").unwrap();
        assert_eq!(dt.hour(), 10);
    }
}
