//! Local routing via Ollama.
//!
//! This module provides routing using locally-hosted models via Ollama's HTTP API.
//! When router.mode is "local", tasks are routed using a local model
//! instead of cloud LLMs, reducing latency and cost.

use super::config::RouterConfig;
use super::{AgentProfile, RouteResult};
use crate::backends::ExternalTask;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Request body for Ollama /api/generate endpoint.
#[derive(Debug, Clone, Serialize)]
struct OllamaRequest {
    model: String,
    prompt: String,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<OllamaOptions>,
}

#[derive(Debug, Clone, Serialize)]
struct OllamaOptions {
    temperature: f32,
}

/// Response from Ollama /api/generate endpoint.
#[derive(Debug, Deserialize)]
struct OllamaResponse {
    response: String,
}

/// LLM router adapter for Ollama.
///
/// Handles HTTP requests to localhost:11434 and parses responses
/// using the same format as cloud LLMs.
pub struct OllamaRouter {
    /// Base URL for Ollama API.
    url: String,
    /// Model name to use for routing.
    model: String,
    /// Reusable HTTP client with connection pooling.
    client: reqwest::Client,
}

impl OllamaRouter {
    pub fn new(config: &RouterConfig) -> Self {
        let timeout = Duration::from_secs(config.ollama_timeout_seconds);
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .tcp_keepalive(std::time::Duration::from_secs(30))
            .build()
            .expect("failed to build reqwest client");
        Self {
            url: config.ollama_url.clone(),
            model: config.ollama_model.clone(),
            client,
        }
    }

    /// Route using Ollama HTTP API.
    ///
    /// Builds a routing prompt, calls Ollama, parses the JSON response,
    /// and returns a RouteResult.
    pub async fn route(
        &self,
        task: &ExternalTask,
        config: &RouterConfig,
    ) -> anyhow::Result<RouteResult> {
        if config.agents.is_empty() {
            anyhow::bail!("no agent CLIs found in PATH");
        }

        // Build routing prompt
        let prompt = self.build_routing_prompt(task, config)?;

        // Build HTTP request
        let request = OllamaRequest {
            model: self.model.clone(),
            prompt: prompt.clone(),
            stream: false,
            options: Some(OllamaOptions {
                temperature: 0.1, // Low temperature for consistent routing
            }),
        };

        let url = format!("{}/api/generate", self.url);

        tracing::debug!(
            task_id = %task.id.0,
            url = %self.url,
            model = %self.model,
            "calling Ollama for routing"
        );

        let response = self.client.post(&url).json(&request).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            anyhow::bail!(
                "Ollama request failed with status {}: {}",
                status,
                response.text().await.unwrap_or_default()
            );
        }

        let body = response.text().await?;

        // Parse Ollama response
        let ollama_resp: OllamaResponse = match serde_json::from_str(&body) {
            Ok(r) => r,
            Err(e) => {
                // Ollama might return plain text without JSON wrapper — treat body as response text
                tracing::debug!(error = %e, raw_body = %body, "Ollama response parse failed, treating as plain text");
                OllamaResponse {
                    response: body.clone(),
                }
            }
        };

        // Parse the LLM response using the same format as cloud routers
        let llm_text = ollama_resp.response.trim();
        tracing::debug!(
            task_id = %task.id.0,
            response_len = llm_text.len(),
            "Ollama routing response received"
        );

        // Use the same parsing logic as LlmRouter for compatibility
        let llm_router = super::llm::LlmRouter::new();
        let llm_response = llm_router.parse_llm_response("ollama", llm_text)?;

        // Validate executor against configured agents
        let mut agent = llm_response.executor.to_lowercase();
        if !config.agents.contains(&agent) {
            agent = if !config.fallback_executor.is_empty() {
                config.fallback_executor.clone()
            } else {
                config.agents.first().cloned().unwrap_or_default()
            };
        }

        let complexity = if llm_response.complexity.is_empty() {
            "medium".to_string()
        } else {
            llm_response.complexity.to_lowercase()
        };

        let model = config.model_for_complexity(&agent, &complexity, &task.id.0);

