//! Agent router — selects the best agent and model for each task.
//!
//! The router uses LLM-based classification to route tasks to the best agent
//! (claude, codex, opencode, kimi, or minimax) based on task content, labels,
//! and configured routing rules. It also generates a specialized agent profile.
//!
//! Routing logic (in priority order):
//! 1. Check for `agent:*` label on task — use that agent directly
//! 2. If weighted_round_robin enabled, select by capacity-weighted probability
//! 3. If round_robin mode, cycle through agents (stateful, skips last-used)
//! 4. Call LLM classifier for intelligent routing
//! 5. After N LLM failures, fall back to round-robin
//! 6. Track last routed agent to distribute load across agents

pub mod config;
mod llm;
mod ollama;
pub mod profiles;
mod selection;
mod strategies;
pub mod weights;

use crate::backends::ExternalTask;
use crate::cmd_cache;
use crate::engine::cooldown;
use crate::engine::cooldown::{
    cooldown_until, is_agent_degraded, is_agent_in_cooldown, is_model_in_cooldown,
    list_degraded_agents, record_agent_success, refresh_degraded_agents, sync_from_kv,
};
use crate::engine::runner::agents::opencode;
use crate::engine::runner::response;
use crate::store::WaitingOnDependency;
use crate::store::{TaskStore, get_task_field_direct, store_log_activity};
pub use config::{RouterConfig, parse_pool_entry};
use llm::{LlmRouter, TIMEOUT_PREFIX};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::sync::Arc;
use tokio::task;
pub use weights::AgentWeights;

fn fmt_degraded(degraded: &[String]) -> String {
    if degraded.is_empty() {
        String::new()
    } else {
        format!(" (degraded: {})", degraded.join(", "))
    }
}

/// A turn-cap error comes from the router command line (`--max-turns 1`)
fn is_max_turns_error(err_msg: &str) -> bool {
    let lower = err_msg.to_ascii_lowercase();
    lower.contains("max turns reached") || lower.contains("reached the maximum number of turns")
}

/// Error returned when all agents/models are in cooldown and routing is temporarily unavailable.
///
/// This is a transient error that should not count against `max_route_attempts` since
/// no agent was actually invoked. The caller should back off and retry later.
#[derive(Debug, thiserror::Error)]
pub enum AllAgentsCooledError {
    #[error("all agents/models in cooldown for {scope}{}", fmt_degraded(.degraded))]
    AllCooled {
        scope: String,
        remaining_secs: Option<u64>,
        degraded: Vec<String>,
    },
}

impl AllAgentsCooledError {
    /// Create a new error for the given scope, naming currently degraded agents
    pub fn new(scope: String, remaining_secs: Option<u64>) -> Self {
        Self::AllCooled {
            scope,
            remaining_secs,
            degraded: list_degraded_agents()
                .into_iter()
                .map(|(agent, _reason)| agent)
                .collect(),
        }
    }

    // LLM bypass helpers moved to `impl Router` so they can access Router fields.

    /// Get the scope of the cooldown (e.g., "medium", "all agents", "router pool").
    pub fn scope(&self) -> &str {
        match self {
            Self::AllCooled { scope, .. } => scope,
        }
    }

    /// Remaining cooldown duration in seconds when available.
    pub fn remaining_secs(&self) -> Option<u64> {
        match self {
            Self::AllCooled { remaining_secs, .. } => *remaining_secs,
        }
    }

    /// Agents currently marked degraded, shown by `orch cooldown list`
    pub fn degraded(&self) -> &[String] {
        match self {
            Self::AllCooled { degraded, .. } => degraded,
        }
    }
}

/// Typed error returned by [`get_route_result`].
#[derive(Debug, thiserror::Error)]
pub enum RouteResultError {
    /// The task has no agent field — it needs to be re-routed.
    #[error("no agent configured for task {task_id}")]
    NoAgent { task_id: String },
    /// Any other failure (store lookup, SQL, etc.).
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Result of routing a task to an agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RouteResult {
    /// The selected agent: "claude", "codex", or "opencode"
    pub agent: String,
    /// Optional model suggestion, e.g., "claude-sonnet-4-6", "claude-opus-4-6", "o3"
    pub model: Option<String>,
    /// Complexity level: "simple", "medium", or "complex"
    pub complexity: String,
    /// Fibonacci effort estimate (1, 2, 3, 5, 8, 13, or 21). 0 means not provided.
    pub estimate: u8,
    /// Why this agent was selected
    pub reason: String,
    /// Specialized agent profile (role, tools, constraints)
    pub profile: AgentProfile,
    /// Optional warning about routing decision
    pub warning: Option<String>,
}

/// Specialized agent profile for a task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AgentProfile {
    /// Role name, e.g., "backend specialist"
    pub role: String,
    /// Tools allowed for this task
    pub tools: Vec<String>,
    /// Constraints for this task
    pub constraints: Vec<String>,
    /// The model/effort choice associated with this route, cleared on failover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<config::NamedProfile>,
}

impl AgentProfile {
    /// Build the default "general" profile from router config.
    pub fn default_from_config(config: &RouterConfig) -> Self {
        Self {
            role: "general".to_string(),
            tools: config.allowed_tools.clone(),
            constraints: vec![],
            execution: None,
        }
    }
}

impl RouteResult {
    /// The model for the task's tier, or one from the nearest tier with a free model. The warning
    /// names the tier used when it differs, so the switch shows in the route activity
    pub(crate) fn resolve_model(
        config: &RouterConfig,
        agent: &str,
        complexity: &str,
        task_id: &str,
    ) -> (Option<String>, Option<String>) {
        match config.model_for_complexity_or_adjacent(agent, complexity, task_id) {
            Some((model, tier)) if tier != complexity => {
                let warning =
                    format!("no available {complexity} model for {agent}; using {tier} tier");
                (Some(model), Some(warning))
            }
            Some((model, _)) => (Some(model), None),
            None => (None, None),
        }
    }

    /// Build a standard route result from the three task-specific values.
    ///
    /// Fills `profile` and `warning` with router-config defaults so call sites
    /// only need to supply what varies per task.
    pub fn from_config(
        agent: String,
        complexity: String,
        reason: String,
        config: &RouterConfig,
        task_id: &str,
    ) -> Self {
        let (model, warning) = Self::resolve_model(config, &agent, &complexity, task_id);
        Self {
            agent,
            model,
            complexity,
            estimate: 0,
            reason,
            profile: AgentProfile::default_from_config(config),
            warning,
        }
    }
}

