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
    /// Model map for complexity levels
    pub model_map: HashMap<String, HashMap<String, String>>,
    /// Enable weighted round-robin routing based on rate limit capacity.
    /// When true, agents that hit rate limits get fewer tasks.
    pub weighted_round_robin: bool,
}

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
            timeout_seconds: 60,
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

        // Load model_map overrides from config (model_map.{complexity}.{agent})
        let known_agents = ["claude", "codex", "opencode", "kimi", "minimax"];
        for complexity in config.model_map.keys().cloned().collect::<Vec<_>>() {
            for agent in &known_agents {
                let key = format!("model_map.{complexity}.{agent}");
                if let Ok(model) = crate::config::get(&key) {
                    if !model.is_empty() {
                        config
                            .model_map
                            .entry(complexity.clone())
                            .or_default()
                            .insert(agent.to_string(), model);
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

    /// Get the model for a given agent and complexity level.
    pub fn model_for_complexity(&self, agent: &str, complexity: &str) -> Option<String> {
        self.model_map
            .get(complexity)
            .and_then(|m| m.get(agent))
            .cloned()
    }

    /// Get the model for a given agent and complexity level, falling back to built-in defaults.
    ///
    /// Unlike [`model_for_complexity`], this always returns a non-empty `String`.
    /// Use this instead of hardcoding fallback model names at call sites.
    pub fn model_for_complexity_or_default(&self, agent: &str, complexity: &str) -> String {
        self.model_for_complexity(agent, complexity)
            .or_else(|| Self::default().model_for_complexity(agent, complexity))
            .unwrap_or_else(|| "claude-sonnet-4-6".to_string())
    }
}
