//! LLM-based task routing.
//!
//! This module contains all logic for calling an LLM to classify tasks and
//! select the best agent. It is intentionally separate from the agent selection
//! strategies (round-robin, weighted, label-based) in `mod.rs`.
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

use super::{AgentProfile, RouteResult, RouterConfig};
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
pub(super) struct LlmRouter {
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

        // Prune old debug files — keep only the 50 most recent of each type
        if let Ok(state_dir) = crate::home::state_dir() {
            prune_route_debug_files(&state_dir, "route-prompt-", 50).await;
            prune_route_debug_files(&state_dir, "route-response-", 50).await;
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
        let template = include_str!("../../../prompts/route.md");

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

        let model = if config.router_model.is_empty() {
            None
        } else {
            Some(config.router_model.as_str())
        };
        let mut cmd = crate::engine::runner::agents::get_runner(&config.router_agent)
            .router_command(prompt, model)?;
        let output = tokio::time::timeout(timeout_duration, cmd.output_with_context()).await;

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

/// Prune route debug files matching `{state_dir}/{prefix}*.txt`, keeping only the `keep` most
/// recent (by mtime). Files that cannot be stat'd are treated as oldest and removed first.
async fn prune_route_debug_files(state_dir: &std::path::Path, prefix: &str, keep: usize) {
    let suffix = ".txt";
    let mut entries: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();

    let mut read_dir = match tokio::fs::read_dir(state_dir).await {
        Ok(rd) => rd,
        Err(_) => return,
    };

    while let Ok(Some(entry)) = read_dir.next_entry().await {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with(prefix) && name_str.ends_with(suffix) {
            let mtime = entry
                .metadata()
                .await
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            entries.push((mtime, entry.path()));
        }
    }

    if entries.len() <= keep {
        return;
    }

    // Sort newest-first; truncate to keep, then delete the rest
    entries.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, path) in entries.into_iter().skip(keep) {
        let _ = tokio::fs::remove_file(&path).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::{ExternalId, ExternalTask};
    use crate::engine::router::AgentProfile;

    fn make_router() -> LlmRouter {
        LlmRouter::new()
    }

    fn make_task(labels: Vec<&str>) -> ExternalTask {
        ExternalTask {
            id: ExternalId("42".to_string()),
            title: "Test task".to_string(),
            body: "Test body".to_string(),
            state: "open".to_string(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            author: "testuser".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: "https://github.com/test/test/issues/42".to_string(),
        }
    }

    fn make_profile(skills: Vec<&str>) -> AgentProfile {
        AgentProfile {
            role: "developer".to_string(),
            skills: skills.iter().map(|s| s.to_string()).collect(),
            tools: vec!["git".to_string()],
            constraints: vec![],
        }
    }

    // ── parse_llm_response ────────────────────────────────────────────────────

    #[test]
    fn parse_direct_json_executor_field() {
        let router = make_router();
        let json = r#"{"executor":"claude","complexity":"medium","reason":"good fit"}"#;
        let resp = router.parse_llm_response(json).unwrap();
        assert_eq!(resp.executor, "claude");
        assert_eq!(resp.complexity, "medium");
    }

    #[test]
    fn parse_direct_json_agent_alias() {
        // LLM sometimes returns "agent" instead of "executor"
        let router = make_router();
        let json = r#"{"agent":"codex","complexity":"simple","reason":"straightforward"}"#;
        let resp = router.parse_llm_response(json).unwrap();
        assert_eq!(resp.executor, "codex");
    }

    #[test]
    fn parse_full_json_with_profile() {
        let router = make_router();
        let json = r#"{
            "executor": "opencode",
            "complexity": "complex",
            "reason": "needs refactoring",
            "profile": {
                "role": "architect",
                "skills": ["rust", "design"],
                "tools": ["git", "rg"],
                "constraints": ["no unsafe"]
            },
            "selected_skills": ["gh"]
        }"#;
        let resp = router.parse_llm_response(json).unwrap();
        assert_eq!(resp.executor, "opencode");
        assert_eq!(resp.complexity, "complex");
        assert_eq!(resp.profile.role, "architect");
        assert_eq!(resp.profile.skills, vec!["rust", "design"]);
        assert_eq!(resp.selected_skills, vec!["gh"]);
    }

    #[test]
    fn parse_claude_envelope_string_result() {
        // Claude --output-format json wraps the result in a {"type":"result","result":"..."} envelope
        let router = make_router();
        let inner = r#"{"executor":"claude","complexity":"medium","reason":"test"}"#;
        let envelope = format!(
            r#"{{"type":"result","subtype":"text","is_error":false,"result":"{}","usage":{{"input":10,"output":5}}}}"#,
            inner.replace('"', "\\\"")
        );
        let resp = router.parse_llm_response(&envelope).unwrap();
        assert_eq!(resp.executor, "claude");
    }

    #[test]
    fn parse_claude_envelope_object_result() {
        // When result is already a JSON object (not a string)
        let router = make_router();
        let envelope = r#"{"type":"result","is_error":false,"result":{"executor":"kimi","complexity":"simple","reason":"fast"}}"#;
        let resp = router.parse_llm_response(envelope).unwrap();
        assert_eq!(resp.executor, "kimi");
    }

    #[test]
    fn parse_claude_error_envelope() {
        let router = make_router();
        let envelope = r#"{"type":"result","is_error":true,"result":"auth error: invalid key"}"#;
        let err = router.parse_llm_response(envelope).unwrap_err();
        assert!(
            err.to_string().contains("error"),
            "should surface the error"
        );
    }

    #[test]
    fn parse_markdown_json_fenced_block() {
        let router = make_router();
        let md = "Here is my routing decision:\n\n```json\n{\"executor\":\"codex\",\"complexity\":\"simple\",\"reason\":\"easy\"}\n```\n\nDone.";
        let resp = router.parse_llm_response(md).unwrap();
        assert_eq!(resp.executor, "codex");
    }

    #[test]
    fn parse_markdown_plain_fenced_block() {
        let router = make_router();
        let md = "```\n{\"executor\":\"minimax\",\"complexity\":\"medium\",\"reason\":\"ok\"}\n```";
        let resp = router.parse_llm_response(md).unwrap();
        assert_eq!(resp.executor, "minimax");
    }

    #[test]
    fn parse_embedded_json_in_prose() {
        let router = make_router();
        let text = r#"I analyzed the task. My decision is {"executor":"claude","complexity":"complex","reason":"hard task"}. Please proceed."#;
        let resp = router.parse_llm_response(text).unwrap();
        assert_eq!(resp.executor, "claude");
        assert_eq!(resp.complexity, "complex");
    }

    #[test]
    fn parse_empty_response_fails() {
        let router = make_router();
        assert!(router.parse_llm_response("").is_err());
        assert!(router.parse_llm_response("   ").is_err());
    }

    #[test]
    fn parse_invalid_response_fails() {
        let router = make_router();
        assert!(router.parse_llm_response("not json at all").is_err());
        assert!(router.parse_llm_response("{ invalid json }").is_err());
    }

    #[test]
    fn parse_defaults_apply_for_missing_fields() {
        // Only "executor" is required — other fields should default
        let router = make_router();
        let json = r#"{"executor":"claude"}"#;
        let resp = router.parse_llm_response(json).unwrap();
        assert_eq!(resp.executor, "claude");
        assert_eq!(resp.complexity, "");
        assert_eq!(resp.reason, "");
        assert!(resp.selected_skills.is_empty());
    }

    #[test]
    fn parse_fixture_route_response_string() {
        let router = make_router();
        let response = include_str!("../../../tests/fixtures/route-response-string.json");
        let resp = router.parse_llm_response(response).unwrap();
        // Fixture should parse without error and produce a valid agent name
        assert!(!resp.executor.is_empty(), "executor must not be empty");
    }

    #[test]
    fn parse_fixture_route_response_object() {
        let router = make_router();
        let response = include_str!("../../../tests/fixtures/route-response-object.json");
        let resp = router.parse_llm_response(response).unwrap();
        assert!(!resp.executor.is_empty(), "executor must not be empty");
    }

    #[test]
    fn parse_fixture_route_response_markdown() {
        let router = make_router();
        let response = include_str!("../../../tests/fixtures/route-response-markdown.json");
        let resp = router.parse_llm_response(response).unwrap();
        assert!(!resp.executor.is_empty(), "executor must not be empty");
    }

    // ── check_routing_sanity ─────────────────────────────────────────────────

    #[test]
    fn sanity_warns_backend_task_routed_to_claude() {
        let router = make_router();
        let task = make_task(vec!["backend", "priority:high"]);
        let profile = make_profile(vec!["rust"]);
        let warning = router.check_routing_sanity(&task, "claude", &profile);
        assert!(
            warning.is_some(),
            "should warn when backend task goes to claude"
        );
        assert!(warning.unwrap().contains("backend"));
    }

    #[test]
    fn sanity_warns_api_label_routed_to_claude() {
        let router = make_router();
        let task = make_task(vec!["api", "feature"]);
        let profile = make_profile(vec!["rest"]);
        let warning = router.check_routing_sanity(&task, "claude", &profile);
        assert!(warning.is_some());
    }

    #[test]
    fn sanity_warns_docs_task_routed_to_codex() {
        let router = make_router();
        let task = make_task(vec!["documentation"]);
        let profile = make_profile(vec!["writing"]);
        let warning = router.check_routing_sanity(&task, "codex", &profile);
        assert!(
            warning.is_some(),
            "should warn when docs task goes to codex"
        );
        assert!(warning.unwrap().contains("docs"));
    }

    #[test]
    fn sanity_warns_writing_label_routed_to_codex() {
        let router = make_router();
        let task = make_task(vec!["writing"]);
        let profile = make_profile(vec!["markdown"]);
        let warning = router.check_routing_sanity(&task, "codex", &profile);
        assert!(warning.is_some());
    }

    #[test]
    fn sanity_warns_empty_skills_in_profile() {
        let router = make_router();
        let task = make_task(vec!["feature"]);
        let profile = make_profile(vec![]); // no skills
        let warning = router.check_routing_sanity(&task, "opencode", &profile);
        assert!(warning.is_some(), "should warn when profile has no skills");
        assert!(warning.unwrap().contains("skills"));
    }

    #[test]
    fn sanity_no_warning_for_clean_routing() {
        let router = make_router();
        let task = make_task(vec!["feature", "rust"]);
        let profile = make_profile(vec!["rust", "async"]);
        let warning = router.check_routing_sanity(&task, "claude", &profile);
        assert!(warning.is_none(), "no warning expected for clean routing");
    }

    #[test]
    fn sanity_no_warning_backend_to_codex() {
        // Backend label to codex is fine — only backend→claude warns
        let router = make_router();
        let task = make_task(vec!["backend"]);
        let profile = make_profile(vec!["node"]);
        let warning = router.check_routing_sanity(&task, "codex", &profile);
        assert!(warning.is_none());
    }

    #[test]
    fn sanity_case_insensitive_label_matching() {
        let router = make_router();
        let task = make_task(vec!["Backend", "API"]); // mixed case
        let profile = make_profile(vec!["rust"]);
        let warning = router.check_routing_sanity(&task, "claude", &profile);
        assert!(
            warning.is_some(),
            "label matching should be case-insensitive"
        );
    }
}
