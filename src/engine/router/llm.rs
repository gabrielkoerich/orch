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
use crate::engine::runner::agents::{self, claude, opencode, AgentError};
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
    /// Fibonacci effort estimate (1, 2, 3, 5, 8, 13, or 21). 0 means not provided.
    #[serde(default)]
    pub(crate) estimate: u8,
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

/// Detect a known error envelope in a parsed JSON value.
///
/// Returns `Some(summary)` if the value looks like an error response from the LLM
/// provider (type=error, error.message, rate limit signatures, etc.), or `None`
/// if it doesn't match any known pattern.
fn detect_error_envelope(value: &serde_json::Value) -> Option<String> {
    let obj = value.as_object()?;

    // Direct `{"type":"error",...}` envelope — check this BEFORE nested error branch
    // since type=error envelopes also have an `error` key that would match the nested branch.
    if obj.get("type").and_then(|v| v.as_str()) == Some("error") {
        if let Some(msg) = obj
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
        {
            return Some(format!("type=error: {msg}"));
        }
        if let Some(msg) = obj.get("error").and_then(|e| e.as_str()) {
            return Some(format!("type=error: {msg}"));
        }
        // Handle `{"error":{"name":"...","data":{"message":"..."}}}` inside type=error envelope
        if let Some(error_obj) = obj.get("error").and_then(|e| e.as_object()) {
            if let Some(data) = error_obj.get("data").and_then(|d| d.as_object()) {
                if let Some(data_msg) = data.get("message").and_then(|m| m.as_str()) {
                    let name = error_obj
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("UnknownError");
                    return Some(format!("type=error: error.name={name}: {data_msg}"));
                }
            }
            if let Some(msg) = error_obj.get("message").and_then(|m| m.as_str()) {
                let name = error_obj
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("UnknownError");
                return Some(format!("type=error: error.name={name}: {msg}"));
            }
        }
        if let Some(msg) = obj.get("message").and_then(|m| m.as_str()) {
            return Some(format!("type=error: {msg}"));
        }
        if let Some(name) = obj
            .get("error")
            .and_then(|e| e.get("name"))
            .and_then(|n| n.as_str())
        {
            return Some(format!("type=error: error.name={name}"));
        }
        return Some("type=error (no message extracted)".to_string());
    }

    // Nested `{"error":{"name":"...","message":"...",...}}` or `{"error":{"type":"...","message":"...",...}}` envelope
    if let Some(error_obj) = obj.get("error").and_then(|e| e.as_object()) {
        let name = error_obj
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("UnknownError");
        if let Some(msg) = error_obj.get("message").and_then(|m| m.as_str()) {
            return Some(format!("error.name={name}: {msg}"));
        }
        if let Some(data) = error_obj.get("data").and_then(|d| d.as_object()) {
            if let Some(data_msg) = data.get("message").and_then(|m| m.as_str()) {
                // PermissionError with data.message is an OpenCode tool-use rejection,
                // not an API auth/quota error. Skip it to avoid false-positive cooldowns.
                if name == "PermissionError" {
                    return None;
                }
                return Some(format!("error.name={name}: {data_msg}"));
            }
        }
        if let Some(etype) = error_obj.get("type").and_then(|t| t.as_str()) {
            if let Some(msg) = error_obj.get("message").and_then(|m| m.as_str()) {
                return Some(format!("error.type={etype}: {msg}"));
            }
            return Some(format!("error.type={etype}"));
        }
        return Some(format!("error.name={name}"));
    }

    // OpenAI-style `{"error": "message string"}`
    if let Some(err_str) = obj.get("error").and_then(|e| e.as_str()) {
        return Some(format!("error: {err_str}"));
    }

    // Error message substring detection (defense in depth)
    let raw = serde_json::to_string(value).ok()?;
    let raw_lower = raw.to_lowercase();
    let error_indicators = [
        "rate limit",
        "overloaded",
        "quota",
        "permission_error",
        "unauthorized",
        "authentication",
        "429",
        "401",
        "403",
        "503",
        "500",
        "529",
        "context_length",
        "max_tokens",
    ];
    for indicator in &error_indicators {
        if raw_lower.contains(indicator) {
            // Extract a short snippet around the indicator
            if let Some(pos) = raw_lower.find(indicator) {
                let start = pos.saturating_sub(30);
                let end = (pos + indicator.len() + 30).min(raw.len());
                let snippet = &raw[start..end];
                return Some(format!(
                    "error indicator '{indicator}' found near: {snippet}"
                ));
            }
        }
    }

    None
}

/// Classify a router LLM non-zero exit failure by extracting meaningful error information
/// from stdout and stderr.
///
/// This function runs the same tolerant parsing used by `parse_llm_response()` to detect:
/// - Structured error envelopes in stdout (type=error, error.message, etc.)
/// - NDJSON/system envelopes that contain no actual routing output
/// - Raw text errors in stderr
///
/// Returns a descriptive error message suitable for logging and cooldown recording.
fn classify_router_llm_failure(agent: &str, stdout: &str, stderr: &str) -> String {
    let stdout_trimmed = stdout.trim();

    // If stdout has content, try to detect structured errors or startup-only envelopes
    if !stdout_trimmed.is_empty() {
        // Try to parse stdout as JSON to detect error envelopes
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(stdout_trimmed) {
            if let Some(err_msg) = detect_error_envelope(&val) {
                return err_msg;
            }
        }

        // Check for startup/system-only NDJSON by running through extract_agent_text
        // This mirrors the logic in parse_llm_response() to detect when stdout only
        // contains system envelopes (hook_started, init, etc.) with no actual result.
        match extract_agent_text_for_classification(agent, stdout_trimmed) {
            Ok(text) => {
                let trimmed_text = text.trim();

                // Check if all lines are valid JSON system/event envelopes
                let lines: Vec<&str> = trimmed_text
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .collect();
                if !lines.is_empty() {
                    let mut all_system = true;
                    for line in &lines {
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                            let typ = val.get("type").and_then(|v| v.as_str());
                            let subtype = val.get("subtype").and_then(|v| v.as_str());
                            if typ != Some("system")
                                && typ != Some("event")
                                && subtype != Some("init")
                            {
                                all_system = false;
                                break;
                            }
                        } else {
                            all_system = false;
                            break;
                        }
                    }
                    if all_system {
                        return "router LLM produced only system/startup envelope".to_string();
                    }
                }

                // If extracted text is empty or very short, it likely means the agent produced
                // only metadata envelopes without actual routing output
                if trimmed_text.is_empty() || trimmed_text.len() < 10 {
                    return "router LLM produced no text output (NDJSON envelopes only)"
                        .to_string();
                }
            }
            Err(e) => {
                return e.to_string();
            }
        }

        // Try tolerant reparse to find embedded error envelopes
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(stdout_trimmed) {
            if let Some(err_msg) = detect_error_envelope(&val) {
                return err_msg;
            }
        }

        // Last resort: check for common error indicators in stdout
        let stdout_lower = stdout_trimmed.to_lowercase();
        let error_indicators = [
            "rate limit",
            "overloaded",
            "quota",
            "permission",
            "unauthorized",
            "authentication",
            "429",
            "401",
            "403",
            "503",
            "500",
            "529",
            "context_length",
            "max_tokens",
        ];
        for indicator in &error_indicators {
            if stdout_lower.contains(indicator) {
                return format!("error indicator '{indicator}' found in stdout");
            }
        }
    }

    // No useful error from stdout — fall back to stderr
    let stderr_trimmed = stderr.trim();
    if !stderr_trimmed.is_empty() {
        let stderr_preview = stderr_trimmed[..stderr_trimmed.len().min(200)].to_string();
        return stderr_preview;
    }

    // Both stdout and stderr are empty or uninformative
    "router LLM failed with no output (empty stdout and stderr)".to_string()
}