/// The agent router.
#[expect(
    clippy::struct_field_names,
    reason = "the router holds sub-routers whose names say which backend they use"
)]
pub struct Router {
    /// Router configuration
    pub config: RouterConfig,
    /// Available agents discovered at runtime
    pub available_agents: Vec<String>,
    /// Per-agent rate limit weights (used when weighted_round_robin is enabled)
    pub weights: AgentWeights,
    /// LLM routing subsystem (wrapped in Arc so callers can clone a handle
    /// without holding the router RwLock across async operations)
    llm_router: Arc<LlmRouter>,
    /// Ollama router for local routing (contains reusable reqwest::Client)
    ollama_router: Option<ollama::OllamaRouter>,
    /// Round-robin index for task routing
    pub(crate) rr_index: usize,
    /// Last agent routed to (for distribution tracking)
    pub(crate) last_agent: Option<String>,
    /// Round-robin index for review agent selection
    pub(crate) review_rr_index: usize,
    /// Expanded pool of (agent, model) pairs for router LLM round-robin.
    /// `opencode:free` entries are expanded at construction time.
    pub(crate) router_pool: Vec<(String, String)>,
    /// Current round-robin index into router_pool
    pub(crate) pool_index: usize,
}

impl Debug for Router {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Router")
            .field("config", &self.config)
            .field("available_agents", &self.available_agents)
            .field("weights", &self.weights)
            .field("ollama_router", &self.ollama_router)
            .field("rr_index", &self.rr_index)
            .field("last_agent", &self.last_agent)
            .field("review_rr_index", &self.review_rr_index)
            .field("router_pool", &self.router_pool)
            .field("pool_index", &self.pool_index)
            .finish_non_exhaustive()
    }
}

impl Router {
    pub fn available_agent_count(&self) -> usize {
        self.available_agents.len()
    }

    /// Agents are installed but every review agent is cooled down or degraded
    pub fn review_agents_all_cooled(&self) -> bool {
        !self.available_agents.is_empty() && self.healthy_agent_count("review") == 0
    }

    pub fn has_routable_review_agent(&self) -> bool {
        !self.available_agents.is_empty() && self.healthy_agent_count("review") > 0
    }

    pub fn model_for_complexity(
        &self,
        agent: &str,
        complexity: &str,
        task_id: &str,
    ) -> Option<String> {
        self.config.model_for_complexity(agent, complexity, task_id)
    }

    /// Create a new router with the given configuration.
    pub fn new(config: RouterConfig) -> Self {
        let available_agents = Self::discover_agents(&config.agents);
        let weights = config.agent_weights(&available_agents);
        let router_pool = Self::expand_pool(&config);
        tracing::info!(
            pool = ?router_pool,
            fallback = %config.effective_fallback(),
            "router LLM pool initialized"
        );
        // Prime only when this config names opencode, so a default or claude-only
        // router never runs `opencode models`
        if config.uses_opencode() {
            opencode::prime_free_model_cache();
        }
        let ollama_router = if config.mode == "local" {
            Some(ollama::OllamaRouter::new(&config))
        } else {
            None
        };
        Self {
            config,
            available_agents,
            weights,
            llm_router: Arc::new(LlmRouter::new()),
            ollama_router,
            rr_index: 0,
            last_agent: None,
            review_rr_index: 0,
            router_pool,
            pool_index: 0,
        }
    }

    /// Create a router with default configuration loaded from files.
    pub fn from_config() -> Self {
        Self::new(RouterConfig::from_config())
    }

    /// Create a router with a forced agent list, bypassing CLI discovery.
    /// Only for use in unit tests.
    #[cfg(test)]
    pub(crate) fn new_for_test(config: RouterConfig, available_agents: Vec<String>) -> Self {
        let mut weights = AgentWeights {
            base_weights: config.weights.clone(),
            ..Default::default()
        };
        weights.ensure_agents(&available_agents);
        let router_pool = Self::expand_pool(&config);
        Self {
            config,
            available_agents,
            weights,
            llm_router: Arc::new(LlmRouter::new()),
            ollama_router: None,
            rr_index: 0,
            last_agent: None,
            review_rr_index: 0,
            router_pool,
            pool_index: 0,
        }
    }

    fn advance_pool_index_after_attempt(&mut self, idx: usize, pool_len: usize) {
        debug_assert!(pool_len > 0, "router pool must not be empty");
        self.pool_index = (idx + 1) % pool_len;
    }

    /// Discover available agent CLIs in PATH.
    /// Checks all agents from the configured list.
    fn discover_agents(configured_agents: &[String]) -> Vec<String> {
        let mut agents = Vec::new();
        for agent in configured_agents {
            if cmd_cache::command_exists(agent) {
                agents.push(agent.clone());
            }
        }
        agents
    }

    /// Expand the router LLM pool, resolving `opencode:free` to discovered free models.
    ///
    /// - Entries other than `opencode:free` are parsed as `agent:model` and added as-is.
    /// - `opencode:free` is expanded by running `opencode models | grep free` at startup.
    /// - If `opencode` is not installed or the command fails, the entry is skipped.
    /// - If the expanded pool is empty, falls back to a single entry from `effective_fallback()`.
    fn expand_pool(config: &RouterConfig) -> Vec<(String, String)> {
        let raw_pool = config.effective_pool();
        let mut expanded: Vec<(String, String)> = Vec::new();

        for entry in &raw_pool {
            if entry == "opencode:free" {
                let free_models = Self::discover_free_opencode_models();
                if free_models.is_empty() {
                    tracing::debug!("opencode:free expanded to nothing — skipping");
                } else {
                    tracing::debug!(models = ?free_models, "opencode:free expanded");
                    for model in free_models {
                        expanded.push(("opencode".to_string(), model));
                    }
                }
            } else {
                let (agent, model) = parse_pool_entry(entry);
                expanded.push((agent, model));
            }
        }

        if expanded.is_empty() {
            // All entries were opencode:free and opencode isn't installed — use fallback
            let (agent, model) = parse_pool_entry(&config.effective_fallback());
            tracing::debug!(
                agent = %agent, model = %model,
                "pool empty after expansion, using fallback as sole pool entry"
            );
            expanded.push((agent, model));
        }

        expanded
    }

    /// Async version of expand_pool which may query the opencode CLI without
    /// blocking the Tokio runtime.
    async fn expand_pool_async(config: &RouterConfig) -> Vec<(String, String)> {
        let raw_pool = config.effective_pool();
        let mut expanded: Vec<(String, String)> = Vec::new();

        for entry in &raw_pool {
            if entry == "opencode:free" {
                let free_models = Self::discover_free_opencode_models_async().await;
                if free_models.is_empty() {
                    tracing::debug!("opencode:free expanded to nothing — skipping (async)");
                } else {
                    tracing::debug!(models = ?free_models, "opencode:free expanded (async)");
                    for model in free_models {
                        expanded.push(("opencode".to_string(), model));
                    }
                }
            } else {
                let (agent, model) = parse_pool_entry(entry);
                expanded.push((agent, model));
            }
        }

        if expanded.is_empty() {
            // All entries were opencode:free and opencode isn't installed — use fallback
            let (agent, model) = parse_pool_entry(&config.effective_fallback());
            tracing::debug!(
                agent = %agent, model = %model,
                "pool empty after async expansion, using fallback as sole pool entry"
            );
            expanded.push((agent, model));
        }

        expanded
    }

