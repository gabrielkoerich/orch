//! Named routing choices, using the normal task lifecycle and failure recovery.

use super::{AgentProfile, RouteResult, Router};
use crate::backends::ExternalTask;
use crate::engine::cooldown::{is_agent_degraded, is_agent_in_cooldown, is_model_in_cooldown};
use serde::{Deserialize, Serialize};

/// A local CLI selection. This is not a worker identity or a remote host binding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NamedProfile {
    pub agent: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

impl NamedProfile {
    pub fn validate(&self, agents: &[String]) -> anyhow::Result<()> {
        anyhow::ensure!(
            agents.contains(&self.agent),
            "profile agent is not configured"
        );
        anyhow::ensure!(
            !self.model.trim().is_empty(),
            "profile model must not be empty"
        );
        if let Some(effort) = &self.effort {
            anyhow::ensure!(
                !effort.trim().is_empty(),
                "profile effort must not be empty"
            );
            anyhow::ensure!(
                matches!(self.agent.as_str(), "claude" | "codex" | "opencode"),
                "profile effort is unsupported for this agent"
            );
        }
        Ok(())
    }

    /// Failover can change agent/model without rerunning the router. Never carry
    /// another selection's effort into that replacement invocation.
    pub fn matches_route(&self, agent: &str, model: Option<&str>) -> bool {
        self.agent == agent && model == Some(self.model.as_str())
    }
}

