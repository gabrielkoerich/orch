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

/// Check if a cron expression matches now, or (with `since`) has matched
/// at any point between `since` and now.
///
/// Returns `true` if the cron fired, `false` otherwise.
pub fn check(expression: &str, since: Option<&str>) -> anyhow::Result<bool> {
    // cron crate expects 7-field expressions (sec min hour dom mon dow year)
    // We accept 5-field (min hour dom mon dow) and wrap with "0" seconds + "*" year
    let full_expr = format!("0 {expression} *");

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
