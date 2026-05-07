//! Router configuration — loading, defaults, and model map.

use std::collections::HashMap;

/// Minimum healthy agents threshold for graceful degradation.
/// When healthy agents fall below this number, dispatch switches to sequential mode.
pub fn min_healthy_agents_threshold() -> usize {
    crate::config::get("dispatch.min_healthy_agents")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2)
}

/// Base delay in milliseconds between dispatches in sequential (degraded) mode.
pub fn sequential_dispatch_delay_ms() -> u64 {
    crate::config::get("dispatch.sequential_delay_ms")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000)
}

/// Lookback window (in hours) for the pre-emptive agent health check.
///
/// The health check queries `rate_limits` for events within this window.
/// Configurable via `dispatch.health_check_window_hours` (default: 6).
pub fn health_check_window_hours() -> u32 {
    crate::config::get("dispatch.health_check_window_hours")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(crate::engine::cooldown::DEFAULT_HEALTH_CHECK_WINDOW_HOURS)
}

/// Minimum number of rate-limit events within the health check window to
/// consider an agent degraded.
///
/// A single transient 429 error shouldn't mark an agent degraded; we need
/// a pattern of repeated failures to detect genuine degradation. This
/// threshold is compared against the count of rate_limit events in the
/// `rate_limits` table within the health check window.
///
/// IMPORTANT: An agent with `rate_limit_count == 0` is NOT degraded by this
/// criterion — it must meet or exceed the threshold. The threshold should
/// be set to a value >= 2 to avoid false positives when there are no events.
///
/// Configurable via `dispatch.degraded_rate_limit_threshold` (default: 3).
pub fn degraded_rate_limit_threshold() -> i64 {
    crate::config::get("dispatch.degraded_rate_limit_threshold")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(crate::engine::cooldown::DEFAULT_DEGRADED_RATE_LIMIT_THRESHOLD)
}

/// Base delay in milliseconds for exponential backoff between fallback retries.
pub fn retry_base_delay_ms() -> u64 {
    crate::config::get("dispatch.retry_base_delay_ms")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000)
}

/// Maximum delay in milliseconds for exponential backoff between fallback retries.
pub fn retry_max_delay_ms() -> u64 {
    crate::config::get("dispatch.retry_max_delay_ms")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120_000)
}

/// Upper bound for router LLM timeout (seconds).
///
/// Routing is a short classification step and should fail fast so a single
/// slow model does not stall fallback to other healthy agents.
pub const MAX_ROUTER_TIMEOUT_SECS: u64 = 45;

