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

use crate::backends::ExternalTask;
use futures::stream::{FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

/// Default weight for agents with full capacity.
const DEFAULT_WEIGHT: f64 = 1.0;

/// Minimum weight — an agent never drops below this (still gets occasional tasks).
const MIN_WEIGHT: f64 = 0.05;

/// How much to reduce weight on each rate limit hit (multiplicative decay).
const RATE_LIMIT_DECAY: f64 = 0.3;

/// Duration after which a rate-limited agent starts recovering weight.
const RECOVERY_DELAY: Duration = Duration::from_secs(60);

/// Per-tick weight recovery amount (additive, applied each routing call).
const RECOVERY_RATE: f64 = 0.1;

/// Rate limit state for a single agent.
#[derive(Debug, Clone)]
pub struct RateLimitState {
    /// Current routing weight (0.0..=1.0). Higher = more tasks.
    pub weight: f64,
    /// When the last rate limit error was recorded.
    pub last_limited_at: Option<Instant>,
    /// How many consecutive rate limit hits.
    pub consecutive_hits: u32,
}

impl Default for RateLimitState {
    fn default() -> Self {
        Self {
            weight: DEFAULT_WEIGHT,
            last_limited_at: None,
            consecutive_hits: 0,
        }
    }
}

impl RateLimitState {
    /// Record a rate limit event — decay the weight.
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

    /// Tick recovery: if enough time has passed since the last limit, gradually restore.
    pub fn maybe_recover(&mut self) {
        if let Some(last) = self.last_limited_at {
            if last.elapsed() >= RECOVERY_DELAY {
                self.weight = (self.weight + RECOVERY_RATE).min(DEFAULT_WEIGHT);
                if self.weight >= DEFAULT_WEIGHT {
                    self.last_limited_at = None;
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
        self.states
            .entry(agent.to_string())
            .or_default()
            .record_rate_limit();
        tracing::info!(
            agent,
            weight = self.states[agent].weight,
            hits = self.states[agent].consecutive_hits,
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
    pub fn weighted_select(&self, agents: &[String]) -> Option<String> {
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
            let idx = simple_hash_index(agents.len());
            return Some(agents[idx].clone());
        }

        // Deterministic-ish selection using a hash of the current time
        // to avoid requiring rand crate. Good enough for load distribution.
        let pick = simple_hash_fraction() * total;
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

/// Simple deterministic-ish fraction [0.0, 1.0) based on Instant::now().
/// Not cryptographic, but sufficient for load distribution.
fn simple_hash_fraction() -> f64 {
    // Use SystemTime since Instant::now().elapsed() measures time since
    // the instant was created and is nearly always ~0 here. SystemTime
    // gives a clock relative to the UNIX epoch which varies across calls.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;

    // Mix bits using a simple hash
    let hash = nanos
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (hash % 10000) as f64 / 10000.0
}

/// Simple index selection using instant-based hash.
fn simple_hash_index(len: usize) -> usize {
    if len == 0 {
        return 0;
    }

    // Use SystemTime for a variable seed instead of Instant::now().elapsed().
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;

    let hash = nanos
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (hash as usize) % len
}

/// Router configuration.
#[derive(Debug, Clone)]
pub struct RouterConfig {
    /// Routing mode: "llm" or "round_robin"
    pub mode: String,
    /// Which agent performs routing (default: "claude")
    pub router_agent: String,
    /// Model for routing (default: "haiku")
    pub router_model: String,
    /// Timeout for routing LLM call in seconds
    pub timeout_seconds: u64,
    /// Fallback executor if routing fails
    pub fallback_executor: String,
    /// Configurable agent list (checked against PATH at runtime)
    pub agents: Vec<String>,
    /// Max LLM routing attempts before falling back to round-robin
    pub max_route_attempts: u32,
    /// Default tools allowed
    pub allowed_tools: Vec<String>,
    /// Default skills to always include
    pub default_skills: Vec<String>,
    /// Model map for complexity levels
    pub model_map: HashMap<String, HashMap<String, String>>,
    /// Enable weighted round-robin routing based on rate limit capacity.
    /// When true, agents that hit rate limits get fewer tasks.
    pub weighted_round_robin: bool,
}

/// Default agents to check in PATH.
///
/// All 5 agents are listed, but availability is checked at runtime via
/// `which::which()`. Agents not installed (e.g. kimi, minimax) are
/// automatically skipped during routing. Users can customize this list
/// in their `config.yml` under `routing.agents`.
pub const DEFAULT_AGENTS: &[&str] = &["claude", "codex", "opencode", "kimi", "minimax"];

impl Default for RouterConfig {
    fn default() -> Self {
        let mut model_map = HashMap::new();

        // Simple tasks — fast, cheap models
        let mut simple = HashMap::new();
        simple.insert(
            "claude".to_string(),
            "claude-haiku-4-5-20251001".to_string(),
        );
        simple.insert("codex".to_string(), "o4-mini".to_string());
        simple.insert("opencode".to_string(), "openai/gpt-4.1-mini".to_string());
        simple.insert("kimi".to_string(), "claude-haiku-4-5-20251001".to_string());
        simple.insert(
            "minimax".to_string(),
            "claude-haiku-4-5-20251001".to_string(),
        );
        model_map.insert("simple".to_string(), simple);

        // Medium tasks — balanced cost/capability
        let mut medium = HashMap::new();
        medium.insert("claude".to_string(), "claude-sonnet-4-6".to_string());
        medium.insert("codex".to_string(), "gpt-4.1".to_string());
        medium.insert(
            "opencode".to_string(),
            "anthropic/claude-sonnet-4-6".to_string(),
        );
        medium.insert("kimi".to_string(), "claude-sonnet-4-6".to_string());
        medium.insert("minimax".to_string(), "claude-sonnet-4-6".to_string());
        model_map.insert("medium".to_string(), medium);

        // Complex tasks — most capable models
        let mut complex = HashMap::new();
        complex.insert("claude".to_string(), "claude-opus-4-6".to_string());
        complex.insert("codex".to_string(), "o3".to_string());
        complex.insert(
            "opencode".to_string(),
            "anthropic/claude-opus-4-6".to_string(),
        );
        complex.insert("kimi".to_string(), "claude-opus-4-6".to_string());
        complex.insert("minimax".to_string(), "claude-opus-4-6".to_string());
        model_map.insert("complex".to_string(), complex);

        // Review tasks — strong reasoning, moderate cost
        let mut review = HashMap::new();
        review.insert("claude".to_string(), "claude-sonnet-4-6".to_string());
        review.insert("codex".to_string(), "gpt-4.1".to_string());
        review.insert(
            "opencode".to_string(),
            "anthropic/claude-sonnet-4-6".to_string(),
        );
        review.insert("kimi".to_string(), "claude-sonnet-4-6".to_string());
        review.insert("minimax".to_string(), "claude-sonnet-4-6".to_string());
        model_map.insert("review".to_string(), review);

        Self {
            mode: "llm".to_string(),
            router_agent: "claude".to_string(),
            router_model: "claude-haiku-4-5-20251001".to_string(),
            timeout_seconds: 120,
            fallback_executor: "codex".to_string(),
            agents: DEFAULT_AGENTS.iter().map(|s| s.to_string()).collect(),
            max_route_attempts: 3,
            allowed_tools: vec![
                "yq".to_string(),
                "jq".to_string(),
                "bash".to_string(),
                "just".to_string(),
                "git".to_string(),
                "rg".to_string(),
                "sed".to_string(),
                "awk".to_string(),
                "python3".to_string(),
                "node".to_string(),
                "npm".to_string(),
                "bun".to_string(),
            ],
            default_skills: vec!["gh".to_string(), "git-worktree".to_string()],
            model_map,
            weighted_round_robin: false,
        }
    }
}

impl RouterConfig {
    /// Load configuration from config files.
    pub fn from_config() -> Self {
        let mut config = Self::default();

        // Try to load from config
        if let Ok(mode) = crate::config::get("router.mode") {
            if mode == "round_robin" || mode == "llm" {
                config.mode = mode;
            }
        }

        if let Ok(agent) = crate::config::get("router.agent") {
            if !agent.is_empty() {
                config.router_agent = agent;
            }
        }

        if let Ok(model) = crate::config::get("router.model") {
            if !model.is_empty() {
                config.router_model = model;
            }
        }

        if let Ok(timeout) = crate::config::get("router.timeout_seconds") {
            if let Ok(secs) = timeout.parse::<u64>() {
                config.timeout_seconds = secs;
            }
        }

        if let Ok(fallback) = crate::config::get("router.fallback_executor") {
            if !fallback.is_empty() {
                config.fallback_executor = fallback;
            }
        }

        // Parse agents list
        if let Ok(agents_str) = crate::config::get("router.agents") {
            if !agents_str.is_empty() && agents_str != "[]" {
                if let Ok(agents_arr) = serde_json::from_str::<Vec<String>>(&agents_str) {
                    config.agents = agents_arr;
                } else {
                    config.agents = agents_str
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
            }
        }

        if let Ok(max_attempts) = crate::config::get("router.max_route_attempts") {
            if let Ok(n) = max_attempts.parse::<u32>() {
                config.max_route_attempts = n;
            }
        }

        // Parse allowed_tools as comma-separated or YAML array
        if let Ok(tools_str) = crate::config::get("router.allowed_tools") {
            if !tools_str.is_empty() && tools_str != "[]" {
                // Try to parse as JSON/YAML array first
                if let Ok(tools_arr) = serde_json::from_str::<Vec<String>>(&tools_str) {
                    config.allowed_tools = tools_arr;
                } else {
                    // Fall back to comma-separated
                    config.allowed_tools = tools_str
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
            }
        }

        // Parse weighted_round_robin
        if let Ok(val) = crate::config::get("router.weighted_round_robin") {
            config.weighted_round_robin = val == "true" || val == "1";
        }

        // Parse default_skills
        if let Ok(skills_str) = crate::config::get("router.default_skills") {
            if !skills_str.is_empty() && skills_str != "[]" {
                if let Ok(skills_arr) = serde_json::from_str::<Vec<String>>(&skills_str) {
                    config.default_skills = skills_arr;
                } else {
                    config.default_skills = skills_str
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
            }
        }

        config
    }

    /// Get the model for a given agent and complexity level.
    pub fn model_for_complexity(&self, agent: &str, complexity: &str) -> Option<String> {
        self.model_map
            .get(complexity)
            .and_then(|m| m.get(agent))
            .cloned()
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
}

/// Response from the LLM router.
#[derive(Debug, Deserialize)]
struct LlmRouteResponse {
    executor: String,
    #[serde(default)]
    complexity: String,
    reason: String,
    profile: LlmAgentProfile,
    #[serde(default)]
    selected_skills: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
struct LlmAgentProfile {
    #[serde(default)]
    role: String,
    #[serde(default)]
    skills: Vec<String>,
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default)]
    constraints: Vec<String>,
}

impl Router {
    /// Create a new router with the given configuration.
    pub fn new(config: RouterConfig) -> Self {
        let available_agents = Self::discover_agents(&config.agents);
        let mut weights = AgentWeights::default();
        weights.ensure_agents(&available_agents);
        Self {
            config,
            available_agents,
            weights,
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
        tracing::info!(
            mode = %new_config.mode,
            agents = ?new_agents,
            fallback = %new_config.fallback_executor,
            weighted_rr = new_config.weighted_round_robin,
            "router reloaded"
        );
        self.config = new_config;
        self.available_agents = new_agents.clone();
        // Ensure new agents have weight entries (preserves existing weights)
        self.weights.ensure_agents(&new_agents);
    }

    /// Discover available agent CLIs in PATH.
    /// Checks all agents from the configured list.
    fn discover_agents(configured_agents: &[String]) -> Vec<String> {
        let mut agents = Vec::new();
        for agent in configured_agents {
            if Self::command_exists(agent) {
                agents.push(agent.to_string());
            }
        }
        agents
    }

    /// Check if a command exists in PATH.
    fn command_exists(cmd: &str) -> bool {
        which::which(cmd).is_ok()
    }

    /// Check if an agent is available.
    pub fn is_agent_available(&self, agent: &str) -> bool {
        self.available_agents.contains(&agent.to_string())
    }

    /// Get the first available agent.
    fn first_available_agent(&self) -> Option<String> {
        self.available_agents.first().cloned()
    }

    /// Route a task to the best agent.
    ///
    /// Routing logic (in priority order):
    /// 1. Check for `agent:*` label — use that agent directly
    /// 2. If weighted_round_robin enabled, select by capacity-weighted probability
    /// 3. If round_robin mode, cycle through agents (stateful)
    /// 4. Call LLM classifier for intelligent routing
    /// 5. After max_route_attempts LLM failures, fall back to round-robin
    pub async fn route(&self, task: &ExternalTask) -> anyhow::Result<RouteResult> {
        // 1. Check for explicit agent label
        if let Some(agent) = self.extract_agent_from_labels(&task.labels) {
            if self.is_agent_available(&agent) {
                let complexity = self.extract_complexity_from_labels(&task.labels);
                let model = self.config.model_for_complexity(&agent, &complexity);
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
            return self.route_weighted_round_robin(task);
        }

        // 3. Round-robin mode — use stateful round-robin
        if self.config.mode == "round_robin" {
            tracing::debug!(task_id = %task.id.0, "routing via round-robin mode");
            return self.route_round_robin_stateful(task);
        }

        // 3. LLM-based routing with retry tracking
        let route_attempts = self.get_route_attempts(&task.id.0);

        if route_attempts >= self.config.max_route_attempts {
            tracing::warn!(
                task_id = %task.id.0,
                attempts = route_attempts,
                max = self.config.max_route_attempts,
                "max LLM route attempts reached, falling back to round-robin"
            );
            return self.route_round_robin_stateful(task);
        }

        // Log routing start (before await)
        tracing::debug!(task_id = %task.id.0, "starting LLM routing");

        match self.route_with_llm(task).await {
            Ok(result) => {
                // Reset attempts on success
                let _ = self.set_route_attempts(&task.id.0, 0);
                tracing::info!(task_id = %task.id.0, agent = %result.agent, complexity = %result.complexity, "routed via LLM");
                Ok(result)
            }
            Err(e) => {
                let new_attempts = route_attempts + 1;
                let _ = self.set_route_attempts(&task.id.0, new_attempts);
                tracing::warn!(
                    task_id = %task.id.0,
                    error = %e,
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
                    self.route_round_robin_stateful(task)
                } else {
                    self.route_fallback(task)
                }
            }
        }
    }

    /// Get the number of LLM routing attempts for a task from sidecar.
    fn get_route_attempts(&self, task_id: &str) -> u32 {
        crate::sidecar::get(task_id, "route_attempts")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    /// Set the number of LLM routing attempts for a task in sidecar.
    fn set_route_attempts(&self, task_id: &str, attempts: u32) -> anyhow::Result<()> {
        crate::sidecar::set(task_id, &[format!("route_attempts={}", attempts)])
    }

    /// Extract agent from labels (e.g., "agent:claude" -> "claude").
    /// Accepts any agent from the configured agents list.
    fn extract_agent_from_labels(&self, labels: &[String]) -> Option<String> {
        for label in labels {
            if let Some(agent) = label.strip_prefix("agent:") {
                let agent = agent.to_lowercase();
                if self.config.agents.iter().any(|a| a == &agent) {
                    return Some(agent);
                }
            }
        }
        None
    }

    /// Extract complexity from labels (e.g., "complexity:simple" -> "simple").
    fn extract_complexity_from_labels(&self, labels: &[String]) -> String {
        for label in labels {
            if let Some(comp) = label.strip_prefix("complexity:") {
                let comp = comp.to_lowercase();
                if ["simple", "medium", "complex"].contains(&comp.as_str()) {
                    return comp;
                }
            }
        }
        "medium".to_string()
    }

    /// Route using round-robin algorithm (task-ID based, stateless).
    /// Kept for backward compatibility; prefer `route_round_robin_stateful`.
    #[allow(dead_code)]
    fn route_round_robin(&self, task: &ExternalTask) -> anyhow::Result<RouteResult> {
        let agents = &self.available_agents;
        if agents.is_empty() {
            anyhow::bail!("no agent CLIs found in PATH");
        }

        // Parse task ID as number for modulo operation
        let task_num: usize = task.id.0.parse().unwrap_or(0);
        let agent_idx = task_num % agents.len();
        let agent = agents[agent_idx].clone();

        let profile = AgentProfile {
            role: "general".to_string(),
            skills: vec![],
            tools: self.config.allowed_tools.clone(),
            constraints: vec![],
        };

        Ok(RouteResult {
            agent: agent.clone(),
            model: self.config.model_for_complexity(&agent, "medium"),
            complexity: "medium".to_string(),
            reason: format!("round_robin (task {} % {} agents)", task.id.0, agents.len()),
            profile,
            selected_skills: self.config.default_skills.clone(),
            warning: None,
        })
    }

    /// Stateful round-robin: cycles through agents using a persistent index,
    /// skipping the last-used agent when possible.
    fn route_round_robin_stateful(&self, task: &ExternalTask) -> anyhow::Result<RouteResult> {
        let agents = &self.available_agents;
        if agents.is_empty() {
            anyhow::bail!("no agent CLIs found in PATH");
        }

        // Get current round-robin index from sidecar KV
        let current_idx: usize = crate::sidecar::get("_router", "rr_index")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        // Get last routed agent
        let last_agent = crate::sidecar::get("_router", "last_agent").ok();

        // Pick the next agent, skipping last-used if we have >1 agent
        let mut agent_idx = current_idx % agents.len();
        if agents.len() > 1 {
            if let Some(ref last) = last_agent {
                if agents.get(agent_idx).map(|a| a.as_str()) == Some(last.as_str()) {
                    agent_idx = (agent_idx + 1) % agents.len();
                }
            }
        }

        let agent = agents[agent_idx].clone();

        // Persist the next index and last agent
        let next_idx = (agent_idx + 1) % agents.len();
        if let Err(e) = crate::sidecar::set(
            "_router",
            &[
                format!("rr_index={}", next_idx),
                format!("last_agent={}", agent),
            ],
        ) {
            tracing::warn!(error = ?e, "failed to persist round-robin state");
        }

        let complexity = self.extract_complexity_from_labels(&task.labels);
        let model = self.config.model_for_complexity(&agent, &complexity);

        let profile = AgentProfile {
            role: "general".to_string(),
            skills: vec![],
            tools: self.config.allowed_tools.clone(),
            constraints: vec![],
        };

        Ok(RouteResult {
            agent: agent.clone(),
            model,
            complexity,
            reason: format!(
                "round_robin (index {} of {} agents)",
                agent_idx,
                agents.len()
            ),
            profile,
            selected_skills: self.config.default_skills.clone(),
            warning: None,
        })
    }

    /// Weighted round-robin: selects an agent based on capacity weights.
    ///
    /// Agents with higher weights (more capacity) get more tasks.
    /// Rate-limited agents have reduced weights and receive fewer tasks.
    fn route_weighted_round_robin(&self, task: &ExternalTask) -> anyhow::Result<RouteResult> {
        let agents = &self.available_agents;
        if agents.is_empty() {
            anyhow::bail!("no agent CLIs found in PATH");
        }

        let agent = self
            .weights
            .weighted_select(agents)
            .unwrap_or_else(|| agents[0].clone());

        let weight = self.weights.get_weight(&agent);
        let complexity = self.extract_complexity_from_labels(&task.labels);
        let model = self.config.model_for_complexity(&agent, &complexity);

        // Build weight summary for reason
        let weight_summary: Vec<String> = self
            .weights
            .snapshot()
            .iter()
            .filter(|(a, _, _)| agents.contains(a))
            .map(|(a, w, _)| format!("{a}={w:.2}"))
            .collect();

        let profile = AgentProfile {
            role: "general".to_string(),
            skills: vec![],
            tools: self.config.allowed_tools.clone(),
            constraints: vec![],
        };

        // Track last agent
        let _ = crate::sidecar::set("_router", &[format!("last_agent={}", agent)]);

        let reason = format!(
            "weighted_round_robin (weight={weight:.2}, weights=[{}])",
            weight_summary.join(", ")
        );

        tracing::info!(
            task_id = %task.id.0,
            agent = %agent,
            weight,
            "weighted round-robin selected agent"
        );

        Ok(RouteResult {
            agent,
            model,
            complexity,
            reason,
            profile,
            selected_skills: self.config.default_skills.clone(),
            warning: None,
        })
    }

    /// Route using LLM classification.
    async fn route_with_llm(&self, task: &ExternalTask) -> anyhow::Result<RouteResult> {
        if self.available_agents.is_empty() {
            anyhow::bail!("no agent CLIs found in PATH");
        }

        // Build the routing prompt
        let prompt = self.build_routing_prompt(task)?;

        // Save prompt to file for debugging
        let prompt_path = self.route_prompt_path(&task.id.0);
        if let Some(parent) = prompt_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let _ = tokio::fs::write(&prompt_path, &prompt).await;

        // Call the LLM router
        let response = self.call_router_llm(&prompt).await?;

        // Parse the response
        let llm_response: LlmRouteResponse = self.parse_llm_response(&response)?;

        // Validate the selected agent
        let mut agent = llm_response.executor.to_lowercase();
        if !self.is_agent_available(&agent) {
            let first_available = self.first_available_agent().unwrap_or_default();
            tracing::warn!(
                requested = %agent,
                fallback = %first_available,
                "selected agent not available, using fallback"
            );
            agent = first_available;
        }

        // Build the profile
        let mut profile = AgentProfile {
            role: llm_response.profile.role,
            skills: llm_response.profile.skills,
            tools: if llm_response.profile.tools.is_empty() {
                self.config.allowed_tools.clone()
            } else {
                llm_response.profile.tools
            },
            constraints: llm_response.profile.constraints,
        };

        // Ensure tools includes allowed_tools
        for tool in &self.config.allowed_tools {
            if !profile.tools.contains(tool) {
                profile.tools.push(tool.clone());
            }
        }

        // Determine complexity
        let complexity = if llm_response.complexity.is_empty() {
            "medium".to_string()
        } else {
            llm_response.complexity.to_lowercase()
        };

        // Get model for complexity
        let model = self.config.model_for_complexity(&agent, &complexity);

        // Build selected skills list
        let mut selected_skills = llm_response.selected_skills;
        for skill in &self.config.default_skills {
            if !selected_skills.contains(skill) {
                selected_skills.push(skill.clone());
            }
        }

        // Run sanity checks
        let warning = self.check_routing_sanity(task, &agent, &profile);

        // Track last routed agent for distribution
        if let Err(e) = crate::sidecar::set("_router", &[format!("last_agent={}", agent)]) {
            tracing::warn!(error = ?e, "failed to persist last_agent");
        }

        Ok(RouteResult {
            agent,
            model,
            complexity,
            reason: llm_response.reason,
            profile,
            selected_skills,
            warning,
        })
    }

    /// Build the routing prompt from the template.
    fn build_routing_prompt(&self, task: &ExternalTask) -> anyhow::Result<String> {
        let template = include_str!("../../prompts/route.md");

        // Build available agents string
        let available_agents = self.available_agents.join(", ");

        // Build labels string
        let labels = task.labels.join(", ");

        // Load skills catalog if available
        let skills_catalog = self.load_skills_catalog();

        // Simple template substitution
        let prompt = template
            .replace("{{AVAILABLE_AGENTS}}", &available_agents)
            .replace("{{SKILLS_CATALOG}}", &skills_catalog)
            .replace("{{TASK_ID}}", &task.id.0)
            .replace("{{TASK_TITLE}}", &task.title)
            .replace("{{TASK_LABELS}}", &labels)
            .replace("{{TASK_BODY}}", &task.body);

        Ok(prompt)
    }

    /// Load skills catalog from skills.yml or skills directory.
    fn load_skills_catalog(&self) -> String {
        // Try skills.yml in current directory
        if let Ok(content) = std::fs::read_to_string("skills.yml") {
            if let Ok(yaml) = serde_yml::from_str::<serde_yml::Value>(&content) {
                if let Some(skills) = yaml.get("skills") {
                    if let Ok(json) = serde_json::to_string(skills) {
                        return json;
                    }
                }
            }
        }

        // Try ORCH_HOME/skills directory
        if let Ok(orch_home) = std::env::var("ORCH_HOME") {
            let skills_dir = PathBuf::from(orch_home).join("skills");
            if let Ok(catalog) = self.build_skills_catalog_from_dir(&skills_dir) {
                return catalog;
            }
        }

        // Try ~/.orch/skills
        if let Ok(skills_dir) = crate::home::skills_dir() {
            if let Ok(catalog) = self.build_skills_catalog_from_dir(&skills_dir) {
                return catalog;
            }
        }

        // Return empty array as default
        "[]".to_string()
    }

    /// Build skills catalog from a directory.
    fn build_skills_catalog_from_dir(&self, dir: &PathBuf) -> anyhow::Result<String> {
        if !dir.exists() {
            anyhow::bail!("skills directory does not exist");
        }

        let mut skills = Vec::new();

        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                let skill_id = path.file_name().unwrap_or_default().to_string_lossy();
                let skill_file = path.join("SKILL.md");

                if skill_file.exists() {
                    // Read SKILL.md for metadata
                    let content = std::fs::read_to_string(&skill_file).unwrap_or_default();

                    // Extract name from first line (title)
                    let name = content
                        .lines()
                        .next()
                        .unwrap_or("")
                        .trim_start_matches("# ")
                        .to_string();

                    skills.push(serde_json::json!({
                        "id": skill_id,
                        "name": name,
                    }));
                }
            }
        }

        Ok(serde_json::to_string(&skills)?)
    }

    /// Call the router LLM to classify the task.
    async fn call_router_llm(&self, prompt: &str) -> anyhow::Result<String> {
        let timeout_secs = self.config.timeout_seconds;
        let timeout_duration = Duration::from_secs(timeout_secs);

        let output = match self.config.router_agent.as_str() {
            "claude" => {
                let mut cmd = tokio::process::Command::new("claude");
                cmd.arg("--output-format").arg("json").arg("--print");

                if !self.config.router_model.is_empty() {
                    cmd.arg("--model").arg(&self.config.router_model);
                }

                cmd.arg(prompt);

                tokio::time::timeout(timeout_duration, cmd.output()).await
            }
            "codex" => {
                let mut cmd = tokio::process::Command::new("codex");
                cmd.arg("exec").arg("--json");

                if !self.config.router_model.is_empty() {
                    cmd.arg("--model").arg(&self.config.router_model);
                }

                cmd.arg(prompt);

                tokio::time::timeout(timeout_duration, cmd.output()).await
            }
            "opencode" => {
                let mut cmd = tokio::process::Command::new("opencode");
                cmd.arg("run").arg("--format").arg("json").arg(prompt);

                tokio::time::timeout(timeout_duration, cmd.output()).await
            }
            _ => {
                anyhow::bail!("unknown router agent: {}", self.config.router_agent);
            }
        };

        match output {
            Ok(Ok(output)) => {
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    anyhow::bail!("router LLM failed: {stderr}");
                }
                Ok(String::from_utf8_lossy(&output.stdout).to_string())
            }
            Ok(Err(e)) => Err(e.into()),
            Err(_) => anyhow::bail!("router LLM timed out after {timeout_secs}s"),
        }
    }

    /// Parse the LLM response into a structured format.
    fn parse_llm_response(&self, response: &str) -> anyhow::Result<LlmRouteResponse> {
        // First, try to parse directly as JSON
        if let Ok(parsed) = serde_json::from_str::<LlmRouteResponse>(response) {
            return Ok(parsed);
        }

        // Try to extract JSON from markdown code blocks
        if let Some(json_start) = response.find("```json") {
            let after_start = &response[json_start + 7..];
            if let Some(json_end) = after_start.find("```") {
                let json_str = &after_start[..json_end].trim();
                if let Ok(parsed) = serde_json::from_str::<LlmRouteResponse>(json_str) {
                    return Ok(parsed);
                }
            }
        }

        // Try without json specifier
        if let Some(json_start) = response.find("```") {
            let after_start = &response[json_start + 3..];
            if let Some(json_end) = after_start.find("```") {
                let json_str = &after_start[..json_end].trim();
                if let Ok(parsed) = serde_json::from_str::<LlmRouteResponse>(json_str) {
                    return Ok(parsed);
                }
            }
        }

        // Try to find JSON object between curly braces
        if let Some(start) = response.find('{') {
            if let Some(end) = response.rfind('}') {
                let json_str = &response[start..=end];
                if let Ok(parsed) = serde_json::from_str::<LlmRouteResponse>(json_str) {
                    return Ok(parsed);
                }
            }
        }

        anyhow::bail!("could not parse LLM response as JSON")
    }

    /// Fallback routing when LLM fails.
    fn route_fallback(&self, task: &ExternalTask) -> anyhow::Result<RouteResult> {
        let agent = if self.is_agent_available(&self.config.fallback_executor) {
            self.config.fallback_executor.clone()
        } else {
            self.first_available_agent()
                .ok_or_else(|| anyhow::anyhow!("no agents available"))?
        };

        let complexity = self.extract_complexity_from_labels(&task.labels);
        let model = self.config.model_for_complexity(&agent, &complexity);

        let profile = AgentProfile {
            role: "general".to_string(),
            skills: vec![],
            tools: self.config.allowed_tools.clone(),
            constraints: vec![],
        };

        Ok(RouteResult {
            agent: agent.clone(),
            model,
            complexity,
            reason: format!("router failed; fallback to {agent}"),
            profile,
            selected_skills: self.config.default_skills.clone(),
            warning: None,
        })
    }

    /// Run sanity checks on routing decision.
    fn check_routing_sanity(
        &self,
        task: &ExternalTask,
        agent: &str,
        profile: &AgentProfile,
    ) -> Option<String> {
        let labels_lower: Vec<String> = task.labels.iter().map(|l| l.to_lowercase()).collect();

        // Check for backend tasks routed to claude
        let backend_labels: Vec<_> = labels_lower
            .iter()
            .filter(|l| {
                l.contains("backend")
                    || l.contains("api")
                    || l.contains("database")
                    || l.contains("db")
            })
            .collect();

        if !backend_labels.is_empty() && agent == "claude" {
            return Some("backend-labeled task routed to claude".to_string());
        }

        // Check for docs tasks routed to codex
        let docs_labels: Vec<_> = labels_lower
            .iter()
            .filter(|l| l.contains("docs") || l.contains("documentation") || l.contains("writing"))
            .collect();

        if !docs_labels.is_empty() && agent == "codex" {
            return Some("docs-labeled task routed to codex".to_string());
        }

        // Check for missing skills
        if profile.skills.is_empty() {
            return Some("profile missing skills".to_string());
        }

        None
    }

    /// Get the path for saving route prompts.
    fn route_prompt_path(&self, task_id: &str) -> PathBuf {
        crate::home::state_dir()
            .unwrap_or_else(|_| PathBuf::from("/tmp").join(".orch").join(".orch"))
            .join(format!("route-prompt-{task_id}.txt"))
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

    /// Get a snapshot of current agent weights for logging.
    #[allow(dead_code)]
    pub fn weight_snapshot(&self) -> Vec<(String, f64, u32)> {
        self.weights.snapshot()
    }

    /// Store routing result in sidecar file.
    pub fn store_route_result(&self, task_id: &str, result: &RouteResult) -> anyhow::Result<()> {
        let fields = vec![
            format!("agent={}", result.agent),
            format!("complexity={}", result.complexity),
            format!("route_reason={}", result.reason),
            format!("agent_profile={}", serde_json::to_string(&result.profile)?),
            format!("model={}", result.model.as_deref().unwrap_or("")),
            format!("selected_skills={}", result.selected_skills.join(",")),
        ];

        crate::sidecar::set(task_id, &fields)
    }

    /// Route multiple tasks concurrently using FuturesUnordered.
    /// Returns results in completion order (not input order).
    #[allow(dead_code)]
    pub async fn route_batch(
        self: &Arc<Self>,
        tasks: &[ExternalTask],
    ) -> Vec<(String, anyhow::Result<RouteResult>)> {
        let mut futures = FuturesUnordered::new();

        for task in tasks {
            let router = Arc::clone(self);
            let task = task.clone();
            futures.push(async move {
                let task_id = task.id.0.clone();
                let result = router.route(&task).await;
                (task_id, result)
            });
        }

        let mut results = Vec::with_capacity(tasks.len());
        while let Some(result) = futures.next().await {
            results.push(result);
        }
        results
    }
}

/// Retrieve routing result from sidecar file.
pub fn get_route_result(task_id: &str) -> anyhow::Result<RouteResult> {
    let agent = crate::sidecar::get(task_id, "agent")?;
    let complexity =
        crate::sidecar::get(task_id, "complexity").unwrap_or_else(|_| "medium".to_string());
    let reason = crate::sidecar::get(task_id, "route_reason").unwrap_or_default();
    let model = crate::sidecar::get(task_id, "model")
        .ok()
        .filter(|m| !m.is_empty());

    let profile_json = crate::sidecar::get(task_id, "agent_profile").unwrap_or_default();
    let profile: AgentProfile = if !profile_json.is_empty() {
        serde_json::from_str(&profile_json).unwrap_or_default()
    } else {
        AgentProfile::default()
    };

    let selected_skills_str = crate::sidecar::get(task_id, "selected_skills").unwrap_or_default();
    let selected_skills: Vec<String> = if !selected_skills_str.is_empty() {
        selected_skills_str
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        vec![]
    };

    Ok(RouteResult {
        agent,
        model,
        complexity,
        reason,
        profile,
        selected_skills,
        warning: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::{ExternalId, ExternalTask};

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
        let router = Router::new(config);

        assert_eq!(
            router.extract_agent_from_labels(&["agent:claude".to_string()]),
            Some("claude".to_string())
        );
        assert_eq!(
            router.extract_agent_from_labels(&["agent:codex".to_string()]),
            Some("codex".to_string())
        );
        assert_eq!(
            router.extract_agent_from_labels(&["agent:opencode".to_string()]),
            Some("opencode".to_string())
        );
        assert_eq!(
            router.extract_agent_from_labels(&["status:new".to_string()]),
            None
        );
        // Verify kimi and minimax are recognized from labels
        assert_eq!(
            router.extract_agent_from_labels(&["agent:kimi".to_string()]),
            Some("kimi".to_string())
        );
        assert_eq!(
            router.extract_agent_from_labels(&["agent:minimax".to_string()]),
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
        let config = RouterConfig::default();
        let router = Router::new(config);

        assert_eq!(
            router.extract_complexity_from_labels(&["complexity:simple".to_string()]),
            "simple"
        );
        assert_eq!(
            router.extract_complexity_from_labels(&["complexity:medium".to_string()]),
            "medium"
        );
        assert_eq!(
            router.extract_complexity_from_labels(&["complexity:complex".to_string()]),
            "complex"
        );
        assert_eq!(
            router.extract_complexity_from_labels(&["status:new".to_string()]),
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
            config.model_for_complexity("claude", "simple"),
            Some("claude-haiku-4-5-20251001".to_string())
        );
        assert_eq!(
            config.model_for_complexity("claude", "medium"),
            Some("claude-sonnet-4-6".to_string())
        );
        assert_eq!(
            config.model_for_complexity("claude", "complex"),
            Some("claude-opus-4-6".to_string())
        );
        assert_eq!(
            config.model_for_complexity("codex", "simple"),
            Some("o4-mini".to_string())
        );
        // Verify kimi and minimax use same models as claude
        assert_eq!(
            config.model_for_complexity("kimi", "simple"),
            Some("claude-haiku-4-5-20251001".to_string())
        );
        assert_eq!(
            config.model_for_complexity("kimi", "complex"),
            Some("claude-opus-4-6".to_string())
        );
        assert_eq!(
            config.model_for_complexity("minimax", "medium"),
            Some("claude-sonnet-4-6".to_string())
        );
        assert_eq!(
            config.model_for_complexity("minimax", "complex"),
            Some("claude-opus-4-6".to_string())
        );
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
        };

        let task = create_test_task("1", "Test task", vec![]);
        let result = router.route_round_robin(&task).unwrap();

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
        let router = Router {
            config,
            available_agents: agents,
            weights,
        };

        let task = create_test_task("1", "Test", vec!["agent:claude".to_string()]);

        // Should use label override, not LLM
        let result = router.route(&task).await.unwrap();
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
            match weights.weighted_select(&agents) {
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
        assert!(weights.weighted_select(&[]).is_none());
    }

    #[test]
    fn agent_weights_weighted_select_single_agent() {
        let mut weights = AgentWeights::default();
        let agents = vec!["claude".to_string()];
        weights.ensure_agents(&agents);

        let selected = weights.weighted_select(&agents);
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
        let router = Router {
            config,
            available_agents: agents,
            weights,
        };

        let task = create_test_task("1", "Test task", vec![]);
        let result = router.route(&task).await.unwrap();

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
        let router = Router {
            config,
            available_agents: agents,
            weights,
        };

        // Label override should take precedence over weighted routing
        let task = create_test_task("1", "Test task", vec!["agent:codex".to_string()]);
        let result = router.route(&task).await.unwrap();
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
}
