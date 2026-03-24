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
mod selection;
mod strategies;
pub mod weights;

pub use config::{parse_pool_entry, RouterConfig};
pub use weights::AgentWeights;

use crate::backends::ExternalTask;
use serde::{Deserialize, Serialize};

use llm::LlmRouter;

/// Result of routing a task to an agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RouteResult {
    /// The selected agent: "claude", "codex", or "opencode"
    pub agent: String,
    /// Optional model suggestion, e.g., "claude-sonnet-4-6", "claude-opus-4-6", "o3"
    pub model: Option<String>,
    /// Complexity level: "simple", "medium", or "complex"
    pub complexity: String,
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

/// The agent router.
pub struct Router {
    /// Router configuration
    pub config: RouterConfig,
    /// Available agents discovered at runtime
    pub available_agents: Vec<String>,
    /// Per-agent rate limit weights (used when weighted_round_robin is enabled)
    pub weights: AgentWeights,
    /// LLM routing subsystem
    llm_router: LlmRouter,
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

impl Router {
    /// Create a new router with the given configuration.
    pub fn new(config: RouterConfig) -> Self {
        let available_agents = Self::discover_agents(&config.agents);
        let mut weights = AgentWeights::default();
        weights.ensure_agents(&available_agents);
        let router_pool = Self::expand_pool(&config);
        tracing::info!(
            pool = ?router_pool,
            fallback = %config.effective_fallback(),
            "router LLM pool initialized"
        );
        Self {
            config,
            available_agents,
            weights,
            llm_router: LlmRouter::new(),
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

    /// Reload router configuration from config files.
    ///
    /// Re-reads all router settings and re-discovers available agents.
    /// Called when config files change on disk. Preserves existing agent weights.
    pub fn reload(&mut self) {
        let new_config = RouterConfig::from_config();
        let new_agents = Self::discover_agents(&new_config.agents);
        let new_pool = Self::expand_pool(&new_config);
        tracing::info!(
            mode = %new_config.mode,
            agents = ?new_agents,
            fallback = %new_config.fallback_executor,
            weighted_rr = new_config.weighted_round_robin,
            pool = ?new_pool,
            "router reloaded"
        );
        self.config = new_config;
        self.available_agents = new_agents.clone();
        // Ensure new agents have weight entries (preserves existing weights)
        self.weights.ensure_agents(&new_agents);
        self.router_pool = new_pool;
        self.pool_index = 0;
    }

    /// Invalidate the skills catalog cache so the next routing call reloads from disk.
    ///
    /// Call this after `skills_sync()` writes new/updated skill files.
    pub fn invalidate_skills_catalog(&self) {
        self.llm_router.invalidate_skills_catalog();
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

    /// Run `opencode models` and return lines containing "free".
    ///
    /// This is a synchronous blocking call, intentionally used only at startup
    /// (called once from `Router::new()` and `Router::reload()`). The result is
    /// stored in `router_pool` and not re-queried until the next reload.
    fn discover_free_opencode_models() -> Vec<String> {
        if !crate::cmd_cache::command_exists("opencode") {
            tracing::debug!("opencode not in PATH — skipping free model discovery");
            return vec![];
        }

        match std::process::Command::new("opencode")
            .args(["models"])
            .output()
        {
            Ok(output) if output.status.success() => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                stdout
                    .lines()
                    .filter(|l| l.contains("free"))
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .collect()
            }
            Ok(output) => {
                tracing::debug!(status = ?output.status, "opencode models command failed");
                vec![]
            }
            Err(e) => {
                tracing::debug!(error = %e, "failed to run opencode models");
                vec![]
            }
        }
    }

    /// Check if an agent is available.
    pub fn is_agent_available(&self, agent: &str) -> bool {
        self.available_agents.contains(&agent.to_string())
    }

    /// Get the first available agent.
    /// Pick next agent via round-robin (for review or other non-task routing).
    /// Pick the next review agent, optionally excluding one (e.g. the task's original agent).
    /// Falls back to the excluded agent only if it's the only one available.
    pub fn next_round_robin_agent(&mut self, exclude: Option<&str>) -> Option<String> {
        if self.available_agents.is_empty() {
            return None;
        }
        let idx = self.review_rr_index;

        // Try to find an agent that isn't excluded and isn't in cooldown
        let n = self.available_agents.len();
        let agent = (0..n)
            .map(|offset| &self.available_agents[(idx + offset) % n])
            .find(|a| {
                exclude != Some(a.as_str())
                    && !crate::engine::runner::response::is_agent_in_cooldown(a)
            })
            .cloned()
            // Fallback: any non-cooled agent (including excluded) — healthy agent beats rate-limited one
            .or_else(|| {
                (0..n)
                    .map(|offset| &self.available_agents[(idx + offset) % n])
                    .find(|a| !crate::engine::runner::response::is_agent_in_cooldown(a))
                    .cloned()
            })
            // Last resort: non-excluded even if cooled
            .or_else(|| {
                (0..n)
                    .map(|offset| &self.available_agents[(idx + offset) % n])
                    .find(|a| exclude != Some(a.as_str()))
                    .cloned()
            })
            .or_else(|| self.available_agents.get(idx % n).cloned())?;

        self.review_rr_index = (idx + 1) % n;
        Some(agent)
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
        store: &std::sync::Arc<crate::store::TaskStore>,
        repo: &str,
    ) -> anyhow::Result<RouteResult> {
        // 1. Check store agent field first (set by failover, authoritative over labels)
        let store_agent = crate::engine::cleanup::opt_store_get_field(
            &Some(store.clone()),
            repo,
            &task.id.0,
            "agent",
        )
        .await
        .filter(|a| !a.is_empty() && self.config.agents.contains(a));

        // 2. Fall back to explicit agent label
        let resolved_agent = store_agent
            .or_else(|| strategies::extract_agent_from_labels(&self.config.agents, &task.labels));

        if let Some(agent) = resolved_agent {
            if self.is_agent_available(&agent) {
                let complexity = strategies::extract_complexity_from_labels(&task.labels);
                let model = self
                    .config
                    .model_for_complexity(&agent, &complexity, &task.id.0);
                let profile = AgentProfile {
                    role: format!("{} specialist", agent),
                    skills: vec![],
                    tools: self.config.allowed_tools.clone(),
                    constraints: vec![],
                };

                tracing::debug!(task_id = %task.id.0, agent = %agent, complexity = %complexity, "routed via label");
                return Ok(RouteResult {
                    agent: agent.clone(),
                    model,
                    complexity: complexity.clone(),
                    reason: format!("label agent:{agent}"),
                    profile,
                    selected_skills: self.config.default_skills.clone(),
                    warning: None,
                });
            }
        }

        // 2. Weighted round-robin — capacity-based selection
        if self.config.weighted_round_robin {
            return strategies::route_via_weighted_round_robin(
                &self.available_agents,
                &self.weights,
                &self.config,
                task,
                &mut self.last_agent,
            );
        }

        // 3. Round-robin mode — use stateful round-robin
        if self.config.mode == "round_robin" {
            tracing::debug!(task_id = %task.id.0, "routing via round-robin mode");
            return strategies::route_via_round_robin_stateful(
                &self.available_agents,
                &self.config,
                task,
                &mut self.rr_index,
                &mut self.last_agent,
            );
        }

        // 3. LLM-based routing with retry tracking
        let route_attempts = self.get_route_attempts(&task.id.0, store, repo).await;

        if route_attempts >= self.config.max_route_attempts {
            tracing::warn!(
                task_id = %task.id.0,
                attempts = route_attempts,
                max = self.config.max_route_attempts,
                "max LLM route attempts reached, falling back to round-robin"
            );
            return strategies::route_via_round_robin_stateful(
                &self.available_agents,
                &self.config,
                task,
                &mut self.rr_index,
                &mut self.last_agent,
            );
        }

        // Log routing start (before await)
        tracing::debug!(task_id = %task.id.0, "starting LLM routing");

        match self.route_with_llm(task, repo).await {
            Ok(result) => {
                // Reset attempts on success
                self.set_route_attempts(&task.id.0, 0, store, repo).await;
                tracing::info!(task_id = %task.id.0, agent = %result.agent, complexity = %result.complexity, "routed via LLM");
                Ok(result)
            }
            Err(e) => {
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
                    strategies::route_via_round_robin_stateful(
                        &self.available_agents,
                        &self.config,
                        task,
                        &mut self.rr_index,
                        &mut self.last_agent,
                    )
                } else {
                    strategies::route_via_fallback(&self.available_agents, &self.config, task)
                }
            }
        }
    }

    /// Get the number of LLM routing attempts for a task from the store.
    async fn get_route_attempts(
        &self,
        task_id: &str,
        store: &std::sync::Arc<crate::store::TaskStore>,
        repo: &str,
    ) -> u32 {
        crate::engine::cleanup::store_get_field(store, repo, task_id, "route_attempts")
            .await
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    /// Set the number of LLM routing attempts for a task in the store.
    async fn set_route_attempts(
        &self,
        task_id: &str,
        attempts: u32,
        store: &std::sync::Arc<crate::store::TaskStore>,
        repo: &str,
    ) {
        crate::engine::cleanup::store_set(
            &Some(std::sync::Arc::clone(store)),
            repo,
            task_id,
            &[("route_attempts", serde_json::json!(attempts))],
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
        let pool = self.router_pool.clone();
        let n = pool.len();
        let start = self.pool_index;
        let mut last_err: Option<anyhow::Error> = None;

        // Try pool entries in round-robin order, skipping cooled ones
        for i in 0..n {
            let idx = (start + i) % n;
            let (agent, model) = &pool[idx];
            let model_str = model.as_str();

            if crate::engine::runner::response::is_model_in_cooldown(agent, model_str) {
                tracing::debug!(agent, model = model_str, "pool entry on cooldown, skipping");
                continue;
            }

            let model_opt = if model_str.is_empty() {
                None
            } else {
                Some(model_str)
            };

            match self
                .llm_router
                .route_with_llm_using(
                    task,
                    &self.available_agents,
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
                    tracing::warn!(
                        agent,
                        model = model_str,
                        error = %e,
                        "pool entry failed, recording cooldown and trying next"
                    );
                    crate::engine::runner::response::record_model_failure(agent, model_str);
                    last_err = Some(e);
                    self.advance_pool_index_after_attempt(idx, n);
                }
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
                    &self.available_agents,
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
                    tracing::warn!(
                        agent = %fb_agent,
                        model = %fb_model_str,
                        error = %e,
                        "fallback router LLM also failed"
                    );
                    crate::engine::runner::response::record_model_failure(&fb_agent, fb_model_str);
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
        store: &std::sync::Arc<crate::store::TaskStore>,
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
    store: &std::sync::Arc<crate::store::TaskStore>,
    repo: &str,
    task_id: &str,
) -> anyhow::Result<RouteResult> {
    let read = |field: &str| {
        let store = store.clone();
        let repo = repo.to_string();
        let task_id = task_id.to_string();
        let field = field.to_string();
        async move {
            crate::engine::cleanup::store_get_field(&store, &repo, &task_id, &field)
                .await
                .unwrap_or_default()
        }
    };

    let agent = read("agent").await;
    if agent.is_empty() {
        anyhow::bail!("no agent field found for task {task_id}");
    }
    let complexity = {
        let v = read("complexity").await;
        if v.is_empty() {
            "medium".to_string()
        } else {
            v
        }
    };
    let reason = read("route_reason").await;
    let model = {
        let v = read("model").await;
        if v.is_empty() {
            None
        } else {
            Some(v)
        }
    };

    Ok(RouteResult {
        agent,
        model,
        complexity,
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

    async fn test_store() -> std::sync::Arc<crate::store::TaskStore> {
        std::sync::Arc::new(crate::store::TaskStore::open_memory().await.unwrap())
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
        let config = RouterConfig::default();

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
        assert_eq!(DEFAULT_AGENTS.len(), 5);
        assert!(DEFAULT_AGENTS.contains(&"claude"));
        assert!(DEFAULT_AGENTS.contains(&"kimi"));
        assert!(DEFAULT_AGENTS.contains(&"minimax"));
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
        let config = RouterConfig::default();

        assert_eq!(config.mode, "llm");
        assert_eq!(config.router_agent, "claude");
        assert_eq!(config.router_model, "claude-haiku-4-5-20251001");
        assert_eq!(config.fallback_executor, "codex");
        assert_eq!(config.max_route_attempts, 3);
        assert!(!config.allowed_tools.is_empty());
        assert!(!config.default_skills.is_empty());

        // Verify configurable agents list includes all 5 agents
        assert_eq!(config.agents.len(), 5);
        assert!(config.agents.contains(&"claude".to_string()));
        assert!(config.agents.contains(&"codex".to_string()));
        assert!(config.agents.contains(&"opencode".to_string()));
        assert!(config.agents.contains(&"kimi".to_string()));
        assert!(config.agents.contains(&"minimax".to_string()));
    }

    #[test]
    fn model_map_lookup() {
        let config = RouterConfig::default();

        assert_eq!(
            config.model_for_complexity("claude", "simple", ""),
            Some("claude-haiku-4-5-20251001".to_string())
        );
        assert_eq!(
            config.model_for_complexity("claude", "medium", ""),
            Some("claude-sonnet-4-6".to_string())
        );
        assert_eq!(
            config.model_for_complexity("claude", "complex", ""),
            Some("claude-opus-4-6".to_string())
        );
        assert_eq!(
            config.model_for_complexity("codex", "simple", ""),
            Some("o4-mini".to_string())
        );
        // Verify kimi and minimax use same models as claude
        assert_eq!(
            config.model_for_complexity("kimi", "simple", ""),
            Some("claude-haiku-4-5-20251001".to_string())
        );
        assert_eq!(
            config.model_for_complexity("kimi", "complex", ""),
            Some("claude-opus-4-6".to_string())
        );
        assert_eq!(
            config.model_for_complexity("minimax", "medium", ""),
            Some("claude-sonnet-4-6".to_string())
        );
        assert_eq!(
            config.model_for_complexity("minimax", "complex", ""),
            Some("claude-opus-4-6".to_string())
        );
    }

    #[test]
    fn model_pool_selection_skips_cooled() {
        use crate::engine::cooldown::{is_model_in_cooldown, record_model_failure};
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
        record_model_failure("opencode", "model-a");
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

        // Drop the cooldown by removing from map isn't exposed; instead verify all-cooled fallback
        // Cool model-b too
        record_model_failure("opencode", "model-b");
        // All cooled → deterministic fallback to pool[0] = model-a
        let fallback = config.model_for_complexity("opencode", "simple", "task-fallback");
        assert_eq!(fallback, Some("model-a".to_string()));
        let _ = HashMap::<(), ()>::new(); // suppress unused import lint
    }

    #[test]
    fn model_pool_single_string_backward_compat() {
        // Single-item pools behave identically to the old string format
        let config = RouterConfig::default();
        // All default entries are single-item pools
        let m = config.model_for_complexity("claude", "simple", "any-task");
        assert_eq!(m, Some("claude-haiku-4-5-20251001".to_string()));
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
            llm_router: LlmRouter::new(),
            rr_index: 0,
            last_agent: None,
            review_rr_index: 0,
            router_pool: vec![],
            pool_index: 0,
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
        let config = RouterConfig::default();
        let agents = vec!["claude".to_string(), "codex".to_string()];
        let mut weights = AgentWeights::default();
        weights.ensure_agents(&agents);
        let mut router = Router {
            config,
            available_agents: agents,
            weights,
            llm_router: LlmRouter::new(),
            rr_index: 0,
            last_agent: None,
            review_rr_index: 0,
            router_pool: vec![],
            pool_index: 0,
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

    #[test]
    fn router_reload_preserves_structure() {
        let config = RouterConfig::default();
        let agents = vec!["claude".to_string()];
        let mut weights = AgentWeights::default();
        weights.ensure_agents(&agents);
        let mut router = Router {
            config,
            available_agents: agents,
            weights,
            llm_router: LlmRouter::new(),
            rr_index: 0,
            last_agent: None,
            review_rr_index: 0,
            router_pool: vec![],
            pool_index: 0,
        };

        // Reload — should re-read config and remain valid
        router.reload();

        // After reload, mode should be a valid value (llm or round_robin)
        assert!(
            router.config.mode == "llm" || router.config.mode == "round_robin",
            "mode should be 'llm' or 'round_robin', got '{}'",
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
        let config = RouterConfig {
            weighted_round_robin: true,
            ..Default::default()
        };

        let agents = vec!["claude".to_string(), "codex".to_string()];
        let mut weights = AgentWeights::default();
        weights.ensure_agents(&agents);
        let mut router = Router {
            config,
            available_agents: agents,
            weights,
            llm_router: LlmRouter::new(),
            rr_index: 0,
            last_agent: None,
            review_rr_index: 0,
            router_pool: vec![],
            pool_index: 0,
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
        let config = RouterConfig {
            weighted_round_robin: true,
            ..Default::default()
        };

        let agents = vec!["claude".to_string(), "codex".to_string()];
        let mut weights = AgentWeights::default();
        weights.ensure_agents(&agents);
        let mut router = Router {
            config,
            available_agents: agents,
            weights,
            llm_router: LlmRouter::new(),
            rr_index: 0,
            last_agent: None,
            review_rr_index: 0,
            router_pool: vec![],
            pool_index: 0,
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
        let config = RouterConfig::default();
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
            llm_router: LlmRouter::new(),
            rr_index: 0,
            last_agent: None,
            review_rr_index: 0,
            router_pool: vec![],
            pool_index: 0,
        };

        let a1 = router.next_round_robin_agent(None).unwrap();
        let a2 = router.next_round_robin_agent(None).unwrap();
        let a3 = router.next_round_robin_agent(None).unwrap();

        // All three agents should appear (order depends on start index)
        let mut seen = vec![a1, a2, a3];
        seen.sort();
        assert_eq!(seen, vec!["test_a", "test_b", "test_c"]);
    }

    #[tokio::test]
    async fn last_agent_tracks_routing() {
        let config = RouterConfig {
            mode: "round_robin".to_string(),
            ..Default::default()
        };
        let agents = vec!["test_x".to_string(), "test_y".to_string()];
        let mut weights = AgentWeights::default();
        weights.ensure_agents(&agents);
        let mut router = Router {
            config,
            available_agents: agents,
            weights,
            llm_router: LlmRouter::new(),
            rr_index: 0,
            last_agent: None,
            review_rr_index: 0,
            router_pool: vec![],
            pool_index: 0,
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

    #[test]
    fn reload_resets_pool_index() {
        let config = RouterConfig::default();
        let mut router = Router::new(config);
        router.pool_index = 5;
        router.reload();
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
}
