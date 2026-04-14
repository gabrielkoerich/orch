//! Local routing via Ollama.
//!
//! This module provides routing using locally-hosted models via Ollama's HTTP API.
//! When router.mode is "local", tasks are routed using a local model
//! instead of cloud LLMs, reducing latency and cost.

use super::RouteResult;
use crate::backends::ExternalTask;
use super::config::RouterConfig;
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
    model: String,
    response: String,
    #[serde(default)]
    done: bool,
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
    /// Timeout for HTTP requests.
    timeout: Duration,
}

impl OllamaRouter {
    pub fn new(config: &RouterConfig) -> Self {
        Self {
            url: config.ollama_url.clone(),
            model: config.ollama_model.clone(),
            timeout: Duration::from_secs(config.ollama_timeout_seconds),
        }
    }

    /// Route using Ollama HTTP API.
    ///
    /// Builds a routing prompt, calls Ollama, parses the JSON response,
    /// and returns a RouteResult.
    pub async fn route(&self, task: &ExternalTask, config: &RouterConfig) -> anyhow::Result<RouteResult> {
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

        let client = reqwest::Client::builder()
            .timeout(self.timeout)
            .build()?;

        let url = format!("{}/api/generate", self.url);

        tracing::debug!(
            task_id = %task.id.0,
            url = %self.url,
            model = %self.model,
            "calling Ollama for routing"
        );

        let response = client
            .post(&url)
            .json(&request)
            .send()
            .await?;

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
                // Ollama might return plain text without JSON structure
                tracing::debug!(error = %e, raw_body = %body, "Ollama response parse failed, treating as plain text");

                // Try to extract JSON from plain text response
                if let Some(json_start) = body.find('{') {
                    if let Some(json_end) = body.rfind('}') {
                        let json_str = &body[json_start..=json_end + 1];
                        match serde_json::from_str::<OllamaResponse>(json_str) {
                            Ok(r) => r,
                            Err(_) => {
                                // Last resort: treat plain text as response
                                OllamaResponse {
                                    model: self.model.clone(),
                                    response: body.clone(),
                                    done: true,
                                }
                            }
                        }
                    } else {
                        OllamaResponse {
                            model: self.model.clone(),
                            response: body.clone(),
                            done: true,
                        }
                    }
                } else {
                    OllamaResponse {
                        model: self.model.clone(),
                        response: body.clone(),
                        done: true,
                    }
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
        super::llm::LlmRouter::new().parse_ollama_response(
            task,
            llm_text,
            &self.model,
        )
    }

    /// Build routing prompt from template.
    fn build_routing_prompt(&self, task: &ExternalTask, config: &RouterConfig) -> anyhow::Result<String> {
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