    /// Async reload that uses async pool expansion to avoid blocking the runtime
    /// when discovering opencode free models.
    pub async fn reload_async(&mut self) {
        let new_config = RouterConfig::from_config();
        let new_agents = Self::discover_agents(&new_config.agents);
        let new_pool = Self::expand_pool_async(&new_config).await;
        tracing::info!(
            mode = %new_config.mode,
            agents = ?new_agents,
            fallback = %new_config.fallback_executor,
            weighted_rr = new_config.weighted_round_robin,
            pool = ?new_pool,
            "router reloaded (async)"
        );
        self.config = new_config;
        self.available_agents.clone_from(&new_agents);
        // Ensure new agents have weight entries (preserves existing weights)
        self.weights.ensure_agents(&new_agents);
        self.router_pool = new_pool;
        self.pool_index = 0;
        // Reinitialize Ollama router if mode changed to local
        self.ollama_router = if self.config.mode == "local" {
            Some(ollama::OllamaRouter::new(&self.config))
        } else {
            None
        };
    }

    fn discover_free_opencode_models() -> Vec<String> {
        opencode::discover_free_opencode_models()
    }

    async fn discover_free_opencode_models_async() -> Vec<String> {
        task::spawn_blocking(Self::discover_free_opencode_models)
            .await
            .unwrap_or_default()
    }

    /// Run the pre-emptive health check, refreshing the degraded-agent set.
    ///
    /// Queries the `rate_limits` table for recent events and combines with
    /// cooldown state to mark agents as degraded before routing attempts them.
    pub async fn refresh_health(&self, store: &Arc<TaskStore>) {
        // KV is the source of truth for cooldowns. Reconcile the in-memory map
        // from KV every tick so external mutations (e.g. `orch cooldown clear`
        // from a separate CLI process) actually take effect on the running
        // service. Without this, the engine keeps stale entries until they
        // expire on their own, producing the "all agents cooled" log with
        // `orch cooldown list` simultaneously reporting "No active cooldowns".
        sync_from_kv(store).await;

        let agents = self.available_agents.clone();
        let model_checker = |agent: &str| -> bool { self.agent_has_any_available_model(agent) };
        refresh_degraded_agents(
            store,
            &agents,
            &model_checker,
            config::health_check_window_hours(),
            config::degraded_rate_limit_threshold(),
        )
        .await;
    }

    /// Check if an agent is available.
    pub fn is_agent_available(&self, agent: &str) -> bool {
        self.available_agents.contains(&agent.to_string())
    }

    /// True if the agent has at least one uncooled model across any complexity
    /// tier. Used where the eventual complexity isn't known yet (e.g. before
    /// LLM classification), so we can't check a single tier.
    fn agent_has_any_available_model(&self, agent: &str) -> bool {
        ["simple", "medium", "complex", "review"]
            .iter()
            .any(|comp| self.config.has_available_model_for_complexity(agent, comp))
    }

    /// True if the agent has an available (uncooled) model for the `medium`
    /// and `complex` tiers — the tiers real coding tasks are classified into
    /// by the LLM classifier. Used to pre-filter the candidate list handed to
    /// that classifier so an agent whose healthy tiers are only `simple` isn't
    /// offered: the classifier can't be constrained to `simple`, so offering
    /// such an agent produces a near-certain wasted classification call whenever
    /// it picks `medium`/`complex` (issue #3521). `review` is not a candidate
    /// outcome of initial classification and is deliberately excluded.
    fn agent_has_available_model_for_coding_tiers(&self, agent: &str) -> bool {
        ["medium", "complex"]
            .iter()
            .all(|comp| self.config.has_available_model_for_complexity(agent, comp))
    }

    fn agent_is_routable(&self, agent: &str, complexity: &str) -> bool {
        if !self.is_agent_available(agent) {
            return false;
        }
        if is_agent_in_cooldown(agent) {
            tracing::debug!(agent, "agent skipped: in cooldown");
            return false;
        }
        if is_agent_degraded(agent) {
            tracing::debug!(agent, "agent skipped: degraded (pre-emptive health check)");
            return false;
        }
        // If weighted routing is used, consider the agent degraded when its
        // weight has decayed below the threshold. This avoids
        // proactively routing to agents that recently hit many rate limits.
        if self.config.weighted_round_robin {
            let weight = self.weights.get_weight(agent);
            if weight < config::SKIP_LIMITED_THRESHOLD {
                tracing::debug!(
                    agent,
                    weight,
                    "agent considered degraded by weight threshold, skipping"
                );
                return false;
            }
        }
        if !self
            .config
            .has_available_model_in_any_tier(agent, complexity)
        {
            tracing::debug!(agent, complexity, "agent skipped: no available model");
            return false;
        }
        true
    }

    fn available_agents_for_complexity(&self, complexity: &str) -> Vec<String> {
        self.available_agents
            .iter()
            .filter(|agent| self.agent_is_routable(agent, complexity))
            .cloned()
            .collect()
    }

    /// Count of healthy (routable) agents for a given complexity.
    /// Returns the number of agents that are available, not in cooldown, and have an available model.
    pub fn healthy_agent_count(&self, complexity: &str) -> usize {
        self.available_agents_for_complexity(complexity).len()
    }

    /// Check if the system is in degraded mode (fewer than threshold healthy agents).
    /// Uses "simple" complexity as the baseline since it has the widest model support.
    pub fn is_degraded(&self, threshold: usize) -> bool {
        self.healthy_agent_count("simple") < threshold
    }

    pub fn earliest_cooldown_until(&self, complexity: Option<&str>) -> Option<i64> {
        let mut earliest: Option<i64> = None;

        for agent in &self.available_agents {
            if let Some(until) = cooldown_until(agent) {
                earliest = Some(earliest.map_or(until, |current| current.min(until)));
            }

            if let Some(comp) = complexity
                && let Some(pool) = self.config.model_pool_for_complexity(agent, comp)
            {
                for model in pool {
                    let key = format!("{agent}:{model}");
                    if let Some(until) = cooldown_until(&key) {
                        earliest = Some(earliest.map_or(until, |current| current.min(until)));
                    }
                }
            }
        }

        earliest
    }

    /// Find the earliest cooldown expiration among the router LLM pool entries.
    ///
    /// Returns `Some(earliest_ts)` if at least one pool model is on cooldown,
    /// or `None` if no pool model has a cooldown.
    fn earliest_pool_cooldown(&self) -> Option<i64> {
        let mut earliest: Option<i64> = None;
        for (agent, model) in &self.router_pool {
            let key = format!("{agent}:{model}");
            if let Some(until) = cooldown_until(&key) {
                earliest = Some(earliest.map_or(until, |current| current.min(until)));
            }
        }
        earliest
    }

