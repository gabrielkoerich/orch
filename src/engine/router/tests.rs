use super::config::DEFAULT_AGENTS;
use super::llm::LlmRouteResponse;
use super::strategies;
use super::weights::{AgentWeights, DEFAULT_WEIGHT, MIN_WEIGHT};
use super::*;
use crate::backends::{ExternalId, ExternalTask};
use crate::engine::cooldown::{
    clear_agent_degraded, is_agent_degraded, is_model_in_cooldown, mark_agent_degraded,
    record_model_failure, set_model_cooldown,
};
use crate::store::NewTask;
use crate::store::TaskStore;
use serial_test::serial;
use std::env;
#[cfg(unix)]
use std::ffi::OsString;
use std::fs;
#[cfg(unix)]
use std::fs::Permissions;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::process;
#[cfg(unix)]
use std::process::Command;
#[cfg(unix)]
use std::thread;
use std::time::Duration;

#[test]
fn max_turns_error_matches_grok_phrases() {
    assert!(is_max_turns_error(
        "router LLM returned error: Reached the maximum number of turns"
    ));
    assert!(is_max_turns_error("Error: max turns reached"));
    assert!(is_max_turns_error(
        "router LLM failed: router LLM returned error: Reached the maximum number of turns"
    ));
    assert!(!is_max_turns_error("router LLM timed out after 30s"));
    assert!(!is_max_turns_error("rate limited"));
}

#[tokio::test]
#[serial_test::serial(orch_home)]
async fn get_route_result_replaces_a_stale_model_with_the_complexity_model() {
    let home = tempfile::tempdir().unwrap();
    fs::write(
        home.path().join("config.yml"),
        "model_map:\n  medium:\n    claude: sonnet\n",
    )
    .unwrap();
    // SAFETY: nextest runs each test in its own process, so no other test touches the environment
    unsafe { env::set_var("ORCH_HOME", home.path()) };

    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let store_id = store
        .create(&NewTask {
            external_id: Some("7".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "stale model".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    store
        .set_fields(
            store_id,
            &[
                ("agent", serde_json::json!("claude")),
                ("model", serde_json::json!("claude-removed-model")),
                ("complexity", serde_json::json!("medium")),
            ],
        )
        .await
        .unwrap();

    let route = get_route_result(&store, "owner/repo", "7").await;
    let stored = store.get(store_id).await.unwrap().model;
    // SAFETY: nextest runs each test in its own process, so no other test touches the environment
    unsafe { env::remove_var("ORCH_HOME") };

    assert_eq!(route.unwrap().model.as_deref(), Some("sonnet"));
    assert_eq!(stored.as_deref(), Some("sonnet"));
}

// Test-only delegate so tests can call router.parse_llm_response() directly
// without referencing llm_router.
impl Router {
    #[expect(
        clippy::unused_self,
        reason = "the test helper keeps the router method syntax"
    )]
    fn parse_llm_response(&self, response: &str) -> anyhow::Result<LlmRouteResponse> {
        LlmRouter::parse_llm_response("claude", response)
    }
}

async fn test_store() -> Arc<TaskStore> {
    Arc::new(TaskStore::open_memory().await.unwrap())
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

#[serial(cooldown_state)]
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

#[serial(cooldown_state)]
#[tokio::test]
async fn all_agents_cooled_error_names_degraded_agents() {
    cooldown::reset_global_state().await;
    mark_agent_degraded("claude");
    mark_agent_degraded("grok");
    let err = AllAgentsCooledError::new("all agents".to_string(), None);
    let msg = err.to_string();
    assert!(
        msg.contains("degraded: claude, grok"),
        "ALL AGENTS COOLED must name degraded agents, got {msg}"
    );
    assert_eq!(err.degraded(), &["claude".to_string(), "grok".to_string()]);
    clear_agent_degraded("claude");
    clear_agent_degraded("grok");
}

#[serial(cooldown_state)]
#[test]
fn default_agents_constant() {
    assert_eq!(DEFAULT_AGENTS.len(), 7);
    assert!(DEFAULT_AGENTS.contains(&"claude"));
    assert!(DEFAULT_AGENTS.contains(&"kimi"));
    assert!(DEFAULT_AGENTS.contains(&"minimax"));
    assert!(DEFAULT_AGENTS.contains(&"glm"));
    assert!(DEFAULT_AGENTS.contains(&"grok"));
}

#[serial(cooldown_state)]
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

#[serial(cooldown_state)]
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
            tools: vec!["git".to_string()],
            constraints: vec![],
            execution: None,
        },
        warning: None,
    };

    let json = serde_json::to_string(&result).unwrap();
    assert!(json.contains("claude"));
    assert!(json.contains("sonnet"));

    let deserialized: RouteResult = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.agent, "claude");
    assert_eq!(deserialized.model, Some("sonnet".to_string()));
}