/// Return the configured base backoff (seconds) for an agent.
///
/// Returns the value of `router.backoff_base.{agent}` if set, otherwise
/// the default BACKOFF_BASE_SECS (5 minutes).
///
/// This lets opencode and other higher-failure-rate agents use a longer
/// initial backoff to reduce wasted retry attempts against consistently
/// failing models.
pub fn get_agent_backoff_base(agent: &str) -> i64 {
    let key = format!("router.backoff_base.{agent}");
    crate::config::get(&key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(crate::engine::cooldown::BACKOFF_BASE_SECS)
}

/// Maximum number of tasks to route per tick. Prevents blocking the tick loop
/// when multiple tasks are queued simultaneously (e.g., cron jobs). Each routing
/// operation involves an LLM call taking 10-45 seconds, so routing N tasks
/// sequentially would block for N×LLM-latency seconds.
///
/// Configurable via `router.max_tasks_per_tick` (default: 1).
pub fn max_tasks_per_routing_tick() -> usize {
    crate::config::get("router.max_tasks_per_tick")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
}

/// Default agents to check in PATH.
///
/// All 6 agents are listed, but availability is checked at runtime via
/// `which::which()`. Agents not installed (e.g. kimi, minimax, glm) are
/// automatically skipped during routing. Users can customize this list
/// in their `config.yml` under `routing.agents`.
pub const DEFAULT_AGENTS: &[&str] = &["claude", "codex", "opencode", "kimi", "minimax", "glm"];

/// Router configuration.
#[derive(Debug, Clone)]
pub struct RouterConfig {
    /// Routing mode: "llm", "local", or "round_robin"
    ///
    /// - "llm": Use cloud LLMs for routing (default, supports pool-based routing)
    /// - "local": Use local Ollama model for routing
    /// - "round_robin": Distribute tasks evenly across available agents
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
    /// Model map for complexity levels.
    /// Each entry is a pool of models; one is selected randomly (skipping cooled ones) per dispatch.
    /// A single-model pool `vec!["model"]` behaves identically to the old string format.
    pub model_map: HashMap<String, HashMap<String, Vec<String>>>,
    /// Enable weighted round-robin routing based on rate limit capacity.
    /// When true, agents that hit rate limits get fewer tasks.
    pub weighted_round_robin: bool,
    /// Pool of `agent:model` entries to round-robin the router LLM call.
    /// When empty, a single-entry pool is derived from `router_agent:router_model`.
    /// Special value `opencode:free` is expanded at startup via `opencode models`.
    pub pool: Vec<String>,
    /// Fallback `agent:model` when all pool entries are cooled or fail.
    /// When empty, defaults to `router_agent:router_model`.
    pub fallback: String,
    /// Penalty weight applied when the LLM routes a task to itself (the routing agent).
    ///
    /// Range: `0.0` (always redirect away from router agent) to `1.0` (no penalty, default).
    /// When `< 1.0` and the LLM selects its own agent, the router probabilistically
    /// redirects to another available agent, reducing self-routing bias.
    ///
    /// Example: `0.5` means ~50% of self-routed tasks are redirected to another agent.
    pub self_routing_penalty: f64,
    /// Configurable base weights per agent.
    ///
    /// Higher weight = more tasks routed to this agent. Agents without an explicit
    /// weight default to 1.0. The weighted selection normalizes these into
    /// probabilities, so `{claude: 0.6, minimax: 0.03}` means claude gets ~95%
    /// of tasks when both are available.
    ///
    /// Set in `config.yml` under `router.weights`:
    /// ```yaml
    /// router:
    ///   weights:
    ///     claude: 0.6
    ///     codex: 0.2
    ///     opencode: 0.15
    ///     minimax: 0.03
    ///     kimi: 0.02
    /// ```
    pub weights: HashMap<String, f64>,
    /// If an agent's routing weight falls below this threshold, consider it
    /// degraded and skip it during proactive routing decisions. Value in
    /// `0.0..=1.0` where `1.0` means never skip and `0.0` skips only when
    /// weight is exactly zero (practically never). Default: `0.3`.
    pub skip_limited_threshold: f64,
    /// Per-agent base backoff overrides (seconds). Agents not listed use the
    /// default BACKOFF_BASE_SECS (5 minutes). Configure as `router.backoff_base.{agent}`.
    ///
    /// Example: set `router.backoff_base.opencode: 600` for a 10-minute base
    /// instead of the default 5-minute base (useful for agents with higher
    /// failure rates like opencode at 24.8%).
    pub agent_backoff_bases: HashMap<String, i64>,
    /// Maximum number of tasks to route per tick. Prevents blocking the tick loop
    /// when multiple tasks are queued simultaneously (e.g., cron jobs). Each routing
    /// operation involves an LLM call taking 10-45 seconds, so routing N tasks
    /// sequentially would block for N×LLM-latency seconds.
    ///
    /// Configurable via `router.max_tasks_per_tick` (default: 1).
    pub max_tasks_per_tick: usize,
    /// Ollama base URL for local routing (default: "http://localhost:11434").
    pub ollama_url: String,
    /// Ollama model name for routing (default: "qwen2.5-coder:3b-instruct").
    pub ollama_model: String,
    /// Timeout for Ollama HTTP call in seconds (default: 30).
    pub ollama_timeout_seconds: u64,
    /// Total time budget (seconds) for the entire LLM routing cascade (all pool
    /// entries + fallback combined). When exceeded, routing immediately falls back
    /// to round-robin without waiting for the remaining pool entries to time out
    /// individually.
    ///
    /// `timeout_seconds` is a *per-entry* limit; `llm_budget_secs` caps the *total*
    /// cascade. With a 3-entry pool each carrying a 45 s per-entry timeout, the
    /// cascade could block for up to 135 s without this budget. Setting the budget
    /// to 30 s ensures round-robin takes over quickly, preventing tick stalls from
    /// exceeding the watchdog threshold (6 × tick_interval).
    ///
    /// Configurable via `router.llm_budget_secs` (default: `timeout_seconds`).
    pub llm_budget_secs: u64,
}

/// Parse an `agent:model` pool entry string, splitting on the first colon.
///
/// Examples:
/// - `"claude:haiku"` → `("claude", "haiku")`
/// - `"opencode:github-copilot/gpt-5-mini"` → `("opencode", "github-copilot/gpt-5-mini")`
/// - `"claude"` → `("claude", "")`
pub fn parse_pool_entry(entry: &str) -> (String, String) {
    if let Some(colon_pos) = entry.find(':') {
        let agent = entry[..colon_pos].to_string();
        let model = entry[colon_pos + 1..].to_string();
        // Normalize trailing slashes in model part (defense-in-depth for bug #1507)
        let model = RouterConfig::normalize_model_identifier(&model);
        (agent, model)
    } else {
        (entry.to_string(), String::new())
    }
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            mode: "llm".to_string(),
            router_agent: "claude".to_string(),
            router_model: "claude-haiku-4-5-20251001".to_string(),
            timeout_seconds: 45,
            fallback_executor: "codex".to_string(),
            agents: DEFAULT_AGENTS.iter().map(|s| s.to_string()).collect(),
            max_route_attempts: 3,
            pool: vec![],
            fallback: String::new(),
            self_routing_penalty: 1.0,
            weights: HashMap::new(),
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
            // model_map is intentionally empty — config.yml is the sole source of truth.
            // Hardcoded models cause "Model not found" failures when the model does not
            // exist for a particular agent (e.g. anthropic/ prefixed models for opencode).
            model_map: HashMap::new(),
            weighted_round_robin: false,
            skip_limited_threshold: 0.3,
            agent_backoff_bases: HashMap::new(),
            max_tasks_per_tick: 1,
            ollama_url: "http://localhost:11434".to_string(),
            ollama_model: "qwen2.5-coder:3b-instruct".to_string(),
            ollama_timeout_seconds: 30,
            llm_budget_secs: 30,
        }
    }
}