    fn wait_for_cooldown(&self, complexity: Option<&str>) -> anyhow::Result<()> {
        // If any cooldowns are present, return an error immediately so the
        // caller (the tick loop) can skip this task and retry on the next tick.
        // Sleeping inside the router blocks the entire tick loop for the
        // cooldown duration which starves other tasks. The engine tick already
        // retries every `tick_interval` (default 10s), so allow the tick loop
        // to drive retries instead of sleeping here.
        let now = chrono::Utc::now().timestamp();
        let Some(earliest) = self.earliest_cooldown_until(complexity) else {
            // All agents are degraded but none have a timed cooldown entry
            // (e.g. over-threshold rate limiting without explicit cooldown duration).
            // Log at warn and return an error so the tick skips this task and retries after backoff.
            tracing::warn!(
                scope = complexity.unwrap_or("all agents"),
                "all agents degraded (no cooldown timestamp available) — skipping task, will retry after backoff"
            );
            // Return an error similar to the cooldown case to trigger backoff in the tick loop
            return Err(AllAgentsCooledError::new(
                complexity.unwrap_or("all agents").to_string(),
                None,
            )
            .into());
        };

        let remaining = earliest.saturating_sub(now);
        if remaining > 0 {
            let scope_str = complexity.unwrap_or("all agents").to_string();
            tracing::warn!(
                remaining_secs = remaining,
                scope = %scope_str,
                "all agents/models cooled — routing is unavailable until cooldown expires; failing fast to let tick retry"
            );
            // Return a domain error indicating all candidates are cooled so the
            // caller can handle it (tick will skip this task and retry later).
            return Err(AllAgentsCooledError::new(scope_str, u64::try_from(remaining).ok()).into());
        }

        Ok(())
    }

    /// Get the first available agent.
    /// Pick next agent via round-robin (for review or other non-task routing).
    /// Pick the next review agent, optionally excluding one (e.g. the task's original agent).
    /// Falls back to the first candidate whenever every candidate is excluded (#3645).
    pub fn next_round_robin_agent(&mut self, exclude: &[&str], complexity: &str) -> Option<String> {
        // Use available_agents_for_complexity which already filters using agent_is_routable,
        // eliminating duplicate cooldown/degraded/model checks.
        let candidates = self.available_agents_for_complexity(complexity);
        if candidates.is_empty() {
            return None;
        }
        let idx = self.review_rr_index;
        let n = candidates.len();

        // Try to find an agent that isn't excluded
        let agent = (0..n)
            .map(|offset| &candidates[(idx + offset) % n])
            .find(|a| !exclude.contains(&a.as_str()))
            .cloned()
            // Fallback: use the first available candidate (even if excluded)
            .or_else(|| candidates.first().cloned())?;

        self.review_rr_index = (idx + 1) % candidates.len().max(1);
        Some(agent)
    }

    /// If the selected agent matches `no_code_last_agent` for this task, filter it
    /// out of the candidates and return the filtered list. Used to prevent the
    /// same agent from being selected twice in a row after a no-code failure (#2410).
    async fn skip_no_code_agent(
        &self,
        store: &Arc<TaskStore>,
        repo: &str,
        task_id: &str,
        candidates: &[String],
    ) -> Option<Vec<String>> {
        let no_code_agent = get_task_field_direct(store, repo, task_id, "no_code_last_agent")
            .await
            .unwrap_or_else(|| {
                tracing::warn!(
                    task_id,
                    "no_code_last_agent DB read returned None — loop detection may be impaired"
                );
                String::new()
            });
        if no_code_agent.is_empty() || !candidates.iter().any(|a| a == &no_code_agent) {
            return None;
        }
        let filtered: Vec<String> = candidates
            .iter()
            .filter(|a| a.as_str() != no_code_agent)
            .cloned()
            .collect();
        if filtered.is_empty() {
            tracing::warn!(
                task_id,
                no_code_agent = %no_code_agent,
                "all available agents are the no-code agent — cannot skip"
            );
            return None;
        }
        tracing::info!(
            task_id,
            skipped = %no_code_agent,
            remaining = ?filtered,
            "skipping no-code agent, routing to alternative"
        );
        Some(filtered)
    }