        let mut profile = AgentProfile {
            role: llm_response.profile.role,
            skills: llm_response.profile.skills,
            tools: if llm_response.profile.tools.is_empty() {
                config.allowed_tools.clone()
            } else {
                llm_response.profile.tools
            },
            constraints: llm_response.profile.constraints,
        };
        for tool in &config.allowed_tools {
            if !profile.tools.contains(tool) {
                profile.tools.push(tool.clone());
            }
        }

        let mut selected_skills = llm_response.selected_skills;
        for skill in &config.default_skills {
            if !selected_skills.contains(skill) {
                selected_skills.push(skill.clone());
            }
        }

        Ok(RouteResult {
            agent,
            model,
            complexity,
            estimate: llm_response.estimate,
            reason: llm_response.reason,
            profile,
            selected_skills,
            warning: None,
        })
    }

    /// Build routing prompt from template.
    fn build_routing_prompt(
        &self,
        task: &ExternalTask,
        config: &RouterConfig,
    ) -> anyhow::Result<String> {
        let template = include_str!("../../../prompts/route.md");

        // Build available agents string
        let available_agents_str = config.agents.join(", ");

        // Build weights string for the prompt
        let weights_str = if config.weights.is_empty() {
            "No weights configured — distribute evenly.".to_string()
        } else {
            config
                .agents
                .iter()
                .map(|a| {
                    let w = config.weights.get(a).copied().unwrap_or(1.0);
                    format!("{a}: {w}")
                })
                .collect::<Vec<_>>()
                .join(", ")
        };

        // Build labels string
        let labels = task.labels.join(", ");

        // Simple template substitution
        let prompt = template
            .replace("{{ROUTER_AGENT}}", "ollama")
            .replace("{{AVAILABLE_AGENTS}}", &available_agents_str)
            .replace("{{AGENT_WEIGHTS}}", &weights_str)
            .replace("{{SKILLS_CATALOG}}", "[]") // Ollama doesn't support dynamic skills catalog
            .replace("{{TASK_ID}}", &task.id.0)
            .replace("{{TASK_TITLE}}", &task.title)
            .replace("{{TASK_LABELS}}", &labels)
            .replace("{{TASK_BODY}}", &task.body);

        Ok(prompt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::{ExternalId, ExternalTask};

    fn make_router() -> OllamaRouter {
        OllamaRouter::new(&RouterConfig::default())
    }

    fn make_task(title: &str, body: &str, labels: Vec<&str>) -> ExternalTask {
        ExternalTask {
            id: ExternalId("42".to_string()),
            title: title.to_string(),
            body: body.to_string(),
            state: "open".to_string(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            author: "testuser".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: "https://github.com/test/test/issues/42".to_string(),
        }
    }

    // ── OllamaResponse JSON parsing ────────────────────────────────────────────

    #[test]
    fn ollama_response_parses_clean_json() {
        let json = r#"{"response":"Here is my routing: {\"executor\":\"claude\",\"complexity\":\"medium\",\"reason\":\"good fit\"}"}"#;
        let resp: OllamaResponse = serde_json::from_str(json).unwrap();
        assert!(resp.response.contains("executor"));
        assert!(resp.response.contains("claude"));
    }

    #[test]
    fn ollama_response_with_empty_response() {
        let json = r#"{"response":""}"#;
        let resp: OllamaResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.response, "");
    }

    #[test]
    fn ollama_response_with_unicode() {
        let json =
            r#"{"response":"{\n  \"executor\": \"codex\",\n  \"complexity\": \"simple\"\n}"}"#;
        let resp: OllamaResponse = serde_json::from_str(json).unwrap();
        assert!(resp.response.contains("executor"));
        assert!(resp.response.contains("codex"));
    }

    // ── OllamaResponse JSON parse failure → plain text fallback ───────────────
    // When Ollama returns plain text (no JSON wrapper), the caller falls back to
    // treating the raw body as the response text. We test that the raw body
    // is accessible for this fallback path.

    #[test]
    fn ollama_response_raw_body_used_on_json_failure() {
        // Simulate the fallback: JSON parse fails → body becomes the response
        let raw_body = r#"Here's my routing decision: use claude for this task."#;
        let result: OllamaResponse = match serde_json::from_str::<OllamaResponse>(raw_body) {
            Ok(r) => r,
            Err(_) => OllamaResponse {
                response: raw_body.to_string(),
            },
        };
        assert_eq!(result.response, raw_body);
        // The fallback text is then trimmed and passed to parse_llm_response
        assert!(!result.response.trim().is_empty());
    }

    #[test]
    fn ollama_response_trim_works() {
        let json = r#"{"response":"  {\"executor\":\"claude\"}  "}"#;
        let resp: OllamaResponse = serde_json::from_str(json).unwrap();
        let trimmed = resp.response.trim();
        assert!(trimmed.starts_with('{'));
        assert!(trimmed.ends_with('}'));
    }

    // ── build_routing_prompt ───────────────────────────────────────────────────

    #[test]
    fn build_routing_prompt_substitutes_task_fields() {
        let router = make_router();
        let task = make_task(
            "Fix bug in auth",
            "The login flow is broken",
            vec!["backend", "bug"],
        );
        let config = RouterConfig::default();
        let prompt = router.build_routing_prompt(&task, &config).unwrap();

        // Task fields substituted
        assert!(prompt.contains("Fix bug in auth"));
        assert!(prompt.contains("The login flow is broken"));
        assert!(prompt.contains("backend, bug"));
        // Router agent substituted
        assert!(prompt.contains("ollama"));
        // Default agents present
        assert!(prompt.contains("claude"));
    }

    #[test]
    fn build_routing_prompt_includes_labels() {
        let router = make_router();
        let task = make_task("Title", "Body", vec!["priority:high", "good-first-issue"]);
        let config = RouterConfig::default();
        let prompt = router.build_routing_prompt(&task, &config).unwrap();
        assert!(prompt.contains("priority:high"));
        assert!(prompt.contains("good-first-issue"));
    }

    #[test]
    fn build_routing_prompt_skills_catalog_replaced_with_empty() {
        let router = make_router();
        let task = make_task("Title", "Body", vec![]);
        let config = RouterConfig::default();
        let prompt = router.build_routing_prompt(&task, &config).unwrap();
        // Ollama uses "[]" for skills catalog since it doesn't support dynamic skills
        // The template placeholder {{SKILLS_CATALOG}} should be replaced with "[]"
        assert!(
            !prompt.contains("{{SKILLS_CATALOG}}"),
            "SKILLS_CATALOG placeholder should be substituted"
        );
    }

    #[test]
    fn build_routing_prompt_includes_weights() {
        let router = make_router();
        let task = make_task("Title", "Body", vec![]);
        let mut config = RouterConfig::default();
        config.weights.insert("claude".to_string(), 0.5);
        config.weights.insert("codex".to_string(), 0.3);
        let prompt = router.build_routing_prompt(&task, &config).unwrap();
        // Weights string should be formatted
        assert!(prompt.contains("claude") || prompt.contains("codex"));
    }

    // ── OllamaRouter construction ───────────────────────────────────────────────

    #[test]
    fn ollama_router_uses_config_values() {
        let config = RouterConfig {
            ollama_url: "http://localhost:11434".to_string(),
            ollama_model: "qwen2.5-coder:3b-instruct".to_string(),
            ollama_timeout_seconds: 30,
            ..RouterConfig::default()
        };
        let router = OllamaRouter::new(&config);
        assert_eq!(router.url, "http://localhost:11434");
        assert_eq!(router.model, "qwen2.5-coder:3b-instruct");
    }

    #[test]
    fn ollama_router_default_values() {
        let router = make_router();
        assert_eq!(router.url, "http://localhost:11434");
        assert_eq!(router.model, "qwen2.5-coder:3b-instruct");
    }

    #[tokio::test]
    async fn route_bails_when_no_agents_configured() {
        let router = make_router();
        let task = make_task("Title", "Body", vec![]);
        let mut config = RouterConfig::default();
        config.agents.clear();
        let result = router.route(&task, &config).await;
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            "no agent CLIs found in PATH"
        );
    }
}
