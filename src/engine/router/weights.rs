//! Per-agent routing weights and rate-limit recovery.
//!
//! `RateLimitState` tracks a single agent's current routing weight and
//! recovers it over time after rate-limit events.
//!
//! `AgentWeights` aggregates per-agent states and provides weighted
//! probabilistic selection via `weighted_select`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::selection::{hash_task_id, simple_hash_fraction_for, simple_hash_index_for};

/// Default weight for agents with full capacity.
pub(super) const DEFAULT_WEIGHT: f64 = 1.0;

/// Minimum weight — an agent never drops below this (still gets occasional tasks).
pub(super) const MIN_WEIGHT: f64 = 0.05;

/// How much to reduce weight on each rate limit hit (multiplicative decay).
pub(super) const RATE_LIMIT_DECAY: f64 = 0.3;

/// Weight to use for an agent that is absent from a configured `router.weights`
/// map. Rather than the constant `DEFAULT_WEIGHT` (1.0) — which would silently
/// outrank every agent the operator *did* configure — an omitted agent falls
/// back to the minimum of the explicitly configured weights. An empty map
/// means no weights are configured at all, so `DEFAULT_WEIGHT` applies to
/// everyone equally.
pub(super) fn default_missing_weight(configured_weights: &HashMap<String, f64>) -> f64 {
    configured_weights
        .values()
        .copied()
        .fold(None, |min, w| Some(min.map_or(w, |m: f64| m.min(w))))
        .unwrap_or(DEFAULT_WEIGHT)
}

/// Generate jitter duration for recovery delay based on agent name.
/// Uses a simple hash to create deterministic but varied jitter per agent.
fn generate_recovery_jitter(agent: &str) -> Duration {
    let max_secs = RECOVERY_JITTER_MAX.as_secs();
    let jitter_secs = hash_task_id(agent) % max_secs;
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
    /// The configured base weight this agent should recover toward.
    pub base_weight: f64,
}