impl Router {
    pub(super) fn route_profile(&self, task: &ExternalTask) -> anyhow::Result<Option<RouteResult>> {
        if self.config.mode != "profile" {
            return Ok(None);
        }
        if let Some(err) = &self.config.profiles_error {
            anyhow::bail!("invalid profiles configuration: {err}");
        }
        let mut names = task
            .labels
            .iter()
            .filter_map(|l| l.strip_prefix("profile:"));
        let name = names
            .next()
            .ok_or_else(|| anyhow::anyhow!("profile mode requires a profile:<name> label"))?;
        anyhow::ensure!(names.next().is_none(), "task has multiple profile labels");
        let selection = self
            .config
            .profiles
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown routing profile: {name}"))?;
        selection.validate(&self.config.agents)?;
        anyhow::ensure!(
            self.available_agents.contains(&selection.agent),
            "profile CLI is not installed"
        );
        // Stored failover choices are authoritative, just as in ordinary routing.
        // The caller bypasses this selector when recovery has supplied an agent.
        if is_agent_in_cooldown(&selection.agent)
            || is_agent_degraded(&selection.agent)
            || is_model_in_cooldown(&selection.agent, &selection.model)
        {
            return Ok(None);
        }
        let mut profile = AgentProfile::default_from_config(&self.config);
        profile.execution = Some(selection.clone());
        Ok(Some(RouteResult {
            agent: selection.agent.clone(),
            model: Some(selection.model.clone()),
            complexity: super::strategies::extract_complexity_from_labels(&task.labels),
            estimate: 0,
            reason: format!("profile:{name}"),
            profile,
            selected_skills: self.config.default_skills.clone(),
            warning: None,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::ExternalId;
    use crate::engine::cooldown::{reset_global_state, set_agent_cooldown, set_model_cooldown};
    use crate::engine::router::{get_route_result, RouterConfig};
    use crate::engine::runner::agents::{get_runner, PermissionRules};
    use crate::store::{DepSource, NewDependency, NewTask, TaskStatus, TaskStore};
    use serial_test::serial;
    use std::sync::Arc;

    fn task(labels: &[&str]) -> ExternalTask {
        ExternalTask {
            id: ExternalId("2".into()),
            title: "A task".into(),
            body: String::new(),
            state: "open".into(),
            labels: labels.iter().map(|l| (*l).into()).collect(),
            author: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            url: String::new(),
        }
    }

    fn selection() -> NamedProfile {
        NamedProfile {
            agent: "codex".into(),
            model: "design-model".into(),
            effort: Some("high".into()),
        }
    }

    fn router() -> Router {
        let mut config = RouterConfig {
            mode: "profile".into(),
            agents: vec!["codex".into(), "claude".into()],
            ..Default::default()
        };
        config.profiles.insert("designer".into(), selection());
        config.profiles.insert(
            "implementer".into(),
            NamedProfile {
                model: "implementation-model".into(),
                effort: Some("medium".into()),
                ..selection()
            },
        );
        config.model_map.insert(
            "medium".into(),
            [("claude".into(), vec!["fallback-model".into()])].into(),
        );
        config
            .model_map
            .entry("medium".into())
            .or_default()
            .insert("codex".into(), vec!["fallback-codex-model".into()]);
        let mut r = Router::new(config);
        r.available_agents = vec!["codex".into(), "claude".into()];
        r
    }

    #[tokio::test]
    #[serial(cooldown_state)]
    async fn profiles_select_distinct_models_and_efforts_on_the_same_cli() {
        reset_global_state().await;
        let mut r = router();
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        for (name, model, effort) in [
            ("designer", "design-model", "high"),
            ("implementer", "implementation-model", "medium"),
        ] {
            let label = format!("profile:{name}");
            let result = r
                .route(&task(&[&label, "agent:claude"]), &store, "owner/repo")
                .await
                .unwrap();
            assert_eq!(result.agent, "codex");
            assert_eq!(result.model.as_deref(), Some(model));
            assert_eq!(
                result.profile.execution.unwrap().effort.as_deref(),
                Some(effort)
            );
        }
    }

    #[tokio::test]
    #[serial(cooldown_state)]
    async fn dependency_hold_precedes_profile_selection_and_does_not_count_as_an_attempt() {
        reset_global_state().await;
        let mut r = router();
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let id = store
            .create(&NewTask {
                external_id: Some("2".into()),
                repo: "owner/repo".into(),
                origin: "github".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        store
            .replace_dependencies(
                id,
                &[NewDependency {
                    repo: "owner/repo".into(),
                    number: 1,
                    source: DepSource::Github,
                }],
            )
            .await
            .unwrap();
        // Even an unknown profile must wait on its dependency first.
        let err = r
            .route(&task(&["profile:unknown"]), &store, "owner/repo")
            .await
            .unwrap_err();
        assert!(err
            .downcast_ref::<crate::store::WaitingOnDependency>()
            .is_some());
        let row = store.get(id).await.unwrap();
        assert_eq!(row.status, TaskStatus::Waiting);
        assert_eq!(row.route_attempts, 0);
        assert!(row.agent.is_none());
    }

    #[tokio::test]
    #[serial(cooldown_state)]
    async fn cooled_profile_uses_ordinary_fallback_routing() {
        reset_global_state().await;
        let mut r = router();
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        set_model_cooldown("codex", "design-model", 60, "test").await;
        let result = r
            .route(&task(&["profile:designer"]), &store, "owner/repo")
            .await
            .unwrap();
        assert!(result.model.is_some());
        assert_ne!(result.model.as_deref(), Some("design-model"));
        assert!(result.profile.execution.is_none());
        reset_global_state().await;
    }

    #[tokio::test]
    #[serial(cooldown_state)]
    async fn cleared_failover_route_can_leave_the_original_profile_agent() {
        reset_global_state().await;
        let mut r = router();
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let id = store
            .create(&NewTask {
                external_id: Some("2".into()),
                repo: "owner/repo".into(),
                origin: "github".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let result = r
            .route(&task(&["profile:designer"]), &store, "owner/repo")
            .await
            .unwrap();
        r.store_route_result("2", &result, &store, "owner/repo")
            .await
            .unwrap();
        // Generic failover clears both fields and cools the failed executor.
        store
            .set_fields(
                id,
                &[
                    ("agent", serde_json::json!("")),
                    ("model", serde_json::json!("")),
                ],
            )
            .await
            .unwrap();
        set_agent_cooldown("codex", 60).await;
        let result = r
            .route(
                &task(&["profile:designer", "agent:codex"]),
                &store,
                "owner/repo",
            )
            .await
            .unwrap();
        assert_eq!(result.agent, "claude");
        assert_eq!(result.model.as_deref(), Some("fallback-model"));
        assert!(result.profile.execution.is_none());
        reset_global_state().await;
    }

    #[test]
    fn invalid_or_ambiguous_profiles_do_not_silently_route_to_another_agent() {
        let mut r = router();
        for labels in [
            vec![],
            vec!["profile:unknown"],
            vec!["profile:designer", "profile:implementer"],
        ] {
            assert!(r.route_profile(&task(&labels)).is_err());
        }
        r.config.profiles_error = Some("invalid YAML shape".into());
        assert!(r.route_profile(&task(&["profile:designer"])).is_err());
        r.config.profiles_error = None;
        r.config.profiles.get_mut("designer").unwrap().model.clear();
        assert!(r.route_profile(&task(&["profile:designer"])).is_err());
        r.config.profiles.insert("designer".into(), selection());
        r.available_agents.clear();
        assert!(r.route_profile(&task(&["profile:designer"])).is_err());
    }

    #[test]
    fn profile_names_do_not_change_agent_label_semantics_or_ordinary_mode() {
        let mut r = router();
        r.config.mode = "round_robin".into();
        assert!(r
            .route_profile(&task(&["profile:unknown"]))
            .unwrap()
            .is_none());
        assert_eq!(
            super::super::strategies::extract_agent_from_labels(
                &r.config.agents,
                &["agent:codex".into()]
            ),
            Some("codex".into())
        );
        assert_eq!(
            super::super::strategies::extract_agent_from_labels(
                &r.config.agents,
                &["agent:designer".into()]
            ),
            None
        );
    }

    #[tokio::test]
    #[serial(cooldown_state)]
    async fn stored_failover_agent_remains_authoritative_over_the_profile_label() {
        reset_global_state().await;
        let mut r = router();
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let id = store
            .create(&NewTask {
                external_id: Some("2".into()),
                repo: "owner/repo".into(),
                origin: "github".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        store
            .set_fields(id, &[("agent", serde_json::json!("claude"))])
            .await
            .unwrap();
        let result = r
            .route(&task(&["profile:designer"]), &store, "owner/repo")
            .await
            .unwrap();
        assert_eq!(result.agent, "claude");
        assert_eq!(result.model.as_deref(), Some("fallback-model"));
        assert!(result.profile.execution.is_none());
    }

    #[tokio::test]
    #[serial(cooldown_state, orch_home)]
    async fn restart_retains_profile_choice_but_failover_drops_its_effort() {
        reset_global_state().await;
        let home = tempfile::tempdir().unwrap();
        // A conflicting model_map must not replace a persisted profile model.
        std::fs::write(
            home.path().join("config.yml"),
            "model_map:\n  medium:\n    codex: fallback-model\n",
        )
        .unwrap();
        let previous = std::env::var_os("ORCH_HOME");
        std::env::set_var("ORCH_HOME", home.path());
        let db = home.path().join("tasks.db");
        let store = Arc::new(TaskStore::open(&db).await.unwrap());
        let id = store
            .create(&NewTask {
                external_id: Some("2".into()),
                repo: "owner/repo".into(),
                origin: "github".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let mut r = router();
        let result = r
            .route(&task(&["profile:designer"]), &store, "owner/repo")
            .await
            .unwrap();
        r.store_route_result("2", &result, &store, "owner/repo")
            .await
            .unwrap();
        drop(store);
        let store = Arc::new(TaskStore::open(&db).await.unwrap());
        let restored = get_route_result(&store, "owner/repo", "2").await.unwrap();
        assert_eq!(restored.model, result.model);
        assert_eq!(restored.profile.execution, result.profile.execution);
        // This is the write performed by ordinary same-agent model recovery.
        store
            .set_fields(id, &[("model", serde_json::json!("fallback-model"))])
            .await
            .unwrap();
        let recovered = get_route_result(&store, "owner/repo", "2").await.unwrap();
        assert_eq!(recovered.model.as_deref(), Some("fallback-model"));
        assert!(recovered.profile.execution.is_none());
        match previous {
            Some(v) => std::env::set_var("ORCH_HOME", v),
            None => std::env::remove_var("ORCH_HOME"),
        }
    }

    #[test]
    fn profile_yaml_is_typed_and_rejects_worker_identity_fields() {
        let parsed: std::collections::HashMap<String, NamedProfile> = serde_norway::from_str(
            "designer:\n  agent: codex\n  model: design-model\n  effort: high\n",
        )
        .unwrap();
        assert_eq!(parsed["designer"], selection());
        assert!(
            serde_norway::from_str::<NamedProfile>("agent: codex\nmodel: m\nhost: remote\n")
                .is_err()
        );
        for agent in ["kimi", "unknown"] {
            let p = NamedProfile {
                agent: agent.into(),
                ..selection()
            };
            assert!(p.validate(&[agent.into()]).is_err());
        }
    }

    #[test]
    #[serial(orch_home)]
    fn real_config_loader_accepts_profile_mode_and_reports_malformed_profiles() {
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("ORCH_HOME");
        std::env::set_var("ORCH_HOME", home.path());
        let config_path = home.path().join("config.yml");
        std::fs::write(&config_path, "agents: [codex]\nrouter:\n  mode: profile\nprofiles:\n  designer:\n    agent: codex\n    model: design-model\n    effort: high\n").unwrap();
        crate::config::clear_test_cache();
        let config = RouterConfig::from_config();
        assert_eq!(config.mode, "profile");
        assert_eq!(config.profiles["designer"], selection());
        assert!(config.profiles_error.is_none());
        std::fs::write(
            &config_path,
            "agents: [codex]\nrouter:\n  mode: profile\nprofiles: [invalid]\n",
        )
        .unwrap();
        crate::config::clear_test_cache();
        let config = RouterConfig::from_config();
        assert!(config.profiles_error.is_some());
        match previous {
            Some(v) => std::env::set_var("ORCH_HOME", v),
            None => std::env::remove_var("ORCH_HOME"),
        }
        crate::config::clear_test_cache();
    }

    #[cfg(unix)]
    fn recorded_args(agent: &str, effort: Option<&str>) -> Vec<String> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join(agent);
        std::fs::write(&stub, "#!/bin/sh\nprintf '%s\\0' \"$@\"\n").unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let sys = dir.path().join("sys");
        let msg = dir.path().join("msg");
        std::fs::write(&sys, "system").unwrap();
        std::fs::write(&msg, "message").unwrap();
        let command = get_runner(agent).build_command_with_effort(
            Some("test-model"),
            effort,
            "",
            sys.to_str().unwrap(),
            msg.to_str().unwrap(),
            &PermissionRules::default(),
        );
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(command)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    dir.path().display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
            .stdout
            .split(|b| *b == 0)
            .filter(|b| !b.is_empty())
            .map(|b| String::from_utf8(b.to_vec()).unwrap())
            .collect()
    }

    #[test]
    #[cfg(unix)]
    fn native_effort_flags_reach_cli_argv_without_python_or_provider_calls() {
        for agent in ["codex", "claude", "opencode"] {
            let args = recorded_args(agent, Some("high"));
            let expected = if agent == "codex" {
                "model_reasoning_effort=\"high\""
            } else {
                "high"
            };
            assert!(args.iter().any(|a| a == expected), "{agent}: {args:?}");
            if agent == "opencode" {
                assert!(args.iter().any(|a| a == "--variant"));
            }
            if agent == "claude" {
                assert!(args.iter().any(|a| a == "--effort"));
            }
            assert!(!recorded_args(agent, None).iter().any(|a| a == expected));
            let hostile = "high' ; exit 99; #";
            let args = recorded_args(agent, Some(hostile));
            let expected = if agent == "codex" {
                format!("model_reasoning_effort={}", serde_json::json!(hostile))
            } else {
                hostile.into()
            };
            assert!(args.contains(&expected), "{agent}: {args:?}");
        }
    }
}