#[serial(cooldown_state)]
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

    assert_eq!(config.mode, "round_robin");
    assert_eq!(config.router_agent, "claude");
    assert_eq!(
        config.router_model, "",
        "no router.model means the router agent's CLI default"
    );
    assert_eq!(config.fallback_executor, "codex");
    assert_eq!(config.max_route_attempts, 3);
    assert_ne!(config.allowed_tools, [] as [String; 0]);

    // Verify configurable agents list includes all 7 agents
    assert_eq!(config.agents.len(), 7);
    assert!(config.agents.contains(&"claude".to_string()));
    assert!(config.agents.contains(&"codex".to_string()));
    assert!(config.agents.contains(&"opencode".to_string()));
    assert!(config.agents.contains(&"kimi".to_string()));
    assert!(config.agents.contains(&"minimax".to_string()));
    assert!(config.agents.contains(&"glm".to_string()));
    assert!(config.agents.contains(&"grok".to_string()));
}

#[serial(cooldown_state)]
#[test]
fn model_map_lookup_returns_none_without_config() {
    // Default config has no hardcoded models — config.yml is the source of truth.
    let config = RouterConfig::default();
    assert!(
        config
            .model_for_complexity("claude", "simple", "")
            .is_none()
    );
    assert!(
        config
            .model_for_complexity("claude", "medium", "")
            .is_none()
    );
    assert!(
        config
            .model_for_complexity("claude", "complex", "")
            .is_none()
    );
    assert!(
        config
            .model_for_complexity("opencode", "review", "")
            .is_none()
    );
}

#[serial(cooldown_state)]
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

#[serial(cooldown_state)]
#[tokio::test]
async fn model_pool_selection_skips_cooled() {
    cooldown::reset_global_state().await;

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
}

#[serial(cooldown_state)]
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

#[serial(cooldown_state)]
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

#[serial(cooldown_state)]
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

#[serial(cooldown_state)]
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

#[serial(cooldown_state)]
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

#[serial(cooldown_state)]
#[test]
fn parse_llm_response_claude_envelope_error() {
    let config = RouterConfig::default();
    let router = Router::new(config);

    let response = r#"{"type":"result","subtype":"error","is_error":true,"result":"rate limit exceeded","usage":{}}"#;

    let err = router.parse_llm_response(response).unwrap_err();
    assert!(err.to_string().contains("error"), "got: {err}");
}

#[serial(cooldown_state)]
#[test]
fn parse_llm_response_empty() {
    let config = RouterConfig::default();
    let router = Router::new(config);

    let err = router.parse_llm_response("").unwrap_err();
    assert!(err.to_string().contains("empty"), "got: {err}");
}

/// Claude envelope where result is a JSON object, not a string.
/// Some Claude versions may return the result as a nested object.
#[serial(cooldown_state)]
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
#[serial(cooldown_state)]
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
#[serial(cooldown_state)]
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
#[serial(cooldown_state)]
#[test]
fn fixture_route_response_string() {
    let config = RouterConfig::default();
    let router = Router::new(config);

    let response = include_str!("../../../tests/fixtures/route-response-string.json");
    let parsed = router.parse_llm_response(response).unwrap();
    assert_eq!(parsed.executor, "codex");
    assert_eq!(parsed.complexity, "medium");
    assert_ne!(parsed.reason, "");
    assert_ne!(parsed.profile.role, "");
}

/// Real Claude envelope with result as JSON object (not string).
/// This was the root cause of all router failures on 2026-02-27.
#[serial(cooldown_state)]
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
#[serial(cooldown_state)]
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
#[serial(cooldown_state)]
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

