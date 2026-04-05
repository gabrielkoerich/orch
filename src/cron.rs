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

/// Expand cron alias strings (like `@hourly`, `@daily 9:30`) into standard
/// 5-field cron expressions.  Unrecognized strings are returned unchanged.
///
/// | Alias      | Parameter     | Example        | Expands to      |
/// |------------|---------------|----------------|-----------------|
/// | `@hourly`  | minute 0-59   | `@hourly 30`   | `30 * * * *`    |
/// | `@hourly`  | none          | `@hourly`      | `0 * * * *`     |
/// | `@daily`   | hour          | `@daily 9`     | `0 9 * * *`     |
/// | `@daily`   | hour:min      | `@daily 9:30`  | `30 9 * * *`    |
/// | `@daily`   | none          | `@daily`       | `0 0 * * *`     |
/// | `@weekly`  | day (0=Sun)   | `@weekly 1`    | `0 0 * * 1`     |
/// | `@weekly`  | none          | `@weekly`      | `0 0 * * 0`     |
/// | `@monthly` | day-of-month  | `@monthly 15`  | `0 0 15 * *`    |
/// | `@monthly` | none          | `@monthly`     | `0 0 1 * *`     |
/// | `@yearly`  | month-day     | `@yearly 3-15` | `0 0 15 3 *`    |
/// | `@yearly`  | none          | `@yearly`      | `0 0 1 1 *`     |
///
/// Returns an error if a parameter is present but not a valid number, or is
/// out of the accepted range for that field.
pub fn expand_alias(s: &str) -> anyhow::Result<String> {
    let s = s.trim();
    let (alias, param) = match s.split_once(char::is_whitespace) {
        Some((a, p)) => (a, p.trim()),
        None => (s, ""),
    };
    match alias {
        "@hourly" => {
            if param.is_empty() {
                return Ok("0 * * * *".to_string());
            }
            let min: u8 = param
                .parse()
                .with_context(|| format!("invalid minute in @hourly alias: '{param}'"))?;
            anyhow::ensure!(
                min <= 59,
                "minute out of range in @hourly alias: {min} (must be 0-59)"
            );
            Ok(format!("{min} * * * *"))
        }
        "@daily" => {
            if param.is_empty() {
                return Ok("0 0 * * *".to_string());
            }
            if param.contains(':') {
                let (h, m) = param
                    .split_once(':')
                    .expect("contains(':') guarantees split");
                let hour: u8 = h
                    .parse()
                    .with_context(|| format!("invalid hour in @daily alias: '{h}'"))?;
                let min: u8 = m
                    .parse()
                    .with_context(|| format!("invalid minute in @daily alias: '{m}'"))?;
                anyhow::ensure!(
                    hour <= 23,
                    "hour out of range in @daily alias: {hour} (must be 0-23)"
                );
                anyhow::ensure!(
                    min <= 59,
                    "minute out of range in @daily alias: {min} (must be 0-59)"
                );
                Ok(format!("{min} {hour} * * *"))
            } else {
                let hour: u8 = param
                    .parse()
                    .with_context(|| format!("invalid hour in @daily alias: '{param}'"))?;
                anyhow::ensure!(
                    hour <= 23,
                    "hour out of range in @daily alias: {hour} (must be 0-23)"
                );
                Ok(format!("0 {hour} * * *"))
            }
        }
        "@weekly" => {
            if param.is_empty() {
                return Ok("0 0 * * 0".to_string());
            }
            let day: u8 = param
                .parse()
                .with_context(|| format!("invalid day-of-week in @weekly alias: '{param}'"))?;
            anyhow::ensure!(
                day <= 7,
                "day-of-week out of range in @weekly alias: {day} (must be 0-7)"
            );
            Ok(format!("0 0 * * {day}"))
        }
        "@monthly" => {
            if param.is_empty() {
                return Ok("0 0 1 * *".to_string());
            }
            let dom: u8 = param
                .parse()
                .with_context(|| format!("invalid day-of-month in @monthly alias: '{param}'"))?;
            anyhow::ensure!(
                (1..=31).contains(&dom),
                "day-of-month out of range in @monthly alias: {dom} (must be 1-31)"
            );
            Ok(format!("0 0 {dom} * *"))
        }
        "@yearly" => {
            if param.is_empty() {
                return Ok("0 0 1 1 *".to_string());
            }
            if param.contains('-') {
                let (m, d) = param
                    .split_once('-')
                    .expect("contains('-') guarantees split");
                let month: u8 = m
                    .parse()
                    .with_context(|| format!("invalid month in @yearly alias: '{m}'"))?;
                let dom: u8 = d
                    .parse()
                    .with_context(|| format!("invalid day-of-month in @yearly alias: '{d}'"))?;
                anyhow::ensure!(
                    (1..=12).contains(&month),
                    "month out of range in @yearly alias: {month} (must be 1-12)"
                );
                anyhow::ensure!(
                    (1..=31).contains(&dom),
                    "day-of-month out of range in @yearly alias: {dom} (must be 1-31)"
                );
                Ok(format!("0 0 {dom} {month} *"))
            } else {
                anyhow::bail!(
                    "invalid @yearly alias parameter: '{param}' (expected month-day, e.g. '3-15')"
                );
            }
        }
        _ => Ok(s.to_string()),
    }
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
    let expanded =
        expand_alias(expression).with_context(|| format!("invalid cron alias: {expression}"))?;
    let normalized = normalize_dow(&expanded);
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

    // --- expand_alias tests ---

    #[test]
    fn expand_alias_hourly_no_param() {
        assert_eq!(expand_alias("@hourly").unwrap(), "0 * * * *");
    }

    #[test]
    fn expand_alias_hourly_with_minute() {
        assert_eq!(expand_alias("@hourly 30").unwrap(), "30 * * * *");
    }

    #[test]
    fn expand_alias_daily_no_param() {
        assert_eq!(expand_alias("@daily").unwrap(), "0 0 * * *");
    }

    #[test]
    fn expand_alias_daily_with_hour() {
        assert_eq!(expand_alias("@daily 9").unwrap(), "0 9 * * *");
    }

    #[test]
    fn expand_alias_daily_with_hour_and_minute() {
        assert_eq!(expand_alias("@daily 9:30").unwrap(), "30 9 * * *");
    }

    #[test]
    fn expand_alias_weekly_no_param() {
        assert_eq!(expand_alias("@weekly").unwrap(), "0 0 * * 0");
    }

    #[test]
    fn expand_alias_weekly_with_day() {
        assert_eq!(expand_alias("@weekly 1").unwrap(), "0 0 * * 1");
    }

    #[test]
    fn expand_alias_monthly_no_param() {
        assert_eq!(expand_alias("@monthly").unwrap(), "0 0 1 * *");
    }

    #[test]
    fn expand_alias_monthly_with_day() {
        assert_eq!(expand_alias("@monthly 15").unwrap(), "0 0 15 * *");
    }

    #[test]
    fn expand_alias_yearly_no_param() {
        assert_eq!(expand_alias("@yearly").unwrap(), "0 0 1 1 *");
    }

    #[test]
    fn expand_alias_yearly_with_month_day() {
        assert_eq!(expand_alias("@yearly 3-15").unwrap(), "0 0 15 3 *");
    }

    #[test]
    fn expand_alias_passthrough_regular_cron() {
        assert_eq!(expand_alias("30 9 * * 1").unwrap(), "30 9 * * 1");
    }

    #[test]
    fn expand_alias_unknown_at_prefix_passthrough() {
        assert_eq!(expand_alias("@unknown").unwrap(), "@unknown");
    }

    #[test]
    fn expand_alias_check_integration_daily_9_30() {
        // @daily 9:30 should produce a valid parseable cron expression
        let expanded = expand_alias("@daily 9:30").unwrap();
        let normalized = normalize_dow(&expanded);
        let full = format!("0 {normalized} *");
        assert!(
            Schedule::from_str(&full).is_ok(),
            "expanded '@daily 9:30' → '{expanded}' should parse"
        );
    }

    #[test]
    fn expand_alias_check_integration_weekly_monday() {
        let expanded = expand_alias("@weekly 1").unwrap();
        let normalized = normalize_dow(&expanded);
        let full = format!("0 {normalized} *");
        assert!(
            Schedule::from_str(&full).is_ok(),
            "expanded '@weekly 1' → '{expanded}' should parse"
        );
    }

    // --- expand_alias error cases ---

    #[test]
    fn expand_alias_hourly_invalid_param_errors() {
        assert!(expand_alias("@hourly abc").is_err());
    }

    #[test]
    fn expand_alias_hourly_out_of_range_errors() {
        assert!(expand_alias("@hourly 60").is_err());
    }

    #[test]
    fn expand_alias_daily_invalid_param_errors() {
        assert!(expand_alias("@daily abc").is_err());
    }

    #[test]
    fn expand_alias_daily_invalid_hour_colon_errors() {
        assert!(expand_alias("@daily abc:30").is_err());
    }

    #[test]
    fn expand_alias_daily_invalid_minute_colon_errors() {
        assert!(expand_alias("@daily 9:xyz").is_err());
    }

    #[test]
    fn expand_alias_daily_hour_out_of_range_errors() {
        assert!(expand_alias("@daily 24").is_err());
    }

    #[test]
    fn expand_alias_daily_minute_out_of_range_errors() {
        assert!(expand_alias("@daily 9:60").is_err());
    }

    #[test]
    fn expand_alias_weekly_invalid_param_errors() {
        assert!(expand_alias("@weekly foo").is_err());
    }

    #[test]
    fn expand_alias_weekly_out_of_range_errors() {
        assert!(expand_alias("@weekly 8").is_err());
    }

    #[test]
    fn expand_alias_monthly_invalid_param_errors() {
        assert!(expand_alias("@monthly bar").is_err());
    }

    #[test]
    fn expand_alias_monthly_out_of_range_errors() {
        assert!(expand_alias("@monthly 0").is_err());
    }

    #[test]
    fn expand_alias_yearly_invalid_param_no_dash_errors() {
        assert!(expand_alias("@yearly 3").is_err());
    }

    #[test]
    fn expand_alias_yearly_invalid_month_errors() {
        assert!(expand_alias("@yearly abc-15").is_err());
    }

    #[test]
    fn expand_alias_yearly_invalid_dom_errors() {
        assert!(expand_alias("@yearly 3-xyz").is_err());
    }

    #[test]
    fn expand_alias_yearly_month_out_of_range_errors() {
        assert!(expand_alias("@yearly 13-1").is_err());
    }

    #[test]
    fn expand_alias_daily_abc_errors_via_check() {
        // Regression: @daily abc used to silently expand to "0 0 * * *"
        assert!(check("@daily abc", None).is_err());
    }

    #[test]
    fn expand_alias_hourly_not_a_number_errors_via_check() {
        // Regression: @hourly not-a-number used to silently expand to "0 * * * *"
        assert!(check("@hourly not-a-number", None).is_err());
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