impl RouterConfig {
    /// Load configuration from config files.
    pub fn from_config() -> Self {
        let mut config = Self::default();

        // Try to load from config
        if let Ok(mode) = crate::config::get("router.mode") {
            if mode == "round_robin" || mode == "llm" || mode == "local" {
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
                config.router_model = RouterConfig::normalize_model_identifier(&model);
            }
        }

        if let Ok(timeout) = crate::config::get("router.timeout_seconds") {
            if let Ok(secs) = timeout.parse::<u64>() {
                let clamped = secs.min(MAX_ROUTER_TIMEOUT_SECS);
                if clamped != secs {
                    static TIMEOUT_CLAMP_WARNED: std::sync::OnceLock<()> =
                        std::sync::OnceLock::new();
                    TIMEOUT_CLAMP_WARNED.get_or_init(|| {
                        tracing::warn!(
                            configured_secs = secs,
                            applied_secs = clamped,
                            max_secs = MAX_ROUTER_TIMEOUT_SECS,
                            "router.timeout_seconds is too high; clamping to keep routing responsive"
                        );
                    });
                }
                config.timeout_seconds = clamped;
            }
        }

        if let Ok(fallback) = crate::config::get("router.fallback_executor") {
            if !fallback.is_empty() {
                config.fallback_executor = fallback;
            }
        }

        // Parse agents list from top-level `agents:` config key
        config.agents = crate::engine::configured_agents();

        // Temporary operational toggle: allow disabling agents without editing
        // config files. Controlled by ORCH_EXCLUDE_AGENTS env var (comma-separated
        // list of agent names to exclude from routing). This keeps changes reversible
        // (unset the env var to restore behavior) and avoids editing user config files.
        // Example: ORCH_EXCLUDE_AGENTS=glm,some-other-agent
        let exclude_agents = std::env::var("ORCH_EXCLUDE_AGENTS")
            .map(|v| {
                v.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if !exclude_agents.is_empty() {
            let original_len = config.agents.len();
            config.agents.retain(|a| !exclude_agents.contains(a));
            let removed_count = original_len - config.agents.len();
            if removed_count > 0 {
                tracing::warn!(
                    "ORCH_EXCLUDE_AGENTS={}: temporarily removing {} agent(s) from router",
                    exclude_agents.join(","),
                    removed_count
                );
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

        // Parse self_routing_penalty
        if let Ok(val) = crate::config::get("router.self_routing_penalty") {
            if let Ok(penalty) = val.parse::<f64>() {
                config.self_routing_penalty = penalty.clamp(0.0, 1.0);
            }
        }

        // Parse per-agent weights from router.weights.<agent>
        for agent in &config.agents {
            let key = format!("router.weights.{agent}");
            if let Ok(val) = crate::config::get(&key) {
                if let Ok(w) = val.parse::<f64>() {
                    config.weights.insert(agent.clone(), w.max(0.0));
                }
            }
        }

        // Parse per-agent backoff base seconds from router.backoff_base.<agent>
        for agent in &config.agents {
            let key = format!("router.backoff_base.{agent}");
            if let Ok(val) = crate::config::get(&key) {
                if let Ok(secs) = val.parse::<i64>() {
                    config.agent_backoff_bases.insert(agent.clone(), secs);
                }
            }
        }

        // Parse max_tasks_per_tick
        if let Ok(val) = crate::config::get("router.max_tasks_per_tick") {
            if let Ok(n) = val.parse::<usize>() {
                if n > 0 {
                    config.max_tasks_per_tick = n;
                }
            }
        }

        // Parse llm_budget_secs — total time cap for the entire pool cascade.
        // Default to 30s so round-robin takes over before tick exceeds watchdog
        // threshold (6 × tick_interval). User config overrides this.
        config.llm_budget_secs = 30;
        if let Ok(val) = crate::config::get("router.llm_budget_secs") {
            if let Ok(secs) = val.parse::<u64>() {
                if secs > 0 {
                    config.llm_budget_secs = secs;
                }
            }
        }

        // Parse pool: list of "agent:model" entries.
        // `config::get_list()` returns Ok(vec![]) when the key is missing, so we
        // can't rely on `is_ok()` to indicate presence.
        match crate::config::get_list("router.pool") {
            Ok(pool_list) if !pool_list.is_empty() => {
                config.pool = pool_list;
            }
            _ => {
                // Accept comma-separated string lists: "a,b,c"
                if let Ok(pool_raw) = crate::config::get("router.pool") {
                    let parsed = pool_raw
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>();
                    if !parsed.is_empty() {
                        config.pool = parsed;
                    }
                }
            }
        }

        // Parse fallback: single "agent:model" entry
        if let Ok(fallback) = crate::config::get("router.fallback") {
            if !fallback.is_empty() {
                config.fallback = fallback;
            }
        }

        // Load model_map overrides from config (model_map.{complexity}.{agent})
        // Value may be a single string ("model-name") or a JSON array (["m1","m2"]).
        // Iterate over all known complexity tiers regardless of what is in model_map —
        // the Default impl has no hardcoded entries, so iterating over keys() would be a no-op.
        let known_agents = crate::engine::configured_agents();
        let known_complexities = ["simple", "medium", "complex", "review"];
        for complexity in known_complexities {
            for agent in &known_agents {
                let key = format!("model_map.{complexity}.{agent}");
                // Try as list first (YAML arrays), fall back to single string
                if let Ok(list) = crate::config::get_list(&key) {
                    if !list.is_empty() {
                        let normalized: Vec<String> = list
                            .iter()
                            .map(|m| RouterConfig::normalize_model_identifier(m))
                            .collect();
                        config
                            .model_map
                            .entry(complexity.to_string())
                            .or_default()
                            .insert(agent.to_string(), normalized);
                    }
                } else if let Ok(val) = crate::config::get(&key) {
                    if !val.is_empty() {
                        let normalized = RouterConfig::normalize_model_identifier(&val);
                        config
                            .model_map
                            .entry(complexity.to_string())
                            .or_default()
                            .insert(agent.to_string(), vec![normalized]);
                    }
                }
            }
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

        // Parse llm_budget_secs — total time cap for the entire pool cascade.
        // Default to 30s so round-robin takes over before tick exceeds watchdog
        // threshold (6 × tick_interval). User config overrides this.
        config.llm_budget_secs = 30;
        if let Ok(val) = crate::config::get("router.llm_budget_secs") {
            if let Ok(secs) = val.parse::<u64>() {
                if secs > 0 {
                    config.llm_budget_secs = secs;
                }
            }
        }

        // Parse Ollama-specific config for local routing mode
        if let Ok(url) = crate::config::get("router.ollama_url") {
            if !url.is_empty() {
                config.ollama_url = url;
            }
        }
        if let Ok(model) = crate::config::get("router.ollama_model") {
            if !model.is_empty() {
                config.ollama_model = model;
            }
        }
        if let Ok(timeout) = crate::config::get("router.ollama_timeout_seconds") {
            if let Ok(secs) = timeout.parse::<u64>() {
                if secs > 0 {
                    config.ollama_timeout_seconds = secs;
                }
            }
        }

        config
    }

    /// Return the effective pool — the configured pool, or a single-entry pool from `router_agent:router_model`.
    pub fn effective_pool(&self) -> Vec<String> {
        if !self.pool.is_empty() {
            self.pool.clone()
        } else {
            vec![format!("{}:{}", self.router_agent, self.router_model)]
        }
    }

    /// Return the effective fallback entry — the configured fallback, or `router_agent:router_model`.
    pub fn effective_fallback(&self) -> String {
        if !self.fallback.is_empty() {
            self.fallback.clone()
        } else {
            format!("{}:{}", self.router_agent, self.router_model)
        }
    }

    /// Check if a model identifier looks syntactically valid.
    /// Rejects empty strings, strings ending with slash, and strings with whitespace.
    fn is_valid_model_identifier(model: &str) -> bool {
        if model.is_empty() {
            return false;
        }
        // Reject trailing slash (e.g., "opus/")
        if model.ends_with('/') {
            return false;
        }
        // Reject whitespace
        if model.chars().any(char::is_whitespace) {
            return false;
        }
        // Ensure that if there is a slash, both parts are non-empty
        if let Some(slash_pos) = model.find('/') {
            let before = &model[..slash_pos];
            let after = &model[slash_pos + 1..];
            if before.is_empty() || after.is_empty() {
                return false;
            }
        }
        true
    }

    /// Normalize a model identifier by stripping trailing slashes.
    /// This is a defense-in-depth measure to handle edge cases where
    /// a model string might have been incorrectly formatted.
    pub fn normalize_model_identifier(model: &str) -> String {
        model.trim_end_matches('/').to_string()
    }

    fn expanded_model_pool(&self, agent: &str, complexity: &str) -> Option<Vec<String>> {
        let pool = self.model_map.get(complexity)?.get(agent)?;
        if pool.is_empty() {
            return Some(Vec::new());
        }

        let mut expanded_pool = Vec::new();
        let mut has_free = false;

        for model in pool {
            // Normalize the model identifier first to handle edge cases like trailing slashes
            let normalized = Self::normalize_model_identifier(model);
            if normalized == "opencode:free" {
                has_free = true;
            } else if Self::is_valid_model_identifier(&normalized) {
                expanded_pool.push(normalized);
            } else {
                tracing::debug!(agent, model, "skipping invalid model identifier in config");
            }
        }

        if has_free {
            expanded_pool
                .extend(crate::engine::runner::agents::opencode::discover_free_opencode_models());
        }

        // Also filter out invalid discovered free models (should be valid but just in case)
        expanded_pool.retain(|m| Self::is_valid_model_identifier(m));

        Some(expanded_pool)
    }

    pub(crate) fn model_pool_for_complexity(
        &self,
        agent: &str,
        complexity: &str,
    ) -> Option<Vec<String>> {
        self.expanded_model_pool(agent, complexity)
    }

    pub fn has_available_model_for_complexity(&self, agent: &str, complexity: &str) -> bool {
        match self.expanded_model_pool(agent, complexity) {
            None => false,
            Some(pool) => pool
                .iter()
                .any(|model| !crate::engine::cooldown::is_model_in_cooldown(agent, model)),
        }
    }

    /// Get the model for a given agent and complexity level.
    ///
    /// When the complexity tier has a pool of models, selects randomly using
    /// `task_id` as an entropy source and skips models currently in cooldown.
    /// Falls back to `pool[0]` if all models are cooled.
    ///
    /// Backward-compatible: a single-model pool always returns that model.
    pub fn model_for_complexity(
        &self,
        agent: &str,
        complexity: &str,
        task_id: &str,
    ) -> Option<String> {
        let expanded_pool = self.expanded_model_pool(agent, complexity)?;
        if expanded_pool.is_empty() {
            return None;
        }
        // Random starting index — varies per task_id to distribute across the pool
        let start =
            crate::engine::router::selection::simple_hash_index_for(expanded_pool.len(), task_id);
        // Walk the pool from start, skipping cooled and invalid models
        for i in 0..expanded_pool.len() {
            let model = &expanded_pool[(start + i) % expanded_pool.len()];
            // Safety check: skip invalid identifiers (should already be filtered)
            if !Self::is_valid_model_identifier(model) {
                tracing::debug!(
                    agent,
                    model,
                    "invalid model identifier slipped through validation"
                );
                continue;
            }
            if !crate::engine::cooldown::is_model_in_cooldown(agent, model) {
                // Normalize the model identifier to handle edge cases like trailing slashes
                return Some(Self::normalize_model_identifier(model));
            }
        }
        // All models cooled — return None so the caller can fall back to a different agent
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{RouterConfig, MAX_ROUTER_TIMEOUT_SECS};
    use std::sync::{Mutex, OnceLock};

    struct CurrentDirGuard {
        original: std::path::PathBuf,
    }

    impl CurrentDirGuard {
        fn set(path: &std::path::Path) -> Self {
            let original = std::env::current_dir().unwrap();
            std::env::set_current_dir(path).unwrap();
            Self { original }
        }
    }

    impl Drop for CurrentDirGuard {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.original).unwrap();
        }
    }

    fn cwd_mutex() -> &'static Mutex<()> {
        static CWD_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
        CWD_MUTEX.get_or_init(|| Mutex::new(()))
    }

    #[tokio::test]
    async fn model_for_complexity_returns_none_when_all_models_cooled() {
        let mut config = RouterConfig::default();
        // Inject a two-model pool for a unique agent name to avoid cross-test pollution
        config
            .model_map
            .entry("complex".to_string())
            .or_default()
            .insert(
                "testagent_allcooled".to_string(),
                vec![
                    "cooldown-model-a".to_string(),
                    "cooldown-model-b".to_string(),
                ],
            );

        // Put both models into cooldown
        crate::engine::cooldown::record_model_failure("testagent_allcooled", "cooldown-model-a")
            .await;
        crate::engine::cooldown::record_model_failure("testagent_allcooled", "cooldown-model-b")
            .await;

        let result = config.model_for_complexity("testagent_allcooled", "complex", "task-0");
        assert!(
            result.is_none(),
            "should return None when all models in pool are in cooldown, got {result:?}"
        );
    }

    #[tokio::test]
    async fn model_for_complexity_returns_none_when_single_model_cooled() {
        let mut config = RouterConfig::default();
        config
            .model_map
            .entry("complex".to_string())
            .or_default()
            .insert(
                "testagent_singlecooled".to_string(),
                vec!["single-cooled-model".to_string()],
            );

        crate::engine::cooldown::record_model_failure(
            "testagent_singlecooled",
            "single-cooled-model",
        )
        .await;

        let result = config.model_for_complexity("testagent_singlecooled", "complex", "task-0");
        assert!(
            result.is_none(),
            "should return None when the only model in pool is in cooldown, got {result:?}"
        );
    }

    #[test]
    fn model_for_complexity_returns_model_when_not_cooled() {
        let mut config = RouterConfig::default();
        config
            .model_map
            .entry("complex".to_string())
            .or_default()
            .insert(
                "testagent_notcooled".to_string(),
                vec!["healthy-model".to_string()],
            );

        // Ensure no cooldown is active for this specific model
        let result = config.model_for_complexity("testagent_notcooled", "complex", "task-0");
        assert_eq!(
            result.as_deref(),
            Some("healthy-model"),
            "should return the model when it is not in cooldown"
        );
    }

    #[test]
    fn has_available_model_returns_false_when_no_pool_configured() {
        let mut config = RouterConfig::default();
        // Simulate olm agent with only simple and medium configured (missing complex and review)
        config
            .model_map
            .entry("simple".to_string())
            .or_default()
            .insert("olm".to_string(), vec!["qwen3.5".to_string()]);
        config
            .model_map
            .entry("medium".to_string())
            .or_default()
            .insert("olm".to_string(), vec!["gemma4".to_string()]);
        // Note: complex and review tiers are NOT configured for olm

        // has_available_model_for_complexity should return false when no pool exists
        assert!(
            config.has_available_model_for_complexity("olm", "simple"),
            "should return true when model pool exists and is configured"
        );
        assert!(
            config.has_available_model_for_complexity("olm", "medium"),
            "should return true when model pool exists and is configured"
        );
        assert!(
            !config.has_available_model_for_complexity("olm", "complex"),
            "should return false when no model pool is configured for agent+complexity"
        );
        assert!(
            !config.has_available_model_for_complexity("olm", "review"),
            "should return false when no model pool is configured for agent+complexity"
        );
    }

    #[test]
    fn default_self_routing_penalty_is_one() {
        let config = RouterConfig::default();
        assert_eq!(
            config.self_routing_penalty, 1.0,
            "default penalty must be 1.0 (no penalty = full self-routing allowed)"
        );
    }

    #[test]
    fn from_config_reads_self_routing_penalty() {
        let _lock = cwd_mutex().lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".orch.yml"),
            "router:\n  self_routing_penalty: 0.5\n",
        )
        .unwrap();
        let _guard = CurrentDirGuard::set(dir.path());

        let config = RouterConfig::from_config();
        assert_eq!(config.self_routing_penalty, 0.5);
    }

    #[test]
    fn default_config_model_map_is_empty() {
        // model_map must be empty — config.yml is the sole source of truth for models.
        // Hardcoded defaults (especially anthropic/ prefixed opencode models) cause
        // "Model not found" failures when the model does not exist for a given agent.
        let config = RouterConfig::default();
        assert!(
            config.model_map.is_empty(),
            "Default RouterConfig must have no hardcoded models; got {:?}",
            config.model_map
        );
    }

    #[test]
    fn from_config_reads_router_pool_yaml_array() {
        let _lock = cwd_mutex().lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".orch.yml"),
            "router:\n  pool:\n    - opencode:free\n    - opencode:github-copilot/gpt-5-mini\n    - kimi:k2p5\n    - claude:haiku\n",
        )
        .unwrap();
        let _guard = CurrentDirGuard::set(dir.path());