    /// Route a task to the best agent.
    ///
    /// Routing logic (in priority order):
    /// 1. Check for `agent:*` label — use that agent directly
    /// 2. If weighted_round_robin enabled, select by capacity-weighted probability
    /// 3. If round_robin mode, cycle through agents (stateful)
    /// 4. Call LLM classifier for intelligent routing
    /// 5. After max_route_attempts LLM failures, fall back to round-robin
    pub async fn route(
        &mut self,
        task: &ExternalTask,
        store: &Arc<TaskStore>,
        repo: &str,
    ) -> anyhow::Result<RouteResult> {
        // NOTE: refresh_health() is called once per tick in tick_route_tasks()
        // before the routing loop, not per-task. Do not re-add it here.

        // Hold the task before any classification, label override, or fallback.
        // A waiting task must not increment route_attempts.
        match store.resolve_id_by_external(repo, &task.id.0).await {
            Ok(Some(id)) => match store.block_claim_if_waiting(id).await {
                Ok(Some(blockers)) => {
                    return Err(WaitingOnDependency { blockers }.into());
                }
                Ok(None) => {}
                Err(err) => return Err(err),
            },
            Ok(None) => {}
            Err(err) => return Err(err),
        }

        loop {
            // 1. Check store agent field first (set by failover, authoritative over labels)
            // Use a lightweight lookup to avoid deserializing the full Task row when
            // only the `agent` column is needed. This avoids two full-row loads
            // per routing pass (resolve_task_id + get) and significantly reduces
            // CPU work on busy instances.
            let store_agent = match store.get_agent_by_external_id(repo, &task.id.0).await {
                Ok(Some(agent)) => {
                    // Router enforces that the agent string matches a configured agent
                    if !agent.is_empty() && self.config.has_agent(&agent) {
                        Some(agent)
                    } else {
                        None
                    }
                }
                Ok(None) => None,
                Err(e) => {
                    tracing::warn!(
                        repo,
                        task_id = %task.id.0,
                        err = %e,
                        "failed to fetch stored agent, falling back to label/LLM routing"
                    );
                    None
                }
            };

            // Named profiles select a route after dependencies and stored recovery.
            if store_agent.is_none()
                && let Some(result) = self.route_profile(task)?
            {
                self.log_route_activity(store, repo, &task.id.0, &result, None)
                    .await;
                return Ok(result);
            }

            // 2. Fall back to explicit agent label
            let resolved_agent = store_agent.or_else(|| {
                strategies::extract_agent_from_labels(&self.config.agents, &task.labels)
            });

            if let Some(agent) = resolved_agent {
                let complexity = strategies::extract_complexity_from_labels(&task.labels);
                // If agent appears routable (not in agent-wide cooldown and has
                // available models or no model_map configured), check whether
                // we actually have a concrete model to dispatch. If the
                // resolved label maps to no model (None), we fall through to
                // the standard routing logic so the router LLM can pick a
                // proper agent/model, instead of dispatching with an empty
                // model which causes some agents (e.g. opencode) to exit
                // silently.
                if self.agent_is_routable(&agent, &complexity) {
                    let (model, tier_warning) =
                        RouteResult::resolve_model(&self.config, &agent, &complexity, &task.id.0);

                    // Only dispatch label overrides when we have a concrete
                    // model to send to the executor. Dispatching with an
                    // empty model led to silent exits for some agents
                    // (notably opencode). If no concrete model is available
                    // for the labeled agent, fall through to the standard
                    // routing logic so the LLM or round-robin can pick an
                    // appropriate agent/model.
                    if model.is_some() {
                        tracing::debug!(
                            task_id = %task.id.0,
                            agent = %agent,
                            complexity = %complexity,
                            "routed via label"
                        );
                        let mut result = RouteResult::from_config(
                            agent.clone(),
                            complexity.clone(),
                            format!("label agent:{agent}"),
                            &self.config,
                            &task.id.0,
                        );
                        // Label routing uses agent-specific role and the
                        // resolved model (may differ from from_config's default).
                        result.profile.role = format!("{agent} specialist");
                        result.model = model.clone();
                        result.warning = tier_warning;
                        self.log_route_activity(store, repo, &task.id.0, &result, None)
                            .await;
                        return Ok(result);
                    }
                    // No concrete model found for the labeled agent — do
                    // not honor the label override. Fall through to the
                    // standard routing logic so another agent or the LLM
                    // can be chosen with a valid model.
                    tracing::warn!(task_id = %task.id.0, agent = %agent, "label agent has no available model — falling through to LLM routing");
                }

                let candidates = self.available_agents_for_complexity(&complexity);
                if candidates.is_empty() {
                    self.wait_for_cooldown(Some(&complexity))?;
                    continue;
                }
                // Label target is cooled or lacked model; fall through to standard routing.
            }

            let complexity = strategies::extract_complexity_from_labels(&task.labels);

            // Check if any agents have available models for this complexity
            let mut candidates = self.available_agents_for_complexity(&complexity);
            if candidates.is_empty() {
                self.wait_for_cooldown(Some(&complexity))?;
                continue;
            }

            // Skip the no-code agent if one is recorded for this task (#2410).
            // If all available agents are filtered out, we fall through to
            // LLM routing with no constraints (LLM will pick the same agent but
            // the response_handler will block it immediately via same-agent check).
            if let Some(filtered) = self
                .skip_no_code_agent(store, repo, &task.id.0, &candidates)
                .await
            {
                candidates = filtered;
            }

            // 2. Weighted round-robin — capacity-based selection
            if self.config.weighted_round_robin {
                let result = strategies::route_via_weighted_round_robin(
                    &candidates,
                    &self.weights,
                    &self.config,
                    task,
                    &mut self.last_agent,
                )?;
                self.log_route_activity(store, repo, &task.id.0, &result, None)
                    .await;
                return Ok(result);
            }

            // 3. Round-robin mode — use stateful round-robin
            if self.config.mode == "round_robin" || self.config.mode == "profile" {
                tracing::debug!(task_id = %task.id.0, "routing via round-robin mode");
                let result = strategies::route_via_round_robin_stateful(
                    &candidates,
                    &self.config,
                    task,
                    &mut self.rr_index,
                    &mut self.last_agent,
                )?;
                self.log_route_activity(store, repo, &task.id.0, &result, None)
                    .await;
                return Ok(result);
            }

            // 4. Local Ollama routing
            if self.config.mode == "local" {
                tracing::debug!(task_id = %task.id.0, ollama_url = %self.config.ollama_url, "routing via local Ollama");
                let ollama_router = self
                    .ollama_router
                    .as_ref()
                    .expect("ollama_router must be initialized in Router::new when mode=local");
                match ollama_router.route(task, &self.config).await {
                    Ok(result) => {
                        self.log_route_activity(store, repo, &task.id.0, &result, None)
                            .await;
                        return Ok(result);
                    }
                    Err(e) => {
                        tracing::warn!(
                            task_id = %task.id.0,
                            error = %e,
                            "Ollama routing failed, falling back to round-robin"
                        );
                        let result = strategies::route_via_round_robin_stateful(
                            &candidates,
                            &self.config,
                            task,
                            &mut self.rr_index,
                            &mut self.last_agent,
                        )?;
                        self.log_route_activity(store, repo, &task.id.0, &result, None)
                            .await;
                        return Ok(result);
                    }
                }
            }

            // 5. LLM-based routing with retry tracking
            let route_attempts = self.get_route_attempts(&task.id.0, store, repo).await;

            if route_attempts >= self.config.max_route_attempts {
                tracing::warn!(
                    task_id = %task.id.0,
                    attempts = route_attempts,
                    max = self.config.max_route_attempts,
                    "max LLM route attempts reached, falling back to round-robin"
                );
                let candidates = self.available_agents_for_complexity(&complexity);
                if candidates.is_empty() {
                    self.wait_for_cooldown(Some(&complexity))?;
                    continue;
                }
                let result = strategies::route_via_round_robin_stateful(
                    &candidates,
                    &self.config,
                    task,
                    &mut self.rr_index,
                    &mut self.last_agent,
                )?;
                self.log_route_activity(store, repo, &task.id.0, &result, None)
                    .await;
                return Ok(result);
            }

            // Log routing start (before await)
            tracing::debug!(task_id = %task.id.0, "starting LLM routing");

            match self.route_with_llm(task, repo).await {
                Ok(result) => {
                    let candidates = self.available_agents_for_complexity(&result.complexity);
                    if candidates.is_empty() {
                        self.wait_for_cooldown(Some(&result.complexity))?;
                        continue;
                    }

                    // If the LLM selected the no-code agent, filter it out and
                    // pick an alternative (#2410). Re-use the existing fallback logic.
                    let no_code_agent =
                         get_task_field_direct(store, repo, &task.id.0, "no_code_last_agent")
                             .await
                             .unwrap_or_else(|| {
                                 tracing::warn!(task_id = %task.id.0, "no_code_last_agent DB read returned None — loop detection may be impaired");
                                 String::new()
                             });
                    if result.agent == no_code_agent {
                        // Pick the first agent that isn't the no-code agent.
                        let fallback_agent = candidates
                            .iter()
                            .find(|a| a.as_str() != no_code_agent)
                            .cloned()
                            .unwrap_or_else(|| candidates[0].clone());
                        if fallback_agent.as_str() == no_code_agent {
                            tracing::warn!(
                                task_id = %task.id.0,
                                no_code_agent = %no_code_agent,
                                complexity = %result.complexity,
                                "all available agents are the no-code agent — waiting for another agent"
                            );
                            self.wait_for_cooldown(Some(&result.complexity))?;
                            continue;
                        }
                        tracing::info!(
                            task_id = %task.id.0,
                            llm_selected = %result.agent,
                            skipped = %no_code_agent,
                            fallback = %fallback_agent,
                            "LLM selected the no-code agent — routing to alternative"
                        );
                        let (fallback_model, _) = RouteResult::resolve_model(
                            &self.config,
                            &fallback_agent,
                            &result.complexity,
                            &task.id.0,
                        );

                        self.set_route_attempts(&task.id.0, 0, store, repo).await;
                        let result = RouteResult {
                            agent: fallback_agent.clone(),
                            model: fallback_model,
                            complexity: result.complexity.clone(),
                            estimate: result.estimate,
                            reason: format!(
                                "LLM selected no-code agent {}; routed to {}",
                                result.agent, fallback_agent
                            ),
                            profile: result.profile.clone(),
                            warning: Some(format!(
                                "LLM selected no-code agent {}; used fallback {}",
                                result.agent, fallback_agent
                            )),
                        };
                        self.log_route_activity(store, repo, &task.id.0, &result, None)
                            .await;
                        return Ok(result);
                    }

                    let model_cooled = result
                        .model
                        .as_deref()
                        .is_some_and(|model| is_model_in_cooldown(&result.agent, model));
                    if !candidates.contains(&result.agent) || model_cooled {
                        let fallback_agent = candidates
                            .iter()
                            .find(|agent| *agent != &result.agent)
                            .cloned()
                            .unwrap_or_else(|| candidates[0].clone());
                        let (fallback_model, _) = RouteResult::resolve_model(
                            &self.config,
                            &fallback_agent,
                            &result.complexity,
                            &task.id.0,
                        );

                        // If the fallback model is None and the fallback agent is the
                        // same as the LLM-selected agent, it means all models for the
                        // only available agent are cooled. In that case, wait for
                        // cooldowns instead of returning a RouteResult with
                        // model: None which would be dispatched incorrectly.
                        if fallback_model.is_none() && fallback_agent == result.agent {
                            tracing::warn!(
                                task_id = %task.id.0,
                                agent = %result.agent,
                                complexity = %result.complexity,
                                "LLM-selected model(s) are cooled for the only available agent; waiting for cooldown"
                            );
                            self.wait_for_cooldown(Some(&result.complexity))?;
                            continue;
                        }

                        // Distinguish logging and reasons when we actually switch
                        // agents vs when we keep the same agent but pick a
                        // different (non-cooled) model.
                        if fallback_agent == result.agent {
                            tracing::warn!(
                                task_id = %task.id.0,
                                agent = %result.agent,
                                complexity = %result.complexity,
                                "LLM-selected model was cooled; selecting an alternative model for the same agent"
                            );
                        } else {
                            tracing::warn!(
                                task_id = %task.id.0,
                                agent = %result.agent,
                                fallback = %fallback_agent,
                                complexity = %result.complexity,
                                "LLM selected cooled agent/model; rerouting to available agent"
                            );
                        }

                        // Reset attempts on success
                        self.set_route_attempts(&task.id.0, 0, store, repo).await;

                        let reason = if fallback_agent == result.agent {
                            format!(
                                "LLM selected cooled model; using same agent {fallback_agent} with different model"
                            )
                        } else {
                            format!("LLM selected cooled agent/model; rerouted to {fallback_agent}")
                        };

                        let result = RouteResult {
                            agent: fallback_agent.clone(),
                            model: fallback_model,
                            complexity: result.complexity.clone(),
                            estimate: result.estimate,
                            reason: reason.clone(),
                            profile: result.profile.clone(),
                            warning: Some(if fallback_agent == result.agent {
                                "LLM-selected model was cooled; selected alternative model for same agent"
                                    .to_string()
                            } else {
                                "LLM-selected agent/model was cooled; rerouted to available agent"
                                    .to_string()
                            }),
                        };
                        self.log_route_activity(store, repo, &task.id.0, &result, None)
                            .await;
                        return Ok(result);
                    }

                    // Reset attempts on success
                    self.set_route_attempts(&task.id.0, 0, store, repo).await;
                    tracing::info!(
                        task_id = %task.id.0,
                        agent = %result.agent,
                        complexity = %result.complexity,
                        "routed via LLM"
                    );
                    self.log_route_activity(store, repo, &task.id.0, &result, None)
                        .await;
                    return Ok(result);
                }
                Err(e) => {
                    if let Some(err) = e.downcast_ref::<AllAgentsCooledError>() {
                        let scope = err.scope();
                        tracing::warn!(scope = %scope, "router cooldown gate tripped");
                        // "router pool" scope means all router LLM pool entries were pre-cooled.
                        // Pass None so wait_for_cooldown queries all agent+model cooldowns generically
                        // (no complexity-specific pool lookup, which would fail for "router pool").
                        let scope_opt = match scope {
                            "all agents" | "router pool" => None,
                            _ => Some(scope),
                        };
                        self.wait_for_cooldown(scope_opt)?;
                        continue;
                    }

                    let new_attempts = route_attempts + 1;
                    self.set_route_attempts(&task.id.0, new_attempts, store, repo)
                        .await;
                    tracing::warn!(
                        task_id = %task.id.0,
                        error = %e,
                        error_chain = ?e,
                        attempt = new_attempts,
                        max = self.config.max_route_attempts,
                        "LLM routing failed"
                    );

                    if new_attempts >= self.config.max_route_attempts {
                        tracing::info!(
                            task_id = %task.id.0,
                            "falling back to round-robin after {} failed attempts",
                            new_attempts
                        );
                        let candidates = self.available_agents_for_complexity(&complexity);
                        if candidates.is_empty() {
                            self.wait_for_cooldown(Some(&complexity))?;
                            continue;
                        }
                        let result = strategies::route_via_round_robin_stateful(
                            &candidates,
                            &self.config,
                            task,
                            &mut self.rr_index,
                            &mut self.last_agent,
                        )?;
                        self.log_route_activity(store, repo, &task.id.0, &result, None)
                            .await;
                        return Ok(result);
                    }

                    let candidates = self.available_agents_for_complexity(&complexity);
                    if candidates.is_empty() {
                        self.wait_for_cooldown(Some(&complexity))?;
                        continue;
                    }
                    let result = strategies::route_via_fallback(
                        &candidates,
                        &self.config,
                        task,
                        Some(&self.weights),
                    )?;
                    self.log_route_activity(store, repo, &task.id.0, &result, None)
                        .await;
                    return Ok(result);
                }
            }
        }
    }