impl Default for RateLimitState {
    fn default() -> Self {
        Self {
            weight: DEFAULT_WEIGHT,
            last_limited_at: None,
            recovery_jitter: Duration::ZERO,
            consecutive_hits: 0,
            base_weight: DEFAULT_WEIGHT,
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

    /// Record a successful completion — bump weight back toward base_weight.
    pub fn record_success(&mut self) {
        self.consecutive_hits = 0;
        self.weight = (self.weight + RECOVERY_RATE).min(self.base_weight);
    }

    /// Tick recovery: if enough time has passed since the last limit (plus jitter), gradually restore.
    pub fn maybe_recover(&mut self) {
        if let Some(last) = self.last_limited_at {
            let recovery_threshold = RECOVERY_DELAY + self.recovery_jitter;
            if last.elapsed() >= recovery_threshold {
                self.weight = (self.weight + RECOVERY_RATE).min(self.base_weight);
                if self.weight >= self.base_weight {
                    self.last_limited_at = None;
                    self.recovery_jitter = Duration::ZERO;
                    self.consecutive_hits = 0;
                }
            }
        }
    }

    /// Is this agent currently rate-limited (weight below its base weight)?
    pub fn is_limited(&self) -> bool {
        self.weight < self.base_weight
    }
}

/// Tracks per-agent weights for weighted round-robin routing.
#[derive(Debug, Clone, Default)]
pub struct AgentWeights {
    pub states: HashMap<String, RateLimitState>,
    /// Configured base weights per agent (from config.yml).
    /// Used as the recovery ceiling instead of DEFAULT_WEIGHT.
    pub base_weights: HashMap<String, f64>,
}

impl AgentWeights {
    /// Ensure all available agents have an entry, using configured base weights.
    pub fn ensure_agents(&mut self, agents: &[String]) {
        for agent in agents {
            self.states.entry(agent.clone()).or_default();
        }
    }

    /// Ensure all available agents have an entry, applying configured base weights.
    ///
    /// Agents with an explicit weight in `config.yml` use that as their base.
    /// Agents without a configured weight default to the minimum of the
    /// explicitly configured weights (see `default_missing_weight`), so an
    /// omission never outranks an agent the operator actually configured.
    pub fn ensure_agents_with_weights(
        &mut self,
        agents: &[String],
        configured_weights: &HashMap<String, f64>,
    ) {
        for agent in agents {
            let base = configured_weights
                .get(agent)
                .copied()
                .unwrap_or_else(|| default_missing_weight(configured_weights));
            let state = self
                .states
                .entry(agent.clone())
                .or_insert_with(|| RateLimitState {
                    weight: base,
                    base_weight: base,
                    ..Default::default()
                });
            state.base_weight = base;
            // Update base weight if agent was already initialized (e.g. after config reload)
            // but only if the agent isn't currently rate-limited (don't override decay)
            if !state.is_limited() {
                state.weight = base;
            }
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
                self.states.get(a).map_or_else(
                    || {
                        self.base_weights
                            .get(a)
                            .copied()
                            .unwrap_or_else(|| default_missing_weight(&self.base_weights))
                    },
                    |s| s.weight,
                )
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

        // Rounding edge case — return last agent (agents is guaranteed non-empty by caller)
        debug_assert!(!agents.is_empty(), "agents must not be empty");
        Some(agents[agents.len() - 1].clone())
    }

    /// Get the current weight for an agent.
    pub fn get_weight(&self, agent: &str) -> f64 {
        self.states.get(agent).map_or_else(
            || {
                self.base_weights
                    .get(agent)
                    .copied()
                    .unwrap_or_else(|| default_missing_weight(&self.base_weights))
            },
            |s| s.weight,
        )
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

#[cfg(test)]
#[expect(
    clippy::float_cmp,
    reason = "the weights are stored constants, so they compare exactly"
)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn recovery_jitter_is_the_byte_hash_modulo_the_cap() {
        assert_eq!(generate_recovery_jitter(""), Duration::from_secs(0));
        assert_eq!(generate_recovery_jitter("a"), Duration::from_secs(7));
        assert_eq!(
            generate_recovery_jitter("claude"),
            generate_recovery_jitter("claude")
        );
        assert!(generate_recovery_jitter("claude") < RECOVERY_JITTER_MAX);
    }

    #[serial(cooldown_state)]
    #[test]
    fn rate_limit_state_defaults_to_full_weight() {
        let state = RateLimitState::default();
        assert_eq!(state.weight, DEFAULT_WEIGHT);
        assert_eq!(state.consecutive_hits, 0);
        assert!(state.last_limited_at.is_none());
        assert!(!state.is_limited());
    }

    #[serial(cooldown_state)]
    #[test]
    fn rate_limit_state_decays_on_hit() {
        let mut state = RateLimitState::default();
        state.record_rate_limit();

        assert!(state.weight < DEFAULT_WEIGHT);
        assert_eq!(state.consecutive_hits, 1);
        assert!(state.last_limited_at.is_some());
        assert!(state.is_limited());

        // Weight should be DEFAULT_WEIGHT * RATE_LIMIT_DECAY
        let expected = DEFAULT_WEIGHT * RATE_LIMIT_DECAY;
        assert!((state.weight - expected).abs() < 1e-10);
    }

    #[serial(cooldown_state)]
    #[test]
    fn rate_limit_state_never_drops_below_min() {
        let mut state = RateLimitState::default();

        // Hit many times
        for _ in 0..100 {
            state.record_rate_limit();
        }

        assert!(state.weight >= MIN_WEIGHT);
        assert_eq!(state.consecutive_hits, 100);
    }

    #[serial(cooldown_state)]
    #[test]
    fn rate_limit_state_recovers_on_success() {
        let mut state = RateLimitState::default();

        // Decay first
        state.record_rate_limit();
        state.record_rate_limit();
        let after_decay = state.weight;

        // Record success
        state.record_success();
        assert!(state.weight > after_decay);
        assert_eq!(state.consecutive_hits, 0);
    }

    #[serial(cooldown_state)]
    #[test]
    fn rate_limit_state_success_caps_at_default() {
        let mut state = RateLimitState::default();

        // Already at full weight, success shouldn't exceed it
        state.record_success();
        assert_eq!(state.weight, DEFAULT_WEIGHT);
    }

    #[serial(cooldown_state)]
    #[test]
    fn agent_weights_ensure_agents() {
        let mut weights = AgentWeights::default();
        let agents = vec!["claude".to_string(), "codex".to_string()];
        weights.ensure_agents(&agents);

        assert_eq!(weights.states.len(), 2);
        assert_eq!(weights.get_weight("claude"), DEFAULT_WEIGHT);
        assert_eq!(weights.get_weight("codex"), DEFAULT_WEIGHT);
    }

    #[serial(cooldown_state)]
    #[test]
    fn agent_weights_ensure_agents_preserves_existing() {
        let mut weights = AgentWeights::default();
        let agents = vec!["claude".to_string(), "codex".to_string()];
        weights.ensure_agents(&agents);

        // Reduce claude's weight
        weights.record_rate_limit("claude");
        let claude_weight = weights.get_weight("claude");
        assert!(claude_weight < DEFAULT_WEIGHT);

        // Ensure agents again — claude's weight should be preserved
        let agents2 = vec![
            "claude".to_string(),
            "codex".to_string(),
            "opencode".to_string(),
        ];
        weights.ensure_agents(&agents2);

        assert_eq!(weights.get_weight("claude"), claude_weight);
        assert_eq!(weights.get_weight("opencode"), DEFAULT_WEIGHT);
    }

    #[test]
    fn default_missing_weight_uses_minimum_of_configured() {
        let mut configured = HashMap::new();
        configured.insert("claude".to_string(), 0.2);
        configured.insert("codex".to_string(), 0.2);
        configured.insert("opencode".to_string(), 0.2);
        configured.insert("kimi".to_string(), 0.2);

        // minimax omitted from router.weights — must not silently outrank
        // every explicitly configured agent (issue #3596).
        assert_eq!(default_missing_weight(&configured), 0.2);
    }

    #[test]
    fn default_missing_weight_falls_back_when_none_configured() {
        let configured = HashMap::new();
        assert_eq!(default_missing_weight(&configured), DEFAULT_WEIGHT);
    }

    #[serial(cooldown_state)]
    #[test]
    fn ensure_agents_with_weights_missing_agent_does_not_outrank_configured() {
        let mut weights = AgentWeights::default();
        let mut configured = HashMap::new();
        configured.insert("claude".to_string(), 0.2);
        configured.insert("codex".to_string(), 0.2);
        configured.insert("opencode".to_string(), 0.2);
        configured.insert("kimi".to_string(), 0.2);

        let agents = vec![
            "claude".to_string(),
            "codex".to_string(),
            "opencode".to_string(),
            "kimi".to_string(),
            "minimax".to_string(),
        ];
        weights.ensure_agents_with_weights(&agents, &configured);

        // minimax was omitted from router.weights but must not get the old
        // DEFAULT_WEIGHT of 1.0, which would outrank every configured agent.
        assert_eq!(weights.get_weight("minimax"), 0.2);
        assert_eq!(weights.get_weight("claude"), 0.2);
    }

    #[serial(cooldown_state)]
    #[test]
    fn agent_weights_record_rate_limit() {
        let mut weights = AgentWeights::default();
        weights.ensure_agents(&["claude".to_string(), "codex".to_string()]);

        weights.record_rate_limit("claude");

        assert!(weights.get_weight("claude") < DEFAULT_WEIGHT);
        assert_eq!(weights.get_weight("codex"), DEFAULT_WEIGHT);
    }

    #[serial(cooldown_state)]
    #[test]
    fn agent_weights_record_success() {
        let mut weights = AgentWeights::default();
        weights.ensure_agents(&["claude".to_string()]);

        weights.record_rate_limit("claude");
        let after_limit = weights.get_weight("claude");

        weights.record_success("claude");
        assert!(weights.get_weight("claude") > after_limit);
    }

    #[serial(cooldown_state)]
    #[test]
    fn agent_weights_snapshot() {
        let mut weights = AgentWeights::default();
        weights.ensure_agents(&["claude".to_string(), "codex".to_string()]);
        weights.record_rate_limit("claude");

        let snap = weights.snapshot();
        assert_eq!(snap.len(), 2);

        // Snapshot is sorted alphabetically
        assert_eq!(snap[0].0, "claude");
        assert!(snap[0].1 < DEFAULT_WEIGHT);
        assert_eq!(snap[0].2, 1); // 1 hit

        assert_eq!(snap[1].0, "codex");
        assert_eq!(snap[1].1, DEFAULT_WEIGHT);
        assert_eq!(snap[1].2, 0); // 0 hits
    }

    #[serial(cooldown_state)]
    #[test]
    fn agent_weights_weighted_select_favors_higher_weight() {
        let mut weights = AgentWeights::default();
        let agents = vec!["claude".to_string(), "codex".to_string()];
        weights.ensure_agents(&agents);

        // Heavily penalize claude
        for _ in 0..10 {
            weights.record_rate_limit("claude");
        }

        // Run many selections and count
        let mut claude_count = 0;
        let mut codex_count = 0;
        for _ in 0..100 {
            match weights.weighted_select(&agents, "task-1") {
                Some(ref a) if a == "claude" => claude_count += 1,
                Some(ref a) if a == "codex" => codex_count += 1,
                _ => {}
            }
        }

        // Codex should get significantly more selections
        // (claude weight is near MIN_WEIGHT, codex is at 1.0)
        assert!(
            codex_count > claude_count,
            "codex ({codex_count}) should get more selections than claude ({claude_count})"
        );
    }

    #[serial(cooldown_state)]
    #[test]
    fn agent_weights_weighted_select_empty_returns_none() {
        let weights = AgentWeights::default();
        assert!(weights.weighted_select(&[], "task-1").is_none());
    }

    #[serial(cooldown_state)]
    #[test]
    fn agent_weights_batch_routing_produces_varied_selections() {
        // Regression test: when routing a batch of tasks in the same tick,
        // different task IDs should not all select the same agent.
        let mut weights = AgentWeights::default();
        let agents = vec!["claude".to_string(), "codex".to_string()];
        weights.ensure_agents(&agents);

        // Simulate batch routing: 20 different task IDs called in rapid succession
        let task_ids: Vec<String> = (1..=20).map(|i| format!("task-{i}")).collect();
        let selections: Vec<String> = task_ids
            .iter()
            .filter_map(|id| weights.weighted_select(&agents, id))
            .collect();

        // With 20 tasks and 2 agents of equal weight, we should see both agents selected.
        // The probability of all 20 selecting the same agent by chance is (0.5)^19 < 1e-6.
        let claude_count = selections.iter().filter(|a| a.as_str() == "claude").count();
        let codex_count = selections.iter().filter(|a| a.as_str() == "codex").count();
        assert!(
            claude_count > 0 && codex_count > 0,
            "batch routing should distribute across agents; got claude={claude_count} codex={codex_count}"
        );
    }

    #[serial(cooldown_state)]
    #[test]
    fn agent_weights_weighted_select_single_agent() {
        let mut weights = AgentWeights::default();
        let agents = vec!["claude".to_string()];
        weights.ensure_agents(&agents);

        let selected = weights.weighted_select(&agents, "task-1");
        assert_eq!(selected, Some("claude".to_string()));
    }

    #[serial(cooldown_state)]
    #[test]
    fn agent_weights_tick_recovery_restores_weight() {
        let mut weights = AgentWeights::default();
        weights.ensure_agents(&["claude".to_string()]);

        // Set up a rate limit that looks like it happened long ago
        let state = weights.states.get_mut("claude").unwrap();
        state.weight = 0.5;
        state.last_limited_at = Some(
            Instant::now()
                .checked_sub(RECOVERY_DELAY + Duration::from_secs(1))
                .expect("the clock is past the recovery delay"),
        );
        state.consecutive_hits = 2;

        let before = weights.get_weight("claude");
        weights.tick_recovery();
        let after = weights.get_weight("claude");

        assert!(after > before, "weight should increase after recovery tick");
    }

    #[serial(cooldown_state)]
    #[test]
    fn agent_weights_tick_recovery_clears_on_full_restore() {
        let mut weights = AgentWeights::default();
        weights.ensure_agents(&["claude".to_string()]);

        // Set weight just below full with an old limit
        let state = weights.states.get_mut("claude").unwrap();
        state.weight = DEFAULT_WEIGHT - 0.01;
        state.last_limited_at = Some(
            Instant::now()
                .checked_sub(RECOVERY_DELAY + Duration::from_secs(1))
                .expect("the clock is past the recovery delay"),
        );
        state.consecutive_hits = 1;

        weights.tick_recovery();

        let state = weights.states.get("claude").unwrap();
        assert_eq!(state.weight, DEFAULT_WEIGHT);
        assert!(state.last_limited_at.is_none());
        assert_eq!(state.consecutive_hits, 0);
    }
}
