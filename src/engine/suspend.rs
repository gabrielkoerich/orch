//! Host suspend/resume detection.
//!
//! `Instant` is monotonic and does not advance while the host is asleep; wall-clock
//! time does. Comparing the two across a checkpoint reveals suspend/resume gaps that
//! would otherwise be misread by wall-clock-only comparisons as a stalled tick loop
//! (the watchdog in `engine::mod`) or a hung agent (`stuck_task_timing_from_map` in
//! `engine::tick`).
//!
//! `checkpoint()` is the single detection point, called once per main tick loop
//! iteration. Both consumers pull from the resulting gap log instead of re-deriving
//! suspend information from their own wall-clock deltas.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Wall-clock-minus-monotonic gap large enough to be a host suspend rather than
/// scheduling jitter or a slow tick.
const SUSPEND_GAP_THRESHOLD: Duration = Duration::from_secs(20);

/// Bound on retained gap history so a long-running process doesn't grow this
/// unboundedly; suspend events are rare so this comfortably covers any realistic
/// task age window.
const MAX_GAP_LOG: usize = 128;

struct GapRecord {
    detected_at: DateTime<Utc>,
    gap: ChronoDuration,
}

struct Checkpoint {
    instant: Instant,
    wall: DateTime<Utc>,
}

static LAST_CHECKPOINT: Mutex<Option<Checkpoint>> = Mutex::new(None);
static GAP_LOG: Mutex<Vec<GapRecord>> = Mutex::new(Vec::new());

/// Record a checkpoint and return the suspend gap (if any) detected since the
/// previous one. Call this once per main tick loop iteration.
pub fn checkpoint() -> Option<ChronoDuration> {
    let now_instant = Instant::now();
    let now_wall = Utc::now();

    let prev = {
        let mut guard = LAST_CHECKPOINT.lock().unwrap_or_else(|e| e.into_inner());
        guard.replace(Checkpoint {
            instant: now_instant,
            wall: now_wall,
        })
    };
    let prev = prev?;

    let monotonic_elapsed = now_instant.saturating_duration_since(prev.instant);
    let wall_elapsed = (now_wall - prev.wall).to_std().unwrap_or_default();
    let gap = wall_elapsed.saturating_sub(monotonic_elapsed);
    if gap < SUSPEND_GAP_THRESHOLD {
        return None;
    }

    let gap = ChronoDuration::from_std(gap).unwrap_or_else(|_| ChronoDuration::zero());
    let mut log = GAP_LOG.lock().unwrap_or_else(|e| e.into_inner());
    log.push(GapRecord {
        detected_at: now_wall,
        gap,
    });
    if log.len() > MAX_GAP_LOG {
        let excess = log.len() - MAX_GAP_LOG;
        log.drain(0..excess);
    }
    Some(gap)
}

/// Total suspend time detected since `since`. Subtract this from a wall-clock age
/// computation to avoid mistaking suspend time for hung agent/tick runtime.
pub fn suspended_duration_since(since: DateTime<Utc>) -> ChronoDuration {
    let log = GAP_LOG.lock().unwrap_or_else(|e| e.into_inner());
    log.iter()
        .filter(|r| r.detected_at > since)
        .fold(ChronoDuration::zero(), |acc, r| acc + r.gap)
}

/// Most recent suspend gap detected within the last `window`, if any. Lets the tick
/// watchdog downgrade a stale-tick alert to informational when it is fully explained
/// by a host suspend rather than an actual stall.
pub fn gap_detected_within(window: Duration) -> Option<ChronoDuration> {
    let window = ChronoDuration::from_std(window).unwrap_or_else(|_| ChronoDuration::zero());
    let cutoff = Utc::now() - window;
    let log = GAP_LOG.lock().unwrap_or_else(|e| e.into_inner());
    log.iter()
        .rev()
        .find(|r| r.detected_at >= cutoff)
        .map(|r| r.gap)
}

/// Test-only helper to inject a synthetic gap record without going through the real
/// `Instant`/wall-clock comparison in `checkpoint()`, which can't be driven
/// deterministically in tests.
#[cfg(test)]
pub(crate) fn inject_gap_for_test(detected_at: DateTime<Utc>, gap: ChronoDuration) {
    let mut log = GAP_LOG.lock().unwrap_or_else(|e| e.into_inner());
    log.push(GapRecord { detected_at, gap });
}

/// Test-only helper to reset shared gap state between tests. Callers must run
/// `#[serial(suspend_state)]` since `GAP_LOG` is a process-wide static.
#[cfg(test)]
pub(crate) fn clear_for_test() {
    let mut log = GAP_LOG.lock().unwrap_or_else(|e| e.into_inner());
    log.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial(suspend_state)]
    fn suspended_duration_since_sums_only_later_gaps() {
        clear_for_test();
        let t0 = Utc::now() - ChronoDuration::minutes(10);
        let t1 = Utc::now() - ChronoDuration::minutes(5);
        inject_gap_for_test(t0, ChronoDuration::minutes(3));
        inject_gap_for_test(t1, ChronoDuration::minutes(2));

        // Since before t0: both gaps count.
        let total = suspended_duration_since(t0 - ChronoDuration::seconds(1));
        assert_eq!(total, ChronoDuration::minutes(5));

        // Since between t0 and t1: only the later gap counts.
        let total = suspended_duration_since(t0 + ChronoDuration::seconds(1));
        assert_eq!(total, ChronoDuration::minutes(2));

        // Since after t1: no gaps count.
        let total = suspended_duration_since(t1 + ChronoDuration::seconds(1));
        assert_eq!(total, ChronoDuration::zero());

        clear_for_test();
    }

    #[test]
    #[serial(suspend_state)]
    fn gap_detected_within_finds_recent_gap() {
        clear_for_test();
        inject_gap_for_test(Utc::now(), ChronoDuration::minutes(7));

        let found = gap_detected_within(Duration::from_secs(60));
        assert_eq!(found, Some(ChronoDuration::minutes(7)));

        clear_for_test();
    }
}
