//! Hash-based selection utilities for load-balanced routing.
//!
//! Provides deterministic-ish hash functions used by `AgentWeights::weighted_select`
//! to distribute tasks across agents without requiring an external RNG crate.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Global sequence counter to decorrelate hash inputs across rapid calls.
static HASH_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Mix time, a sequence counter, and the task id into a 64-bit seed
fn mix_hash(task_id: &str) -> u64 {
    // Use SystemTime since Instant::now().elapsed() measures time since
    // the instant was created and is nearly always ~0 here. SystemTime
    // gives a clock relative to the UNIX epoch which varies across calls.
    let nanos = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    )
    .unwrap_or(u64::MAX);

    let seq = HASH_COUNTER.fetch_add(1, Ordering::Relaxed);
    let seed = nanos ^ seq ^ hash_task_id(task_id);
    seed.wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407)
}

/// Simple deterministic-ish fraction [0.0, 1.0) based on time + task data.
/// Not cryptographic, but sufficient for load distribution.
pub(super) fn simple_hash_fraction_for(task_id: &str) -> f64 {
    f64::from(u16::try_from(mix_hash(task_id) % 10000).unwrap_or(0)) / 10000.0
}

/// Simple index selection using instant-based hash.
pub(crate) fn simple_hash_index_for(len: usize, task_id: &str) -> usize {
    if len == 0 {
        return 0;
    }
    usize::try_from(mix_hash(task_id) % len as u64).unwrap_or(0)
}

/// Polynomial byte hash used by selection and recovery jitter
pub(super) fn hash_task_id(task_id: &str) -> u64 {
    task_id.bytes().fold(0u64, |acc, b| {
        acc.wrapping_mul(31).wrapping_add(u64::from(b))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_task_id_is_a_polynomial_byte_hash() {
        assert_eq!(hash_task_id(""), 0);
        assert_eq!(hash_task_id("a"), 97);
        assert_eq!(hash_task_id("ab"), 97 * 31 + 98);
    }

    #[test]
    fn simple_hash_index_for_empty_len_is_zero() {
        assert_eq!(simple_hash_index_for(0, "any"), 0);
    }

    #[test]
    fn simple_hash_index_for_stays_in_range() {
        for len in 1..=16 {
            for _ in 0..20 {
                let idx = simple_hash_index_for(len, "task-1");
                assert!(idx < len, "idx={idx} len={len}");
            }
        }
    }

    #[test]
    fn simple_hash_fraction_for_stays_in_unit_interval() {
        for _ in 0..20 {
            let fraction = simple_hash_fraction_for("task-1");
            assert!(
                (0.0..1.0).contains(&fraction),
                "fraction={fraction} is outside [0, 1)"
            );
        }
    }

    #[test]
    fn hash_counter_advances_on_each_call() {
        let before = HASH_COUNTER.load(Ordering::Relaxed);
        let _ = simple_hash_fraction_for("counter");
        let _ = simple_hash_index_for(4, "counter");
        let after = HASH_COUNTER.load(Ordering::Relaxed);
        assert!(after >= before + 2, "before={before} after={after}");
    }
}
