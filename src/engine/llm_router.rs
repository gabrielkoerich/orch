//! LLM-based task routing.
//!
//! This module contains all logic for calling an LLM to classify tasks and
//! select the best agent. It is intentionally separate from the agent selection
//! strategies (round-robin, weighted, label-based) in `router.rs`.
//!
//! # Responsibilities
//! - Building the routing prompt from a task and the skills catalog
//! - Calling the configured LLM agent (`claude`, `codex`, or `opencode`)
//! - Parsing the JSON response with multiple fallback strategies
//! - Sanity-checking the routing decision
//! - Loading and caching the skills catalog from disk

use crate::backends::ExternalTask;
use crate::cmd::CommandErrorContext;
use std::path::PathBuf;
use std::time::Duration;

use super::router::{AgentProfile, RouteResult, RouterConfig};
use serde::Deserialize;

/// Response from the LLM router.
#[derive(Debug, Deserialize)]
pub(crate) struct LlmRouteResponse {
    /// The selected agent. Accepts both "executor" and "agent" from the LLM.
    #[serde(alias = "agent")]
    pub(crate) executor: String,
    #[serde(default)]
    pub(crate) complexity: String,
    #[serde(default)]
    pub(crate) reason: String,
    #[serde(default)]
    pub(crate) profile: LlmAgentProfile,
    #[serde(default)]
    pub(crate) selected_skills: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct LlmAgentProfile {
    #[serde(default)]
    pub(crate) role: String,
    #[serde(default)]
    pub(crate) skills: Vec<String>,
    #[serde(default)]
    pub(crate) tools: Vec<String>,
    #[serde(default)]
    pub(crate) constraints: Vec<String>,
}

/// Handles LLM-based task routing.
///
/// `LlmRouter` is a self-contained unit: it builds a prompt from the task and
/// the skills catalog, calls the configured LLM agent, parses the JSON
/// response, and applies sanity checks. It holds only the skills catalog
/// cache as mutable state.
///
/// `Router` holds a `LlmRouter` and delegates `route_with_llm` to it.
pub struct LlmRouter {
    /// Cached skills catalog loaded once to avoid repeated blocking I/O.
    skills_catalog: std::sync::Mutex<Option<String>>,
}

impl LlmRouter {
    pub fn new() -> Self {
        Self {
            skills_catalog: std::sync::Mutex::new(None),
        }
    }

    /// Route using LLM classification.
    pub async fn route_with_llm(
        &self,
        task: &ExternalTask,
        available_agents: &[String],
        config: &RouterConfig,
    ) -> anyhow::Result<RouteResult> {
        if available_agents.is_empty() {
            anyhow::bail!("no agent CLIs found in PATH");
        }

        // Build the routing prompt
        let prompt = self.build_routing_prompt(task, available_agents)?;

        // Save prompt to file for debugging
        let prompt_path = self.route_prompt_path(&task.id.0);
        if let Some(parent) = prompt_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let _ = tokio::fs::write(&prompt_path, &prompt).await;

        // Call the LLM router
        let response = self.call_router_llm(&prompt, config).await?;

        tracing::info!(
            task_id = task.id.0,
            response_len = response.len(),
            response_preview = %if response.len() > 500 { &response[..500] } else { &response },
            "LLM router raw response"
        );

        // Save raw response for debugging (next to the prompt file)
        let response_path =
            crate::home::state_dir().map(|d| d.join(format!("route-response-{}.txt", task.id.0)));
        if let Ok(path) = response_path {
            let _ = std::fs::write(&path, &response);
        }

        // Parse the response
        let llm_response: LlmRouteResponse = self.parse_llm_response(&response)?;

        // Validate the selected agent
        let mut agent = llm_response.executor.to_lowercase();
        if !available_agents.contains(&agent) {
            let first_available = available_agents.first().cloned().unwrap_or_default();
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
                config.allowed_tools.clone()
            } else {
                llm_response.profile.tools
            },
            constraints: llm_response.profile.constraints,
        };

