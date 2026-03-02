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

pub use config::RouterConfig;
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
            llm_router: LlmRouter::new(),
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
            if crate::cmd_cache::command_exists(agent) {
                agents.push(agent.to_string());
            }
        }
        agents
    }

    /// Check if an agent is available.
    pub fn is_agent_available(&self, agent: &str) -> bool {
        self.available_agents.contains(&agent.to_string())
    }

    /// Get the first available agent.
    /// Pick next agent via round-robin (for review or other non-task routing).
    pub fn next_round_robin_agent(&self) -> Option<String> {
        if self.available_agents.is_empty() {
            return None;
        }
        let idx: usize = crate::sidecar::get("_review_rr", "index")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let agent = self.available_agents[idx % self.available_agents.len()].clone();
        let next = (idx + 1) % self.available_agents.len();
        let _ = crate::sidecar::set("_review_rr", &[format!("index={next}")]);
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
    pub async fn route(&self, task: &ExternalTask) -> anyhow::Result<RouteResult> {
        // 1. Check for explicit agent label
        if let Some(agent) =
            strategies::extract_agent_from_labels(&self.config.agents, &task.labels)
        {
            if self.is_agent_available(&agent) {
                let complexity = strategies::extract_complexity_from_labels(&task.labels);
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
            return strategies::route_via_weighted_round_robin(
                &self.available_agents,
                &self.weights,
                &self.config,
                task,
            );
        }

        // 3. Round-robin mode — use stateful round-robin
        if self.config.mode == "round_robin" {
            tracing::debug!(task_id = %task.id.0, "routing via round-robin mode");
            return strategies::route_via_round_robin_stateful(
                &self.available_agents,
                &self.config,
                task,
            );
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
            return strategies::route_via_round_robin_stateful(
                &self.available_agents,
                &self.config,
                task,
            );
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
                    )
                } else {
                    strategies::route_via_fallback(&self.available_agents, &self.config, task)
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

    /// Route using LLM classification. Delegates to `self.llm_router`.
    async fn route_with_llm(&self, task: &ExternalTask) -> anyhow::Result<RouteResult> {
        self.llm_router
            .route_with_llm(task, &self.available_agents, &self.config)
            .await
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
            self.llm_router.parse_llm_response(response)
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
        let router = Router {
            config,
            available_agents: agents,
            weights,
            llm_router: LlmRouter::new(),
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
            llm_router: LlmRouter::new(),
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
        let router = Router {
            config,
            available_agents: agents,
            weights,
            llm_router: LlmRouter::new(),
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
            llm_router: LlmRouter::new(),
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