    /// Get the number of LLM routing attempts for a task from the store.
    async fn get_route_attempts(&self, task_id: &str, store: &Arc<TaskStore>, repo: &str) -> u32 {
        match store.resolve_task_id(repo, task_id).await {
            Ok(Some(store_id)) => store
                .get(store_id)
                .await
                .map_or(0, |t| u32::try_from(t.route_attempts).unwrap_or(0)),
            _ => 0,
        }
    }

    /// Set the number of LLM routing attempts for a task in the store.
    async fn set_route_attempts(
        &self,
        task_id: &str,
        attempts: u32,
        store: &Arc<TaskStore>,
        repo: &str,
    ) {
        if let Ok(Some(store_id)) = store.resolve_task_id(repo, task_id).await
            && let Err(e) = store
                .set_fields(store_id, &[("route_attempts", serde_json::json!(attempts))])
                .await
        {
            tracing::warn!(task_id, error = %e, "failed to store route_attempts");
        }
    }

    /// Log a route event to task activity timeline.
    async fn log_route_activity(
        &self,
        store: &Arc<TaskStore>,
        repo: &str,
        task_id: &str,
        result: &RouteResult,
        from_agent: Option<&str>,
    ) {
        let details = serde_json::json!({
            "reason": result.reason,
            "complexity": result.complexity,
            "role": result.profile.role,
            "warning": result.warning,
        });
        let event_type = if from_agent.is_some() {
            "rerouted"
        } else {
            "routed"
        };
        store_log_activity(
            &Some(Arc::clone(store)),
            repo,
            task_id,
            event_type,
            None,
            Some("routed"),
            Some(&result.agent),
            result.model.as_deref(),
            Some(&details),
        )
        .await;
    }

