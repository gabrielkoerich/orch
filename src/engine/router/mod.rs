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
mod selection;
mod strategies;
pub mod weights;

pub use config::{parse_pool_entry, RouterConfig};
pub use weights::AgentWeights;

/// Error returned when all agents/models are in cooldown and routing is temporarily unavailable.
///
/// This is a transient error that should not count against `max_route_attempts` since
/// no agent was actually invoked. The caller should back off and retry later.
#[derive(Debug, thiserror::Error)]
pub enum AllAgentsCooledError {
    #[error("all agents/models in cooldown for {scope}")]
    AllCooled {
        scope: String,
        remaining_secs: Option<u64>,
    },
}

impl AllAgentsCooledError {
    /// Create a new error for the given scope.
    pub fn new(scope: String, remaining_secs: Option<u64>) -> Self {
        Self::AllCooled {
            scope,
            remaining_secs,
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

use crate::backends::ExternalTask;
use crate::engine::cooldown::{
    cooldown_until, is_agent_degraded, is_agent_in_cooldown, is_model_in_cooldown,
    refresh_degraded_agents,
};
use crate::store::{get_task_field_direct, store_log_activity, TaskStore};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use llm::{LlmRouter, TIMEOUT_PREFIX};

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
    /// Specialized agent profile (skills, tools, constraints)
    pub profile: AgentProfile,
    /// Selected skill IDs from the catalog
    pub selected_skills: Vec<String>,
    /// Optional warning about routing decision
    pub warning: Option<String>,
}

/// Specialized agent profile for a task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AgentProfile {
    /// Role name, e.g., "backend specialist"
    pub role: String,
    /// Focus skills for this task
    pub skills: Vec<String>,
    /// Tools allowed for this task
    pub tools: Vec<String>,
    /// Constraints for this task
    pub constraints: Vec<String>,
}

impl AgentProfile {
    /// Build the default "general" profile from router config.
    pub fn default_from_config(config: &RouterConfig) -> Self {
        Self {
            role: "general".to_string(),
            skills: vec![],
            tools: config.allowed_tools.clone(),
            constraints: vec![],
        }
    }
}

impl RouteResult {
    /// Build a standard route result from the three task-specific values.
    ///
    /// Fills `profile`, `selected_skills`, and `warning` with router-config
    /// defaults so call sites only need to supply what varies per task.
    pub fn from_config(
        agent: String,
        complexity: String,
        reason: String,
        config: &RouterConfig,
        task_id: &str,
    ) -> Self {
        let model = config.model_for_complexity(&agent, &complexity, task_id);
        Self {
            agent,
            model,
            complexity,
            estimate: 0,
            reason,
            profile: AgentProfile::default_from_config(config),
            selected_skills: config.default_skills.clone(),
            warning: None,
        }
    }
}

/// An opaque, cheaply-cloneable handle to the LLM routing subsystem.
///
/// Obtain one via [`Router::llm_router_handle`].  Holding this handle does
/// **not** require holding the router's `RwLock`, so async operations can
/// safely be called without starving write-lock waiters.
pub struct LlmRouterHandle(std::sync::Arc<LlmRouter>);

impl LlmRouterHandle {
    /// Invalidate the skills catalog cache so the next routing call reloads
    /// from disk.  Safe to await without holding any `RwLock` on `Router`.
    pub async fn invalidate_skills_catalog(&self) {
        self.0.invalidate_skills_catalog().await;
    }
}

/// The agent router.
pub struct Router {
    /// Router configuration
    pub config: RouterConfig,
    /// Available agents discovered at runtime
    pub available_agents: Vec<String>,
    /// Per-agent rate limit weights (used when weighted_round_robin is enabled)
    pub weights: AgentWeights,
    /// LLM routing subsystem (wrapped in Arc so callers can clone a handle
    /// without holding the router RwLock across async operations)
    llm_router: std::sync::Arc<LlmRouter>,
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
    /// Short-horizon LLM routing budget failure tracking: number of failures
    /// observed within the configured window. Used to trigger a temporary
    /// bypass of LLM routing to avoid repeatedly hitting the llm_budget_secs
    /// timeout on many consecutive tasks.
    llm_budget_fail_count: u32,
    /// Window start timestamp (seconds since epoch) for llm_budget_fail_count.
    llm_budget_window_start: Option<i64>,
    /// When set, skip LLM routing until this timestamp (seconds since epoch).
    llm_bypass_until: Option<i64>,
}

impl Router {
    /// Create a new router with the given configuration.
    pub fn new(config: RouterConfig) -> Self {
        let available_agents = Self::discover_agents(&config.agents);
        let mut weights = AgentWeights {
            base_weights: config.weights.clone(),
            ..Default::default()
        };
        weights.ensure_agents_with_weights(&available_agents, &config.weights);
        if !config.weights.is_empty() {
            tracing::info!(
                weights = ?config.weights,
                "configured agent routing weights"
            );
        }
        let router_pool = Self::expand_pool(&config);
        tracing::info!(
            pool = ?router_pool,
            fallback = %config.effective_fallback(),
            "router LLM pool initialized"
        );
        // Prime the RouterConfig free-model cache at startup (sync context) so
        // that async callers hitting expanded_model_pool() later return
        // instantly from cache instead of blocking a Tokio worker thread.
        crate::engine::runner::agents::opencode::prime_free_model_cache();
        let ollama_router = if config.mode == "local" {
            Some(ollama::OllamaRouter::new(&config))
        } else {
            None
        };
        Self {
            config,
            available_agents,
            weights,
            llm_router: std::sync::Arc::new(LlmRouter::new()),
            ollama_router,
            rr_index: 0,
            last_agent: None,
            review_rr_index: 0,
            router_pool,
            pool_index: 0,
            llm_budget_fail_count: 0,
            llm_budget_window_start: None,
            llm_bypass_until: None,
        }
    }

    /// Create a router with default configuration loaded from files.
    pub fn from_config() -> Self {
        Self::new(RouterConfig::from_config())
    }

    /// Return a cloned handle to the LLM router subsystem.
    ///
    /// Callers that need to invoke async methods on the underlying `LlmRouter`
    /// should clone this handle *before* releasing any `RwLock` read guard on
    /// `Router`, then call the async method after the guard is dropped.  This
    /// prevents holding the read lock across await points (which starves
    /// write-lock waiters due to Tokio's write-preferring `RwLock`).
    pub fn llm_router_handle(&self) -> LlmRouterHandle {
        LlmRouterHandle(self.llm_router.clone())
    }

    fn advance_pool_index_after_attempt(&mut self, idx: usize, pool_len: usize) {
        debug_assert!(pool_len > 0, "router pool must not be empty");
        self.pool_index = (idx + 1) % pool_len;
    }

    /// Check whether the router-level LLM bypass is currently active.
    fn llm_bypass_active(&self) -> bool {
        if let Some(until) = self.llm_bypass_until {
            let now = chrono::Utc::now().timestamp();
            return now < until;
        }
        false
    }

    /// Record an LLM routing budget timeout occurrence. If enough timeouts
    /// happen within the configured short window, enable a temporary bypass
    /// that avoids calling LLM routing for a cooldown period.
    fn record_llm_budget_timeout(&mut self) {
        let window_secs = self.config.llm_bypass_window_secs as i64;
        let fail_threshold = self.config.llm_bypass_fail_threshold;
        let bypass_secs = self.config.llm_bypass_duration_secs as i64;

        let now = chrono::Utc::now().timestamp();
        match self.llm_budget_window_start {
            Some(start) if now - start <= window_secs => {
                self.llm_budget_fail_count = self.llm_budget_fail_count.saturating_add(1);
            }
            _ => {
                // reset window
                self.llm_budget_window_start = Some(now);
                self.llm_budget_fail_count = 1;
            }
        }

        if self.llm_budget_fail_count >= fail_threshold {
            self.llm_bypass_until = Some(now + bypass_secs);
            tracing::warn!(
                until = self.llm_bypass_until.unwrap(),
                "router entering short-term LLM bypass due to repeated budget timeouts",
            );
            // reset counters so we don't immediately re-trigger
            self.llm_budget_fail_count = 0;
            self.llm_budget_window_start = None;
        }
    }

    /// Reset the in-memory LLM budget failure counters.
    ///
    /// Called on successful LLM routing so only *consecutive* timeouts
    /// contribute toward triggering the short-horizon bypass.
    fn reset_llm_budget_counters(&mut self) {
        self.llm_budget_fail_count = 0;
        self.llm_budget_window_start = None;
    }