        let config = RouterConfig::from_config();

        assert_eq!(
            config.pool,
            vec![
                "opencode:free",
                "opencode:github-copilot/gpt-5-mini",
                "kimi:k2p5",
                "claude:haiku",
            ]
        );
    }

    #[test]
    fn from_config_clamps_router_timeout_to_max() {
        let _lock = cwd_mutex().lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".orch.yml"),
            "router:\n  timeout_seconds: 90\n",
        )
        .unwrap();
        let _guard = CurrentDirGuard::set(dir.path());

        let config = RouterConfig::from_config();
        assert_eq!(config.timeout_seconds, MAX_ROUTER_TIMEOUT_SECS);
    }

    #[test]
    fn normalize_model_identifier_strips_trailing_slash() {
        // Bug #1507: model assignment was producing "opus/" instead of "opus"
        assert_eq!(
            RouterConfig::normalize_model_identifier("opus/"),
            "opus",
            "trailing slash should be stripped from model identifier"
        );
        assert_eq!(
            RouterConfig::normalize_model_identifier("sonnet/"),
            "sonnet",
            "trailing slash should be stripped from model identifier"
        );
        assert_eq!(
            RouterConfig::normalize_model_identifier("github-copilot/gpt-5-mini/"),
            "github-copilot/gpt-5-mini",
            "trailing slash should be stripped from model identifier with internal slash"
        );
        // Edge cases: multiple trailing slashes
        assert_eq!(
            RouterConfig::normalize_model_identifier("opus//"),
            "opus",
            "multiple trailing slashes should be stripped"
        );
        // Edge cases: no trailing slash
        assert_eq!(
            RouterConfig::normalize_model_identifier("opus"),
            "opus",
            "model without trailing slash should remain unchanged"
        );
    }

    #[test]
    fn is_valid_model_identifier_rejects_trailing_slash() {
        // Ensure validation correctly rejects models with trailing slashes
        assert!(
            !RouterConfig::is_valid_model_identifier("opus/"),
            "model with trailing slash should be invalid"
        );
        assert!(
            RouterConfig::is_valid_model_identifier("opus"),
            "model without trailing slash should be valid"
        );
        assert!(
            RouterConfig::is_valid_model_identifier("github-copilot/gpt-5-mini"),
            "model with internal slash should be valid"
        );
        assert!(
            !RouterConfig::is_valid_model_identifier("github-copilot/"),
            "model ending with slash after prefix should be invalid"
        );
    }

    #[test]
    fn from_config_loads_model_map_for_custom_agents() {
        let _lock = cwd_mutex().lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        // Simulate a config with a custom agent (olm) in both agents and model_map
        std::fs::write(
            dir.path().join(".orch.yml"),
            r#"
agents:
  - claude
  - olm
model_map:
  simple:
    claude: haiku
    olm: qwen3.5
  medium:
    claude: sonnet
    olm: gemma4
  complex:
    claude: opus
    olm: gemma4
"#,
        )
        .unwrap();
        let _guard = CurrentDirGuard::set(dir.path());

        let config = RouterConfig::from_config();

        // olm models should be loaded from the model_map
        assert_eq!(
            config.model_for_complexity("olm", "simple", "test-task"),
            Some("qwen3.5".to_string()),
            "custom agent olm should have simple model from config"
        );
        assert_eq!(
            config.model_for_complexity("olm", "medium", "test-task"),
            Some("gemma4".to_string()),
            "custom agent olm should have medium model from config"
        );
        // claude should also work
        assert_eq!(
            config.model_for_complexity("claude", "simple", "test-task"),
            Some("haiku".to_string()),
        );
    }

    #[test]
    fn model_for_complexity_normalizes_trailing_slash_from_pool() {
        // Bug #1507: full path test - model identifiers with trailing slashes
        // should be normalized (not dropped) when loading from model_map pool
        let mut config = RouterConfig::default();
        config
            .model_map
            .entry("complex".to_string())
            .or_default()
            .insert(
                "testagent_trailing".to_string(),
                vec![
                    "opus/".to_string(),
                    "sonnet//".to_string(),
                    "github-copilot/gpt-5-mini/".to_string(),
                ],
            );

        // Should return normalized "opus" instead of dropping it as invalid
        let result = config.model_for_complexity("testagent_trailing", "complex", "task-0");
        assert!(
            result.is_some(),
            "should return a model from pool with trailing slashes normalized, got None"
        );
        let model = result.unwrap();
        // The model returned should be one of the normalized versions
        assert!(
            model == "opus" || model == "sonnet" || model == "github-copilot/gpt-5-mini",
            "returned model should be normalized (no trailing slash), got: {}",
            model
        );
        // Verify no trailing slashes in the returned model
        assert!(
            !model.ends_with('/'),
            "returned model should not have trailing slash, got: {}",
            model
        );
    }

    #[test]
    fn default_llm_budget_secs_is_30() {
        // Bug #3048: llm_budget_secs must be low enough that a single slow pool
        // entry times out before the tick watchdog fires (6 × tick_interval = 60s).
        // With a 5-entry pool and 90s per-entry timeout, 30s budget ensures at most
        // one entry can time out before round-robin fallback, preventing tick stalls.
        let config = RouterConfig::default();
        assert_eq!(
            config.llm_budget_secs, 30,
            "llm_budget_secs default should be 30s to prevent tick watchdog stalls"
        );
    }
}