    /// Route using LLM classification with pool round-robin.
    ///
    /// Iterates through `router_pool` (skipping cooled entries) until one succeeds.
    /// On full exhaustion, tries the configured fallback. If the fallback also fails,
    /// returns the last error (the caller falls back to round-robin agent selection).
    async fn route_with_llm(
        &mut self,
        task: &ExternalTask,
        repo: &str,
    ) -> anyhow::Result<RouteResult> {
        // Filter out cooled and degraded agents, and agents without an available
        // model for the coding tiers (`medium`/`complex`) that real tasks are
        // classified into, so the LLM only sees candidates that could actually
        // be dispatched for whatever complexity it picks. An agent whose only
        // healthy tier is `simple` is excluded too: the classifier can't be
        // constrained to `simple`, so leaving such an agent in would waste the
        // call whenever it picks `medium`/`complex` (issue #3521).
        // Fall back to the full list if all agents are cooled/degraded.
        // Cloned to avoid borrow conflict with &mut self in the loop.
        let uncooled_agents: Vec<String> = self
            .available_agents
            .iter()
            .filter(|a| {
                !is_agent_in_cooldown(a)
                    && !is_agent_degraded(a)
                    && self.agent_has_available_model_for_coding_tiers(a)
            })
            .cloned()
            .collect();
        if uncooled_agents.is_empty() {
            return Err(AllAgentsCooledError::new("all agents".to_string(), None).into());
        }
        let llm_agents = uncooled_agents;

        let pool = self.router_pool.clone();
        let n = pool.len();
        let start = self.pool_index;
        let mut last_err: Option<anyhow::Error> = None;
        let mut attempted_any = false;

        // Try pool entries in round-robin order, skipping cooled ones
        for i in 0..n {
            let idx = (start + i) % n;
            let (agent, model) = &pool[idx];
            let model_str = model.as_str();

            if response::is_model_in_cooldown(agent, model_str) {
                tracing::debug!(agent, model = model_str, "pool entry on cooldown, skipping");
                continue;
            }

            // At least one pool entry was available to attempt
            attempted_any = true;

            let model_opt = if model_str.is_empty() {
                None
            } else {
                Some(model_str)
            };

            match self
                .llm_router
                .route_with_llm_using(
                    task,
                    &llm_agents,
                    &self.config,
                    &mut self.last_agent,
                    repo,
                    agent,
                    model_opt,
                )
                .await
            {
                Ok(result) => {
                    // Advance index so the next call starts at the next pool entry
                    self.advance_pool_index_after_attempt(idx, n);
                    tracing::debug!(
                        agent,
                        model = model_str,
                        pool_idx = idx,
                        "router LLM pool entry succeeded"
                    );
                    record_agent_success(agent, model_str).await;
                    return Ok(result);
                }
                Err(e) => {
                    let err_msg = e.to_string();
                    let is_timeout = err_msg.contains(TIMEOUT_PREFIX);
                    if is_timeout {
                        // Advance pool index so the next tick starts at the next entry,
                        // then return immediately. Continuing to try further pool entries
                        // in the same tick would accumulate multiple timeout windows and
                        // stall the tick loop for minutes (issue #3187).
                        //
                        // Also record a standard cooldown, same as the non-timeout branch,
                        // so a chronically-timing-out entry is skipped by is_model_in_cooldown
                        // on future ticks instead of burning the full timeout window again
                        // every time round-robin reaches it (issue #3422).
                        self.advance_pool_index_after_attempt(idx, n);
                        tracing::warn!(
                            agent,
                            model = model_str,
                            next_pool_idx = self.pool_index,
                            "router LLM pool entry timed out — recording cooldown, will retry next tick"
                        );
                        response::record_model_failure(agent, model_str).await;
                        return Err(e);
                    }
                    if is_max_turns_error(&err_msg) {
                        tracing::warn!(
                            agent,
                            model = model_str,
                            error = %e,
                            "pool entry hit max turns — trying next without cooldown"
                        );
                    } else {
                        tracing::warn!(
                            agent,
                            model = model_str,
                            error = %e,
                            "pool entry failed, recording cooldown and trying next"
                        );
                        if !cooldown::record_credit_exhaustion_from_message(
                            agent,
                            Some(model_str),
                            &err_msg,
                        )
                        .await
                        {
                            response::record_model_failure(agent, model_str).await;
                        }
                    }
                    last_err = Some(e);
                    self.advance_pool_index_after_attempt(idx, n);
                }
            }
        }

        // If every pool entry was pre-cooled (nothing was attempted), do not
        // immediately exhaust the pool again — fail fast and let the tick loop
        // retry on the next cycle instead of cascading failures.
        // However, we should still try the fallback if configured.
        if !attempted_any {
            let now = chrono::Utc::now().timestamp();
            let earliest = self.earliest_pool_cooldown();
            if let Some(until) = earliest {
                // Pool entries exist but are all cooled - we'll try fallback below
                let remaining = until.saturating_sub(now);
                tracing::warn!(
                    remaining_secs = remaining,
                    pool = ?pool,
                    "all router LLM pool entries on cooldown — will try fallback"
                );
                // Continue to fallback logic instead of returning early
            } else {
                // No pool cooldowns found either — this means the pool was empty
                // or every entry was filtered out for some other reason.
                return Err(last_err
                    .unwrap_or_else(|| anyhow::anyhow!("all router LLM pool entries exhausted")));
            }
        }

        // All pool entries failed or were cooled — try the configured fallback
        let fallback = self.config.effective_fallback();
        let (fb_agent, fb_model) = parse_pool_entry(&fallback);
        let fb_model_str = fb_model.as_str();

        // Only try fallback if it wasn't already in the pool (avoid double-try)
        let fallback_already_tried = pool.iter().any(|(a, m)| a == &fb_agent && m == &fb_model);

        if !fallback_already_tried && !response::is_model_in_cooldown(&fb_agent, fb_model_str) {
            tracing::info!(
                agent = %fb_agent,
                model = %fb_model_str,
                "all pool entries exhausted — trying fallback router LLM"
            );
            let fb_model_opt = if fb_model_str.is_empty() {
                None
            } else {
                Some(fb_model_str)
            };
            match self
                .llm_router
                .route_with_llm_using(
                    task,
                    &llm_agents,
                    &self.config,
                    &mut self.last_agent,
                    repo,
                    &fb_agent,
                    fb_model_opt,
                )
                .await
            {
                Ok(result) => {
                    record_agent_success(&fb_agent, fb_model_str).await;
                    return Ok(result);
                }
                Err(e) => {
                    let err_msg = e.to_string();
                    let is_timeout = err_msg.contains(TIMEOUT_PREFIX);
                    if is_timeout {
                        // Cool down on timeout like pool entries (#3422), else a
                        // chronically-timing-out fallback burns the full timeout window every tick (#3616)
                        response::record_model_failure(&fb_agent, fb_model_str).await;
                        tracing::warn!(
                            agent = %fb_agent,
                            model = %fb_model_str,
                            "fallback router LLM timed out — returning immediately to unblock tick loop"
                        );
                        return Err(e);
                    }
                    tracing::warn!(
                        agent = %fb_agent,
                        model = %fb_model_str,
                        error = %e,
                        "fallback router LLM also failed"
                    );
                    if !is_max_turns_error(&err_msg) {
                        response::record_model_failure(&fb_agent, fb_model_str).await;
                    }
                    last_err = Some(e);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("all router LLM pool entries exhausted")))
    }

    /// Record a rate limit event for an agent, reducing its routing weight.
    ///
    /// Called by the engine when an agent returns a 429/rate limit error.
    pub fn record_rate_limit(&mut self, agent: &str) {
        self.weights.record_rate_limit(agent);
    }

    /// Record a successful task completion, restoring agent weight.
    pub fn record_success(&mut self, agent: &str) {
        self.weights.record_success(agent);
    }

    /// Tick weight recovery for all agents.
    ///
    /// Called periodically by the engine to gradually restore weights
    /// as rate limit windows expire.
    pub fn tick_weight_recovery(&mut self) {
        self.weights.tick_recovery();
    }

    /// Store routing result in the task store.
    pub async fn store_route_result(
        &self,
        task_id: &str,
        result: &RouteResult,
        store: &Arc<TaskStore>,
        repo: &str,
    ) -> anyhow::Result<()> {
        if let Ok(Some(store_id)) = store.resolve_task_id(repo, task_id).await {
            store
                .set_fields(
                    store_id,
                    &[
                        ("agent", serde_json::json!(result.agent)),
                        ("complexity", serde_json::json!(result.complexity)),
                        ("route_reason", serde_json::json!(result.reason)),
                        (
                            "agent_profile",
                            serde_json::json!(serde_json::to_string(&result.profile)?),
                        ),
                        (
                            "model",
                            serde_json::json!(result.model.as_deref().unwrap_or("")),
                        ),
                    ],
                )
                .await?;
        }
        Ok(())
    }
}

/// Retrieve routing result from the task store.
pub async fn get_route_result(
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
) -> Result<RouteResult, RouteResultError> {
    let store_id = store
        .resolve_task_id(repo, task_id)
        .await
        .map_err(RouteResultError::Other)?
        .ok_or_else(|| {
            RouteResultError::Other(anyhow::anyhow!("task {task_id} not found in store"))
        })?;
    let task = store.get(store_id).await.map_err(RouteResultError::Other)?;

    let agent = task
        .agent
        .clone()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| RouteResultError::NoAgent {
            task_id: task_id.to_string(),
        })?;

