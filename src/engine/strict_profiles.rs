//! Opt-in, fail-closed named-persona routing proof.
//!
//! Enable only with ORCH_STRICT_PROFILE_REGISTRY and ORCH_STRICT_WORKER_HOST.
//! The native route and command builders are implemented; live execution remains
//! deliberately unavailable until fresh authorization and distributed leases are
//! implemented. A true worker_execution_enabled value is rejected, not ignored.

use crate::backends::{ExternalBackend, ExternalId, ExternalTask};
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
    #[error("strict profile: exactly one independent reviewer profile is required")]
    Reviewer,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<ReviewerPin>,
}

/// Separately selected identity and route; never a generic review-model fallback.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReviewerPin {
    pub persona: String,
    pub source_agent_id: String,
    pub host: String,
    pub cli: String,
    pub provider: String,
    pub cli_provider: String,
    pub model: String,
    pub effort: String,
}

impl From<ProfilePin> for ReviewerPin {
    fn from(pin: ProfilePin) -> Self {
        Self {
            persona: pin.persona,
            source_agent_id: pin.source_agent_id,
            host: pin.host,
            cli: pin.cli,
            provider: pin.provider,
            cli_provider: pin.cli_provider,
            model: pin.model,
            effort: pin.effort,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct CliCapability {
    pub available: bool,
    pub models: BTreeMap<String, BTreeMap<String, Vec<String>>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCapability {
    pub available: bool,
    pub clis: BTreeMap<String, CliCapability>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    pub hosts: BTreeMap<String, HostCapability>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub schema_version: u32,
    pub worker_execution_enabled: bool,
    pub profiles: BTreeMap<String, NamedProfile>,
    pub provider_bindings: BTreeMap<String, BTreeMap<String, String>>,
    pub capabilities: Capabilities,
    #[serde(skip)]
    pub digest: String,
}

fn control_value<'a>(label: &'a str, prefix: &str) -> Option<&'a str> {
    label
        .get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .and_then(|_| label.get(prefix.len()..))
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
        let mut pin = self.resolve_executor(task, repository, worker_host)?;
        let reviewers: BTreeSet<_> = task
            .labels
            .iter()
            .filter_map(|v| control_value(v, "reviewer:"))
            .collect();
        if !reviewers.is_empty() {
            if reviewers.len() != 1 {
                return Err(GuardError::Reviewer);
            }
            let persona = *reviewers.first().ok_or(GuardError::Reviewer)?;
            let profile = self.profiles.get(persona).ok_or(GuardError::Reviewer)?;
            let host = text(&profile.host, "reviewer host")?;
            let mut review_task = task.clone();
            review_task
                .labels
                .retain(|v| control_value(v, "agent:").is_none());
            review_task.labels.push(format!("agent:{persona}"));
            let reviewer = self.resolve_executor(&review_task, repository, &host)?;
            if reviewer.persona == pin.persona || reviewer.source_agent_id == pin.source_agent_id {
                return Err(GuardError::Reviewer);
            }
            pin.reviewer = Some(reviewer.into());
        }
        Ok(pin)
    }

    fn resolve_executor(
        &self,
        task: &ExternalTask,
        repository: &str,
        worker_host: &str,
    ) -> Result<ProfilePin, GuardError> {
        if task
            .labels
            .iter()
            .any(|v| v.eq_ignore_ascii_case("execution:held"))
        {
            return Err(GuardError::Held);
        }
        let execution: BTreeSet<_> = task
            .labels
            .iter()
            .filter(|v| control_value(v, "execution:").is_some())
            .collect();
        if task.state != "open"
            || execution.len() != 1
            || !execution
                .iter()
                .any(|v| v.eq_ignore_ascii_case("execution:ready"))
            || task.labels.iter().any(|v| {
                ["archive", "status:archived", "status:done", "no-agent"]
                    .iter()
                    .any(|blocked| v.eq_ignore_ascii_case(blocked))
            })
        {
            return Err(GuardError::NotReady);
        }
        let personas: BTreeSet<_> = task
            .labels
            .iter()
            .filter_map(|v| control_value(v, "agent:"))
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
            reviewer: None,
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
    /// Materialize a reviewer invocation pin without borrowing the executor's
    /// model, provider, effort or worker host. A different CLI is not identity.
    pub fn reviewer_pin(&self) -> Result<ProfilePin, GuardError> {
        let reviewer = self.reviewer.as_ref().ok_or(GuardError::Reviewer)?;
        if reviewer.persona == self.persona || reviewer.source_agent_id == self.source_agent_id {
            return Err(GuardError::Reviewer);
        }
        Ok(ProfilePin {
            repository: self.repository.clone(),
            issue_id: self.issue_id.clone(),
            persona: reviewer.persona.clone(),
            source_agent_id: reviewer.source_agent_id.clone(),
            host: reviewer.host.clone(),
            cli: reviewer.cli.clone(),
            provider: reviewer.provider.clone(),
            cli_provider: reviewer.cli_provider.clone(),
            model: reviewer.model.clone(),
            effort: reviewer.effort.clone(),
            registry_digest: self.registry_digest.clone(),
            reviewer: None,
        })
    }

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

/// Re-read the authoritative backend rather than treating cached labels as
/// authorization. This is a preparation check, not an atomic spawn grant: live
/// execution still requires authenticated workers and a fenced shared lease.
pub async fn recheck_authorization(
    backend: &dyn ExternalBackend,
    policy: &Policy,
    pin: &ProfilePin,
) -> anyhow::Result<()> {
    let fresh = backend.get_task(&ExternalId(pin.issue_id.clone())).await?;
    if fresh.id.0 != pin.issue_id || policy.resolve(&fresh, &pin.repository, &pin.host)? != *pin {
        return Err(GuardError::PinMismatch.into());
    }
    Ok(())
}

/// The generic conversion intentionally replaces workflow labels and assumes
/// an open task. Strict policy must preserve all cached revocations instead.
/// Only a fresh backend read can establish the current remote open/closed state.
pub fn cached_task_snapshot(task: &crate::store::Task) -> ExternalTask {
    let mut external = crate::engine::tasks::store_task_to_external(task);
    external.labels = task.labels.clone();
    if task.status == crate::store::TaskStatus::Done {
        external.state = "closed".into();
    }
    external
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
    let direct = pin_at_key(store, repo, task_id).await?;
    // The native resolver accepts internal:<row> for external tasks too. A
    // caller's alternate spelling must not hide an existing strict assignment
    // when the policy option has been removed, including at fallback/dispatch.
    if !task_id.starts_with("internal:") {
        return Ok(direct);
    }
    let Some(id) = store.resolve_task_id(repo, task_id).await? else {
        return Ok(direct);
    };
    let task = store.get(id).await?;
    let canonical = task.external_id.unwrap_or_else(|| format!("internal:{id}"));
    if canonical == task_id {
        return Ok(direct);
    }
    let resolved = pin_at_key(store, repo, &canonical).await?;
    if direct.is_some() && resolved.is_some() && direct != resolved {
        return Err(GuardError::PinMismatch.into());
    }
    // Preserve direct evidence as well: a malformed/conflicting alias cannot
    // downgrade a task just because the canonical key is absent.
    Ok(resolved.or(direct))
}

async fn pin_at_key(
    store: &TaskStore,
    repo: &str,
    task_id: &str,
) -> anyhow::Result<Option<ProfilePin>> {
    let pin: Option<ProfilePin> = store
        .kv_get(&key(repo, task_id))
        .await?
        .map(|s| serde_json::from_str(&s))
        .transpose()?;
    if pin
        .as_ref()
        .is_some_and(|pin| pin.repository != repo || pin.issue_id != task_id)
    {
        return Err(GuardError::PinMismatch.into());
    }
    Ok(pin)
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

/// Commit the immutable assignment and native route together. The KV insert
/// obtains SQLite's writer lock before reading the task, so cached revocation,
/// deletion and competing route writers cannot interleave with this commit.
/// This local transaction is deliberately not a cross-host execution lease.
pub async fn persist_route(
    store: &TaskStore,
    repo: &str,
    task_id: &str,
    route: &crate::engine::router::RouteResult,
) -> anyhow::Result<()> {
    let pin = route
        .strict_profile
        .as_ref()
        .ok_or(GuardError::Missing("immutable profile pin"))?;
    if pin.repository != repo || pin.issue_id != task_id {
        return Err(GuardError::PinMismatch.into());
    }
    pin.check_selection(Some(&route.agent), route.model.as_deref())?;
    let (policy, host) = load()?.ok_or(GuardError::Missing("strict policy"))?;
    let id = store
        .resolve_task_id(repo, task_id)
        .await?
        .ok_or(GuardError::Missing("stored task"))?;
    let mut tx = store.pool().begin().await?;
    let pin_key = key(repo, task_id);
    let inserted = sqlx::query("INSERT OR IGNORE INTO kv (key, value) VALUES (?, ?)")
        .bind(&pin_key)
        .bind(serde_json::to_string(pin)?)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 1;
    let (winner,): (String,) = sqlx::query_as("SELECT value FROM kv WHERE key = ?")
        .bind(&pin_key)
        .fetch_one(&mut *tx)
        .await?;
    if serde_json::from_str::<ProfilePin>(&winner)? != *pin {
        return Err(GuardError::PinMismatch.into());
    }
    let (external_id, agent, model, labels, status): (
        Option<String>,
        Option<String>,
        Option<String>,
        String,
        String,
    ) = sqlx::query_as(
        "SELECT external_id, agent, model, labels, status FROM tasks WHERE id = ? AND repo = ?",
    )
    .bind(id)
    .bind(repo)
    .fetch_one(&mut *tx)
    .await?;
    if !inserted {
        pin.check_selection(agent.as_deref(), model.as_deref())?;
    }
    if external_id.as_deref().unwrap_or(&format!("internal:{id}")) != task_id
        || agent
            .as_ref()
            .is_some_and(|a| !a.is_empty() && a != &pin.cli)
        || model
            .as_ref()
            .is_some_and(|m| !m.is_empty() && m != &pin.model)
    {
        return Err(GuardError::PinMismatch.into());
    }
    let cached = ExternalTask {
        id: ExternalId(task_id.into()),
        title: String::new(),
        body: String::new(),
        state: if status == "done" { "closed" } else { "open" }.into(),
        labels: serde_json::from_str(&labels)?,
        author: String::new(),
        created_at: String::new(),
        updated_at: String::new(),
        url: String::new(),
    };
    if policy.resolve(&cached, repo, &host)? != *pin {
        return Err(GuardError::PinMismatch.into());
    }
    sqlx::query(
        "UPDATE tasks SET agent = ?, model = ?, complexity = ?, estimate = ?, route_reason = ?,
         agent_profile = ?, selected_skills = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
         WHERE id = ? AND repo = ?",
    )
    .bind(&route.agent)
    .bind(&route.model)
    .bind(&route.complexity)
    .bind(route.estimate)
    .bind(&route.reason)
    .bind(serde_json::to_string(&route.profile)?)
    .bind(serde_json::to_string(&route.selected_skills)?)
    .bind(id)
    .bind(repo)
    .execute(&mut *tx)
    .await?;
    let details = serde_json::json!({
        "complexity": route.complexity, "estimate": route.estimate,
        "reason": crate::security::redact(&route.reason),
    });
    sqlx::query("INSERT INTO task_activity (task_id, event_type, agent, model, details) VALUES (?, 'dispatch', ?, ?, ?)")
        .bind(id).bind(&route.agent).bind(&route.model).bind(details.to_string())
        .execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// Review selection never enters the generic reviewer cascade. All dimensions
/// are part of the executor's immutable assignment from initial admission.
pub async fn review_pin(
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
    current: &ExternalTask,
) -> anyhow::Result<ProfilePin> {
    let route = crate::engine::router::get_route_result(store, repo, task_id).await?;
    let pin = route
        .strict_profile
        .ok_or(GuardError::Missing("immutable profile pin"))?;
    let (policy, _) = load()?.ok_or(GuardError::Missing("strict policy"))?;
    if current.id.0 != task_id || policy.resolve(current, repo, &pin.host)? != pin {
        return Err(GuardError::PinMismatch.into());
    }
    pin.reviewer_pin().map_err(Into::into)
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

async fn persist_blocked(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
    reason: &str,
) -> anyhow::Result<()> {
    let store = store.as_ref().ok_or(GuardError::Missing("task store"))?;
    let id = store
        .resolve_task_id(repo, task_id)
        .await?
        .ok_or(GuardError::Missing("stored task"))?;
    store
        .update_status_and_fields(
            id,
            crate::store::TaskStatus::Blocked,
            &[
                ("last_error", serde_json::json!(reason)),
                ("block_reason", serde_json::json!(reason)),
            ],
        )
        .await
}

/// Reject before InProgress, tmux, capture, or the generic error-to-review path.
/// Persisted pins keep this guard active when the policy environment is removed.
pub async fn block_dispatch(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
) -> anyhow::Result<bool> {
    if !task_is_strict(store, repo, task_id).await? {
        return Ok(false);
    }
    persist_blocked(store, repo, task_id, &GuardError::ProofOnly.to_string()).await?;
    Ok(true)
}

pub async fn block_no_fallback(
    store: &Option<Arc<TaskStore>>,
    repo: &str,
    task_id: &str,
) -> anyhow::Result<bool> {
    if !task_is_strict(store, repo, task_id).await? {
        return Ok(false);
    }
    persist_blocked(store, repo, task_id, "strict profile: fallback disabled").await?;
    Ok(true)
}

#[cfg(test)]
mod tests;