/// Extract agent text for classification purposes.
///
/// This is a simplified version of `extract_agent_text()` that doesn't bail on
/// system-only envelopes — instead it returns the raw text so the caller can
/// classify the failure.
fn extract_agent_text_for_classification(agent: &str, raw: &str) -> anyhow::Result<String> {
    match agent {
        "opencode" => {
            let events = agents::parse_ndjson(raw.trim());
            if events.is_empty() {
                Ok(raw.to_string())
            } else {
                Ok(opencode::extract_ndjson_text(&events).unwrap_or_else(|| raw.to_string()))
            }
        }
        "claude" | "kimi" | "minimax" => match claude::extract_stream_json_result_text(raw) {
            Ok(text) => Ok(text),
            Err(AgentError::AgentFailed { message }) => {
                Err(anyhow::anyhow!("router LLM returned error: {message}"))
            }
            Err(AgentError::InvalidResponse { raw }) => {
                if let Some(text) = opencode::extract_router_text(&raw) {
                    Ok(text)
                } else {
                    Ok(raw)
                }
            }
            Err(err) => Err(anyhow::anyhow!("router LLM returned error: {err}")),
        },
        _ => Ok(raw.to_string()),
    }
}

/// Apply self-routing penalty: if the LLM chose the same agent that's running the router,
/// probabilistically redirect to another agent.
///
/// Returns `Some(agent)` with the redirected agent, or `None` if no redirect is needed.
///
/// - `penalty = 1.0` → never redirect (no penalty, default)
/// - `penalty = 0.0` → always redirect (maximum penalty)
/// - `penalty = 0.5` → redirect ~50% of self-routed tasks
pub(super) fn apply_self_routing_penalty(
    chosen_agent: &str,
    router_agent: &str,
    available_agents: &[String],
    penalty: f64,
    task_id: &str,
) -> Option<String> {
    // No penalty configured, or LLM chose a different agent — nothing to do
    if penalty >= 1.0 || chosen_agent != router_agent {
        return None;
    }

    // Collect alternative agents (everyone except the router)
    let alternatives: Vec<&String> = available_agents
        .iter()
        .filter(|a| a.as_str() != router_agent)
        .collect();

    if alternatives.is_empty() {
        return None;
    }

    // Probabilistically override: keep original if rand < penalty, else redirect.
    // Using simple_hash_fraction_for gives deterministic-ish behavior per task_id
    // so the same task always routes consistently.
    let rand = super::selection::simple_hash_fraction_for(task_id);
    if rand < penalty {
        return None; // Keep the LLM's choice
    }

    // Redirect to an alternative agent chosen by hash
    let idx =
        super::selection::simple_hash_index_for(alternatives.len(), &format!("penalty:{task_id}"));
    Some(alternatives[idx].clone())
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
    skills_catalog: tokio::sync::Mutex<Option<String>>,
}

/// Scan a skills directory for skill subdirectories containing SKILL.md files.
/// Returns a JSON array of skills with id and name, or None if the directory
/// is not accessible or contains no valid skills.
///
/// This function is synchronous and should only be called from within a
/// `tokio::task::spawn_blocking` context to avoid blocking the async reactor.
fn scan_skills_directory(skills_dir: &std::path::Path) -> Option<String> {
    if !skills_dir.exists() {
        return None;
    }

    let mut skills = Vec::new();
    if let Ok(entries) = std::fs::read_dir(skills_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let file_name = path.file_name().unwrap_or_default();
            let skill_id = file_name.to_string_lossy().into_owned();

            let skill_file = path.join("SKILL.md");
            if !skill_file.exists() {
                continue;
            }

            let content = std::fs::read_to_string(&skill_file).unwrap_or_default();
            let name = content
                .lines()
                .next()
                .unwrap_or("")
                .trim_start_matches("# ")
                .to_string();

            skills.push(serde_json::json!({"id": skill_id, "name": name}));
        }
    }

    if skills.is_empty() {
        return None;
    }

    serde_json::to_string(&skills).ok()
}

impl LlmRouter {
    pub fn new() -> Self {
        Self {
            skills_catalog: tokio::sync::Mutex::new(None),
        }
    }

