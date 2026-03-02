//! Hash-based selection utilities for load-balanced routing.
//!
//! Provides deterministic-ish hash functions used by `AgentWeights::weighted_select`
//! to distribute tasks across agents without requiring an external RNG crate.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Global sequence counter to decorrelate hash inputs across rapid calls.
pub(super) static HASH_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Simple deterministic-ish fraction [0.0, 1.0) based on time + task data.
/// Not cryptographic, but sufficient for load distribution.
pub(super) fn simple_hash_fraction_for(task_id: &str) -> f64 {
    // Use SystemTime since Instant::now().elapsed() measures time since
    // the instant was created and is nearly always ~0 here. SystemTime
    // gives a clock relative to the UNIX epoch which varies across calls.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;

    let seq = HASH_COUNTER.fetch_add(1, Ordering::Relaxed);
    let task_hash = hash_task_id(task_id);
    let seed = nanos ^ seq ^ task_hash;

    // Mix bits using a simple hash
    let hash = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (hash % 10000) as f64 / 10000.0
}

/// Simple index selection using instant-based hash.
pub(super) fn simple_hash_index_for(len: usize, task_id: &str) -> usize {
    if len == 0 {
        return 0;
    }

    // Use SystemTime for a variable seed instead of Instant::now().elapsed().
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;

    let seq = HASH_COUNTER.fetch_add(1, Ordering::Relaxed);
    let task_hash = hash_task_id(task_id);
    let seed = nanos ^ seq ^ task_hash;

    let hash = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (hash as usize) % len
}

fn hash_task_id(task_id: &str) -> u64 {
    task_id
        .bytes()
        .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64))
}
