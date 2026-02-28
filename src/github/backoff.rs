//! GitHub API rate-limit backoff.
//!
//! Exponential backoff state for GitHub API 403 responses.
//! Base delay doubles on each hit, caps at a configurable maximum.
//! Resets on any successful API call.

use std::time::{Duration, Instant};

/// Exponential backoff state for GitHub rate-limit protection.
pub struct GhBackoff {
    /// When the current backoff window expires (None = not in backoff).
    until: Option<Instant>,
    /// Last delay applied (doubles on each consecutive hit).
    delay: Duration,
    /// Initial backoff delay (default 30s).
    base: Duration,
    /// Maximum backoff delay (default 900s = 15min).
    max: Duration,
}

impl GhBackoff {
    pub fn new() -> Self {
        let base = crate::config::get("gh.backoff.base_seconds")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(30);

        let max = crate::config::get("gh.backoff.max_seconds")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(900);

        Self {
            until: None,
            delay: Duration::ZERO,
            base: Duration::from_secs(base),
            max: Duration::from_secs(max),
        }
    }

    /// Returns remaining wait time if backoff is active, None otherwise.
    pub fn is_active(&self) -> Option<Duration> {
        self.until.and_then(|until| {
            let now = Instant::now();
            if now < until {
                Some(until - now)
            } else {
                None
            }
        })
    }

    /// Record a rate-limit hit — escalate the backoff window.
    pub fn record_rate_limit(&mut self) {
        self.delay = if self.delay.is_zero() {
            self.base
        } else {
            (self.delay * 2).min(self.max)
        };
        self.until = Some(Instant::now() + self.delay);
        tracing::warn!(
            delay_secs = self.delay.as_secs(),
            "GitHub rate limit hit, backing off"
        );
    }

    /// Record a successful API call — reset backoff state.
    pub fn record_success(&mut self) {
        if self.delay > Duration::ZERO {
            tracing::info!("GitHub backoff cleared after successful API call");
        }
        self.delay = Duration::ZERO;
        self.until = None;
    }
}

/// Check if stderr from `gh` indicates a rate-limit / abuse-detection error.
pub fn is_rate_limit_error(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    lower.contains("rate limit")
        || lower.contains("http 403")
        || lower.contains("abuse detection")
        || lower.contains("api rate limit exceeded")
        || lower.contains("secondary rate limit")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_backoff_is_inactive() {
        let b = GhBackoff {
            until: None,
            delay: Duration::ZERO,
            base: Duration::from_secs(30),
            max: Duration::from_secs(900),
        };
        assert!(b.is_active().is_none());
    }

    #[test]
    fn record_rate_limit_activates_backoff() {
        let mut b = GhBackoff {
            until: None,
            delay: Duration::ZERO,
            base: Duration::from_secs(5),
            max: Duration::from_secs(60),
        };
        b.record_rate_limit();
        assert!(b.is_active().is_some());
        assert_eq!(b.delay, Duration::from_secs(5));
    }

    #[test]
    fn backoff_doubles_on_consecutive_hits() {
        let mut b = GhBackoff {
            until: None,
            delay: Duration::ZERO,
            base: Duration::from_secs(5),
            max: Duration::from_secs(60),
        };
        b.record_rate_limit();
        assert_eq!(b.delay, Duration::from_secs(5));
        b.record_rate_limit();
        assert_eq!(b.delay, Duration::from_secs(10));
        b.record_rate_limit();
        assert_eq!(b.delay, Duration::from_secs(20));
    }

    #[test]
    fn backoff_caps_at_max() {
        let mut b = GhBackoff {
            until: None,
            delay: Duration::ZERO,
            base: Duration::from_secs(30),
            max: Duration::from_secs(60),
        };
        b.record_rate_limit(); // 30
        b.record_rate_limit(); // 60
        b.record_rate_limit(); // capped at 60
        assert_eq!(b.delay, Duration::from_secs(60));
    }

    #[test]
    fn record_success_resets_backoff() {
        let mut b = GhBackoff {
            until: None,
            delay: Duration::ZERO,
            base: Duration::from_secs(5),
            max: Duration::from_secs(60),
        };
        b.record_rate_limit();
        assert!(b.is_active().is_some());
        b.record_success();
        assert!(b.is_active().is_none());
        assert_eq!(b.delay, Duration::ZERO);
    }

    #[test]
    fn is_rate_limit_error_detects_variants() {
        assert!(is_rate_limit_error("API rate limit exceeded"));
        assert!(is_rate_limit_error("HTTP 403 Forbidden"));
        assert!(is_rate_limit_error("abuse detection mechanism"));
        assert!(is_rate_limit_error("secondary rate limit"));
        assert!(is_rate_limit_error("You have exceeded a rate limit"));
        assert!(!is_rate_limit_error("not found"));
        assert!(!is_rate_limit_error("gh api failed: 404"));
    }
}