    /// Discover available agent CLIs in PATH.
    /// Checks all agents from the configured list.
    fn discover_agents(configured_agents: &[String]) -> Vec<String> {
        let mut agents = Vec::new();
        for agent in configured_agents {
            if crate::cmd_cache::command_exists(agent) {
                agents.push(agent.to_string());
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
        self.available_agents = new_agents.clone();
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
        crate::engine::runner::agents::opencode::discover_free_opencode_models()
    }

    async fn discover_free_opencode_models_async() -> Vec<String> {
        tokio::task::spawn_blocking(Self::discover_free_opencode_models)
            .await
            .unwrap_or_default()
    }

    /// Run the pre-emptive health check, refreshing the degraded-agent set.
    ///
    /// Queries the `rate_limits` table for recent events and combines with
    /// cooldown state to mark agents as degraded before routing attempts them.
    pub async fn refresh_health(&self, store: &std::sync::Arc<TaskStore>) {
        let config_ref = &self.config;
        let agents = self.available_agents.clone();
        let model_checker = |agent: &str| -> bool {
            // Agent has at least one available model across any complexity tier
            for comp in &["simple", "medium", "complex", "review"] {
                if config_ref.has_available_model_for_complexity(agent, comp) {
                    return true;
                }
            }
            false
        };
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
        // weight has decayed below the configured threshold. This avoids
        // proactively routing to agents that recently hit many rate limits.
        if self.config.weighted_round_robin {
            let weight = self.weights.get_weight(agent);
            if weight < self.config.skip_limited_threshold {
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
            .has_available_model_for_complexity(agent, complexity)
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

            if let Some(comp) = complexity {
                if let Some(pool) = self.config.model_pool_for_complexity(agent, comp) {
                    for model in pool {
                        let key = format!("{agent}:{model}");
                        if let Some(until) = cooldown_until(&key) {
                            earliest = Some(earliest.map_or(until, |current| current.min(until)));
                        }
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

    async fn wait_for_cooldown(&self, complexity: Option<&str>) -> anyhow::Result<()> {
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
            return Err(AllAgentsCooledError::new(scope_str, Some(remaining as u64)).into());
        }

        Ok(())
    }

    /// Get the first available agent.
    /// Pick next agent via round-robin (for review or other non-task routing).
    /// Pick the next review agent, optionally excluding one (e.g. the task's original agent).
    /// Falls back to the excluded agent only if it's the only one available.
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
        store: &std::sync::Arc<TaskStore>,
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
        store: &std::sync::Arc<TaskStore>,
        repo: &str,
    ) -> anyhow::Result<RouteResult> {
        // NOTE: refresh_health() is called once per tick in tick_route_tasks()
        // before the routing loop, not per-task. Do not re-add it here.

        loop {
            // 1. Check store agent field first (set by failover, authoritative over labels)
            // Use a lightweight lookup to avoid deserializing the full Task row when
            // only the `agent` column is needed. This avoids two full-row loads
            // per routing pass (resolve_task_id + get) and significantly reduces
            // CPU work on busy instances.
            let store_agent = match store.get_agent_by_external_id(repo, &task.id.0).await {
                Ok(Some(agent)) => {
                    // Router enforces that the agent string matches a configured agent
                    if !agent.is_empty() && self.config.agents.iter().any(|cfg| cfg == &agent) {
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
                    let model = self
                        .config
                        .model_for_complexity(&agent, &complexity, &task.id.0);

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
                        result.profile.role = format!("{} specialist", agent);
                        result.model = model.clone();
                        self.log_route_activity(store, repo, &task.id.0, &result, None)
                            .await;
                        return Ok(result);
                    } else {
                        // No concrete model found for the labeled agent — do
                        // not honor the label override. Fall through to the
                        // standard routing logic so another agent or the LLM
                        // can be chosen with a valid model.
                        tracing::warn!(task_id = %task.id.0, agent = %agent, "label agent has no available model — falling through to LLM routing");
                    }
                }

                let candidates = self.available_agents_for_complexity(&complexity);
                if candidates.is_empty() {
                    self.wait_for_cooldown(Some(&complexity)).await?;
                    continue;
                }
                // Label target is cooled or lacked model; fall through to standard routing.
            }

            let complexity = strategies::extract_complexity_from_labels(&task.labels);

            // Check if any agents have available models for this complexity
            let mut candidates = self.available_agents_for_complexity(&complexity);
            if candidates.is_empty() {
                self.wait_for_cooldown(Some(&complexity)).await?;
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
            if self.config.mode == "round_robin" {
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
                        // Ollama produced a routing decision — treat this as a
                        // successful LLM routing and reset the short-horizon
                        // LLM budget failure counters so intermittent timeouts
                        // don't accumulate toward the bypass threshold.
                        self.reset_llm_budget_counters();
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
                    self.wait_for_cooldown(Some(&complexity)).await?;
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

            // If router-level LLM bypass is active (short-circuit), skip the
            // LLM cascade entirely and fall back to round-robin. This prevents
            // repeatedly invoking slow/unstable routing LLMs during known
            // degraded periods.
            if self.llm_bypass_active() {
                let until = self.llm_bypass_until.unwrap_or(0);
                tracing::warn!(
                    task_id = %task.id.0,
                    until = until,
                    "LLM routing bypass active — using round-robin instead of LLM routing",
                );
                let candidates = self.available_agents_for_complexity(&complexity);
                if candidates.is_empty() {
                    self.wait_for_cooldown(Some(&complexity)).await?;
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

            let budget = std::time::Duration::from_secs(self.config.llm_budget_secs);
            let llm_result = tokio::time::timeout(budget, self.route_with_llm(task, repo)).await;

            // Budget exceeded: the pool cascade took longer than the total
            // allowed budget (llm_budget_secs). Fall back to round-robin
            // immediately without incrementing route_attempts — this is not a
            // model failure, just a latency problem.
            if let Err(_elapsed) = llm_result {
                tracing::warn!(
                    task_id = %task.id.0,
                    budget_secs = self.config.llm_budget_secs,
                    "LLM routing budget exceeded — falling back to round-robin immediately"
                );
                // Record the budget timeout and possibly enter bypass mode
                self.record_llm_budget_timeout();
                let candidates = self.available_agents_for_complexity(&complexity);
                if candidates.is_empty() {
                    self.wait_for_cooldown(Some(&complexity)).await?;
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

            // Budget not exceeded — unwrap the inner Result<RouteResult, _>
            match llm_result.unwrap() {
                Ok(result) => {
                    let candidates = self.available_agents_for_complexity(&result.complexity);
                    if candidates.is_empty() {
                        self.wait_for_cooldown(Some(&result.complexity)).await?;
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
                            self.wait_for_cooldown(Some(&result.complexity)).await?;
                            continue;
                        }
                        tracing::info!(
                            task_id = %task.id.0,
                            llm_selected = %result.agent,
                            skipped = %no_code_agent,
                            fallback = %fallback_agent,
                            "LLM selected the no-code agent — routing to alternative"
                        );
                        let fallback_model = self.config.model_for_complexity(
                            &fallback_agent,
                            &result.complexity,
                            &task.id.0,
                        );

                        self.set_route_attempts(&task.id.0, 0, store, repo).await;
                        // Reset short-horizon LLM budget counters on this successful
                        // LLM-driven decision (we routed to a fallback agent after
                        // the LLM chose the no-code agent) so intermittent timeouts
                        // don't accumulate toward the bypass threshold.
                        self.reset_llm_budget_counters();
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
                            selected_skills: result.selected_skills.clone(),
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
                        let fallback_model = self.config.model_for_complexity(
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
                            self.wait_for_cooldown(Some(&result.complexity)).await?;
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
                        // Reset the short-horizon LLM counters on this successful
                        // LLM-derived reroute so only consecutive failures count.
                        self.reset_llm_budget_counters();

                        let reason = if fallback_agent == result.agent {
                            format!(
                                "LLM selected cooled model; using same agent {} with different model",
                                fallback_agent
                            )
                        } else {
                            format!(
                                "LLM selected cooled agent/model; rerouted to {}",
                                fallback_agent
                            )
                        };

                        let result = RouteResult {
                            agent: fallback_agent.clone(),
                            model: fallback_model,
                            complexity: result.complexity.clone(),
                            estimate: result.estimate,
                            reason: reason.clone(),
                            profile: result.profile.clone(),
                            selected_skills: result.selected_skills.clone(),
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
                    // Successful LLM routing should reset the short-horizon
                    // llm budget failure counters so only *consecutive* timeouts
                    // contribute to triggering the bypass.
                    self.reset_llm_budget_counters();
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
                        self.wait_for_cooldown(scope_opt).await?;
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
                            self.wait_for_cooldown(Some(&complexity)).await?;
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
                        self.wait_for_cooldown(Some(&complexity)).await?;
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
    async fn get_route_attempts(
        &self,
        task_id: &str,
        store: &std::sync::Arc<TaskStore>,
        repo: &str,
    ) -> u32 {
        match store.resolve_task_id(repo, task_id).await {
            Ok(Some(store_id)) => store
                .get(store_id)
                .await
                .map(|t| t.route_attempts as u32)
                .unwrap_or(0),
            _ => 0,
        }
    }

    /// Set the number of LLM routing attempts for a task in the store.
    async fn set_route_attempts(
        &self,
        task_id: &str,
        attempts: u32,
        store: &std::sync::Arc<TaskStore>,
        repo: &str,
    ) {
        if let Ok(Some(store_id)) = store.resolve_task_id(repo, task_id).await {
            if let Err(e) = store
                .set_fields(store_id, &[("route_attempts", serde_json::json!(attempts))])
                .await
            {
                tracing::warn!(task_id, error = %e, "failed to store route_attempts");
            }
        }
    }

    /// Log a route event to task activity timeline.
    async fn log_route_activity(
        &self,
        store: &std::sync::Arc<TaskStore>,
        repo: &str,
        task_id: &str,
        result: &RouteResult,
        from_agent: Option<&str>,
    ) {
        let details = serde_json::json!({
            "reason": result.reason,
            "complexity": result.complexity,
            "skills": result.selected_skills,
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
        // Filter out cooled and degraded agents so the LLM only sees available ones.
        // Fall back to the full list if all agents are cooled/degraded.
        // Cloned to avoid borrow conflict with &mut self in the loop.
        let uncooled_agents: Vec<String> = self
            .available_agents
            .iter()
            .filter(|a| !is_agent_in_cooldown(a) && !is_agent_degraded(a))
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

            if crate::engine::runner::response::is_model_in_cooldown(agent, model_str) {
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
                    return Ok(result);
                }
                Err(e) => {
                    let err_msg = e.to_string();
                    let is_timeout = err_msg.contains(TIMEOUT_PREFIX);
                    if is_timeout {
                        tracing::warn!(
                            agent,
                            model = model_str,
                            "router LLM pool entry timed out — skipping cooldown, trying next entry"
                        );
                    } else {
                        tracing::warn!(
                            agent,
                            model = model_str,
                            error = %e,
                            "pool entry failed, recording cooldown and trying next"
                        );
                        crate::engine::runner::response::record_model_failure(agent, model_str)
                            .await;
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

        if !fallback_already_tried
            && !crate::engine::runner::response::is_model_in_cooldown(&fb_agent, fb_model_str)
        {
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
                Ok(result) => return Ok(result),
                Err(e) => {
                    let err_msg = e.to_string();
                    let is_timeout = err_msg.contains(TIMEOUT_PREFIX);
                    if is_timeout {
                        tracing::warn!(
                            agent = %fb_agent,
                            model = %fb_model_str,
                            "fallback router LLM timed out — skipping cooldown"
                        );
                    } else {
                        tracing::warn!(
                            agent = %fb_agent,
                            model = %fb_model_str,
                            error = %e,
                            "fallback router LLM also failed"
                        );
                        crate::engine::runner::response::record_model_failure(
                            &fb_agent,
                            fb_model_str,
                        )
                        .await;
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
        store: &std::sync::Arc<TaskStore>,
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
    store: &std::sync::Arc<TaskStore>,
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
        let valid = all_pools.iter().any(|pool| pool.contains(m));
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

    // If a stale model was discarded, update the database to clear the model
    // and record a diagnostic last_error so task_runs.error is populated if
    // the subsequent run fails (fixes issue #2527).
    let original_model = task.model.clone().filter(|m| !m.is_empty());
    if original_model.is_some() && model.is_none() {
        let stale_model = original_model.as_deref().unwrap_or("unknown");
        let stale_err = format!("stale model discarded: {stale_model} not in {agent} model pools");
        if let Err(e) = store
            .set_fields(
                store_id,
                &[
                    ("model", serde_json::json!("")),
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
        profile: AgentProfile::default(),
        selected_skills: vec![],
        warning: None,
    })
}

#[cfg(test)]
mod tests {
    use super::config::DEFAULT_AGENTS;
    use super::llm::LlmRouteResponse;
    use super::strategies;
    use super::weights::{
        AgentWeights, RateLimitState, DEFAULT_WEIGHT, MIN_WEIGHT, RATE_LIMIT_DECAY, RECOVERY_DELAY,
    };
    use super::*;
    use crate::backends::{ExternalId, ExternalTask};
    use crate::engine::cooldown::{
        clear_agent_degraded, is_agent_degraded, is_model_in_cooldown, mark_agent_degraded,
        record_model_failure, set_model_cooldown,
    };
    use crate::store::TaskStore;
    use std::time::{Duration, Instant};

    // Test-only delegates so tests can call router.parse_llm_response() and
    // router.check_routing_sanity() directly without referencing llm_router.
    impl Router {
        fn parse_llm_response(&self, response: &str) -> anyhow::Result<LlmRouteResponse> {
            self.llm_router.parse_llm_response("claude", response)
        }

        fn check_routing_sanity(
            &self,
            task: &ExternalTask,
            agent: &str,
            profile: &AgentProfile,
        ) -> Option<String> {
            self.llm_router.check_routing_sanity(task, agent, profile)
        }
    }

    async fn test_store() -> std::sync::Arc<TaskStore> {
        std::sync::Arc::new(TaskStore::open_memory().await.unwrap())
    }

    fn create_test_task(id: &str, title: &str, labels: Vec<String>) -> ExternalTask {
        ExternalTask {
            id: ExternalId(id.to_string()),
            title: title.to_string(),
            body: "Test body".to_string(),
            state: "open".to_string(),
            labels,
            author: "test".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: format!("https://github.com/test/test/issues/{id}"),
        }
    }

    #[test]
    fn extract_agent_from_labels() {
        let mut config = RouterConfig::default();
        // Ensure a concrete model exists for the labeled agent so the label
        // override is honored by the router (routing requires a model).
        config
            .model_map
            .entry("medium".to_string())
            .or_default()
            .insert("claude".to_string(), vec!["haiku".to_string()]);

        assert_eq!(
            strategies::extract_agent_from_labels(&config.agents, &["agent:claude".to_string()]),
            Some("claude".to_string())
        );
        assert_eq!(
            strategies::extract_agent_from_labels(&config.agents, &["agent:codex".to_string()]),
            Some("codex".to_string())
        );
        assert_eq!(
            strategies::extract_agent_from_labels(&config.agents, &["agent:opencode".to_string()]),
            Some("opencode".to_string())
        );
        assert_eq!(
            strategies::extract_agent_from_labels(&config.agents, &["status:new".to_string()]),
            None
        );
        // Verify kimi and minimax are recognized from labels
        assert_eq!(
            strategies::extract_agent_from_labels(&config.agents, &["agent:kimi".to_string()]),
            Some("kimi".to_string())
        );
        assert_eq!(
            strategies::extract_agent_from_labels(&config.agents, &["agent:minimax".to_string()]),
            Some("minimax".to_string())
        );
    }

    #[test]
    fn default_agents_constant() {
        assert_eq!(DEFAULT_AGENTS.len(), 6);
        assert!(DEFAULT_AGENTS.contains(&"claude"));
        assert!(DEFAULT_AGENTS.contains(&"kimi"));
        assert!(DEFAULT_AGENTS.contains(&"minimax"));
        assert!(DEFAULT_AGENTS.contains(&"glm"));
    }

    #[test]
    fn extract_complexity_from_labels() {
        assert_eq!(
            strategies::extract_complexity_from_labels(&["complexity:simple".to_string()]),
            "simple"
        );
        assert_eq!(
            strategies::extract_complexity_from_labels(&["complexity:medium".to_string()]),
            "medium"
        );
        assert_eq!(
            strategies::extract_complexity_from_labels(&["complexity:complex".to_string()]),
            "complex"
        );
        assert_eq!(
            strategies::extract_complexity_from_labels(&["status:new".to_string()]),
            "medium"
        );
    }

    #[test]
    fn route_result_serialization() {
        let result = RouteResult {
            agent: "claude".to_string(),
            model: Some("sonnet".to_string()),
            complexity: "medium".to_string(),
            estimate: 5,
            reason: "test".to_string(),
            profile: AgentProfile {
                role: "backend".to_string(),
                skills: vec!["rust".to_string()],
                tools: vec!["git".to_string()],
                constraints: vec![],
            },
            selected_skills: vec!["gh".to_string()],
            warning: None,
        };

        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("claude"));
        assert!(json.contains("sonnet"));

        let deserialized: RouteResult = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.agent, "claude");
        assert_eq!(deserialized.model, Some("sonnet".to_string()));
    }

    #[test]
    fn router_config_default() {
        let mut config = RouterConfig::default();
        // Ensure a concrete model exists for the labeled agent so the label
        // override is honored by the router (routing requires a model).
        config
            .model_map
            .entry("medium".to_string())
            .or_default()
            .insert("claude".to_string(), vec!["haiku".to_string()]);

        assert_eq!(config.mode, "llm");
        assert_eq!(config.router_agent, "claude");
        assert_eq!(config.router_model, "claude-haiku-4-5-20251001");
        assert_eq!(config.fallback_executor, "codex");
        assert_eq!(config.max_route_attempts, 3);
        assert!(!config.allowed_tools.is_empty());
        assert!(!config.default_skills.is_empty());

        // Verify configurable agents list includes all 6 agents
        assert_eq!(config.agents.len(), 6);
        assert!(config.agents.contains(&"claude".to_string()));
        assert!(config.agents.contains(&"codex".to_string()));
        assert!(config.agents.contains(&"opencode".to_string()));
        assert!(config.agents.contains(&"kimi".to_string()));
        assert!(config.agents.contains(&"minimax".to_string()));
        assert!(config.agents.contains(&"glm".to_string()));
    }

    #[test]
    fn model_map_lookup_returns_none_without_config() {
        // Default config has no hardcoded models — config.yml is the source of truth.
        let config = RouterConfig::default();
        assert!(config
            .model_for_complexity("claude", "simple", "")
            .is_none());
        assert!(config
            .model_for_complexity("claude", "medium", "")
            .is_none());
        assert!(config
            .model_for_complexity("claude", "complex", "")
            .is_none());
        assert!(config
            .model_for_complexity("opencode", "review", "")
            .is_none());
    }

    #[test]
    fn model_map_lookup_returns_configured_value() {
        // When model_map is populated (from config), it is returned correctly.
        let mut config = RouterConfig::default();
        config
            .model_map
            .entry("simple".to_string())
            .or_default()
            .insert("claude".to_string(), vec!["haiku".to_string()]);
        assert_eq!(
            config.model_for_complexity("claude", "simple", ""),
            Some("haiku".to_string())
        );
    }

    #[tokio::test]
    async fn model_pool_selection_skips_cooled() {
        use std::collections::HashMap;

        let mut config = RouterConfig::default();
        // Set up a two-model pool for opencode/simple
        config
            .model_map
            .entry("simple".to_string())
            .or_default()
            .insert(
                "opencode".to_string(),
                vec!["model-a".to_string(), "model-b".to_string()],
            );

        // Before any cooldown, both models are candidates
        let m = config.model_for_complexity("opencode", "simple", "task-1");
        assert!(m == Some("model-a".to_string()) || m == Some("model-b".to_string()));

        // Cool model-a
        record_model_failure("opencode", "model-a").await;
        assert!(is_model_in_cooldown("opencode", "model-a"));

        // Now only model-b should be returned
        // (try many task_ids to rule out hash coincidence)
        for i in 0..20 {
            let result = config.model_for_complexity("opencode", "simple", &i.to_string());
            assert_eq!(
                result,
                Some("model-b".to_string()),
                "expected model-b when model-a is cooled, got {result:?} for task_id={i}"
            );
        }

        // Cool model-b too — now all models are cooled
        record_model_failure("opencode", "model-b").await;
        // All cooled → return None so the caller can fall back to a different agent
        let fallback = config.model_for_complexity("opencode", "simple", "task-fallback");
        assert_eq!(fallback, None);
        let _ = HashMap::<(), ()>::new(); // suppress unused import lint
    }

    #[test]
    fn model_pool_single_string_backward_compat() {
        // Single-item pools behave identically to the old string format.
        // Manually insert a single-item pool and verify it is returned correctly.
        let mut config = RouterConfig::default();
        config
            .model_map
            .entry("simple".to_string())
            .or_default()
            .insert("claude".to_string(), vec!["haiku".to_string()]);
        let m = config.model_for_complexity("claude", "simple", "any-task");
        assert_eq!(m, Some("haiku".to_string()));
    }

    #[test]
    fn parse_llm_response_direct_json() {
        let config = RouterConfig::default();
        let router = Router::new(config);

        let response = r#"{
            "executor": "claude",
            "complexity": "complex",
            "reason": "requires architecture analysis",
            "profile": {
                "role": "architect",
                "skills": ["rust", "design"],
                "tools": ["git", "rg"],
                "constraints": []
            },
            "selected_skills": ["gh"]
        }"#;

        let parsed = router.parse_llm_response(response).unwrap();
        assert_eq!(parsed.executor, "claude");
        assert_eq!(parsed.complexity, "complex");
        assert_eq!(parsed.reason, "requires architecture analysis");
        assert_eq!(parsed.profile.role, "architect");
    }

    #[test]
    fn parse_llm_response_markdown_fenced() {
        let config = RouterConfig::default();
        let router = Router::new(config);

        let response = r#"Here's my analysis:

```json
{
    "executor": "codex",
    "complexity": "medium",
    "reason": "coding task",
    "profile": {
        "role": "developer",
        "skills": ["coding"],
        "tools": [],
        "constraints": []
    },
    "selected_skills": []
}
```

Hope that helps!"#;

        let parsed = router.parse_llm_response(response).unwrap();
        assert_eq!(parsed.executor, "codex");
        assert_eq!(parsed.complexity, "medium");
    }

    #[test]
    fn parse_llm_response_claude_envelope() {
        let config = RouterConfig::default();
        let router = Router::new(config);

        // Claude --output-format json wraps the response in an envelope
        let response = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":1500,"result":"{\"executor\":\"claude\",\"complexity\":\"complex\",\"reason\":\"requires deep analysis\",\"profile\":{\"role\":\"architect\",\"skills\":[],\"tools\":[],\"constraints\":[]},\"selected_skills\":[]}","usage":{"input_tokens":100,"output_tokens":50}}"#;

        let parsed = router.parse_llm_response(response).unwrap();
        assert_eq!(parsed.executor, "claude");
        assert_eq!(parsed.complexity, "complex");
        assert_eq!(parsed.reason, "requires deep analysis");
    }

    #[test]
    fn parse_llm_response_claude_envelope_with_code_block() {
        let config = RouterConfig::default();
        let router = Router::new(config);

        // Claude may return the JSON inside a code block within the envelope
        let inner = "```json\n{\"executor\":\"codex\",\"complexity\":\"simple\",\"reason\":\"simple fix\",\"profile\":{\"role\":\"developer\",\"skills\":[],\"tools\":[],\"constraints\":[]},\"selected_skills\":[]}\n```";
        let envelope = serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": inner,
            "usage": {"input_tokens": 50, "output_tokens": 30}
        });

        let parsed = router.parse_llm_response(&envelope.to_string()).unwrap();
        assert_eq!(parsed.executor, "codex");
        assert_eq!(parsed.complexity, "simple");
    }

    #[test]
    fn parse_llm_response_claude_envelope_error() {
        let config = RouterConfig::default();
        let router = Router::new(config);

        let response = r#"{"type":"result","subtype":"error","is_error":true,"result":"rate limit exceeded","usage":{}}"#;

        let err = router.parse_llm_response(response).unwrap_err();
        assert!(err.to_string().contains("error"), "got: {err}");
    }

    #[test]
    fn parse_llm_response_empty() {
        let config = RouterConfig::default();
        let router = Router::new(config);

        let err = router.parse_llm_response("").unwrap_err();
        assert!(err.to_string().contains("empty"), "got: {err}");
    }

    /// Claude envelope where result is a JSON object, not a string.
    /// Some Claude versions may return the result as a nested object.
    #[test]
    fn parse_llm_response_claude_envelope_result_as_object() {
        let config = RouterConfig::default();
        let router = Router::new(config);

        let response = r#"{"type":"result","subtype":"success","is_error":false,"result":{"executor":"claude","complexity":"medium","reason":"multi-file feature","profile":{"role":"developer","skills":[],"tools":[],"constraints":[]},"selected_skills":[]},"usage":{"input_tokens":100,"output_tokens":50}}"#;

        let parsed = router.parse_llm_response(response);
        assert!(
            parsed.is_ok(),
            "should handle result-as-object: {}",
            parsed.unwrap_err()
        );
        assert_eq!(parsed.unwrap().executor, "claude");
    }

    /// LLM returns "agent" instead of "executor" — common hallucination.
    #[test]
    fn parse_llm_response_agent_instead_of_executor() {
        let config = RouterConfig::default();
        let router = Router::new(config);

        let response = r#"{"agent":"claude","complexity":"medium","reason":"multi-file feature","profile":{"role":"developer","skills":[],"tools":[],"constraints":[]},"selected_skills":[]}"#;

        let parsed = router.parse_llm_response(response);
        assert!(
            parsed.is_ok(),
            "should accept 'agent' alias: {}",
            parsed.unwrap_err()
        );
        assert_eq!(parsed.unwrap().executor, "claude");
    }

    /// LLM returns minimal JSON without profile or reason.
    #[test]
    fn parse_llm_response_minimal_json() {
        let config = RouterConfig::default();
        let router = Router::new(config);

        let response = r#"{"executor":"codex","complexity":"simple"}"#;

        let parsed = router.parse_llm_response(response);
        assert!(
            parsed.is_ok(),
            "should accept minimal JSON: {}",
            parsed.unwrap_err()
        );
        assert_eq!(parsed.unwrap().executor, "codex");
    }

    // ── Real fixture tests: route prompt → response → parsed result ──

    /// Real Claude envelope with result as escaped JSON string.
    #[test]
    fn fixture_route_response_string() {
        let config = RouterConfig::default();
        let router = Router::new(config);

        let response = include_str!("../../../tests/fixtures/route-response-string.json");
        let parsed = router.parse_llm_response(response).unwrap();
        assert_eq!(parsed.executor, "codex");
        assert_eq!(parsed.complexity, "medium");
        assert!(!parsed.reason.is_empty());
        assert!(!parsed.profile.role.is_empty());
    }

    /// Real Claude envelope with result as JSON object (not string).
    /// This was the root cause of all router failures on 2026-02-27.
    #[test]
    fn fixture_route_response_object() {
        let config = RouterConfig::default();
        let router = Router::new(config);

        let response = include_str!("../../../tests/fixtures/route-response-object.json");
        let parsed = router.parse_llm_response(response).unwrap();
        assert_eq!(parsed.executor, "claude");
        assert_eq!(parsed.complexity, "complex");
    }

    /// Real Claude envelope with result as string containing markdown + JSON.
    #[test]
    fn fixture_route_response_markdown() {
        let config = RouterConfig::default();
        let router = Router::new(config);

        let response = include_str!("../../../tests/fixtures/route-response-markdown.json");
        let parsed = router.parse_llm_response(response).unwrap();
        assert_eq!(parsed.executor, "codex");
        assert_eq!(parsed.complexity, "medium");
    }

    /// Claude envelope with escaped JSON string containing newlines and markdown.
    #[test]
    fn parse_llm_response_claude_envelope_with_prose() {
        let config = RouterConfig::default();
        let router = Router::new(config);

        let inner = "Based on the task description, here's my routing decision:\n\n```json\n{\"executor\":\"codex\",\"complexity\":\"medium\",\"reason\":\"standard feature work\",\"profile\":{\"role\":\"developer\",\"skills\":[\"rust\"],\"tools\":[\"git\",\"cargo\"],\"constraints\":[]},\"selected_skills\":[]}\n```\n\nThis task involves adding slash commands which is standard development work.";
        let envelope = serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": inner,
            "usage": {"input_tokens": 100, "output_tokens": 80}
        });

        let parsed = router.parse_llm_response(&envelope.to_string());
        assert!(
            parsed.is_ok(),
            "should handle prose + code block in envelope: {}",
            parsed.unwrap_err()
        );
        assert_eq!(parsed.unwrap().executor, "codex");
    }

    #[tokio::test]
    async fn route_round_robin_basic() {
        // Force at least one agent to be available for testing
        // In real usage, discover_agents finds installed CLIs
        let config = RouterConfig {
            mode: "round_robin".to_string(),
            ..Default::default()
        };

        // Create router with mock available agents
        let agents = vec!["claude".to_string(), "codex".to_string()];
        let mut weights = AgentWeights::default();
        weights.ensure_agents(&agents);
        let router = Router {
            config,
            available_agents: agents,
            weights,
            llm_router: std::sync::Arc::new(LlmRouter::new()),
            ollama_router: None,
            rr_index: 0,
            last_agent: None,
            review_rr_index: 0,
            router_pool: vec![],
            pool_index: 0,
            llm_budget_fail_count: 0,
            llm_budget_window_start: None,
            llm_bypass_until: None,
        };

        let task = create_test_task("1", "Test task", vec![]);
        let result =
            strategies::route_via_round_robin(&router.available_agents, &router.config, &task)
                .unwrap();

        // Task 1 % 2 agents = agent at index 1 = codex
        assert_eq!(result.agent, "codex");
        assert_eq!(result.reason, "round_robin (task 1 % 2 agents)");
    }

    #[tokio::test]
    async fn route_uses_label_override() {
        let mut config = RouterConfig::default();
        // Ensure a concrete model exists so the label override can be honored
        // in the test environment where no external config.yml is present.
        config
            .model_map
            .entry("medium".to_string())
            .or_default()
            .insert("claude".to_string(), vec!["haiku".to_string()]);
        let agents = vec!["claude".to_string(), "codex".to_string()];
        let mut weights = AgentWeights::default();
        weights.ensure_agents(&agents);
        let mut router = Router {
            config,
            available_agents: agents,
            weights,
            llm_router: std::sync::Arc::new(LlmRouter::new()),
            ollama_router: None,
            rr_index: 0,
            last_agent: None,
            review_rr_index: 0,
            router_pool: vec![],
            pool_index: 0,
            llm_budget_fail_count: 0,
            llm_budget_window_start: None,
            llm_bypass_until: None,
        };

        let task = create_test_task("1", "Test", vec!["agent:claude".to_string()]);
        let store = test_store().await;

        // Should use label override, not LLM
        let result = router.route(&task, &store, "test/repo").await.unwrap();
        assert_eq!(result.agent, "claude");
        assert!(result.reason.contains("label"));
    }

    #[test]
    fn check_routing_sanity_warnings() {
        let config = RouterConfig::default();
        let router = Router::new(config);

        // Backend task routed to claude should warn
        let task = create_test_task("1", "Fix API", vec!["backend".to_string()]);
        let profile = AgentProfile {
            role: "general".to_string(),
            skills: vec!["api".to_string()],
            tools: vec![],
            constraints: vec![],
        };
        let warning = router.check_routing_sanity(&task, "claude", &profile);
        assert!(warning.is_some());
        assert!(warning.unwrap().contains("backend"));

        // Docs task routed to codex should warn
        let task = create_test_task("1", "Update README", vec!["docs".to_string()]);
        let warning = router.check_routing_sanity(&task, "codex", &profile);
        assert!(warning.is_some());
        assert!(warning.unwrap().contains("docs"));

        // Normal routing should not warn
        let task = create_test_task("1", "Fix bug", vec!["bug".to_string()]);
        let warning = router.check_routing_sanity(&task, "codex", &profile);
        assert!(warning.is_none());
    }

    #[tokio::test]
    async fn router_reload_preserves_structure() {
        let config = RouterConfig::default();
        let agents = vec!["claude".to_string()];
        let mut weights = AgentWeights::default();
        weights.ensure_agents(&agents);
        let mut router = Router {
            config,
            available_agents: agents,
            weights,
            llm_router: std::sync::Arc::new(LlmRouter::new()),
            ollama_router: None,
            rr_index: 0,
            last_agent: None,
            review_rr_index: 0,
            router_pool: vec![],
            pool_index: 0,
            llm_budget_fail_count: 0,
            llm_budget_window_start: None,
            llm_bypass_until: None,
        };

        // Reload — should re-read config and remain valid
        router.reload_async().await;

        // After reload, mode should be a valid value (llm, local, or round_robin)
        assert!(
            router.config.mode == "llm"
                || router.config.mode == "round_robin"
                || router.config.mode == "local",
            "mode should be 'llm', 'local', or 'round_robin', got '{}'",
            router.config.mode
        );
        // Fallback executor should always be set
        assert!(!router.config.fallback_executor.is_empty());
        // Tools should always be populated
        assert!(!router.config.allowed_tools.is_empty());
    }

    // --- Weighted round-robin tests ---

    #[test]
    fn rate_limit_state_defaults_to_full_weight() {
        let state = RateLimitState::default();
        assert_eq!(state.weight, DEFAULT_WEIGHT);
        assert_eq!(state.consecutive_hits, 0);
        assert!(state.last_limited_at.is_none());
        assert!(!state.is_limited());
    }

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

    #[test]
    fn rate_limit_state_success_caps_at_default() {
        let mut state = RateLimitState::default();

        // Already at full weight, success shouldn't exceed it
        state.record_success();
        assert_eq!(state.weight, DEFAULT_WEIGHT);
    }

    #[test]
    fn agent_weights_ensure_agents() {
        let mut weights = AgentWeights::default();
        let agents = vec!["claude".to_string(), "codex".to_string()];
        weights.ensure_agents(&agents);

        assert_eq!(weights.states.len(), 2);
        assert_eq!(weights.get_weight("claude"), DEFAULT_WEIGHT);
        assert_eq!(weights.get_weight("codex"), DEFAULT_WEIGHT);
    }

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
    fn agent_weights_record_rate_limit() {
        let mut weights = AgentWeights::default();
        weights.ensure_agents(&["claude".to_string(), "codex".to_string()]);

        weights.record_rate_limit("claude");

        assert!(weights.get_weight("claude") < DEFAULT_WEIGHT);
        assert_eq!(weights.get_weight("codex"), DEFAULT_WEIGHT);
    }

    #[test]
    fn agent_weights_record_success() {
        let mut weights = AgentWeights::default();
        weights.ensure_agents(&["claude".to_string()]);

        weights.record_rate_limit("claude");
        let after_limit = weights.get_weight("claude");

        weights.record_success("claude");
        assert!(weights.get_weight("claude") > after_limit);
    }

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

    #[test]
    fn agent_weights_weighted_select_empty_returns_none() {
        let weights = AgentWeights::default();
        assert!(weights.weighted_select(&[], "task-1").is_none());
    }

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

    #[test]
    fn agent_weights_weighted_select_single_agent() {
        let mut weights = AgentWeights::default();
        let agents = vec!["claude".to_string()];
        weights.ensure_agents(&agents);

        let selected = weights.weighted_select(&agents, "task-1");
        assert_eq!(selected, Some("claude".to_string()));
    }

    #[test]
    fn agent_weights_tick_recovery_restores_weight() {
        let mut weights = AgentWeights::default();
        weights.ensure_agents(&["claude".to_string()]);

        // Set up a rate limit that looks like it happened long ago
        let state = weights.states.get_mut("claude").unwrap();
        state.weight = 0.5;
        state.last_limited_at = Some(Instant::now() - RECOVERY_DELAY - Duration::from_secs(1));
        state.consecutive_hits = 2;

        let before = weights.get_weight("claude");
        weights.tick_recovery();
        let after = weights.get_weight("claude");

        assert!(after > before, "weight should increase after recovery tick");
    }

    #[test]
    fn agent_weights_tick_recovery_clears_on_full_restore() {
        let mut weights = AgentWeights::default();
        weights.ensure_agents(&["claude".to_string()]);

        // Set weight just below full with an old limit
        let state = weights.states.get_mut("claude").unwrap();
        state.weight = DEFAULT_WEIGHT - 0.01;
        state.last_limited_at = Some(Instant::now() - RECOVERY_DELAY - Duration::from_secs(1));
        state.consecutive_hits = 1;

        weights.tick_recovery();

        let state = weights.states.get("claude").unwrap();
        assert_eq!(state.weight, DEFAULT_WEIGHT);
        assert!(state.last_limited_at.is_none());
        assert_eq!(state.consecutive_hits, 0);
    }

    #[tokio::test]
    async fn route_weighted_round_robin_basic() {
        let mut config = RouterConfig {
            weighted_round_robin: true,
            ..Default::default()
        };
        // Provide a concrete model for codex so label override can be used.
        config
            .model_map
            .entry("medium".to_string())
            .or_default()
            .insert("codex".to_string(), vec!["gpt-5.2".to_string()]);

        let agents = vec!["claude".to_string(), "codex".to_string()];
        let mut weights = AgentWeights::default();
        weights.ensure_agents(&agents);
        let mut router = Router {
            config,
            available_agents: agents,
            weights,
            llm_router: std::sync::Arc::new(LlmRouter::new()),
            ollama_router: None,
            rr_index: 0,
            last_agent: None,
            review_rr_index: 0,
            router_pool: vec![],
            pool_index: 0,
            llm_budget_fail_count: 0,
            llm_budget_window_start: None,
            llm_bypass_until: None,
        };

        let task = create_test_task("1", "Test task", vec![]);
        let store = test_store().await;
        let result = router.route(&task, &store, "test/repo").await.unwrap();

        // Should use weighted_round_robin
        assert!(result.reason.contains("weighted_round_robin"));
        assert!(
            result.agent == "claude" || result.agent == "codex",
            "agent should be claude or codex, got '{}'",
            result.agent
        );
    }

    #[tokio::test]
    async fn route_weighted_round_robin_respects_label_override() {
        let mut config = RouterConfig {
            weighted_round_robin: true,
            ..Default::default()
        };
        // Provide a concrete model for codex so label override can be used.
        config
            .model_map
            .entry("medium".to_string())
            .or_default()
            .insert("codex".to_string(), vec!["gpt-5.2".to_string()]);

        let agents = vec!["claude".to_string(), "codex".to_string()];
        let mut weights = AgentWeights::default();
        weights.ensure_agents(&agents);
        let mut router = Router {
            config,
            available_agents: agents,
            weights,
            llm_router: std::sync::Arc::new(LlmRouter::new()),
            ollama_router: None,
            rr_index: 0,
            last_agent: None,
            review_rr_index: 0,
            router_pool: vec![],
            pool_index: 0,
            llm_budget_fail_count: 0,
            llm_budget_window_start: None,
            llm_bypass_until: None,
        };

        // Label override should take precedence over weighted routing
        let task = create_test_task("1", "Test task", vec!["agent:codex".to_string()]);
        let store = test_store().await;
        let result = router.route(&task, &store, "test/repo").await.unwrap();
        assert_eq!(result.agent, "codex");
        assert!(result.reason.contains("label"));
    }

    #[test]
    fn router_config_weighted_round_robin_default_false() {
        let config = RouterConfig::default();
        assert!(!config.weighted_round_robin);
    }

    #[test]
    fn router_record_and_recover_weights() {
        let config = RouterConfig {
            weighted_round_robin: true,
            ..Default::default()
        };
        let mut router = Router::new(config);
        // Override discovered agents for test
        router.available_agents = vec!["claude".to_string(), "codex".to_string()];
        router.weights.ensure_agents(&router.available_agents);

        // Record rate limit
        router.record_rate_limit("claude");
        assert!(router.weights.get_weight("claude") < DEFAULT_WEIGHT);
        assert_eq!(router.weights.get_weight("codex"), DEFAULT_WEIGHT);

        // Record success for claude
        router.record_success("claude");
        let after_success = router.weights.get_weight("claude");
        assert!(after_success > MIN_WEIGHT);
    }

    #[test]
    fn review_rr_index_advances() {
        let mut config = RouterConfig::default();
        // Set up model_map so has_available_model_for_complexity returns true for test agents
        for agent in &["test_a", "test_b", "test_c"] {
            config
                .model_map
                .entry("medium".to_string())
                .or_default()
                .insert((*agent).to_string(), vec!["test-model".to_string()]);
        }
        let agents = vec![
            "test_a".to_string(),
            "test_b".to_string(),
            "test_c".to_string(),
        ];
        let mut weights = AgentWeights::default();
        weights.ensure_agents(&agents);
        let mut router = Router {
            config,
            available_agents: agents,
            weights,
            llm_router: std::sync::Arc::new(LlmRouter::new()),
            ollama_router: None,
            rr_index: 0,
            last_agent: None,
            review_rr_index: 0,
            router_pool: vec![],
            pool_index: 0,
            llm_budget_fail_count: 0,
            llm_budget_window_start: None,
            llm_bypass_until: None,
        };

        let a1 = router.next_round_robin_agent(&[], "medium").unwrap();
        let a2 = router.next_round_robin_agent(&[], "medium").unwrap();
        let a3 = router.next_round_robin_agent(&[], "medium").unwrap();

        // All three agents should appear (order depends on start index)
        let mut seen = vec![a1, a2, a3];
        seen.sort();
        assert_eq!(seen, vec!["test_a", "test_b", "test_c"]);
    }

    #[test]
    fn review_rr_fallback_with_cooled_agent() {
        // Regression test for #2428: when one agent is in cooldown (candidates.len() <
        // available_agents.len()), review_rr_index can exceed candidates.len()-1 causing
        // candidates.get(idx) to return None even though candidates is non-empty.
        let mut config = RouterConfig::default();
        // Register 3 agents in model_map but only 2 will be "available" (simulate cooldown)
        for agent in &["test_a", "test_b", "test_c"] {
            config
                .model_map
                .entry("medium".to_string())
                .or_default()
                .insert((*agent).to_string(), vec!["test-model".to_string()]);
        }
        // Only 2 candidates available (test_c is in cooldown / not in available_agents)
        let agents = vec!["test_a".to_string(), "test_b".to_string()];
        let mut weights = AgentWeights::default();
        weights.ensure_agents(&agents);
        let mut router = Router {
            config,
            available_agents: agents,
            weights,
            llm_router: std::sync::Arc::new(LlmRouter::new()),
            ollama_router: None,
            rr_index: 0,
            last_agent: None,
            // Start index at 2, which is >= candidates.len() (2), triggering the bug
            review_rr_index: 2,
            router_pool: vec![],
            pool_index: 0,
            llm_budget_fail_count: 0,
            llm_budget_window_start: None,
            llm_bypass_until: None,
        };

        // All candidates excluded — should fall back to first candidate, not None
        let result = router.next_round_robin_agent(&["test_a", "test_b"], "medium");
        assert!(
            result.is_some(),
            "next_round_robin_agent must not return None when candidates is non-empty"
        );
    }

    #[test]
    fn review_rr_index_uses_candidates_len_not_available_len() {
        // Regression test for #2449: review_rr_index must advance modulo candidates.len(),
        // not available_agents.len(). When an agent has no model for the complexity,
        // candidates.len() < available_agents.len() and the old code produced a drifting
        // index that caused non-uniform distribution.
        let mut config = RouterConfig::default();
        // Only register models for test_a and test_b — test_c has no model, so it is
        // filtered out by has_available_model_for_complexity → candidates.len() == 2.
        for agent in &["test_a", "test_b"] {
            config
                .model_map
                .entry("medium".to_string())
                .or_default()
                .insert((*agent).to_string(), vec!["test-model".to_string()]);
        }
        let agents = vec![
            "test_a".to_string(),
            "test_b".to_string(),
            "test_c".to_string(), // available but no model → filtered out of candidates
        ];
        let mut weights = AgentWeights::default();
        weights.ensure_agents(&agents);
        let mut router = Router {
            config,
            available_agents: agents,
            weights,
            llm_router: std::sync::Arc::new(LlmRouter::new()),
            ollama_router: None,
            rr_index: 0,
            last_agent: None,
            review_rr_index: 0,
            router_pool: vec![],
            pool_index: 0,
            llm_budget_fail_count: 0,
            llm_budget_window_start: None,
            llm_bypass_until: None,
        };

        // With the bug: index advances modulo 3 (available_agents.len()), so the
        // sequence would be idx=0→1, idx=1→2, idx=2→0, meaning selection is
        // candidates[0%2]=test_a, candidates[1%2]=test_b, candidates[2%2]=test_a —
        // looks uniform by accident, but adding a 4th available agent breaks it.
        //
        // The fix: index advances modulo 2 (candidates.len()), giving a clean 0→1→0→1
        // cycle. Verify this by checking review_rr_index after each call.

        let r1 = router.next_round_robin_agent(&[], "medium").unwrap();
        assert_eq!(
            router.review_rr_index, 1,
            "index should be 1 after first call"
        );

        let r2 = router.next_round_robin_agent(&[], "medium").unwrap();
        assert_eq!(
            router.review_rr_index, 0,
            "index should wrap to 0 after second call"
        );

        let r3 = router.next_round_robin_agent(&[], "medium").unwrap();
        assert_eq!(
            router.review_rr_index, 1,
            "index should be 1 after third call"
        );

        // Agents must alternate uniformly
        assert_ne!(r1, r2, "consecutive calls should select different agents");
        assert_eq!(r1, r3, "third call should match first (clean cycle)");
    }

    #[tokio::test]
    async fn last_agent_tracks_routing() {
        let mut config = RouterConfig {
            mode: "round_robin".to_string(),
            ..Default::default()
        };
        config
            .model_map
            .entry("medium".to_string())
            .or_default()
            .insert("test_x".to_string(), vec!["test-model-x".to_string()]);
        config
            .model_map
            .entry("medium".to_string())
            .or_default()
            .insert("test_y".to_string(), vec!["test-model-y".to_string()]);
        let agents = vec!["test_x".to_string(), "test_y".to_string()];
        let mut weights = AgentWeights::default();
        weights.ensure_agents(&agents);
        let mut router = Router {
            config,
            available_agents: agents,
            weights,
            llm_router: std::sync::Arc::new(LlmRouter::new()),
            ollama_router: None,
            rr_index: 0,
            last_agent: None,
            review_rr_index: 0,
            router_pool: vec![],
            pool_index: 0,
            llm_budget_fail_count: 0,
            llm_budget_window_start: None,
            llm_bypass_until: None,
        };

        assert!(router.last_agent.is_none());

        let task = create_test_task("1", "Test routing", vec![]);
        let store = test_store().await;
        let result = router.route(&task, &store, "test/repo").await.unwrap();

        assert!(router.last_agent.is_some());
        assert_eq!(router.last_agent.as_deref(), Some(result.agent.as_str()));
    }

    // ── pool config ───────────────────────────────────────────────────────────

    #[test]
    fn parse_pool_entry_splits_on_first_colon() {
        let (agent, model) = super::parse_pool_entry("claude:haiku");
        assert_eq!(agent, "claude");
        assert_eq!(model, "haiku");
    }

    #[test]
    fn parse_pool_entry_model_with_slash() {
        // Models like "github-copilot/gpt-5-mini" must not be split on the slash
        let (agent, model) = super::parse_pool_entry("opencode:github-copilot/gpt-5-mini");
        assert_eq!(agent, "opencode");
        assert_eq!(model, "github-copilot/gpt-5-mini");
    }

    #[test]
    fn parse_pool_entry_no_colon() {
        let (agent, model) = super::parse_pool_entry("claude");
        assert_eq!(agent, "claude");
        assert_eq!(model, "");
    }

    #[test]
    fn effective_pool_uses_configured_pool() {
        let config = RouterConfig {
            pool: vec!["kimi:k2p5".to_string(), "claude:haiku".to_string()],
            ..RouterConfig::default()
        };
        let pool = config.effective_pool();
        assert_eq!(pool, vec!["kimi:k2p5", "claude:haiku"]);
    }

    #[test]
    fn effective_pool_falls_back_to_router_agent_model() {
        let config = RouterConfig::default();
        // Default has no pool → derives from router_agent:router_model
        let pool = config.effective_pool();
        assert_eq!(pool.len(), 1);
        assert!(pool[0].starts_with(&config.router_agent));
        assert!(pool[0].contains(&config.router_model));
    }

    #[test]
    fn effective_fallback_uses_configured_fallback() {
        let config = RouterConfig {
            fallback: "claude:haiku".to_string(),
            ..RouterConfig::default()
        };
        assert_eq!(config.effective_fallback(), "claude:haiku");
    }

    #[test]
    fn effective_fallback_derives_from_router_agent_model() {
        let config = RouterConfig::default();
        let fallback = config.effective_fallback();
        assert!(fallback.starts_with(&config.router_agent));
        assert!(fallback.contains(&config.router_model));
    }

    // ── pool expansion ────────────────────────────────────────────────────────

    #[test]
    fn expand_pool_basic_entries() {
        let config = RouterConfig {
            pool: vec!["claude:haiku".to_string(), "kimi:k2p5".to_string()],
            ..RouterConfig::default()
        };
        let expanded = Router::expand_pool(&config);
        assert_eq!(expanded.len(), 2);
        assert_eq!(expanded[0], ("claude".to_string(), "haiku".to_string()));
        assert_eq!(expanded[1], ("kimi".to_string(), "k2p5".to_string()));
    }

    #[test]
    fn expand_pool_uses_effective_pool_when_pool_is_empty() {
        let config = RouterConfig {
            pool: vec![],
            router_agent: "claude".to_string(),
            router_model: "haiku".to_string(),
            ..RouterConfig::default()
        };
        let expanded = Router::expand_pool(&config);
        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0], ("claude".to_string(), "haiku".to_string()));
    }

    #[test]
    fn expand_pool_opencode_free_skipped_when_not_installed() {
        // `opencode` is unlikely to be in the test environment's PATH.
        // If it's absent the entry should be silently skipped and the pool
        // should fall back to the effective_fallback entry.
        if crate::cmd_cache::command_exists("opencode") {
            // If opencode IS installed we can't control what it returns, so skip
            return;
        }
        let config = RouterConfig {
            pool: vec!["opencode:free".to_string()],
            fallback: "claude:haiku".to_string(),
            ..RouterConfig::default()
        };
        let expanded = Router::expand_pool(&config);
        // Should fall back to the fallback entry since all pool entries were skipped
        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0].0, "claude");
        assert_eq!(expanded[0].1, "haiku");
    }

    #[test]
    fn expand_pool_single_entry_from_default() {
        let config = RouterConfig::default();
        let expanded = Router::expand_pool(&config);
        assert!(
            !expanded.is_empty(),
            "pool must never be empty after expansion"
        );
    }

    // ── pool round-robin index ────────────────────────────────────────────────

    #[test]
    fn pool_index_initializes_to_zero() {
        let config = RouterConfig::default();
        let router = Router::new(config);
        assert_eq!(router.pool_index, 0);
    }

    #[test]
    fn router_pool_non_empty_after_construction() {
        let config = RouterConfig::default();
        let router = Router::new(config);
        assert!(!router.router_pool.is_empty());
    }

    #[tokio::test]
    async fn reload_resets_pool_index() {
        let config = RouterConfig::default();
        let mut router = Router::new(config);
        router.pool_index = 5;
        router.reload_async().await;
        assert_eq!(router.pool_index, 0);
    }

    #[test]
    fn pool_index_advances_after_failed_attempt() {
        let mut router = Router::new(RouterConfig::default());
        router.router_pool = vec![
            ("claude".to_string(), "haiku".to_string()),
            ("codex".to_string(), "gpt-5.2".to_string()),
            (
                "opencode".to_string(),
                "github-copilot/gpt-5-mini".to_string(),
            ),
        ];
        router.pool_index = 0;

        router.advance_pool_index_after_attempt(0, router.router_pool.len());

        assert_eq!(router.pool_index, 1);
    }

    #[test]
    fn pool_index_wraps_after_last_attempt() {
        let mut router = Router::new(RouterConfig::default());
        router.router_pool = vec![
            ("claude".to_string(), "haiku".to_string()),
            ("codex".to_string(), "gpt-5.2".to_string()),
            (
                "opencode".to_string(),
                "github-copilot/gpt-5-mini".to_string(),
            ),
        ];
        router.pool_index = 2;

        router.advance_pool_index_after_attempt(2, router.router_pool.len());

        assert_eq!(router.pool_index, 0);
    }

    #[test]
    fn earliest_pool_cooldown_returns_none_when_no_cooldowns() {
        let router = Router::new(RouterConfig::default());
        assert!(router.earliest_pool_cooldown().is_none());
    }

    #[tokio::test]
    async fn earliest_pool_cooldown_returns_earliest_when_set() {
        // Use explicit pool with predictable model names to avoid opencode discovery.
        let config = RouterConfig {
            pool: vec![
                "claude:haiku-4-5-20251001".to_string(),
                "opencode:haiku-4-5-20251001".to_string(),
            ],
            ..Default::default()
        };
        let router = Router::new(config);
        set_model_cooldown("claude", "haiku-4-5-20251001", 300).await; // 5 min
        set_model_cooldown("opencode", "haiku-4-5-20251001", 600).await; // 10 min

        let earliest = router.earliest_pool_cooldown();
        assert!(earliest.is_some());
        let now = chrono::Utc::now().timestamp();
        let remaining = earliest.unwrap() - now;
        // Should be close to 300 (5 min, the minimum) — allow 5s tolerance for test runtime
        assert!((295..=300).contains(&remaining));
    }

    #[tokio::test]
    async fn earliest_pool_cooldown_skips_non_pool_entries() {
        // Use explicit pool so we can predict what entries exist.
        let config = RouterConfig {
            pool: vec![
                "claude:pool-model-a".to_string(),
                "opencode:pool-model-b".to_string(),
            ],
            ..Default::default()
        };
        let router = Router::new(config);
        // Set cooldown on a non-pool model — should not affect earliest_pool_cooldown
        set_model_cooldown("claude", "opus", 999).await;
        // Set cooldown on a pool entry (opencode:pool-model-b)
        set_model_cooldown("opencode", "pool-model-b", 300).await;

        let earliest = router.earliest_pool_cooldown();
        assert!(earliest.is_some());
        let now = chrono::Utc::now().timestamp();
        let remaining = earliest.unwrap() - now;
        // Should be ~300 (the pool-model-b cooldown), NOT 999 (claude:opus is not in pool)
        assert!((295..=300).contains(&remaining));
    }

    // ---- Pre-emptive health check: degraded agent exclusion ----

    #[test]
    fn degraded_agent_excluded_from_routing() {
        let mut config = RouterConfig::default();
        config
            .model_map
            .entry("medium".to_string())
            .or_default()
            .insert(
                "test_degraded_routing".to_string(),
                vec!["model-a".to_string()],
            );

        let mut router = Router::new(config);
        router.available_agents = vec!["test_degraded_routing".to_string()];

        // Agent is routable before degradation
        assert!(router.agent_is_routable("test_degraded_routing", "medium"));

        // Mark as degraded
        mark_agent_degraded("test_degraded_routing");

        // Now excluded from routing
        assert!(!router.agent_is_routable("test_degraded_routing", "medium"));

        // available_agents_for_complexity returns empty
        assert!(
            router.available_agents_for_complexity("medium").is_empty(),
            "degraded agent should be excluded from available agents"
        );

        // healthy_agent_count reflects exclusion
        assert_eq!(router.healthy_agent_count("medium"), 0);

        // Cleanup
        clear_agent_degraded("test_degraded_routing");
    }

    #[test]
    fn healthy_agents_skips_degraded_but_keeps_healthy() {
        let mut config = RouterConfig::default();
        for agent in &["agent_healthy", "agent_degraded"] {
            config
                .model_map
                .entry("medium".to_string())
                .or_default()
                .insert(agent.to_string(), vec!["model-x".to_string()]);
        }

        let mut router = Router::new(config);
        router.available_agents = vec!["agent_healthy".to_string(), "agent_degraded".to_string()];

        mark_agent_degraded("agent_degraded");

        let candidates = router.available_agents_for_complexity("medium");
        assert_eq!(candidates, vec!["agent_healthy".to_string()]);
        assert_eq!(router.healthy_agent_count("medium"), 1);

        // Cleanup
        clear_agent_degraded("agent_degraded");
    }

    #[tokio::test]
    async fn refresh_health_marks_degraded_from_rate_limits() {
        let store = test_store().await;
        let agent = "test_refresh_health_agent";

        let mut config = RouterConfig::default();
        config
            .model_map
            .entry("medium".to_string())
            .or_default()
            .insert(agent.to_string(), vec!["model-z".to_string()]);

        let mut router = Router::new(config);
        router.available_agents = vec![agent.to_string()];

        // Insert rate limit events exceeding default threshold (3)
        for _ in 0..4 {
            store
                .record_rate_limit(agent, "rate_limit", None)
                .await
                .unwrap();
        }

        router.refresh_health(&store).await;

        assert!(
            is_agent_degraded(agent),
            "agent should be degraded after exceeding rate limit threshold"
        );
        assert!(
            !router.agent_is_routable(agent, "medium"),
            "degraded agent should not be routable"
        );

        // Cleanup
        clear_agent_degraded(agent);
    }

    #[test]
    fn llm_bypass_triggers_after_consecutive_timeouts() {
        let mut router = Router::new(RouterConfig::default());
        // Ensure bypass not active initially
        assert!(!router.llm_bypass_active());

        // Simulate three timeouts within window
        router.record_llm_budget_timeout();
        router.record_llm_budget_timeout();
        router.record_llm_budget_timeout();

        // Now bypass should be set
        assert!(router.llm_bypass_until.is_some());
        assert!(router.llm_bypass_active());
    }
}
