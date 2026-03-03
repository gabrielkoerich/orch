//! Per-agent routing weights and rate-limit recovery.
//!
//! `RateLimitState` tracks a single agent's current routing weight and
//! recovers it over time after rate-limit events.
//!
//! `AgentWeights` aggregates per-agent states and provides weighted
//! probabilistic selection via `weighted_select`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::selection::{simple_hash_fraction_for, simple_hash_index_for};

/// Default weight for agents with full capacity.
pub(super) const DEFAULT_WEIGHT: f64 = 1.0;

/// Minimum weight — an agent never drops below this (still gets occasional tasks).
pub(super) const MIN_WEIGHT: f64 = 0.05;

/// How much to reduce weight on each rate limit hit (multiplicative decay).
pub(super) const RATE_LIMIT_DECAY: f64 = 0.3;

/// Generate jitter duration for recovery delay based on agent name.
/// Uses a simple hash to create deterministic but varied jitter per agent.
fn generate_recovery_jitter(agent: &str) -> Duration {
    // Simple hash: sum of char values modulo a large prime
    let hash: u64 = agent
        .bytes()
        .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
    // Map hash to 0..RECOVERY_JITTER_MAX range
    let max_secs = RECOVERY_JITTER_MAX.as_secs();
    let jitter_secs = hash % max_secs;
    Duration::from_secs(jitter_secs)
}

/// Duration after which a rate-limited agent starts recovering weight.
pub(super) const RECOVERY_DELAY: Duration = Duration::from_secs(60);

/// Maximum jitter added to recovery delay to stagger agent recoveries.
/// This prevents thundering herd where all agents recover simultaneously.
pub(super) const RECOVERY_JITTER_MAX: Duration = Duration::from_secs(30);

/// Per-tick weight recovery amount (additive, applied each routing call).
pub(super) const RECOVERY_RATE: f64 = 0.1;

/// Rate limit state for a single agent.
#[derive(Debug, Clone)]
pub struct RateLimitState {
    /// Current routing weight (0.0..=1.0). Higher = more tasks.
    pub weight: f64,
    /// When the last rate limit error was recorded.
    pub last_limited_at: Option<Instant>,
    /// Jitter added to recovery delay to stagger recoveries (avoid thundering herd).
    pub recovery_jitter: Duration,
    /// How many consecutive rate limit hits.
    pub consecutive_hits: u32,
}

impl Default for RateLimitState {
    fn default() -> Self {
        Self {
            weight: DEFAULT_WEIGHT,
            last_limited_at: None,
            recovery_jitter: Duration::ZERO,
            consecutive_hits: 0,
        }
    }
}

impl RateLimitState {
    /// Record a rate limit event — decay the weight.
    /// Note: Jitter is set at the AgentWeights level using the agent name.
    pub fn record_rate_limit(&mut self) {
        self.consecutive_hits += 1;
        self.weight = (self.weight * RATE_LIMIT_DECAY).max(MIN_WEIGHT);
        self.last_limited_at = Some(Instant::now());
    }

    /// Record a successful completion — bump weight back toward 1.0.
    pub fn record_success(&mut self) {
        self.consecutive_hits = 0;
        self.weight = (self.weight + RECOVERY_RATE).min(DEFAULT_WEIGHT);
    }

    /// Tick recovery: if enough time has passed since the last limit (plus jitter), gradually restore.
    pub fn maybe_recover(&mut self) {
        if let Some(last) = self.last_limited_at {
            let recovery_threshold = RECOVERY_DELAY + self.recovery_jitter;
            if last.elapsed() >= recovery_threshold {
                self.weight = (self.weight + RECOVERY_RATE).min(DEFAULT_WEIGHT);
                if self.weight >= DEFAULT_WEIGHT {
                    self.last_limited_at = None;
                    self.recovery_jitter = Duration::ZERO;
                    self.consecutive_hits = 0;
                }
            }
        }
    }

    /// Is this agent currently rate-limited (weight below full)?
    pub fn is_limited(&self) -> bool {
        self.weight < DEFAULT_WEIGHT
    }
}

/// Tracks per-agent weights for weighted round-robin routing.
#[derive(Debug, Clone, Default)]
pub struct AgentWeights {
    pub states: HashMap<String, RateLimitState>,
}

impl AgentWeights {
    /// Ensure all available agents have an entry.
    pub fn ensure_agents(&mut self, agents: &[String]) {
        for agent in agents {
            self.states.entry(agent.clone()).or_default();
        }
    }

    /// Record a rate limit event for an agent.
    pub fn record_rate_limit(&mut self, agent: &str) {
        let state = self.states.entry(agent.to_string()).or_default();
        state.record_rate_limit();
        // Generate jitter to stagger recovery times across agents
        state.recovery_jitter = generate_recovery_jitter(agent);
        tracing::info!(
            agent,
            weight = state.weight,
            hits = state.consecutive_hits,
            jitter_secs = state.recovery_jitter.as_secs(),
            "agent weight reduced (rate limit)"
        );
    }

    /// Record a successful task completion for an agent.
    pub fn record_success(&mut self, agent: &str) {
        self.states
            .entry(agent.to_string())
            .or_default()
            .record_success();
    }

    /// Tick recovery for all agents.
    pub fn tick_recovery(&mut self) {
        for (agent, state) in &mut self.states {
            let was_limited = state.is_limited();
            state.maybe_recover();
            if was_limited && !state.is_limited() {
                tracing::info!(agent, "agent weight fully recovered");
            }
        }
    }

    /// Select an agent by weighted probability from the given list.
    ///
    /// Uses a simple weighted random selection: each agent's probability is
    /// proportional to its weight. If all weights are zero (shouldn't happen
    /// due to MIN_WEIGHT), falls back to uniform selection.
    pub fn weighted_select(&self, agents: &[String], task_id: &str) -> Option<String> {
        if agents.is_empty() {
            return None;
        }

        let weights: Vec<f64> = agents
            .iter()
            .map(|a| {
                self.states
                    .get(a)
                    .map(|s| s.weight)
                    .unwrap_or(DEFAULT_WEIGHT)
            })
            .collect();

        let total: f64 = weights.iter().sum();
        if total <= 0.0 {
            // Safety fallback: uniform random
            let idx = simple_hash_index_for(agents.len(), task_id);
            return Some(agents[idx].clone());
        }

        // Deterministic-ish selection using a hash of the current time
        // to avoid requiring rand crate. Good enough for load distribution.
        let pick = simple_hash_fraction_for(task_id) * total;
        let mut cumulative = 0.0;
        for (i, w) in weights.iter().enumerate() {
            cumulative += w;
            if pick < cumulative {
                return Some(agents[i].clone());
            }
        }

        // Rounding edge case — return last agent
        Some(agents.last().unwrap().clone())
    }

    /// Get the current weight for an agent.
    pub fn get_weight(&self, agent: &str) -> f64 {
        self.states
            .get(agent)
            .map(|s| s.weight)
            .unwrap_or(DEFAULT_WEIGHT)
    }

    /// Get a snapshot of all agent weights (for logging/debugging).
    pub fn snapshot(&self) -> Vec<(String, f64, u32)> {
        let mut snap: Vec<_> = self
            .states
            .iter()
            .map(|(a, s)| (a.clone(), s.weight, s.consecutive_hits))
            .collect();
        snap.sort_by(|a, b| a.0.cmp(&b.0));
        snap
    }
}
