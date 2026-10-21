//! Opt-in, fail-closed named-persona routing proof.
//!
//! Enable only with ORCH_STRICT_PROFILE_REGISTRY and ORCH_STRICT_WORKER_HOST.
//! The native route and command builders are implemented; live execution remains
//! deliberately unavailable until fresh authorization and distributed leases are
//! implemented. A true worker_execution_enabled value is rejected, not ignored.

use crate::backends::ExternalTask;
use crate::store::TaskStore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub const REGISTRY_ENV: &str = "ORCH_STRICT_PROFILE_REGISTRY";
pub const HOST_ENV: &str = "ORCH_STRICT_WORKER_HOST";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GuardError {
    #[error("strict profile: execution:held")]
    Held,
    #[error("strict profile: exactly execution:ready on an open task is required")]
    NotReady,
    #[error("strict profile: one known, enabled, current persona is required")]
    Persona,
    #[error("strict profile: missing {0}")]
    Missing(&'static str),
    #[error("strict profile: unavailable or incompatible {0}; no fallback")]
    Unavailable(&'static str),
    #[error("strict profile: persisted route or invocation differs from immutable pin")]
    PinMismatch,
    #[error("strict profile: live execution is disabled pending authenticated hosts, fresh admission and durable claims")]
    ProofOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfilePin {
    pub repository: String,
    pub issue_id: String,
    pub persona: String,
    pub source_agent_id: String,
    pub host: String,
    pub cli: String,
    pub provider: String,
    pub cli_provider: String,
    pub model: String,
    pub effort: String,
    pub registry_digest: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NamedProfile {
    pub enabled: bool,
    pub current_roster: bool,
    pub source_agent_id: Option<String>,
    pub host: Option<String>,
    pub cli: Option<String>,
    pub provider: Option<String>,
    pub cli_provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub allow_fallback: bool,
    pub repositories: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CliCapability {
    pub available: bool,
    pub models: BTreeMap<String, BTreeMap<String, Vec<String>>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HostCapability {
    pub available: bool,
    pub clis: BTreeMap<String, CliCapability>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Capabilities {
    pub hosts: BTreeMap<String, HostCapability>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Policy {
    pub schema_version: u32,
    pub worker_execution_enabled: bool,
    pub profiles: BTreeMap<String, NamedProfile>,
    pub provider_bindings: BTreeMap<String, BTreeMap<String, String>>,
    pub capabilities: Capabilities,
    #[serde(skip)]
    pub digest: String,
}

fn text(value: &Option<String>, dimension: &'static str) -> Result<String, GuardError> {
    value
        .as_ref()
        .filter(|v| !v.is_empty() && v.trim() == *v && !v.chars().any(char::is_control))
        .cloned()
        .ok_or(GuardError::Missing(dimension))
}

impl Policy {
    pub fn from_json(raw: &str) -> anyhow::Result<Self> {
        let mut policy: Self = serde_json::from_str(raw)?;
        anyhow::ensure!(
            policy.schema_version == 1,
            "unsupported strict profile schema"
        );
        if policy.worker_execution_enabled {
            return Err(GuardError::ProofOnly.into());
        }
        policy.digest = format!("{:x}", Sha256::digest(raw.as_bytes()));
        Ok(policy)
    }

    pub fn resolve(
        &self,
        task: &ExternalTask,
        repository: &str,
        worker_host: &str,
    ) -> Result<ProfilePin, GuardError> {
        if task.labels.iter().any(|v| v == "execution:held") {
            return Err(GuardError::Held);
        }
        let execution: BTreeSet<_> = task
            .labels
            .iter()
            .filter(|v| v.starts_with("execution:"))
            .collect();
        if task.state != "open"
            || execution.len() != 1
            || !execution.iter().any(|v| v.as_str() == "execution:ready")
            || task
                .labels
                .iter()
                .any(|v| matches!(v.as_str(), "archive" | "status:archived" | "status:done"))
        {
            return Err(GuardError::NotReady);
        }
        let personas: BTreeSet<_> = task
            .labels
            .iter()
            .filter_map(|v| v.strip_prefix("agent:"))
            .collect();
        if personas.len() != 1 {
            return Err(GuardError::Persona);
        }
        let persona = *personas.first().ok_or(GuardError::Persona)?;
        let profile = self.profiles.get(persona).ok_or(GuardError::Persona)?;
        if !profile.enabled || !profile.current_roster || profile.allow_fallback {
            return Err(GuardError::Persona);
        }
        let pin = ProfilePin {
            repository: repository.to_string(),
            issue_id: task.id.0.clone(),
            persona: persona.to_string(),
            source_agent_id: text(&profile.source_agent_id, "source agent id")?,
            host: text(&profile.host, "host")?,
            cli: text(&profile.cli, "CLI")?,
            provider: text(&profile.provider, "provider")?,
            cli_provider: text(&profile.cli_provider, "CLI provider")?,
            model: text(&profile.model, "model")?,
            effort: text(&profile.effort, "effort")?,
            registry_digest: self.digest.clone(),
        };
        if pin.host != worker_host || !profile.repositories.iter().any(|r| r == repository) {
            return Err(GuardError::Unavailable("host or repository"));
        }
        if !matches!(pin.cli.as_str(), "codex" | "claude" | "opencode") {
            return Err(GuardError::Unavailable("CLI"));
        }
        let binding = self
            .provider_bindings
            .get(&pin.cli)
            .and_then(|p| p.get(&pin.provider));
        if binding != Some(&pin.cli_provider)
            || (pin.cli == "claude" && pin.cli_provider != "anthropic")
        {
            return Err(GuardError::Unavailable("provider binding"));
        }
        let host = self
            .capabilities
            .hosts
            .get(worker_host)
            .filter(|h| h.available)
            .ok_or(GuardError::Unavailable("host"))?;
        let cli = host
            .clis
            .get(&pin.cli)
            .filter(|c| c.available)
            .ok_or(GuardError::Unavailable("CLI"))?;
        let efforts = cli
            .models
            .get(&pin.cli_provider)
            .and_then(|m| m.get(&pin.model))
            .ok_or(GuardError::Unavailable("model"))?;
        if !efforts.contains(&pin.effort) {
            return Err(GuardError::Unavailable("effort"));
        }
        Ok(pin)
    }
}

impl ProfilePin {
    pub fn check_selection(
        &self,
        agent: Option<&str>,
        model: Option<&str>,
    ) -> Result<(), GuardError> {
        if agent != Some(self.cli.as_str()) || model != Some(self.model.as_str()) {
            return Err(GuardError::PinMismatch);
        }
        Ok(())
    }
}

pub fn configured() -> bool {
    std::env::var_os(REGISTRY_ENV).is_some()
}

pub fn load() -> anyhow::Result<Option<(Policy, String)>> {
    let Some(path) = std::env::var_os(REGISTRY_ENV) else {
        return Ok(None);
    };
    let host = std::env::var(HOST_ENV).map_err(|_| GuardError::Missing("worker host"))?;
    anyhow::ensure!(!host.trim().is_empty(), "strict worker host is empty");
    let policy = Policy::from_json(&std::fs::read_to_string(path)?)?;
    Ok(Some((policy, host)))
}

fn key(repo: &str, task_id: &str) -> String {
    // JSON tuple avoids ambiguous delimiter joins and is not a filesystem path.
    format!("strict_profile_pin:{}", serde_json::json!([repo, task_id]))
}

pub async fn stored_pin(
    store: &TaskStore,
    repo: &str,
    task_id: &str,
) -> anyhow::Result<Option<ProfilePin>> {
    store
        .kv_get(&key(repo, task_id))
        .await?
        .map(|s| serde_json::from_str(&s).map_err(Into::into))
        .transpose()
}

pub async fn persist_pin(store: &TaskStore, pin: &ProfilePin) -> anyhow::Result<()> {
    let winner = store
        .kv_insert_if_absent(
            &key(&pin.repository, &pin.issue_id),
            &serde_json::to_string(pin)?,
        )
        .await?;
    if serde_json::from_str::<ProfilePin>(&winner)? != *pin {
        return Err(GuardError::PinMismatch.into());
    }
    Ok(())
}

pub async fn task_is_strict(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
) -> anyhow::Result<bool> {
    if configured() {
        return Ok(true);
    }
    match store {
        Some(store) => Ok(stored_pin(store, repo, task_id).await?.is_some()),
        None => Ok(false),
    }
}

/// Strict proof deliberately cannot cross a live process boundary. In
/// particular, a missing pin or a removed environment option is not a fallback.
pub fn guard_spawn(
    pin: Option<&ProfilePin>,
    agent: &str,
    model: Option<&str>,
) -> anyhow::Result<()> {
    if configured() || pin.is_some() {
        let pin = pin.ok_or(GuardError::Missing("immutable profile pin"))?;
        pin.check_selection(Some(agent), model)?;
        let (_policy, host) = load()?.ok_or(GuardError::Missing("strict policy"))?;
        anyhow::ensure!(host == pin.host, "strict worker host differs from pin");
        return Err(GuardError::ProofOnly.into());
    }
    Ok(())
}

/// Direct/control/reviewer paths do not yet carry a complete persona pin.
pub fn guard_unpinned_path() -> anyhow::Result<()> {
    if configured() {
        return Err(GuardError::ProofOnly.into());
    }
    Ok(())
}

pub async fn block_no_fallback(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
) -> anyhow::Result<bool> {
    if !task_is_strict(store, repo, task_id).await? {
        return Ok(false);
    }
    crate::store::store_set_result(
        store,
        repo,
        task_id,
        &[
            (
                "last_error",
                serde_json::json!("strict profile: fallback disabled"),
            ),
            (
                "block_reason",
                serde_json::json!("strict profile: fallback disabled"),
            ),
            ("status", serde_json::json!("blocked")),
        ],
    )
    .await?;
    Ok(true)
}

#[cfg(test)]
mod tests;
