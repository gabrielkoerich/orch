//! Routing strategy implementations.
//!
//! Each strategy is a free function that takes the data it needs as parameters
//! and returns a `RouteResult`. `Router::route()` in `mod.rs` orchestrates
//! which strategy to invoke based on config and task labels.
//!
//! Strategies (in dispatch priority order):
//! 1. `route_via_label` — explicit `agent:*` label wins
//! 2. `route_via_weighted_round_robin` — capacity-weighted probabilistic selection
//! 3. `route_via_round_robin_stateful` — persistent index, skips last-used agent
//! 4. `route_via_fallback` — named fallback agent or stateless round-robin
//! 5. `route_via_round_robin` — stateless modulo-based (tests + fallback)

use crate::backends::ExternalTask;

use super::{AgentProfile, RouteResult};
use super::config::RouterConfig;
use super::weights::AgentWeights;

/// Extract an explicit `agent:*` label from the task, if present and valid.
///
/// Returns the agent name if the label matches one of `agents_config`.
pub(super) fn extract_agent_from_labels(
    agents_config: &[String],
    labels: &[String],
) -> Option<String> {
    for label in labels {
        if let Some(agent) = label.strip_prefix("agent:") {
            let agent = agent.to_lowercase();
            if agents_config.iter().any(|a| a == &agent) {
                return Some(agent);
            }
        }
    }
    None
}

/// Extract complexity level from a `complexity:*` label, defaulting to "medium".
pub(super) fn extract_complexity_from_labels(labels: &[String]) -> String {
    for label in labels {
        if let Some(comp) = label.strip_prefix("complexity:") {
            let comp = comp.to_lowercase();
            if ["simple", "medium", "complex"].contains(&comp.as_str()) {
                return comp;
            }
        }
    }
    "medium".to_string()
}

/// Route using stateless modulo round-robin (task-ID based).
///
/// Kept for backward compatibility and unit tests. The project prefers
/// `route_via_round_robin_stateful` (which persists an index), but this
/// stateless implementation allows deterministic selection based on task ID
/// and is used by existing tests.
pub(super) fn route_via_round_robin(
    agents: &[String],
    config: &RouterConfig,
    task: &ExternalTask,
) -> anyhow::Result<RouteResult> {
    if agents.is_empty() {
        anyhow::bail!("no agent CLIs found in PATH");
    }

    let task_num: usize = task.id.0.parse().unwrap_or(0);
    let agent_idx = task_num % agents.len();
    let agent = agents[agent_idx].clone();

    let profile = AgentProfile {
        role: "general".to_string(),
        skills: vec![],
        tools: config.allowed_tools.clone(),
        constraints: vec![],
    };

    Ok(RouteResult {
        agent: agent.clone(),
        model: config.model_for_complexity(&agent, "medium"),
        complexity: "medium".to_string(),
        reason: format!("round_robin (task {} % {} agents)", task.id.0, agents.len()),
        profile,
        selected_skills: config.default_skills.clone(),
        warning: None,
    })
}

