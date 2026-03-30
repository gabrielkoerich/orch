//! Router configuration — loading, defaults, and model map.

use std::collections::HashMap;

/// Default agents to check in PATH.
///
/// All 5 agents are listed, but availability is checked at runtime via
/// `which::which()`. Agents not installed (e.g. kimi, minimax) are
/// automatically skipped during routing. Users can customize this list
/// in their `config.yml` under `routing.agents`.
pub const DEFAULT_AGENTS: &[&str] = &["claude", "codex", "opencode", "kimi", "minimax"];

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
            timeout_seconds: 60,
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
        }
    }
}

impl RouterConfig {
    /// Run `opencode models` and return lines containing "free".
    ///
    /// This is a synchronous blocking call.
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
        let known_agents = ["claude", "codex", "opencode", "kimi", "minimax"];
        let known_complexities = ["simple", "medium", "complex", "review"];
        for complexity in known_complexities {
            for agent in &known_agents {
                let key = format!("model_map.{complexity}.{agent}");
                // Try as list first (YAML arrays), fall back to single string
                if let Ok(list) = crate::config::get_list(&key) {
                    if !list.is_empty() {
                        config
                            .model_map
                            .entry(complexity.to_string())
                            .or_default()
                            .insert(agent.to_string(), list);
                    }
                } else if let Ok(val) = crate::config::get(&key) {
                    if !val.is_empty() {
                        config
                            .model_map
                            .entry(complexity.to_string())
                            .or_default()
                            .insert(agent.to_string(), vec![val]);
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

    fn expanded_model_pool(&self, agent: &str, complexity: &str) -> Option<Vec<String>> {
        let pool = self.model_map.get(complexity)?.get(agent)?;
        if pool.is_empty() {
            return Some(Vec::new());
        }

        let mut expanded_pool = Vec::new();
        let mut has_free = false;

        for model in pool {
            if model == "opencode:free" {
                has_free = true;
            } else {
                expanded_pool.push(model.clone());
            }
        }

        if has_free {
            expanded_pool.extend(Self::discover_free_opencode_models());
        }

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
            None => true,
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
        // Walk the pool from start, skipping cooled models
        for i in 0..expanded_pool.len() {
            let model = &expanded_pool[(start + i) % expanded_pool.len()];
            if !crate::engine::cooldown::is_model_in_cooldown(agent, model) {
                return Some(model.clone());
            }
        }
        // All models cooled — return None so the caller can fall back to a different agent
        None
    }
}

#[cfg(test)]
mod tests {
    use super::RouterConfig;
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

    #[test]
    fn model_for_complexity_returns_none_when_all_models_cooled() {
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
        crate::engine::cooldown::record_model_failure("testagent_allcooled", "cooldown-model-a");
        crate::engine::cooldown::record_model_failure("testagent_allcooled", "cooldown-model-b");

        let result = config.model_for_complexity("testagent_allcooled", "complex", "task-0");
        assert!(
            result.is_none(),
            "should return None when all models in pool are in cooldown, got {result:?}"
        );
    }

    #[test]
    fn model_for_complexity_returns_none_when_single_model_cooled() {
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
        );

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
}