    let complexity = if task.complexity.is_empty() {
        "medium".to_string()
    } else {
        task.complexity.clone()
    };

    let reason = task.route_reason.clone();
    let model = task.model.clone().filter(|m| !m.is_empty());
    // Only a profile route's model/effort choice is restored; every other route
    // keeps the default profile, as before. Recovery that changed the agent or
    // model invalidates the stored choice.
    let profile = AgentProfile {
        execution: serde_json::from_str::<AgentProfile>(&task.agent_profile)
            .ok()
            .and_then(|stored| stored.execution)
            .filter(|p| p.matches_route(&agent, model.as_deref())),
        ..AgentProfile::default()
    };

    // Validate that the stored model is valid for the resolved agent.
    // Agent-specific aliases (e.g. "opus") can leak across agents during
    // failover, so we verify against the router's model pools (#1604).
    let config = RouterConfig::from_config();
    let model = model.filter(|m| {
        let complexities = ["simple", "medium", "complex", "review"];
        // Collect all model pools configured for this agent across complexity tiers.
        let all_pools: Vec<Vec<String>> = complexities
            .iter()
            .filter_map(|comp| config.model_pool_for_complexity(&agent, comp))
            .collect();
        // If the agent has no model pools configured, we can't validate — pass through.
        if all_pools.is_empty() {
            return true;
        }
        let valid = all_pools.iter().any(|pool| pool.contains(m))
            || profile
                .execution
                .as_ref()
                .is_some_and(|p| p.matches_route(&agent, Some(m)));
        if !valid {
            tracing::warn!(
                task_id,
                agent,
                model = %m,
                "discarding stale model: not in agent's model pools"
            );
        }
        valid
    });

    // A stale model is replaced by the agent's model for the task's complexity, an empty
    // model would reach the agent as `--model ""`. last_error keeps the reason (#2527)
    let original_model = task.model.clone().filter(|m| !m.is_empty());
    let mut model = model;
    if original_model.is_some() && model.is_none() {
        model = config.model_for_complexity(&agent, &complexity, task_id);
        let stale_model = original_model.as_deref().unwrap_or("unknown");
        let stale_err = format!(
            "stale model discarded: {stale_model} not in {agent} model pools, using {}",
            model.as_deref().unwrap_or("the agent default")
        );
        if let Err(e) = store
            .set_fields(
                store_id,
                &[
                    (
                        "model",
                        serde_json::json!(model.clone().unwrap_or_default()),
                    ),
                    ("last_error", serde_json::json!(stale_err)),
                ],
            )
            .await
        {
            // Log the error so transient DB failures are visible and diagnosable.
            tracing::warn!(task_id = %task_id, error = %e, "failed to clear stale model from store — will retry on next dispatch");
        }
    }

    Ok(RouteResult {
        agent,
        model,
        complexity,
        estimate: 0,
        reason,
        profile,
        warning: None,
    })
}

#[cfg(test)]
#[expect(
    clippy::float_cmp,
    reason = "the weights are stored constants, so they compare exactly"
)]
mod tests;