    /// Route using LLM classification with an explicit router agent and model.
    ///
    /// This is the core routing method. The `router_agent` and `router_model` parameters
    /// identify which LLM to use for classification (distinct from the task's target agent).
    /// The `Router` drives pool selection and calls this method for each pool entry.
    #[allow(clippy::too_many_arguments)]
    pub async fn route_with_llm_using(
        &self,
        task: &ExternalTask,
        available_agents: &[String],
        config: &RouterConfig,
        last_agent: &mut Option<String>,
        repo: &str,
        router_agent: &str,
        router_model: Option<&str>,
    ) -> anyhow::Result<RouteResult> {
        if available_agents.is_empty() {
            anyhow::bail!("no agent CLIs found in PATH");
        }

        // Build the routing prompt (async; offload blocking work inside)
        let prompt = self
            .build_routing_prompt(task, available_agents, router_agent, &config.weights)
            .await?;

        // Save prompt and response to per-task routing dir for debugging
        let routing_dir = crate::home::task_dir_async(repo, &task.id.0)
            .await
            .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/orch-routing"))
            .join("routing");
        let _ = tokio::fs::create_dir_all(&routing_dir).await;

        let prompt_path = routing_dir.join("prompt.txt");
        let _ = tokio::fs::write(&prompt_path, &prompt).await;

        // Call the LLM router with the specified agent+model
        let timeout = Duration::from_secs(config.timeout_seconds);
        let response = self
            .call_router_llm(&prompt, router_agent, router_model, timeout)
            .await?;

        // Parse the response
        let llm_response: LlmRouteResponse = self.parse_llm_response(router_agent, &response)?;

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

        // Apply self-routing penalty: reduce bias toward the router's own agent
        if let Some(redirected) = apply_self_routing_penalty(
            &agent,
            router_agent,
            available_agents,
            config.self_routing_penalty,
            &task.id.0,
        ) {
            tracing::info!(
                original_agent = %agent,
                redirected_agent = %redirected,
                router_agent,
                penalty = config.self_routing_penalty,
                "self-routing penalty applied — redirecting to alternate agent"
            );
            agent = redirected;
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
        let model = config.model_for_complexity(&agent, &complexity, &task.id.0);

        // Build selected skills list
        let mut selected_skills = llm_response.selected_skills;
        for skill in &config.default_skills {
            if !selected_skills.contains(skill) {
                selected_skills.push(skill.clone());
            }
        }

        // Save raw response for debugging
        let response_path = routing_dir.join("response.txt");
        let _ = tokio::fs::write(&response_path, &response).await;

        // Log a concise routing summary at info level (after sanity checks complete)
        let warning = self.check_routing_sanity(task, &agent, &profile);

        tracing::info!(
            task_id = task.id.0,
            router_agent,
            router_model = router_model.unwrap_or("default"),
            selected_executor = %agent,
            complexity = %complexity,
            warning = warning.as_deref(),
            "LLM router decision"
        );

        // Log clean text payload at debug level (not raw NDJSON startup noise)
        let debug_preview = {
            let inner = self
                .extract_agent_text(router_agent, response.trim())
                .unwrap_or_else(|_| response.to_string());
            let trimmed = inner.trim();
            if trimmed.len() <= 500 {
                trimmed.to_string()
            } else {
                format!("{}...", &trimmed[..trimmed.floor_char_boundary(500)])
            }
        };
        tracing::debug!(
            task_id = task.id.0,
            response_len = response.len(),
            response_preview = %debug_preview,
            "LLM router parsed response (debug)"
        );

        // Validate and normalize estimate to allowed Fibonacci values.
        const FIBONACCI_ESTIMATES: [u8; 8] = [0, 1, 2, 3, 5, 8, 13, 21];
        let estimate = if FIBONACCI_ESTIMATES.contains(&llm_response.estimate) {
            llm_response.estimate
        } else {
            // Map to nearest allowed value; warn so operators can tune the prompt.
            let nearest = FIBONACCI_ESTIMATES
                .iter()
                .min_by_key(|&&v| (v as i16 - llm_response.estimate as i16).unsigned_abs())
                .copied()
                .unwrap_or(0);
            tracing::warn!(
                task_id = task.id.0,
                raw_estimate = llm_response.estimate,
                normalized_estimate = nearest,
                "LLM returned non-Fibonacci estimate; normalized to nearest allowed value"
            );
            nearest
        };

        // Track last routed agent for distribution
        *last_agent = Some(agent.clone());

        Ok(RouteResult {
            agent,
            model,
            complexity,
            estimate,
            reason: llm_response.reason,
            profile,
            selected_skills,
            warning,
        })
    }

    /// Build the routing prompt from the template.
    async fn build_routing_prompt(
        &self,
        task: &ExternalTask,
        available_agents: &[String],
        router_agent: &str,
        configured_weights: &std::collections::HashMap<String, f64>,
    ) -> anyhow::Result<String> {
        let template = include_str!("../../../prompts/route.md");

        // Build available agents string
        let available_agents_str = available_agents.join(", ");

        // Build weights string for the prompt
        let weights_str = if configured_weights.is_empty() {
            "No weights configured — distribute evenly.".to_string()
        } else {
            available_agents
                .iter()
                .map(|a| {
                    let w = configured_weights.get(a).copied().unwrap_or(1.0);
                    format!("{a}: {w}")
                })
                .collect::<Vec<_>>()
                .join(", ")
        };

        // Build labels string
        let labels = task.labels.join(", ");

        // Load skills catalog if available — perform blocking FS work off the async reactor.
        let skills_catalog = match self.load_skills_catalog().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "failed to load skills catalog, routing without skills");
                "[]".to_string()
            }
        };

        // Simple template substitution
        let prompt = template
            .replace("{{ROUTER_AGENT}}", router_agent)
            .replace("{{AVAILABLE_AGENTS}}", &available_agents_str)
            .replace("{{AGENT_WEIGHTS}}", &weights_str)
            .replace("{{SKILLS_CATALOG}}", &skills_catalog)
            .replace("{{TASK_ID}}", &task.id.0)
            .replace("{{TASK_TITLE}}", &task.title)
            .replace("{{TASK_LABELS}}", &labels)
            .replace("{{TASK_BODY}}", &task.body);

        Ok(prompt)
    }

    /// Invalidate the skills catalog cache so the next routing call reloads from disk.
    ///
    /// Called after `skills_sync()` updates skill files on disk.
    pub async fn invalidate_skills_catalog(&self) {
        let mut cache = self.skills_catalog.lock().await;
        *cache = None;
    }

    /// Load skills catalog from skills.yml or skills directory.
    /// Cached after first load to avoid blocking I/O in async context.
    async fn load_skills_catalog(&self) -> anyhow::Result<String> {
        // Check cache first (quick lock, drop immediately)
        {
            let cache = self.skills_catalog.lock().await;
            if let Some(ref catalog) = *cache {
                return Ok(catalog.clone());
            }
        }

        // Offload uncached loading to blocking thread pool to avoid blocking the Tokio reactor.
        // Clone any data we need to avoid capturing &self across thread boundary.
        let skills_dir_opt: Option<PathBuf> = match std::env::var("ORCH_HOME") {
            Ok(orch_home) => Some(PathBuf::from(orch_home).join("skills")),
            Err(_) => None,
        };

        let catalog = tokio::task::spawn_blocking(move || -> String {
            // Try skills.yml in current directory
            if let Ok(content) = std::fs::read_to_string("skills.yml") {
                if let Ok(yaml) = serde_norway::from_str::<serde_norway::Value>(&content) {
                    if let Some(skills) = yaml.get("skills") {
                        if let Ok(json) = serde_json::to_string(skills) {
                            return json;
                        }
                    }
                }
            }

            // Try ORCH_HOME/skills directory
            if let Some(skills_dir) = skills_dir_opt.as_ref() {
                if let Some(catalog) = scan_skills_directory(skills_dir) {
                    return catalog;
                }
            }

            // Try ~/.orch/skills
            if let Ok(skills_dir) = crate::home::skills_dir() {
                if let Some(catalog) = scan_skills_directory(&skills_dir) {
                    return catalog;
                }
            }

            "[]".to_string()
        })
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking failed: {e}"))?;

        let mut cache = self.skills_catalog.lock().await;
        *cache = Some(catalog.clone());

        Ok(catalog)
    }

    /// Call the specified router LLM to classify the task.
    ///
    /// `agent` is the CLI name (`claude`, `opencode`, etc.) and `model` is the
    /// optional model string to pass. On rate-limit, records a model-level cooldown.
    pub(super) async fn call_router_llm(
        &self,
        prompt: &str,
        agent: &str,
        model: Option<&str>,
        timeout: Duration,
    ) -> anyhow::Result<String> {
        use crate::engine::runner::direct::{run_direct_command_raw, DirectCommandError};

        // Skip immediately if this specific agent+model is on cooldown
        let model_str = model.unwrap_or("");
        if crate::engine::runner::response::is_model_in_cooldown(agent, model_str) {
            anyhow::bail!("router LLM {agent}:{model_str} is on cooldown");
        }

        let mut cmd =
            crate::engine::runner::agents::get_runner(agent).router_command(prompt, model)?;

        match run_direct_command_raw(&mut cmd, timeout).await {
            Ok(stdout) => Ok(stdout),
            Err(DirectCommandError::NonZeroExit { stdout, stderr, .. }) => {
                // Detect rate limit from stdout (agent exits non-zero on API errors)
                use crate::engine::runner::agents::AgentError;
                let runner = crate::engine::runner::agents::get_runner(agent);
                if let Err(AgentError::RateLimit { .. }) = runner.parse_response(&stdout) {
                    tracing::warn!(
                        agent,
                        model = model.unwrap_or("default"),
                        "router LLM rate limited — adding to cooldown"
                    );
                    crate::engine::runner::response::record_model_failure(agent, model_str).await;
                    anyhow::bail!("router LLM rate limited: {agent}:{model_str}");
                }

                // Try to extract meaningful text from stdout and detect structured errors.
                // This handles cases where the agent exits non-zero but stdout contains
                // useful error payloads (not just rate limits).
                let error_msg = classify_router_llm_failure(agent, &stdout, &stderr);
                let stdout_preview = &stdout[..stdout.floor_char_boundary(500)];
                let stderr_preview = &stderr[..stderr.floor_char_boundary(500)];
                tracing::warn!(
                    stderr = %stderr_preview,
                    stdout = %stdout_preview,
                    reason = %error_msg,
                    "router LLM command failed"
                );
                anyhow::bail!("router LLM failed: {error_msg}");
            }
            Err(DirectCommandError::Timeout { secs }) => {
                anyhow::bail!("router LLM timed out after {secs}s");
            }
            Err(DirectCommandError::EmptyResponse { stderr }) => {
                tracing::warn!(stderr = %stderr, "router LLM returned empty stdout");
                anyhow::bail!("router LLM returned empty response");
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Parse the LLM response into a structured format.
    ///
    /// Handles: direct JSON, Claude `--output-format stream-json` NDJSON,
    /// Claude `--output-format json` envelopes, markdown code blocks, and raw
    /// text with embedded JSON.
    pub fn parse_llm_response(
        &self,
        agent: &str,
        response: &str,
    ) -> anyhow::Result<LlmRouteResponse> {
        let trimmed = response.trim();
        if trimmed.is_empty() {
            anyhow::bail!("empty LLM response");
        }

        let inner = self.extract_agent_text(agent, trimmed)?;

        let text = inner.trim();

        // Step 2: Try to parse directly as JSON. If successful, ensure the
        // required `executor` field is present and non-empty before accepting.
        if let Ok(parsed) = serde_json::from_str::<LlmRouteResponse>(text) {
            if parsed.executor.trim().is_empty() {
                anyhow::bail!("LLM response missing required 'executor' field");
            }
            return Ok(parsed);
        }

        // Step 3: Try to extract JSON from markdown code blocks
        if let Some(json_start) = text.find("```json") {
            let after_start = &text[json_start + 7..];
            if let Some(json_end) = after_start.find("```") {
                let json_str = &after_start[..json_end].trim();
                if let Ok(parsed) = serde_json::from_str::<LlmRouteResponse>(json_str) {
                    if parsed.executor.trim().is_empty() {
                        anyhow::bail!(
                            "LLM response missing required 'executor' field in fenced block"
                        );
                    }
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
                // Attempt to parse the curly-brace substring as a generic JSON
                // value first so we can inspect keys before accepting it as a
                // routing decision. This prevents accidental acceptance of
                // unrelated JSON fragments embedded in prose (e.g. log
                // snippets). Only accept the JSON if it contains an
                // identifying routing key ("executor" or alias "agent").
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(json_str) {
                    if let Some(obj) = value.as_object() {
                        // Only accept if it contains routing-identifying keys
                        if obj.contains_key("executor") || obj.contains_key("agent") {
                            if let Ok(parsed) = serde_json::from_str::<LlmRouteResponse>(json_str) {
                                if parsed.executor.trim().is_empty() {
                                    anyhow::bail!("LLM response contained empty 'executor' field");
                                }
                                return Ok(parsed);
                            }
                        }
                    }

                    // json_str is valid JSON but not a routing response — check
                    // for known error envelopes and surface those first.
                    if let Some(err_msg) = detect_error_envelope(&value) {
                        anyhow::bail!("router LLM returned error payload: {err_msg}");
                    }
                }
            }
        }

        // Final fallback: attempt a tolerant re-parse from the original raw
        // response. This covers variants like fenced code blocks with a
        // space after the fence ("``` json"), embedded JSON fragments that
        // weren't picked up above, or NDJSON-like wrappers that can be
        // conservatively scanned for JSON objects. If tolerant re-parse
        // succeeds, accept it; if it yields a known error envelope, surface
        // that error; otherwise treat as a true parse failure.
        match self.tolerant_reparse(response) {
            Ok(Some(parsed)) => Ok(parsed),
            Ok(None) => {
                anyhow::bail!(
                    "could not parse LLM response as JSON: {}",
                    &text[..text.len().min(200)]
                )
            }
            Err(e) => anyhow::bail!("router LLM returned error payload: {e}"),
        }
    }

    /// Attempt a tolerant re-parse of the raw response string.
    ///
    /// Returns:
    /// - Ok(Some(LlmRouteResponse)) when a routing decision could be extracted
    /// - Ok(None) when no usable routing decision was found
    /// - Err(String) when a known error envelope was detected and should be surfaced
    fn tolerant_reparse(&self, raw: &str) -> anyhow::Result<Option<LlmRouteResponse>> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(None);
        }

        // 1) Try to find fenced code blocks leniently. Accept variants like
        //    "```json", "``` json", or plain "```". Try each block's
        //    contents as JSON for LlmRouteResponse.
        let mut idx = 0usize;
        while let Some(start) = raw[idx..].find("```") {
            let abs_start = idx + start;
            // Skip the opening fence
            let after = &raw[abs_start + 3..];
            // Skip optional whitespace and optional "json" language tag
            let after_trimmed = after.trim_start();
            // Find the closing fence from after
            if let Some(end_rel) = after_trimmed.find("```") {
                let json_str = &after_trimmed[..end_rel].trim();
                if !json_str.is_empty() {
                    if let Ok(parsed) = serde_json::from_str::<LlmRouteResponse>(json_str) {
                        if !parsed.executor.trim().is_empty() {
                            return Ok(Some(parsed));
                        }
                    }
                    // Try parsing generic JSON to check for error envelopes
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) {
                        if let Some(err) = detect_error_envelope(&val) {
                            return Err(anyhow::anyhow!(err));
                        }
                    }
                }
                // Advance past this block
                idx = abs_start + 3 + end_rel + 3;
                continue;
            } else {
                // No matching end fence — break to avoid infinite loop
                break;
            }
        }

        // 2) Try to extract balanced JSON objects from the raw text. This is a
        //    conservative scan that attempts to find top-level {...} fragments.
        let bytes = raw.as_bytes();
        let mut i = 0usize;
        while i < bytes.len() {
            if bytes[i] == b'{' {
                let mut depth = 0i32;
                let mut j = i;
                while j < bytes.len() {
                    if bytes[j] == b'{' {
                        depth += 1;
                    } else if bytes[j] == b'}' {
                        depth -= 1;
                        if depth == 0 {
                            // candidate from i..=j
                            if let Ok(candidate) = std::str::from_utf8(&bytes[i..=j]) {
                                if let Ok(val) =
                                    serde_json::from_str::<serde_json::Value>(candidate)
                                {
                                    // If it's an error envelope, surface it
                                    if let Some(err) = detect_error_envelope(&val) {
                                        return Err(anyhow::anyhow!(err));
                                    }
                                    // Only accept if contains routing key
                                    if let Some(obj) = val.as_object() {
                                        if obj.contains_key("executor") || obj.contains_key("agent")
                                        {
                                            if let Ok(parsed) =
                                                serde_json::from_str::<LlmRouteResponse>(candidate)
                                            {
                                                if !parsed.executor.trim().is_empty() {
                                                    return Ok(Some(parsed));
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            break;
                        }
                    }
                    j += 1;
                }
                i = j + 1;
            } else {
                i += 1;
            }
        }

        // 3) As a last-ditch attempt, try the opencode NDJSON extractor which
        //    can pull JSON-like payloads out of NDJSON streams produced by
        //    opencode/Claude wrappers.
        if let Some(text) = opencode::extract_router_text(raw) {
            let trimmed = text.trim();
            if let Ok(parsed) = serde_json::from_str::<LlmRouteResponse>(trimmed) {
                if !parsed.executor.trim().is_empty() {
                    return Ok(Some(parsed));
                }
            }
            // If it's valid JSON but an error envelope, surface it
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
                if let Some(err) = detect_error_envelope(&val) {
                    return Err(anyhow::anyhow!(err));
                }
            }
        }

        Ok(None)
    }

    fn extract_agent_text(&self, agent: &str, raw: &str) -> anyhow::Result<String> {
        match agent {
            "opencode" => {
                let events = agents::parse_ndjson(raw.trim());
                if events.is_empty() {
                    // No parseable NDJSON lines — treat as plain text
                    Ok(raw.to_string())
                } else {
                    match opencode::extract_ndjson_text(&events) {
                        Some(text) => Ok(text),
                        None => {
                            anyhow::bail!(
                                "opencode produced no text output (NDJSON had no text events)"
                            )
                        }
                    }
                }
            }
            "claude" | "kimi" | "minimax" => match claude::extract_stream_json_result_text(raw) {
                Ok(text) => {
                    // Defensive: some wrappers emit a single `system init` envelope
                    // before the actual result. If the extracted text looks like a
                    // system/init envelope (no useful result), try a more lenient
                    // NDJSON extractor that scans all lines for text/result events.
                    let trimmed_text = text.trim();
                    // If the extracted text is itself a JSON envelope that only
                    // contains a `type: system` or `subtype: init` event (startup
                    // envelope), treat it as non-informative and try a lenient
                    // NDJSON extractor that scans all lines for text/result events.
                    let looks_like_system_init =
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed_text) {
                            val.get("type").and_then(|v| v.as_str()) == Some("system")
                                || val
                                    .get("subtype")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s == "init")
                                    .unwrap_or(false)
                        } else {
                            false
                        };

                    if looks_like_system_init {
                        if let Some(fallback) = opencode::extract_router_text(raw) {
                            return Ok(fallback);
                        }
                        anyhow::bail!(
                            "router LLM produced only system/init envelope with no text result"
                        )
                    }

                    Ok(text)
                }
                Err(AgentError::AgentFailed { message }) => {
                    anyhow::bail!("router LLM returned error: {message}")
                }
                Err(AgentError::InvalidResponse { raw }) => {
                    // If the agent's extract_text failed to parse structured
                    // response, try a conservative NDJSON extraction for router
                    // text (opencode-style). If that returns some usable text,
                    // prefer it; otherwise treat as plain raw text.
                    if let Some(text) = opencode::extract_router_text(&raw) {
                        return Ok(text);
                    }
                    Ok(raw)
                }
                Err(err) => anyhow::bail!("router LLM returned error: {err}"),
            },
            _ => Ok(raw.to_string()),
        }
    }

    /// Run sanity checks on routing decision.
    pub fn check_routing_sanity(
        &self,
        task: &ExternalTask,
        agent: &str,
        _profile: &AgentProfile,
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

        None
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

    // ── detect_error_envelope ─────────────────────────────────────────────────

    #[test]
    fn detect_error_type_field_direct() {
        let json: serde_json::Value =
            serde_json::from_str(r#"{"type":"error","message":"connection failed"}"#).unwrap();
        let result = detect_error_envelope(&json);
        assert!(result.is_some());
        assert!(result.unwrap().contains("connection failed"));
    }

    #[test]
    fn detect_error_type_field_with_nested_error() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{"type":"error","timestamp":1743212400,"error":{"name":"UnknownError","data":{"message":"Unable to connect"}}}"#,
        )
        .unwrap();
        let result = detect_error_envelope(&json).unwrap();
        assert!(
            result.contains("type=error") && result.contains("Unable to connect"),
            "should extract data.message, got: {result}"
        );
    }

    #[test]
    fn detect_error_kimi_nested_envelope() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{"error":{"type":"permission_error","message":"You've reached your usage limit"},"type":"error"}"#,
        )
        .unwrap();
        let result = detect_error_envelope(&json).unwrap();
        assert!(
            result.contains("type=error") && result.contains("usage limit"),
            "should extract usage limit message, got: {result}"
        );
    }

    #[test]
    fn detect_error_kimi_nested_without_outer_type() {
        // Kimi-style nested error WITHOUT outer type=error — tests the nested branch in isolation
        let json: serde_json::Value = serde_json::from_str(
            r#"{"error":{"name":"RateLimitError","message":"rate limit exceeded"}}"#,
        )
        .unwrap();
        let result = detect_error_envelope(&json).unwrap();
        assert!(
            result.contains("error.name=RateLimitError") && result.contains("rate limit"),
            "got: {result}"
        );
    }

    #[test]
    fn detect_error_kimi_billing_envelope() {
        // The exact Kimi rate-limit envelope from the issue comments
        let json: serde_json::Value = serde_json::from_str(
            r#"{"error":{"type":"permission_error","message":"You've reached your usage limit for this billing cycle. Your quota will be refreshed in the next cycle. Upgrade to get more: https://example.com"},"type":"error"}"#,
        )
        .unwrap();
        let result = detect_error_envelope(&json).unwrap();
        assert!(
            result.contains("type=error") && result.contains("usage limit"),
            "should extract usage limit message, got: {result}"
        );
    }

    #[test]
    fn detect_error_openai_flat_style() {
        let json: serde_json::Value =
            serde_json::from_str(r#"{"error":"rate limit exceeded","type":"standard"}"#).unwrap();
        let result = detect_error_envelope(&json).unwrap();
        assert!(result.contains("error:"));
    }

    #[test]
    fn detect_error_indicator_429() {
        let json: serde_json::Value =
            serde_json::from_str(r#"{"status":429,"message":"Too Many Requests"}"#).unwrap();
        let result = detect_error_envelope(&json);
        assert!(result.is_some());
        assert!(result.unwrap().contains("429"));
    }

    #[test]
    fn detect_error_indicator_overloaded() {
        let json: serde_json::Value =
            serde_json::from_str(r#"{"status":529,"message":"Service overloaded"}"#).unwrap();
        let result = detect_error_envelope(&json);
        assert!(result.is_some());
        assert!(result.unwrap().contains("overloaded"));
    }

    #[test]
    fn detect_error_indicator_auth() {
        let json: serde_json::Value =
            serde_json::from_str(r#"{"status":401,"message":"Unauthorized"}"#).unwrap();
        let result = detect_error_envelope(&json);
        assert!(result.is_some());
        assert!(result.unwrap().contains("401"));
    }

    #[test]
    fn detect_error_returns_none_for_valid_route_response() {
        // A valid route response should NOT be detected as an error
        let json: serde_json::Value = serde_json::from_str(
            r#"{"executor":"claude","complexity":"medium","reason":"good fit"}"#,
        )
        .unwrap();
        let result = detect_error_envelope(&json);
        assert!(
            result.is_none(),
            "valid route response must not be detected as error"
        );
    }

    #[test]
    fn detect_error_returns_none_for_non_object() {
        // Non-object JSON values should return None
        let json: serde_json::Value = serde_json::from_str(r#""just a string""#).unwrap();
        assert!(detect_error_envelope(&json).is_none());
        let json: serde_json::Value = serde_json::from_str(r#"[1,2,3]"#).unwrap();
        assert!(detect_error_envelope(&json).is_none());
    }

    #[test]
    fn detect_error_returns_none_for_tool_use_permission_rejection() {
        // OpenCode tool-use rejection must NOT be treated as an API error.
        // Before the fix, the broad "permission" substring matched this and caused
        // false-positive cooldowns on the model.
        let json: serde_json::Value = serde_json::from_str(
            r#"{"error":{"name":"PermissionError","data":{"message":"user rejected permission to use this specific tool call"}}}"#,
        )
        .unwrap();
        let result = detect_error_envelope(&json);
        assert!(
            result.is_none(),
            "tool-use permission rejection must not be detected as API error, got: {result:?}"
        );
    }

    // ── parse_llm_response ────────────────────────────────────────────────────

    #[test]
    fn parse_direct_json_executor_field() {
        let router = make_router();
        let json = r#"{"executor":"claude","complexity":"medium","reason":"good fit"}"#;
        let resp = router.parse_llm_response("claude", json).unwrap();
        assert_eq!(resp.executor, "claude");
        assert_eq!(resp.complexity, "medium");
    }

    #[test]
    fn parse_direct_json_agent_alias() {
        // LLM sometimes returns "agent" instead of "executor"
        let router = make_router();
        let json = r#"{"agent":"codex","complexity":"simple","reason":"straightforward"}"#;
        let resp = router.parse_llm_response("claude", json).unwrap();
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
        let resp = router.parse_llm_response("opencode", json).unwrap();
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
        let resp = router.parse_llm_response("claude", &envelope).unwrap();
        assert_eq!(resp.executor, "claude");
    }

    #[test]
    fn parse_claude_envelope_object_result() {
        // When result is already a JSON object (not a string)
        let router = make_router();
        let envelope = r#"{"type":"result","is_error":false,"result":{"executor":"kimi","complexity":"simple","reason":"fast"}}"#;
        let resp = router.parse_llm_response("claude", envelope).unwrap();
        assert_eq!(resp.executor, "kimi");
    }

    #[test]
    fn parse_claude_error_envelope() {
        let router = make_router();
        let envelope = r#"{"type":"result","is_error":true,"result":"auth error: invalid key"}"#;
        let err = router.parse_llm_response("claude", envelope).unwrap_err();
        assert!(
            err.to_string().contains("error"),
            "should surface the error"
        );
    }

    #[test]
    fn parse_markdown_json_fenced_block() {
        let router = make_router();
        let md = "Here is my routing decision:\n\n```json\n{\"executor\":\"codex\",\"complexity\":\"simple\",\"reason\":\"easy\"}\n```\n\nDone.";
        let resp = router.parse_llm_response("claude", md).unwrap();
        assert_eq!(resp.executor, "codex");
    }

    #[test]
    fn parse_markdown_plain_fenced_block() {
        let router = make_router();
        let md = "```\n{\"executor\":\"minimax\",\"complexity\":\"medium\",\"reason\":\"ok\"}\n```";
        let resp = router.parse_llm_response("claude", md).unwrap();
        assert_eq!(resp.executor, "minimax");
    }

    #[test]
    fn parse_valid_json_error_envelope_type_error() {
        // Valid JSON with {"type":"error",...} should NOT produce "could not parse" error.
        // Instead it should surface as "router LLM returned error payload: ...".
        let router = make_router();
        let json = r#"{"type":"error","timestamp":1743212400,"error":{"name":"UnknownError","data":{"message":"Unable to connect to upstream"}}}"#;
        let err = router.parse_llm_response("claude", json).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("router LLM returned error payload"),
            "should be structured error, got: {msg}"
        );
        assert!(
            msg.contains("Unable to connect"),
            "should extract the error message: {msg}"
        );
        assert!(
            !msg.contains("could not parse"),
            "must NOT say 'could not parse' for valid JSON: {msg}"
        );
    }

    #[test]
    fn parse_valid_json_error_envelope_kimi_style() {
        // Kimi-style error envelope with nested error object
        let router = make_router();
        let json = r#"{"error":{"type":"permission_error","message":"You've reached your usage limit for this billing cycle."},"type":"error"}"#;
        let err = router.parse_llm_response("kimi", json).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("error") && !msg.contains("could not parse"),
            "should surface error for Kimi-style envelope, got: {msg}"
        );
    }

    #[test]
    fn parse_valid_json_error_envelope_openai_style() {
        // OpenAI-style {"error": "message string"} envelope
        let router = make_router();
        let json = r#"{"error":"rate limit exceeded","type":"standard"}"#;
        let err = router.parse_llm_response("claude", json).unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.contains("could not parse"),
            "valid JSON should not produce parse failure: {msg}"
        );
    }

    #[test]
    fn parse_valid_json_error_envelope_with_indicator() {
        // Valid JSON that contains error indicators but no explicit error envelope
        let router = make_router();
        let json = r#"{"status":503,"message":"Service overloaded, retry later","retry_after":30}"#;
        let err = router.parse_llm_response("claude", json).unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.contains("could not parse"),
            "valid JSON with error indicator should not produce parse failure: {msg}"
        );
        assert!(
            msg.contains("error indicator"),
            "should mention the detected indicator: {msg}"
        );
    }

    #[test]
    fn parse_malformed_json_produces_parse_failure() {
        // True malformed JSON must still produce "could not parse" error
        let router = make_router();
        let err = router
            .parse_llm_response("claude", "{ invalid json }")
            .unwrap_err();
        assert!(
            err.to_string().contains("could not parse"),
            "malformed JSON must produce parse failure, got: {}",
            err
        );
    }

    #[test]
    fn parse_valid_route_json_still_succeeds() {
        // Ensure the success path still works (valid JSON that is LlmRouteResponse)
        let router = make_router();
        let json = r#"{"executor":"claude","complexity":"medium","reason":"test"}"#;
        let resp = router.parse_llm_response("claude", json).unwrap();
        assert_eq!(resp.executor, "claude");
        assert_eq!(resp.complexity, "medium");
    }

    #[test]
    fn parse_embedded_json_in_prose() {
        let router = make_router();
        let text = r#"I analyzed the task. My decision is {"executor":"claude","complexity":"complex","reason":"hard task"}. Please proceed."#;
        let resp = router.parse_llm_response("claude", text).unwrap();
        assert_eq!(resp.executor, "claude");
        assert_eq!(resp.complexity, "complex");
    }

    #[test]
    fn parse_embedded_json_fragment_without_executor_is_ignored() {
        let router = make_router();
        let text = r#"Here is a debug trace: {"trace":{"duration":123,"id":"abc"}} — end of log."#;
        // The embedded JSON does not contain an `executor`/`agent` key and
        // should not be accepted as a routing decision.
        assert!(router.parse_llm_response("claude", text).is_err());
    }

    #[test]
    fn parse_fenced_json_without_executor_fails() {
        let router = make_router();
        let md = "```json\n{\"trace\":{\"duration\":10}}\n```";
        assert!(router.parse_llm_response("claude", md).is_err());
    }

    #[test]
    fn parse_empty_response_fails() {
        let router = make_router();
        assert!(router.parse_llm_response("claude", "").is_err());
        assert!(router.parse_llm_response("claude", "   ").is_err());
    }

    #[test]
    fn parse_invalid_response_fails() {
        let router = make_router();
        assert!(router
            .parse_llm_response("claude", "not json at all")
            .is_err());
        assert!(router
            .parse_llm_response("claude", "{ invalid json }")
            .is_err());
    }

    #[test]
    fn parse_defaults_apply_for_missing_fields() {
        // Only "executor" is required — other fields should default
        let router = make_router();
        let json = r#"{"executor":"claude"}"#;
        let resp = router.parse_llm_response("claude", json).unwrap();
        assert_eq!(resp.executor, "claude");
        assert_eq!(resp.complexity, "");
        assert_eq!(resp.reason, "");
        assert!(resp.selected_skills.is_empty());
    }

    #[test]
    fn parse_fixture_route_response_string() {
        let router = make_router();
        let response = include_str!("../../../tests/fixtures/route-response-string.json");
        let resp = router.parse_llm_response("claude", response).unwrap();
        // Fixture should parse without error and produce a valid agent name
        assert!(!resp.executor.is_empty(), "executor must not be empty");
    }

    #[test]
    fn parse_fixture_route_response_object() {
        let router = make_router();
        let response = include_str!("../../../tests/fixtures/route-response-object.json");
        let resp = router.parse_llm_response("claude", response).unwrap();
        assert!(!resp.executor.is_empty(), "executor must not be empty");
    }

    #[test]
    fn parse_fixture_route_response_markdown() {
        let router = make_router();
        let response = include_str!("../../../tests/fixtures/route-response-markdown.json");
        let resp = router.parse_llm_response("claude", response).unwrap();
        assert!(!resp.executor.is_empty(), "executor must not be empty");
    }

    #[test]
    fn parse_opencode_ndjson_text_event() {
        let router = make_router();
        let raw = r#"{"type":"step_start","timestamp":1}
{"type":"text","timestamp":2,"part":{"type":"text","text":"progress update"}}
{"type":"text","timestamp":3,"part":{"type":"text","text":"```json\n{\"executor\":\"opencode\",\"complexity\":\"medium\",\"reason\":\"ndjson\"}\n```"}}
{"type":"step_finish","timestamp":4,"part":{"type":"step-finish","reason":"stop"}}"#;
        let resp = router.parse_llm_response("opencode", raw).unwrap();
        assert_eq!(resp.executor, "opencode");
        assert_eq!(resp.complexity, "medium");
    }

    #[test]
    fn parse_opencode_ndjson_direct_text_field() {
        let router = make_router();
        let raw = r#"{"type":"step_start","timestamp":1}
{"type":"text","timestamp":2,"text":"{\"executor\":\"claude\",\"complexity\":\"simple\",\"reason\":\"direct text\"}"}
{"type":"step_finish","timestamp":3,"part":{"type":"step-finish","reason":"stop"}}"#;
        let resp = router.parse_llm_response("opencode", raw).unwrap();
        assert_eq!(resp.executor, "claude");
        assert_eq!(resp.complexity, "simple");
    }

    #[test]
    fn parse_opencode_ndjson_no_text_events_returns_clear_error() {
        // When opencode emits only control events (step_start/step_finish) with
        // no text payload, extract_agent_text must return a clear error instead
        // of falling back to the raw NDJSON string (which would cause parse_llm_response
        // to extract a step_start event and produce a misleading "could not parse" error).
        let router = make_router();
        let raw = r#"{"type":"step_start","timestamp":1774721088828,"sessionID":"ses_abc","part":{"type":"step-start","snapshot":""}}
{"type":"step_finish","timestamp":1774721089000,"part":{"type":"step-finish","reason":"stop","cost":0,"tokens":{"total":100,"input":99,"output":1}}}"#;
        let err = router.parse_llm_response("opencode", raw).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("no text") || msg.contains("no text events"),
            "error should mention missing text output, got: {msg}"
        );
        assert!(
            !msg.contains("step_start"),
            "error must not expose raw NDJSON step_start event, got: {msg}"
        );
    }

    #[test]
    fn parse_kimi_stream_json_result() {
        let router = make_router();
        let raw = r#"{"type":"system","subtype":"init"}
{"type":"result","subtype":"success","is_error":false,"result":"{\"executor\":\"kimi\",\"complexity\":\"medium\",\"reason\":\"wrapper\"}"}"#;
        let resp = router.parse_llm_response("kimi", raw).unwrap();
        assert_eq!(resp.executor, "kimi");
    }

    #[test]
    fn extract_agent_text_for_debug_log_ndjson_success() {
        // Regression test: extract_agent_text should return the clean text payload,
        // not the raw NDJSON startup noise that would appear in debug logs.
        let router = make_router();
        let raw = r#"{"type":"system","subtype":"hook_started","hook_id":"abc"}
{"type":"system","subtype":"init"}
{"type":"text","timestamp":2,"text":"{\"executor\":\"claude\",\"complexity\":\"simple\",\"reason\":\"small fix\"}"}
{"type":"step_finish","timestamp":3}"#;
        let text = router.extract_agent_text("claude", raw.trim()).unwrap();
        let trimmed = text.trim();
        // Should contain the routing decision, not hook_started noise
        assert!(
            trimmed.contains("executor")
                && trimmed.contains("claude")
                && trimmed.contains("simple"),
            "extracted text must contain routing fields: {trimmed}"
        );
        // Must not contain hook_started in the extracted text
        assert!(
            !trimmed.contains("hook_started"),
            "extracted text must not contain hook_started NDJSON noise"
        );
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

    // ── apply_self_routing_penalty ────────────────────────────────────────────

    #[test]
    fn self_routing_penalty_zero_always_redirects() {
        let agents = vec![
            "opencode".to_string(),
            "claude".to_string(),
            "codex".to_string(),
        ];
        // penalty=0.0: any hash ∈ [0,1) is never < 0.0, so always redirects
        let result = apply_self_routing_penalty("opencode", "opencode", &agents, 0.0, "task-42");
        assert!(result.is_some(), "penalty=0.0 must redirect self-routing");
        assert_ne!(
            result.unwrap(),
            "opencode",
            "redirected agent must not be the router agent"
        );
    }

    #[test]
    fn self_routing_penalty_one_keeps_self_routing() {
        let agents = vec!["opencode".to_string(), "claude".to_string()];
        // penalty=1.0: any hash ∈ [0,1) is always < 1.0, so never redirects
        let result = apply_self_routing_penalty("opencode", "opencode", &agents, 1.0, "task-42");
        assert!(result.is_none(), "penalty=1.0 must not redirect");
    }

    #[test]
    fn self_routing_penalty_no_redirect_when_different_agent_chosen() {
        let agents = vec!["opencode".to_string(), "claude".to_string()];
        // LLM chose claude, router is opencode — no self-routing, no penalty
        let result = apply_self_routing_penalty("claude", "opencode", &agents, 0.0, "task-42");
        assert!(
            result.is_none(),
            "no redirect when LLM chose a different agent than the router"
        );
    }

    #[test]
    fn self_routing_penalty_no_redirect_when_no_alternatives() {
        // Only one agent available — can't redirect anywhere else
        let agents = vec!["opencode".to_string()];
        let result = apply_self_routing_penalty("opencode", "opencode", &agents, 0.0, "task-42");
        assert!(
            result.is_none(),
            "no redirect when no alternative agents are available"
        );
    }

    #[test]
    fn self_routing_penalty_redirects_only_to_non_router_agents() {
        let agents = vec![
            "opencode".to_string(),
            "claude".to_string(),
            "codex".to_string(),
        ];
        // penalty=0.0 always redirects — verify the redirected agent is never "opencode"
        for suffix in &["1", "2", "3", "4", "5", "42", "100", "999"] {
            let task_id = format!("task-{suffix}");
            let result = apply_self_routing_penalty("opencode", "opencode", &agents, 0.0, &task_id);
            if let Some(agent) = result {
                assert_ne!(
                    agent, "opencode",
                    "redirected agent must not be router agent"
                );
                assert!(
                    agents.contains(&agent),
                    "redirected agent must be in available_agents"
                );
            }
        }
    }

    // ── classify_router_llm_failure ───────────────────────────────────────────

    #[test]
    fn classify_nonzero_exit_with_structured_stdout_error() {
        let stdout =
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        let stderr = "";
        let result = classify_router_llm_failure("claude", stdout, stderr);
        assert!(result.contains("type=error") && result.contains("Overloaded"));
    }

    #[test]
    fn classify_nonzero_exit_with_claude_ndjson_error() {
        let stdout = r#"{"type":"result","is_error":true,"result":"Claude model haiku is not available","usage":null}"#;
        let stderr = "";
        let result = classify_router_llm_failure("claude", stdout, stderr);
        assert!(result.contains("router LLM returned error: Claude model haiku is not available"));
    }

    #[test]
    fn classify_nonzero_exit_with_system_only_ndjson() {
        let stdout = r#"{"type":"system","subtype":"hook_started","hook_id":"123"}
{"type":"system","subtype":"init","hook_id":"123"}"#;
        let stderr = "some warning here";
        let result = classify_router_llm_failure("claude", stdout, stderr);
        assert_eq!(result, "router LLM produced only system/startup envelope");
    }

    #[test]
    fn classify_nonzero_exit_with_no_text_ndjson_only() {
        let stdout = r#"{"type":"event","data":{}}"#;
        let stderr = "some warning here";
        let result = classify_router_llm_failure("opencode", stdout, stderr);
        assert_eq!(result, "router LLM produced only system/startup envelope");
    }
}