/// Stateful round-robin: cycles through agents using a persistent index,
/// skipping the last-used agent when possible.
pub(super) fn route_via_round_robin_stateful(
    agents: &[String],
    config: &RouterConfig,
    task: &ExternalTask,
) -> anyhow::Result<RouteResult> {
    if agents.is_empty() {
        anyhow::bail!("no agent CLIs found in PATH");
    }

    let current_idx: usize = crate::sidecar::get("_router", "rr_index")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let last_agent = crate::sidecar::get("_router", "last_agent").ok();

    let mut agent_idx = current_idx % agents.len();
    if agents.len() > 1 {
        if let Some(ref last) = last_agent {
            if agents.get(agent_idx).map(|a| a.as_str()) == Some(last.as_str()) {
                agent_idx = (agent_idx + 1) % agents.len();
            }
        }
    }

    let agent = agents[agent_idx].clone();

    let next_idx = (agent_idx + 1) % agents.len();
    if let Err(e) = crate::sidecar::set(
        "_router",
        &[
            format!("rr_index={}", next_idx),
            format!("last_agent={}", agent),
        ],
    ) {
        tracing::warn!(error = ?e, "failed to persist round-robin state");
    }

    let complexity = extract_complexity_from_labels(&task.labels);
    let model = config.model_for_complexity(&agent, &complexity);

    let profile = AgentProfile {
        role: "general".to_string(),
        skills: vec![],
        tools: config.allowed_tools.clone(),
        constraints: vec![],
    };

    Ok(RouteResult {
        agent: agent.clone(),
        model,
        complexity,
        reason: format!(
            "round_robin (index {} of {} agents)",
            agent_idx,
            agents.len()
        ),
        profile,
        selected_skills: config.default_skills.clone(),
        warning: None,
    })
}

/// Weighted round-robin: selects an agent based on capacity weights.
///
/// Agents with higher weights (more capacity) get more tasks.
/// Rate-limited agents have reduced weights and receive fewer tasks.
pub(super) fn route_via_weighted_round_robin(
    agents: &[String],
    weights: &AgentWeights,
    config: &RouterConfig,
    task: &ExternalTask,
) -> anyhow::Result<RouteResult> {
    if agents.is_empty() {
        anyhow::bail!("no agent CLIs found in PATH");
    }

    let agent = weights
        .weighted_select(agents, &task.id.0)
        .unwrap_or_else(|| agents[0].clone());

    let weight = weights.get_weight(&agent);
    let complexity = extract_complexity_from_labels(&task.labels);
    let model = config.model_for_complexity(&agent, &complexity);

    let weight_summary: Vec<String> = weights
        .snapshot()
        .iter()
        .filter(|(a, _, _)| agents.contains(a))
        .map(|(a, w, _)| format!("{a}={w:.2}"))
        .collect();

    let profile = AgentProfile {
        role: "general".to_string(),
        skills: vec![],
        tools: config.allowed_tools.clone(),
        constraints: vec![],
    };

    let _ = crate::sidecar::set("_router", &[format!("last_agent={}", agent)]);

    let reason = format!(
        "weighted_round_robin (weight={weight:.2}, weights=[{}])",
        weight_summary.join(", ")
    );

    tracing::info!(
        task_id = %task.id.0,
        agent = %agent,
        weight,
        "weighted round-robin selected agent"
    );

    Ok(RouteResult {
        agent,
        model,
        complexity,
        reason,
        profile,
        selected_skills: config.default_skills.clone(),
        warning: None,
    })
}

/// Fallback routing when LLM fails.
///
/// If `fallback_executor` is "round_robin", uses stateless round-robin.
/// Otherwise uses the named agent, falling back to the first available.
pub(super) fn route_via_fallback(
    available_agents: &[String],
    config: &RouterConfig,
    task: &ExternalTask,
) -> anyhow::Result<RouteResult> {
    if config.fallback_executor == "round_robin" {
        return route_via_round_robin(available_agents, config, task).map(|mut r| {
            r.reason = format!("router failed; fallback round_robin → {}", r.agent);
            r
        });
    }

    let agent = if available_agents.contains(&config.fallback_executor) {
        config.fallback_executor.clone()
    } else {
        available_agents
            .first()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no agents available"))?
    };

    let complexity = extract_complexity_from_labels(&task.labels);
    let model = config.model_for_complexity(&agent, &complexity);

    let profile = AgentProfile {
        role: "general".to_string(),
        skills: vec![],
        tools: config.allowed_tools.clone(),
        constraints: vec![],
    };

    Ok(RouteResult {
        agent: agent.clone(),
        model,
        complexity,
        reason: format!("router failed; fallback to {agent}"),
        profile,
        selected_skills: config.default_skills.clone(),
        warning: None,
    })
}