#[serial(cooldown_state)]
#[tokio::test]
async fn route_round_robin_basic() {
    cooldown::reset_global_state().await;
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
        llm_router: Arc::new(LlmRouter::new()),
        ollama_router: None,
        rr_index: 0,
        last_agent: None,
        review_rr_index: 0,
        router_pool: vec![],
        pool_index: 0,
    };

    let task = create_test_task("1", "Test task", vec![]);
    let result =
        strategies::route_via_round_robin(&router.available_agents, &router.config, &task).unwrap();

    // Task 1 % 2 agents = agent at index 1 = codex
    assert_eq!(result.agent, "codex");
    assert_eq!(result.reason, "round_robin (task 1 % 2 agents)");
}

#[serial(cooldown_state)]
#[tokio::test]
async fn route_uses_label_override() {
    cooldown::reset_global_state().await;
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
        llm_router: Arc::new(LlmRouter::new()),
        ollama_router: None,
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

#[serial(cooldown_state)]
#[tokio::test]
async fn route_uses_stored_agent_over_label() {
    cooldown::reset_global_state().await;
    let mut config = RouterConfig::default();
    let medium = config.model_map.entry("medium".to_string()).or_default();
    medium.insert("claude".to_string(), vec!["haiku".to_string()]);
    medium.insert("codex".to_string(), vec!["gpt-test".to_string()]);
    let agents = vec!["claude".to_string(), "codex".to_string()];
    let mut router = Router::new_for_test(config, agents);

    let store = test_store().await;
    let store_id = store
        .create(&NewTask {
            external_id: Some("1".to_string()),
            repo: "test/repo".to_string(),
            origin: "github".to_string(),
            title: "Test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    store
        .set_fields(store_id, &[("agent", serde_json::json!("codex"))])
        .await
        .unwrap();

    let task = create_test_task("1", "Test", vec!["agent:claude".to_string()]);
    let result = router.route(&task, &store, "test/repo").await.unwrap();
    assert_eq!(result.agent, "codex");
}

#[serial(cooldown_state)]
#[test]
fn check_routing_sanity_warnings() {
    // Backend task routed to claude should warn
    let task = create_test_task("1", "Fix API", vec!["backend".to_string()]);
    let profile = AgentProfile {
        role: "general".to_string(),
        tools: vec![],
        constraints: vec![],
        execution: None,
    };
    let warning = LlmRouter::check_routing_sanity(&task, "claude", &profile);
    assert!(warning.is_some());
    assert!(warning.unwrap().contains("backend"));

    // Docs task routed to codex should warn
    let task = create_test_task("1", "Update README", vec!["docs".to_string()]);
    let warning = LlmRouter::check_routing_sanity(&task, "codex", &profile);
    assert!(warning.is_some());
    assert!(warning.unwrap().contains("docs"));

    // Normal routing should not warn
    let task = create_test_task("1", "Fix bug", vec!["bug".to_string()]);
    let warning = LlmRouter::check_routing_sanity(&task, "codex", &profile);
    assert!(warning.is_none());
}

#[serial(cooldown_state)]
#[tokio::test]
async fn router_reload_preserves_structure() {
    cooldown::reset_global_state().await;
    let config = RouterConfig::default();
    let agents = vec!["claude".to_string()];
    let mut weights = AgentWeights::default();
    weights.ensure_agents(&agents);
    let mut router = Router {
        config,
        available_agents: agents,
        weights,
        llm_router: Arc::new(LlmRouter::new()),
        ollama_router: None,
        rr_index: 0,
        last_agent: None,
        review_rr_index: 0,
        router_pool: vec![],
        pool_index: 0,
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
    assert_ne!(router.config.fallback_executor, "");
    // Tools should always be populated
    assert_ne!(router.config.allowed_tools, [] as [String; 0]);
}

// --- Weighted round-robin tests ---

#[serial(cooldown_state)]
#[tokio::test]
async fn route_weighted_round_robin_basic() {
    cooldown::reset_global_state().await;
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
        llm_router: Arc::new(LlmRouter::new()),
        ollama_router: None,
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

#[serial(cooldown_state)]
#[tokio::test]
async fn route_weighted_round_robin_respects_label_override() {
    cooldown::reset_global_state().await;
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
        llm_router: Arc::new(LlmRouter::new()),
        ollama_router: None,
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

#[serial(cooldown_state)]
#[tokio::test]
async fn route_takes_the_adjacent_tier_model_when_the_own_tier_is_cooled() {
    cooldown::reset_global_state().await;
    let agent = "tierroute_agent";
    let mut config = RouterConfig {
        agents: vec![agent.to_string()],
        ..Default::default()
    };
    config.model_map.clear();
    for (tier, model) in [("medium", "tr-m"), ("complex", "tr-c")] {
        config
            .model_map
            .entry(tier.to_string())
            .or_default()
            .insert(agent.to_string(), vec![model.to_string()]);
    }
    cooldown::record_model_failure(agent, "tr-m").await;
    let mut router = Router::new_for_test(config, vec![agent.to_string()]);

    let task = create_test_task(
        "1",
        "Test task",
        vec![format!("agent:{agent}"), "complexity:medium".to_string()],
    );
    let store = test_store().await;
    let result = router.route(&task, &store, "test/repo").await.unwrap();
    assert_eq!(result.agent, agent);
    assert_eq!(result.model.as_deref(), Some("tr-c"));
    assert_eq!(result.complexity, "medium");
    assert!(result.warning.unwrap().contains("complex tier"));
}

#[serial(cooldown_state)]
#[test]
fn router_config_weighted_round_robin_default_false() {
    let config = RouterConfig::default();
    assert!(!config.weighted_round_robin);
}

#[serial(cooldown_state)]
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

#[serial(cooldown_state)]
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
        llm_router: Arc::new(LlmRouter::new()),
        ollama_router: None,
        rr_index: 0,
        last_agent: None,
        review_rr_index: 0,
        router_pool: vec![],
        pool_index: 0,
    };

    let a1 = router.next_round_robin_agent(&[], "medium").unwrap();
    let a2 = router.next_round_robin_agent(&[], "medium").unwrap();
    let a3 = router.next_round_robin_agent(&[], "medium").unwrap();

    // All three agents should appear (order depends on start index)
    let mut seen = vec![a1, a2, a3];
    seen.sort();
    assert_eq!(seen, vec!["test_a", "test_b", "test_c"]);
}

#[serial(cooldown_state)]
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
        llm_router: Arc::new(LlmRouter::new()),
        ollama_router: None,
        rr_index: 0,
        last_agent: None,
        // Start index at 2, which is >= candidates.len() (2), triggering the bug
        review_rr_index: 2,
        router_pool: vec![],
        pool_index: 0,
    };

    // All candidates excluded — should fall back to first candidate, not None
    let result = router.next_round_robin_agent(&["test_a", "test_b"], "medium");
    assert!(
        result.is_some(),
        "next_round_robin_agent must not return None when candidates is non-empty"
    );
}

#[serial(cooldown_state)]
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
        llm_router: Arc::new(LlmRouter::new()),
        ollama_router: None,
        rr_index: 0,
        last_agent: None,
        review_rr_index: 0,
        router_pool: vec![],
        pool_index: 0,
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

#[serial(cooldown_state)]
#[tokio::test]
async fn last_agent_tracks_routing() {
    cooldown::reset_global_state().await;
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
        llm_router: Arc::new(LlmRouter::new()),
        ollama_router: None,
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

#[serial(cooldown_state)]
#[test]
fn parse_pool_entry_splits_on_first_colon() {
    let (agent, model) = super::parse_pool_entry("claude:haiku");
    assert_eq!(agent, "claude");
    assert_eq!(model, "haiku");
}

#[serial(cooldown_state)]
#[test]
fn parse_pool_entry_model_with_slash() {
    // Models like "github-copilot/gpt-5-mini" must not be split on the slash
    let (agent, model) = super::parse_pool_entry("opencode:github-copilot/gpt-5-mini");
    assert_eq!(agent, "opencode");
    assert_eq!(model, "github-copilot/gpt-5-mini");
}

#[serial(cooldown_state)]
#[test]
fn parse_pool_entry_no_colon() {
    let (agent, model) = super::parse_pool_entry("claude");
    assert_eq!(agent, "claude");
    assert_eq!(model, "");
}

#[serial(cooldown_state)]
#[test]
fn effective_pool_uses_configured_pool() {
    let config = RouterConfig {
        pool: vec!["kimi:k2p5".to_string(), "claude:haiku".to_string()],
        ..RouterConfig::default()
    };
    let pool = config.effective_pool();
    assert_eq!(pool, vec!["kimi:k2p5", "claude:haiku"]);
}

#[serial(cooldown_state)]
#[test]
fn effective_pool_falls_back_to_router_agent_model() {
    let config = RouterConfig::default();
    // Default has no pool → derives from router_agent:router_model
    let pool = config.effective_pool();
    assert_eq!(pool.len(), 1);
    assert!(pool[0].starts_with(&config.router_agent));
    assert!(pool[0].contains(&config.router_model));
}

#[serial(cooldown_state)]
#[test]
fn effective_fallback_uses_configured_fallback() {
    let config = RouterConfig {
        fallback: "claude:haiku".to_string(),
        ..RouterConfig::default()
    };
    assert_eq!(config.effective_fallback(), "claude:haiku");
}

#[serial(cooldown_state)]
#[test]
fn effective_fallback_derives_from_router_agent_model() {
    let config = RouterConfig::default();
    let fallback = config.effective_fallback();
    assert!(fallback.starts_with(&config.router_agent));
    assert!(fallback.contains(&config.router_model));
}

// ── pool expansion ────────────────────────────────────────────────────────

#[serial(cooldown_state)]
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

#[serial(cooldown_state)]
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

#[serial(cooldown_state)]
#[test]
fn expand_pool_opencode_free_skipped_when_not_installed() {
    // Test/debug builds skip live `opencode models`, so `opencode:free` expands
    // to nothing and the pool falls back to effective_fallback
    let config = RouterConfig {
        pool: vec!["opencode:free".to_string()],
        fallback: "claude:haiku".to_string(),
        ..RouterConfig::default()
    };
    let expanded = Router::expand_pool(&config);
    assert_eq!(expanded.len(), 1);
    assert_eq!(expanded[0].0, "claude");
    assert_eq!(expanded[0].1, "haiku");
}

#[serial(cooldown_state)]
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

#[serial(cooldown_state)]
#[test]
fn pool_index_initializes_to_zero() {
    let config = RouterConfig::default();
    let router = Router::new(config);
    assert_eq!(router.pool_index, 0);
}

#[serial(cooldown_state)]
#[test]
fn router_pool_non_empty_after_construction() {
    let config = RouterConfig::default();
    let router = Router::new(config);
    assert_ne!(router.router_pool, [] as [(String, String); 0]);
}

/**
 * Router construction used to run `opencode models`, and opencode 2.2.1 leaves an
 * `opencode serve --service` child behind, so a suite run leaked one server per test
 * process. The stub on PATH records any exec, so this fails if discovery comes back
 **/
#[cfg(unix)]
#[serial(cooldown_state)]
#[test]
fn building_a_router_never_runs_opencode() {
    struct PathRestore(Option<OsString>);
    impl Drop for PathRestore {
        fn drop(&mut self) {
            match &self.0 {
                // SAFETY: nextest runs each test in its own process, so no other test touches the environment
                Some(p) => unsafe { env::set_var("PATH", p) },
                // SAFETY: nextest runs each test in its own process, so no other test touches the environment
                None => unsafe { env::remove_var("PATH") },
            }
            cmd_cache::reset_for_test();
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("spawned");
    let bin_dir = dir.path().join("bin");
    fs::create_dir(&bin_dir).unwrap();
    let stub = bin_dir.join("opencode");
    fs::write(
        &stub,
        format!(
            "#!/bin/sh\necho \"$@\" > '{}'\necho canary-model-free\n",
            marker.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&stub, Permissions::from_mode(0o755)).unwrap();

    let prev_path = env::var_os("PATH");
    let mut new_path = bin_dir.as_os_str().to_os_string();
    new_path.push(":");
    if let Some(ref p) = prev_path {
        new_path.push(p);
    }
    // SAFETY: nextest runs each test in its own process, so no other test touches the environment
    unsafe { env::set_var("PATH", &new_path) };
    cmd_cache::reset_for_test();
    let _restore = PathRestore(prev_path);

    // A config that names opencode everywhere discovery reads from
    let mut config = RouterConfig {
        pool: vec!["opencode:free".to_string()],
        fallback: "opencode:free".to_string(),
        ..RouterConfig::default()
    };
    config
        .model_map
        .entry("simple".to_string())
        .or_default()
        .insert(
            "opencode".to_string(),
            vec!["canary-model-free".to_string()],
        );

    let _router = Router::new(config.clone());
    let _test_router = Router::new_for_test(config.clone(), vec!["opencode".to_string()]);
    let _pool = Router::expand_pool(&config);
    // Discovery refreshes on a background thread, give it room to exec
    thread::sleep(Duration::from_millis(300));

    assert!(
        !marker.exists(),
        "building a router ran opencode: {}",
        fs::read_to_string(&marker).unwrap_or_default().trim()
    );
    assert!(
        opencode_children().is_empty(),
        "building a router left opencode processes: {:?}",
        opencode_children()
    );
}

/// Commands of this process's direct children that mention opencode
#[cfg(unix)]
fn opencode_children() -> Vec<String> {
    let me = process::id().to_string();
    let output = Command::new("ps")
        .args(["-axo", "ppid=,command="])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let (ppid, cmd) = line.trim().split_once(char::is_whitespace)?;
            (ppid == me && cmd.contains("opencode")).then(|| cmd.to_string())
        })
        .collect()
}

#[serial(cooldown_state)]
#[tokio::test]
async fn reload_resets_pool_index() {
    cooldown::reset_global_state().await;
    let config = RouterConfig::default();
    let mut router = Router::new(config);
    router.pool_index = 5;
    router.reload_async().await;
    assert_eq!(router.pool_index, 0);
}

#[serial(cooldown_state)]
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

#[serial(cooldown_state)]
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

#[serial(cooldown_state)]
#[test]
fn earliest_pool_cooldown_returns_none_when_no_cooldowns() {
    let router = Router::new(RouterConfig::default());
    assert!(router.earliest_pool_cooldown().is_none());
}

#[serial(cooldown_state)]
#[tokio::test]
async fn earliest_pool_cooldown_returns_earliest_when_set() {
    cooldown::reset_global_state().await;
    // Use explicit pool with predictable model names to avoid opencode discovery.
    let config = RouterConfig {
        pool: vec![
            "claude:haiku-4-5-20251001".to_string(),
            "opencode:haiku-4-5-20251001".to_string(),
        ],
        ..Default::default()
    };
    let router = Router::new(config);
    set_model_cooldown("claude", "haiku-4-5-20251001", 300, "silence_detected").await; // 5 min
    set_model_cooldown("opencode", "haiku-4-5-20251001", 600, "silence_detected").await; // 10 min

    let earliest = router.earliest_pool_cooldown();
    assert!(earliest.is_some());
    let now = chrono::Utc::now().timestamp();
    let remaining = earliest.unwrap() - now;
    // Should be close to 300 (5 min, the minimum) — allow 5s tolerance for test runtime
    assert!((295..=300).contains(&remaining));
}

#[serial(cooldown_state)]
#[tokio::test]
async fn earliest_pool_cooldown_skips_non_pool_entries() {
    cooldown::reset_global_state().await;
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
    set_model_cooldown("claude", "opus", 999, "silence_detected").await;
    // Set cooldown on a pool entry (opencode:pool-model-b)
    set_model_cooldown("opencode", "pool-model-b", 300, "silence_detected").await;

    let earliest = router.earliest_pool_cooldown();
    assert!(earliest.is_some());
    let now = chrono::Utc::now().timestamp();
    let remaining = earliest.unwrap() - now;
    // Should be ~300 (the pool-model-b cooldown), NOT 999 (claude:opus is not in pool)
    assert!((295..=300).contains(&remaining));
}

// ---- Pre-emptive health check: degraded agent exclusion ----

#[serial(cooldown_state)]
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

#[serial(cooldown_state)]
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

#[serial(cooldown_state)]
#[tokio::test]
async fn refresh_health_marks_degraded_from_rate_limits() {
    cooldown::reset_global_state().await;
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
        store.record_rate_limit(agent, "rate", None).await.unwrap();
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

// ---- route_with_llm candidate filter: agents with all models cooled (#3509) ----

#[serial(cooldown_state)]
#[tokio::test]
async fn agent_has_any_available_model_false_when_only_model_cooled() {
    cooldown::reset_global_state().await;
    let mut config = RouterConfig::default();
    // Agent's only model, shared across every complexity tier it resolves to.
    for tier in ["simple", "medium", "complex", "review"] {
        config
            .model_map
            .entry(tier.to_string())
            .or_default()
            .insert(
                "test_all_cooled".to_string(),
                vec!["only-model".to_string()],
            );
    }

    let mut router = Router::new(config);
    router.available_agents = vec!["test_all_cooled".to_string()];

    assert!(router.agent_has_any_available_model("test_all_cooled"));

    record_model_failure("test_all_cooled", "only-model").await;
    assert!(is_model_in_cooldown("test_all_cooled", "only-model"));

    assert!(
        !router.agent_has_any_available_model("test_all_cooled"),
        "agent with its only model cooled across every tier should have no available model"
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn llm_candidate_excludes_agent_with_all_models_cooled() {
    // Regression test for #3509: route_with_llm's candidate filter must exclude an
    // agent that has no agent-level cooldown but whose only resolvable model (across
    // every complexity tier) is cooled, so it is never handed to the LLM classifier.
    cooldown::reset_global_state().await;
    let mut config = RouterConfig::default();
    for tier in ["simple", "medium", "complex", "review"] {
        config
            .model_map
            .entry(tier.to_string())
            .or_default()
            .insert("agent_cooled_model".to_string(), vec!["opus".to_string()]);
        config
            .model_map
            .get_mut(tier)
            .unwrap()
            .insert("agent_healthy".to_string(), vec!["model-x".to_string()]);
    }

    let mut router = Router::new(config);
    router.available_agents = vec![
        "agent_cooled_model".to_string(),
        "agent_healthy".to_string(),
    ];

    record_model_failure("agent_cooled_model", "opus").await;
    assert!(is_model_in_cooldown("agent_cooled_model", "opus"));
    // No agent-level cooldown — the old agent-only filter would still offer it.
    assert!(!is_agent_in_cooldown("agent_cooled_model"));

    let uncooled_agents: Vec<String> = router
        .available_agents
        .iter()
        .filter(|a| {
            !is_agent_in_cooldown(a)
                && !is_agent_degraded(a)
                && router.agent_has_available_model_for_coding_tiers(a)
        })
        .cloned()
        .collect();

    assert_eq!(
        uncooled_agents,
        vec!["agent_healthy".to_string()],
        "agent whose only model is cooled must be excluded from the LLM candidate list"
    );
}

#[serial(cooldown_state)]
#[tokio::test]
async fn llm_candidate_excludes_agent_with_only_simple_tier_uncooled() {
    // Regression test for #3521: an agent whose only healthy tier is `simple`
    // (its `medium`/`complex` tiers resolve to the same cooled model) must be
    // excluded from the LLM classification candidate list, otherwise the
    // classifier can pick it for a `medium`/`complex` task it can't serve,
    // wasting the classification call. Mirrors the real-world shape where
    // `simple` → sonnet (uncooled) and `medium`/`complex` → opus (cooled).
    cooldown::reset_global_state().await;
    let mut config = RouterConfig::default();
    config
        .model_map
        .entry("simple".to_string())
        .or_default()
        .insert("minimax_like".to_string(), vec!["sonnet".to_string()]);
    config
        .model_map
        .entry("medium".to_string())
        .or_default()
        .insert("minimax_like".to_string(), vec!["opus".to_string()]);
    config
        .model_map
        .entry("complex".to_string())
        .or_default()
        .insert("minimax_like".to_string(), vec!["opus".to_string()]);
    // review must be irrelevant to the coding-tier filter
    config
        .model_map
        .entry("review".to_string())
        .or_default()
        .insert("minimax_like".to_string(), vec!["opus".to_string()]);
    for tier in ["simple", "medium", "complex", "review"] {
        config
            .model_map
            .get_mut(tier)
            .unwrap()
            .insert("agent_healthy".to_string(), vec!["model-x".to_string()]);
    }

    let mut router = Router::new(config);
    router.available_agents = vec!["minimax_like".to_string(), "agent_healthy".to_string()];

    record_model_failure("minimax_like", "opus").await;
    assert!(is_model_in_cooldown("minimax_like", "opus"));
    // No agent-level cooldown, and the simple tier is healthy, so the loose
    // "any tier" check still passes — this is exactly the leak #3511 missed.
    assert!(!is_agent_in_cooldown("minimax_like"));
    assert!(router.agent_has_any_available_model("minimax_like"));
    assert!(!router.agent_has_available_model_for_coding_tiers("minimax_like"));

    let uncooled_agents: Vec<String> = router
        .available_agents
        .iter()
        .filter(|a| {
            !is_agent_in_cooldown(a)
                && !is_agent_degraded(a)
                && router.agent_has_available_model_for_coding_tiers(a)
        })
        .cloned()
        .collect();

    assert_eq!(
        uncooled_agents,
        vec!["agent_healthy".to_string()],
        "agent with only its simple tier uncooled must be excluded from the LLM candidate list"
    );
}