        // Ensure tools includes allowed_tools
        for tool in &config.allowed_tools {
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
        let model = config.model_for_complexity(&agent, &complexity);

        // Build selected skills list
        let mut selected_skills = llm_response.selected_skills;
        for skill in &config.default_skills {
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
    fn build_routing_prompt(
        &self,
        task: &ExternalTask,
        available_agents: &[String],
    ) -> anyhow::Result<String> {
        let template = include_str!("../../prompts/route.md");

        // Build available agents string
        let available_agents_str = available_agents.join(", ");

        // Build labels string
        let labels = task.labels.join(", ");

        // Load skills catalog if available
        let skills_catalog = self.load_skills_catalog();

        // Simple template substitution
        let prompt = template
            .replace("{{AVAILABLE_AGENTS}}", &available_agents_str)
            .replace("{{SKILLS_CATALOG}}", &skills_catalog)
            .replace("{{TASK_ID}}", &task.id.0)
            .replace("{{TASK_TITLE}}", &task.title)
            .replace("{{TASK_LABELS}}", &labels)
            .replace("{{TASK_BODY}}", &task.body);

        Ok(prompt)
    }

    /// Load skills catalog from skills.yml or skills directory.
    /// Cached after first load to avoid blocking I/O in async context.
    fn load_skills_catalog(&self) -> String {
        // Check cache first
        if let Ok(cache) = self.skills_catalog.lock() {
            if let Some(ref catalog) = *cache {
                return catalog.clone();
            }
        }

        // Load and cache
        let catalog = self.load_skills_catalog_uncached();

        if let Ok(mut cache) = self.skills_catalog.lock() {
            *cache = Some(catalog.clone());
        }

        catalog
    }

    /// Load skills catalog without caching (internal implementation).
    fn load_skills_catalog_uncached(&self) -> String {
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
    async fn call_router_llm(&self, prompt: &str, config: &RouterConfig) -> anyhow::Result<String> {
        let timeout_secs = config.timeout_seconds;
        let timeout_duration = Duration::from_secs(timeout_secs);

        let output = match config.router_agent.as_str() {
            "claude" => {
                let mut cmd = tokio::process::Command::new("claude");
                cmd.env_remove("CLAUDECODE"); // allow nested invocation
                cmd.arg("--output-format").arg("json").arg("--print");

                if !config.router_model.is_empty() {
                    cmd.arg("--model").arg(&config.router_model);
                }

                cmd.arg(prompt);

                tokio::time::timeout(timeout_duration, cmd.output_with_context()).await
            }
            "codex" => {
                let mut cmd = tokio::process::Command::new("codex");
                cmd.arg("exec").arg("--json");

                if !config.router_model.is_empty() {
                    cmd.arg("--model").arg(&config.router_model);
                }

                cmd.arg(prompt);

                tokio::time::timeout(timeout_duration, cmd.output_with_context()).await
            }
            "opencode" => {
                let mut cmd = tokio::process::Command::new("opencode");
                cmd.arg("run").arg("--format").arg("json").arg(prompt);

                tokio::time::timeout(timeout_duration, cmd.output_with_context()).await
            }
            _ => {
                anyhow::bail!("unknown router agent: {}", config.router_agent);
            }
        };

        match output {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                tracing::debug!(
                    exit_code = output.status.code().unwrap_or(-1),
                    stdout_len = stdout.len(),
                    stderr_len = stderr.len(),
                    "router LLM command completed"
                );
                if !output.status.success() {
                    tracing::warn!(
                        stderr = %stderr,
                        stdout = %stdout,
                        "router LLM command failed"
                    );
                    anyhow::bail!("router LLM failed: {stderr}");
                }
                if stdout.is_empty() {
                    tracing::warn!(
                        stderr = %stderr,
                        "router LLM returned empty stdout"
                    );
                    anyhow::bail!("router LLM returned empty response");
                }
                Ok(stdout)
            }
            Ok(Err(e)) => Err(e),
            Err(_) => anyhow::bail!("router LLM timed out after {timeout_secs}s"),
        }
    }

    /// Parse the LLM response into a structured format.
    ///
    /// Handles: direct JSON, Claude `--output-format json` envelopes,
    /// markdown code blocks, and raw text with embedded JSON.
    pub fn parse_llm_response(&self, response: &str) -> anyhow::Result<LlmRouteResponse> {
        let trimmed = response.trim();
        if trimmed.is_empty() {
            anyhow::bail!("empty LLM response");
        }

        // Step 1: Unwrap Claude JSON envelope if present.
        // Claude --output-format json returns {"type":"result","result":"...","usage":{...}}
        // The inner "result" field contains the actual LLM text.
        let inner = if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if val.get("type").and_then(|v| v.as_str()) == Some("result") {
                if let Some(is_error) = val.get("is_error").and_then(|v| v.as_bool()) {
                    if is_error {
                        let msg = val
                            .get("result")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown error");
                        anyhow::bail!("router LLM returned error: {msg}");
                    }
                }
                if let Some(r) = val.get("result").and_then(|v| v.as_str()) {
                    // result is a string — unwrap it
                    tracing::debug!(
                        result_len = r.len(),
                        "unwrapped Claude JSON envelope (string)"
                    );
                    r.to_string()
                } else if let Some(obj) = val.get("result").filter(|v| v.is_object()) {
                    // result is a JSON object — serialize it back for parsing
                    tracing::debug!("unwrapped Claude JSON envelope (object)");
                    obj.to_string()
                } else {
                    trimmed.to_string()
                }
            } else {
                trimmed.to_string()
            }
        } else {
            trimmed.to_string()
        };

        let text = inner.trim();

        // Step 2: Try to parse directly as JSON
        if let Ok(parsed) = serde_json::from_str::<LlmRouteResponse>(text) {
            return Ok(parsed);
        }

        // Step 3: Try to extract JSON from markdown code blocks
        if let Some(json_start) = text.find("```json") {
            let after_start = &text[json_start + 7..];
            if let Some(json_end) = after_start.find("```") {
                let json_str = &after_start[..json_end].trim();
                if let Ok(parsed) = serde_json::from_str::<LlmRouteResponse>(json_str) {
                    return Ok(parsed);
                }
            }
        }

        // Try without json specifier
        if let Some(json_start) = text.find("```") {
            let after_start = &text[json_start + 3..];
            if let Some(json_end) = after_start.find("```") {
                let json_str = &after_start[..json_end].trim();
                if let Ok(parsed) = serde_json::from_str::<LlmRouteResponse>(json_str) {
                    return Ok(parsed);
                }
            }
        }

        // Step 4: Try to find JSON object between curly braces
        if let Some(start) = text.find('{') {
            if let Some(end) = text.rfind('}') {
                let json_str = &text[start..=end];
                if let Ok(parsed) = serde_json::from_str::<LlmRouteResponse>(json_str) {
                    return Ok(parsed);
                }
            }
        }

        anyhow::bail!(
            "could not parse LLM response as JSON: {}",
            &text[..text.len().min(200)]
        )
    }

    /// Run sanity checks on routing decision.
    pub fn check_routing_sanity(
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
}
