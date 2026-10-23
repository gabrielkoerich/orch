use super::*;
use crate::backends::ExternalId;
use crate::engine::router::{get_route_result, Router, RouterConfig};
use crate::engine::runner::agents::{get_runner, PermissionRules};
use serde_json::{json, Value};
use serial_test::serial;

const REPO: &str = "example/project";
const HOST: &str = "worker-a";

fn fixture() -> Value {
    let mut profiles = serde_json::Map::new();
    let mut clis = serde_json::Map::new();
    for (persona, cli, provider, effort) in [
        ("designer", "codex", "openai", "max"),
        ("implementer", "codex", "openai", "medium"),
        ("reviewer", "claude", "anthropic", "high"),
        ("researcher", "opencode", "test-provider", "max"),
    ] {
        profiles.insert(
            persona.into(),
            json!({
                "enabled": true, "current_roster": true, "source_agent_id": persona,
                "host": HOST, "cli": cli, "provider": provider, "cli_provider": provider,
                "model": "model-one", "effort": effort, "allow_fallback": false,
                "repositories": [REPO]
            }),
        );
        clis.insert(
            cli.into(),
            json!({
                "available": true, "models": {provider: {"model-one": ["max", "medium", "high"]}}
            }),
        );
    }
    json!({
        "schema_version": 1, "worker_execution_enabled": false,
        "profiles": profiles,
        "provider_bindings": {
            "codex": {"openai": "openai"}, "claude": {"anthropic": "anthropic"},
            "opencode": {"test-provider": "test-provider"}
        },
        "capabilities": {"hosts": {HOST: {"available": true, "clis": clis}}}
    })
}

fn policy(value: &Value) -> Policy {
    Policy::from_json(&value.to_string()).unwrap()
}

fn task(persona: &str) -> ExternalTask {
    ExternalTask {
        id: ExternalId("1".into()),
        title: "Synthetic task".into(),
        body: String::new(),
        state: "open".into(),
        labels: vec!["execution:ready".into(), format!("agent:{persona}")],
        author: String::new(),
        created_at: String::new(),
        updated_at: String::new(),
        url: String::new(),
    }
}

#[test]
fn explicit_ready_and_held_precedence() {
    let p = policy(&fixture());
    let mut t = task("designer");
    assert!(p.resolve(&t, REPO, HOST).is_ok());
    t.labels.push("execution:held".into());
    assert_eq!(p.resolve(&t, REPO, HOST), Err(GuardError::Held));
    t.labels = vec!["agent:designer".into()];
    assert_eq!(p.resolve(&t, REPO, HOST), Err(GuardError::NotReady));
    t.labels.push("execution:ready".into());
    t.labels.push("execution:unknown".into());
    assert_eq!(p.resolve(&t, REPO, HOST), Err(GuardError::NotReady));
    t = task("designer");
    t.state = "closed".into();
    assert_eq!(p.resolve(&t, REPO, HOST), Err(GuardError::NotReady));
}

#[test]
fn archived_done_and_multiple_personas_block() {
    let p = policy(&fixture());
    for label in ["archive", "status:archived", "status:done"] {
        let mut t = task("designer");
        t.labels.push(label.into());
        assert_eq!(p.resolve(&t, REPO, HOST), Err(GuardError::NotReady));
    }
    let mut t = task("designer");
    t.labels.push("agent:implementer".into());
    assert_eq!(p.resolve(&t, REPO, HOST), Err(GuardError::Persona));
}

#[test]
fn missing_dimensions_and_unknown_persona_do_not_default() {
    for field in [
        "source_agent_id",
        "host",
        "cli",
        "provider",
        "cli_provider",
        "model",
        "effort",
    ] {
        let mut value = fixture();
        value["profiles"]["designer"][field] = Value::Null;
        assert!(matches!(
            policy(&value).resolve(&task("designer"), REPO, HOST),
            Err(GuardError::Missing(_))
        ));
    }
    assert_eq!(
        policy(&fixture()).resolve(&task("unknown-persona"), REPO, HOST),
        Err(GuardError::Persona)
    );
    let mut value = fixture();
    value["profiles"]["designer"]["model"] = json!("unresolved-alias");
    assert_eq!(
        policy(&value).resolve(&task("designer"), REPO, HOST),
        Err(GuardError::Unavailable("model"))
    );
}

#[test]
fn disabled_historical_and_fallback_profiles_block() {
    for (field, v) in [
        ("enabled", false),
        ("current_roster", false),
        ("allow_fallback", true),
    ] {
        let mut value = fixture();
        value["profiles"]["designer"][field] = json!(v);
        assert_eq!(
            policy(&value).resolve(&task("designer"), REPO, HOST),
            Err(GuardError::Persona)
        );
    }
}

#[test]
fn host_repository_provider_and_capability_must_match() {
    let t = task("designer");
    let p = policy(&fixture());
    assert!(p.resolve(&t, REPO, "worker-b").is_err());
    assert!(p.resolve(&t, "example/other", HOST).is_err());
    for pointer in [
        "/capabilities/hosts/worker-a/available",
        "/capabilities/hosts/worker-a/clis/codex/available",
    ] {
        let mut v = fixture();
        *v.pointer_mut(pointer).unwrap() = json!(false);
        assert!(policy(&v).resolve(&t, REPO, HOST).is_err());
    }
    let mut v = fixture();
    v["provider_bindings"]["codex"]["openai"] = json!("different");
    assert_eq!(
        policy(&v).resolve(&t, REPO, HOST),
        Err(GuardError::Unavailable("provider binding"))
    );
    let mut v = fixture();
    v["profiles"]["designer"]["effort"] = json!("unavailable");
    assert_eq!(
        policy(&v).resolve(&t, REPO, HOST),
        Err(GuardError::Unavailable("effort"))
    );
}

#[test]
fn execution_enable_cannot_remove_proof_gate() {
    let mut v = fixture();
    v["worker_execution_enabled"] = json!(true);
    assert!(Policy::from_json(&v.to_string())
        .unwrap_err()
        .to_string()
        .contains("live execution is disabled"));
}

/// Restores process-global configuration on unwind as well as normal return.
/// All tests touching it use the same serial_test lock as other environment tests.
struct TestEnv {
    _dir: tempfile::TempDir,
    old: Vec<(&'static str, Option<std::ffi::OsString>)>,
}
impl TestEnv {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("policy.json");
        std::fs::write(&path, fixture().to_string()).unwrap();
        let old = [REGISTRY_ENV, HOST_ENV, "PATH"]
            .into_iter()
            .map(|k| (k, std::env::var_os(k)))
            .collect();
        std::env::set_var(REGISTRY_ENV, path);
        std::env::set_var(HOST_ENV, HOST);
        // Router discovery sees no production CLI. No login/config is copied.
        std::env::set_var("PATH", dir.path());
        Self { _dir: dir, old }
    }
}
impl Drop for TestEnv {
    fn drop(&mut self) {
        for (k, value) in &self.old {
            match value {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
}

async fn routed() -> (Arc<TaskStore>, Router, ExternalTask, ProfilePin) {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let t = task("designer");
    store.ensure_external_task(REPO, &t).await.unwrap();
    let mut router = Router::new(RouterConfig::default());
    router.available_agents = vec!["codex".into()];
    let result = router.route(&t, &store, REPO).await.unwrap();
    let pin = result.strict_profile.clone().unwrap();
    router
        .store_route_result(&t.id.0, &result, &store, REPO)
        .await
        .unwrap();
    (store, router, t, pin)
}

#[tokio::test]
#[serial]
async fn native_router_roundtrip_preserves_pin_and_model() {
    let _env = TestEnv::new();
    let (store, _, _, pin) = routed().await;
    let result = get_route_result(&store, REPO, "1").await.unwrap();
    assert_eq!(result.strict_profile, Some(pin));
    assert_eq!(result.model.as_deref(), Some("model-one"));
}

#[tokio::test]
#[serial]
async fn stale_store_agent_cannot_override_persona() {
    let _env = TestEnv::new();
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let t = task("designer");
    let id = store.ensure_external_task(REPO, &t).await.unwrap();
    store
        .set_fields(
            id,
            &[("agent", json!("claude")), ("model", json!("other-model"))],
        )
        .await
        .unwrap();
    let mut router = Router::new(RouterConfig::default());
    router.available_agents = vec!["codex".into()];
    assert!(router
        .route(&t, &store, REPO)
        .await
        .unwrap_err()
        .to_string()
        .contains("immutable pin"));
}

#[tokio::test]
#[serial]
async fn persisted_pin_cannot_change_or_be_downgraded() {
    let _env = TestEnv::new();
    let (store, mut router, t, pin) = routed().await;
    let mut other = pin.clone();
    other.effort = "medium".into();
    assert!(persist_pin(&store, &other).await.is_err());
    std::env::remove_var(REGISTRY_ENV);
    assert!(router.route(&t, &store, REPO).await.is_err());
    assert!(get_route_result(&store, REPO, "1").await.is_err());
    assert!(task_is_strict(&Some(store), REPO, "1").await.unwrap());
    assert!(guard_spawn(Some(&pin), "codex", Some("model-one")).is_err());
}

#[tokio::test]
#[serial]
async fn stored_agent_or_model_tampering_blocks_dispatch_lookup() {
    let _env = TestEnv::new();
    let (store, _, _, _) = routed().await;
    let id = store.resolve_task_id(REPO, "1").await.unwrap().unwrap();
    store
        .set_fields(id, &[("model", json!("other-model"))])
        .await
        .unwrap();
    assert!(get_route_result(&store, REPO, "1").await.is_err());
    store
        .set_fields(
            id,
            &[("model", json!("model-one")), ("agent", json!("claude"))],
        )
        .await
        .unwrap();
    assert!(get_route_result(&store, REPO, "1").await.is_err());
}

#[tokio::test]
#[serial]
async fn native_task_init_missing_agent_blocks_before_worktree() {
    let _env = TestEnv::new();
    let (store, _, _, _) = routed().await;
    let project = tempfile::tempdir().unwrap();
    let absent = project.path().join("not-created");
    let error = crate::engine::runner::task_init::prepare_task(
        "1",
        None,
        None,
        None,
        REPO,
        &absent,
        0,
        &Some(store),
    )
    .await
    .err()
    .unwrap();
    assert!(error.to_string().contains("immutable pin"));
    assert!(!absent.exists());
}

#[tokio::test]
#[serial]
async fn native_task_init_complete_pin_remains_proof_only() {
    let _env = TestEnv::new();
    let (store, _, t, _) = routed().await;
    let backend = SnapshotBackend::new(t);
    let project = tempfile::tempdir().unwrap();
    let error = crate::engine::runner::task_init::prepare_task(
        "1",
        Some("codex"),
        Some("model-one"),
        Some(&backend),
        REPO,
        project.path(),
        0,
        &Some(store),
    )
    .await
    .err()
    .unwrap();
    assert!(error.to_string().contains("live execution is disabled"));
    assert_eq!(std::fs::read_dir(project.path()).unwrap().count(), 0);
}

#[tokio::test]
#[serial]
async fn native_failover_cannot_change_persisted_route() {
    let _env = TestEnv::new();
    let (store, _, _, pin) = routed().await;
    let status = crate::engine::runner::response::handle_failover(
        "1",
        "codex",
        crate::engine::runner::response::RetryableError::ModelUnavailable,
        "synthetic error",
        false,
        &Some(store.clone()),
        REPO,
    )
    .await;
    assert_eq!(status, "blocked");
    let blocked = store.get_by_external_id(REPO, "1").await.unwrap().unwrap();
    assert_eq!(blocked.status, crate::store::TaskStatus::Blocked);
    assert_eq!(
        blocked.block_reason.as_deref(),
        Some("strict profile: fallback disabled")
    );
    assert_eq!(stored_pin(&store, REPO, "1").await.unwrap(), Some(pin));
    assert_eq!(
        store
            .get_by_external_id(REPO, "1")
            .await
            .unwrap()
            .unwrap()
            .agent
            .as_deref(),
        Some("codex")
    );
}

#[test]
#[serial]
fn process_guards_reject_even_complete_profile() {
    let _env = TestEnv::new();
    let pin = policy(&fixture())
        .resolve(&task("designer"), REPO, HOST)
        .unwrap();
    assert!(guard_spawn(Some(&pin), "codex", Some("model-one")).is_err());
    assert!(guard_spawn(None, "claude", None).is_err());
    assert!(guard_unpinned_path().is_err());
}

#[test]
#[serial]
fn default_spawn_guards_unchanged_without_opt_in() {
    let _env = TestEnv::new();
    std::env::remove_var(REGISTRY_ENV);
    assert!(guard_spawn(None, "claude", None).is_ok());
    assert!(guard_unpinned_path().is_ok());
}

#[cfg(unix)]
fn record_command(pin: &ProfilePin) -> Vec<String> {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let stub = dir.path().join(&pin.cli);
    std::fs::write(
        &stub,
        "#!/usr/bin/python3\nimport json,sys\nsys.stdin.read()\nprint(json.dumps(sys.argv[1:]))\n",
    )
    .unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).unwrap();
    let sys = dir.path().join("system.txt");
    let msg = dir.path().join("message.txt");
    std::fs::write(&sys, "system").unwrap();
    std::fs::write(&msg, "message").unwrap();
    let command = get_runner(&pin.cli)
        .build_profile_command(
            pin,
            "",
            sys.to_str().unwrap(),
            msg.to_str().unwrap(),
            &PermissionRules::default(),
        )
        .unwrap();
    let output = std::process::Command::new("/bin/bash")
        .arg("-c")
        .arg(command)
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", dir.path().display()))
        .env("HOME", dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
#[cfg(unix)]
fn native_codex_builder_records_distinct_max_and_medium_argv() {
    let p = policy(&fixture());
    for (persona, effort) in [("designer", "max"), ("implementer", "medium")] {
        let pin = p.resolve(&task(persona), REPO, HOST).unwrap();
        let argv = record_command(&pin);
        assert!(argv.windows(2).any(|v| v == ["--model", "model-one"]));
        let expected = format!("model_reasoning_effort=\"{effort}\"");
        assert!(argv.windows(2).any(|v| v == ["-c", &expected]));
        assert!(argv
            .windows(2)
            .any(|v| v == ["-c", "model_provider=\"openai\""]));
        let exec = argv.iter().position(|a| a == "exec").unwrap();
        assert!(argv.iter().position(|a| a == &expected).unwrap() > exec);
    }
}

#[test]
#[cfg(unix)]
fn native_claude_builder_records_effort() {
    let pin = policy(&fixture())
        .resolve(&task("reviewer"), REPO, HOST)
        .unwrap();
    let argv = record_command(&pin);
    assert!(argv.windows(2).any(|v| v == ["--model", "model-one"]));
    assert!(argv.windows(2).any(|v| v == ["--effort", "high"]));
}

#[test]
#[cfg(unix)]
fn native_opencode_builder_records_provider_model_and_variant() {
    let pin = policy(&fixture())
        .resolve(&task("researcher"), REPO, HOST)
        .unwrap();
    let argv = record_command(&pin);
    assert!(argv
        .windows(2)
        .any(|v| v == ["--model", "test-provider/model-one"]));
    assert!(argv.windows(2).any(|v| v == ["--variant", "max"]));
}

#[test]
fn unsupported_backend_and_wrapper_are_rejected() {
    let pin = policy(&fixture())
        .resolve(&task("reviewer"), REPO, HOST)
        .unwrap();
    for cli in ["grok", "kimi", "minimax", "unknown"] {
        assert!(get_runner(cli)
            .build_profile_command(&pin, "", "sys", "msg", &PermissionRules::default())
            .is_err());
    }
}

#[tokio::test]
#[serial]
async fn strict_daemon_entry_is_disabled_before_service_side_effects() {
    let _env = TestEnv::new();
    let error = crate::engine::serve().await.unwrap_err();
    assert!(error.to_string().contains("live execution is disabled"));
}

#[tokio::test]
#[serial]
async fn raw_command_boundary_does_not_start_process() {
    let _env = TestEnv::new();
    let dir = tempfile::tempdir().unwrap();
    let sentinel = dir.path().join("must-not-exist");
    let mut cmd = tokio::process::Command::new("/usr/bin/touch");
    cmd.arg(&sentinel);
    let result = crate::engine::runner::direct::run_direct_command_raw(
        &mut cmd,
        std::time::Duration::from_secs(1),
    )
    .await;
    assert!(result.is_err());
    assert!(!sentinel.exists());
}

#[tokio::test]
#[serial]
async fn native_task_init_unrouted_task_never_defaults_to_claude() {
    let _env = TestEnv::new();
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    store
        .ensure_external_task(REPO, &task("designer"))
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let error = crate::engine::runner::task_init::prepare_task(
        "1",
        None,
        None,
        None,
        REPO,
        dir.path(),
        0,
        &Some(store),
    )
    .await
    .err()
    .unwrap();
    assert!(error.to_string().contains("missing immutable pin"));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
#[cfg(unix)]
fn native_builders_preserve_shell_metacharacters_as_literal_arguments() {
    let p = policy(&fixture());
    for persona in ["designer", "reviewer", "researcher"] {
        let mut pin = p.resolve(&task(persona), REPO, HOST).unwrap();
        pin.model = "model-with-'quote-and-$(false)".into();
        pin.effort = "variant@literal-'quote-$(false)".into();
        let argv = record_command(&pin);
        let expected_model = if pin.cli == "opencode" {
            format!("{}/{}", pin.cli_provider, pin.model)
        } else {
            pin.model.clone()
        };
        assert!(argv.windows(2).any(|v| v == ["--model", &expected_model]));
        let expected_effort = if pin.cli == "codex" {
            format!("model_reasoning_effort={}", json!(pin.effort))
        } else {
            pin.effort.clone()
        };
        assert!(argv.contains(&expected_effort));
    }
}

/// Reads a supplied snapshot and records native status mirror calls; every
/// other mutation fails the test. No provider inference, host auth or lease.
struct SnapshotBackend {
    task: ExternalTask,
    unavailable: bool,
    on_status: Option<Arc<TaskStore>>,
    updates: std::sync::Mutex<Vec<crate::backends::Status>>,
    reads: std::sync::atomic::AtomicUsize,
}
impl SnapshotBackend {
    fn new(task: ExternalTask) -> Self {
        Self {
            task,
            unavailable: false,
            on_status: None,
            updates: std::sync::Mutex::new(Vec::new()),
            reads: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}
#[async_trait::async_trait]
impl ExternalBackend for SnapshotBackend {
    fn name(&self) -> &str {
        "synthetic-snapshot"
    }
    async fn get_task(&self, _id: &ExternalId) -> anyhow::Result<ExternalTask> {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        anyhow::ensure!(!self.unavailable, "synthetic backend unavailable");
        Ok(self.task.clone())
    }
    async fn create_task(&self, _: &str, _: &str, _: &[String]) -> anyhow::Result<ExternalId> {
        panic!("unexpected mutation")
    }
    async fn list_by_status(
        &self,
        _: crate::backends::Status,
    ) -> anyhow::Result<Vec<ExternalTask>> {
        panic!("unexpected list")
    }
    async fn post_comment(&self, _: &ExternalId, _: &str) -> anyhow::Result<()> {
        panic!("unexpected mutation")
    }
    async fn set_labels(&self, _: &ExternalId, _: &[String]) -> anyhow::Result<()> {
        panic!("unexpected mutation")
    }
    async fn remove_label(&self, _: &ExternalId, _: &str) -> anyhow::Result<()> {
        panic!("unexpected mutation")
    }
    async fn get_sub_issues(&self, _: &ExternalId) -> anyhow::Result<Vec<ExternalId>> {
        panic!("unexpected list")
    }
    async fn health_check(&self) -> anyhow::Result<()> {
        panic!("unexpected health check")
    }
    async fn update_status(
        &self,
        id: &ExternalId,
        status: crate::backends::Status,
    ) -> anyhow::Result<()> {
        assert!(matches!(
            status,
            crate::backends::Status::Routed | crate::backends::Status::Blocked
        ));
        self.updates.lock().unwrap().push(status);
        if let Some(store) = &self.on_status {
            let row = store.resolve_task_id(REPO, &id.0).await?.unwrap();
            let mut labels = self.task.labels.clone();
            labels.extend([
                "execution:held".into(),
                "status:archived".into(),
                "concurrent-marker".into(),
            ]);
            store.set_fields(row, &[("labels", json!(labels))]).await?;
        }
        Ok(())
    }
}

#[test]
fn github_control_labels_cannot_bypass_revocation_by_case() {
    let p = policy(&fixture());
    for label in [
        "Execution:Held",
        "STATUS:ARCHIVED",
        "Status:Done",
        "NO-AGENT",
    ] {
        let mut t = task("designer");
        t.labels.push(label.into());
        assert!(p.resolve(&t, REPO, HOST).is_err(), "{label}");
    }
    let mut t = task("designer");
    t.labels.push("AGENT:implementer".into());
    assert_eq!(p.resolve(&t, REPO, HOST), Err(GuardError::Persona));
    t.labels = vec!["Execution:Ready".into(), "Agent:designer".into()];
    assert!(p.resolve(&t, REPO, HOST).is_ok());
    t.labels.push("EXECUTION:unknown".into());
    assert_eq!(p.resolve(&t, REPO, HOST), Err(GuardError::NotReady));
}

#[test]
fn policy_rejects_unknown_fields_instead_of_ignoring_security_typos() {
    for pointer in [
        "",
        "/profiles/designer",
        "/capabilities",
        "/capabilities/hosts/worker-a",
        "/capabilities/hosts/worker-a/clis/codex",
    ] {
        let mut v = fixture();
        v.pointer_mut(pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("unexpected_policy_setting".into(), json!(true));
        assert!(Policy::from_json(&v.to_string()).is_err(), "{pointer}");
    }
}

#[test]
fn reviewer_requires_distinct_persona_and_source_identity_without_backend_substitution() {
    let mut t = task("designer");
    t.labels.push("reviewer:implementer".into());
    let mut v = fixture();
    let pin = policy(&v).resolve(&t, REPO, HOST).unwrap();
    let reviewer = pin.reviewer_pin().unwrap();
    assert_eq!(reviewer.cli, "codex"); // Independent identities may use the same CLI.
    assert_eq!(reviewer.effort, "medium");
    assert_eq!(pin.effort, "max");
    assert_ne!(reviewer.source_agent_id, pin.source_agent_id);
    v["profiles"]["implementer"]["source_agent_id"] = json!("designer");
    assert_eq!(
        policy(&v).resolve(&t, REPO, HOST),
        Err(GuardError::Reviewer)
    );
    for label in ["reviewer:designer", "reviewer:unknown"] {
        t.labels = vec![
            "execution:ready".into(),
            "agent:designer".into(),
            label.into(),
        ];
        assert!(policy(&fixture()).resolve(&t, REPO, HOST).is_err());
    }
    t.labels.push("Reviewer:researcher".into());
    assert_eq!(
        policy(&fixture()).resolve(&t, REPO, HOST),
        Err(GuardError::Reviewer)
    );
    assert_eq!(
        policy(&fixture())
            .resolve(&task("designer"), REPO, HOST)
            .unwrap()
            .reviewer_pin(),
        Err(GuardError::Reviewer)
    );
}

#[tokio::test]
#[serial]
async fn reviewer_is_immutable_through_native_route_lookup_and_retry() {
    let _env = TestEnv::new();
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mut t = task("designer");
    t.labels.push("reviewer:reviewer".into());
    let id = store.ensure_external_task(REPO, &t).await.unwrap();
    let mut router = Router::new(RouterConfig::default());
    router.available_agents = vec!["codex".into()];
    let route = router.route(&t, &store, REPO).await.unwrap();
    router
        .store_route_result("1", &route, &store, REPO)
        .await
        .unwrap();
    let pin = route.strict_profile.unwrap();
    let reviewer = review_pin(&store, REPO, "1", &t).await.unwrap();
    assert_eq!(reviewer.cli, "claude");
    assert_eq!(reviewer.effort, "high");
    assert_eq!(reviewer.source_agent_id, "reviewer");
    assert!(block_no_fallback(&Some(store.clone()), REPO, "1")
        .await
        .unwrap());
    assert_eq!(review_pin(&store, REPO, "1", &t).await.unwrap(), reviewer);
    // A caller cannot hand off review to an unpinned identity, even when all
    // alternative capabilities are declared available in the synthetic policy.
    let mut changed = t.clone();
    changed.labels.retain(|v| !v.starts_with("reviewer:"));
    changed.labels.push("reviewer:implementer".into());
    assert!(review_pin(&store, REPO, "1", &changed).await.is_err());
    store
        .set_fields(id, &[("labels", json!(changed.labels))])
        .await
        .unwrap();
    assert!(get_route_result(&store, REPO, "1").await.is_err());
    assert_eq!(stored_pin(&store, REPO, "1").await.unwrap(), Some(pin));
}

#[test]
#[cfg(unix)]
fn independently_pinned_reviewer_uses_its_own_native_command_dimensions() {
    let mut t = task("designer");
    t.labels.push("reviewer:implementer".into());
    let p = policy(&fixture());
    let executor = p.resolve(&t, REPO, HOST).unwrap();
    let reviewer = executor.reviewer_pin().unwrap();
    let argv = record_command(&reviewer);
    assert!(argv.contains(&"model_reasoning_effort=\"medium\"".into()));
    assert!(!argv.contains(&"model_reasoning_effort=\"max\"".into()));
}

#[tokio::test]
async fn backend_authorization_is_fetched_and_revocations_and_errors_fail_closed() {
    let p = policy(&fixture());
    let t = task("designer");
    let pin = p.resolve(&t, REPO, HOST).unwrap();
    let good = SnapshotBackend::new(t.clone());
    recheck_authorization(&good, &p, &pin).await.unwrap();
    assert_eq!(good.reads.load(std::sync::atomic::Ordering::SeqCst), 1);
    for revocation in [
        "held",
        "closed",
        "archived",
        "wrong-issue",
        "persona",
        "unavailable",
    ] {
        let mut backend = SnapshotBackend::new(t.clone());
        match revocation {
            "held" => backend.task.labels.push("execution:held".into()),
            "closed" => backend.task.state = "closed".into(),
            "archived" => backend.task.labels.push("status:archived".into()),
            "wrong-issue" => backend.task.id.0 = "2".into(),
            "persona" => backend.task.labels = task("implementer").labels,
            "unavailable" => backend.unavailable = true,
            _ => unreachable!(),
        }
        assert!(
            recheck_authorization(&backend, &p, &pin).await.is_err(),
            "{revocation}"
        );
        assert_eq!(backend.reads.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}

#[tokio::test]
#[serial]
async fn cached_archive_is_preserved_during_lookup_and_readmission() {
    let _env = TestEnv::new();
    let (store, mut router, mut t, pin) = routed().await;
    t.labels.push("status:archived".into());
    let id = store.resolve_task_id(REPO, "1").await.unwrap().unwrap();
    store
        .set_fields(id, &[("labels", json!(t.labels))])
        .await
        .unwrap();
    assert!(get_route_result(&store, REPO, "1").await.is_err());
    let cached = store.get(id).await.unwrap();
    // Reproduce the ordinary path's discarded archive label. The router must
    // independently consult raw cached labels before accepting this task.
    let generic = crate::engine::tasks::store_task_to_external(&cached);
    assert!(!generic.labels.contains(&"status:archived".into()));
    assert!(router.route(&generic, &store, REPO).await.is_err());
    assert_eq!(stored_pin(&store, REPO, "1").await.unwrap(), Some(pin));
}

#[tokio::test]
#[serial]
async fn route_commit_rechecks_revocation_and_rolls_back_new_pin() {
    let _env = TestEnv::new();
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let t = task("designer");
    let id = store.ensure_external_task(REPO, &t).await.unwrap();
    let mut router = Router::new(RouterConfig::default());
    router.available_agents = vec!["codex".into()];
    let route = router.route(&t, &store, REPO).await.unwrap();
    let mut labels = t.labels;
    labels.push("execution:held".into());
    store
        .set_fields(id, &[("labels", json!(labels))])
        .await
        .unwrap();
    assert!(router
        .store_route_result("1", &route, &store, REPO)
        .await
        .is_err());
    assert!(stored_pin(&store, REPO, "1").await.unwrap().is_none());
    let cached = store.get(id).await.unwrap();
    assert!(cached.agent.is_none());
    assert!(cached.model.is_none());
}

#[tokio::test]
#[serial]
async fn database_failure_cannot_leave_half_of_a_strict_route() {
    let _env = TestEnv::new();
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let t = task("designer");
    let id = store.ensure_external_task(REPO, &t).await.unwrap();
    let mut router = Router::new(RouterConfig::default());
    router.available_agents = vec!["codex".into()];
    let route = router.route(&t, &store, REPO).await.unwrap();
    sqlx::query("CREATE TRIGGER fail_strict_route BEFORE UPDATE OF agent ON tasks BEGIN SELECT RAISE(ABORT, 'synthetic route failure'); END")
        .execute(store.pool()).await.unwrap();
    assert!(router
        .store_route_result("1", &route, &store, REPO)
        .await
        .is_err());
    assert!(stored_pin(&store, REPO, "1").await.unwrap().is_none());
    assert!(store.get(id).await.unwrap().agent.is_none());
    sqlx::query("DROP TRIGGER fail_strict_route")
        .execute(store.pool())
        .await
        .unwrap();
    router
        .store_route_result("1", &route, &store, REPO)
        .await
        .unwrap();
    assert_eq!(
        get_route_result(&store, REPO, "1")
            .await
            .unwrap()
            .strict_profile,
        route.strict_profile
    );
}

#[tokio::test]
#[serial]
async fn disk_restart_preserves_both_pins_and_rejects_policy_drift() {
    let env = TestEnv::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tasks.db");
    let store = Arc::new(TaskStore::open(&path).await.unwrap());
    let mut t = task("designer");
    t.labels.push("reviewer:reviewer".into());
    store.ensure_external_task(REPO, &t).await.unwrap();
    let mut router = Router::new(RouterConfig::default());
    router.available_agents = vec!["codex".into()];
    let route = router.route(&t, &store, REPO).await.unwrap();
    router
        .store_route_result("1", &route, &store, REPO)
        .await
        .unwrap();
    store.pool().close().await;
    drop(store);
    let reopened = Arc::new(TaskStore::open(&path).await.unwrap());
    assert_eq!(
        get_route_result(&reopened, REPO, "1")
            .await
            .unwrap()
            .strict_profile,
        route.strict_profile
    );
    assert_eq!(
        review_pin(&reopened, REPO, "1", &t).await.unwrap().cli,
        "claude"
    );
    let mut changed = fixture();
    changed["profiles"]["reviewer"]["effort"] = json!("medium");
    std::fs::write(env._dir.path().join("policy.json"), changed.to_string()).unwrap();
    assert!(get_route_result(&reopened, REPO, "1").await.is_err());
    assert_eq!(
        stored_pin(&reopened, REPO, "1").await.unwrap(),
        route.strict_profile
    );
    std::env::remove_var(REGISTRY_ENV);
    assert!(get_route_result(&reopened, REPO, "1").await.is_err());
    assert!(task_is_strict(&Some(reopened), REPO, "1").await.unwrap());
}

#[tokio::test]
#[serial]
async fn concurrent_connections_cannot_commit_conflicting_pins_or_routes() {
    let _env = TestEnv::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("race.db");
    let first = Arc::new(TaskStore::open(&path).await.unwrap());
    let second = TaskStore::open(&path).await.unwrap();
    let t = task("designer");
    first.ensure_external_task(REPO, &t).await.unwrap();
    let mut router = Router::new(RouterConfig::default());
    router.available_agents = vec!["codex".into()];
    let route = router.route(&t, &first, REPO).await.unwrap();
    let mut conflict = route.clone();
    conflict.strict_profile.as_mut().unwrap().effort = "medium".into();
    let (a, b) = tokio::join!(
        persist_route(&first, REPO, "1", &route),
        persist_route(&second, REPO, "1", &conflict)
    );
    assert!(a.is_ok(), "{a:?}");
    assert!(b.is_err());
    assert_eq!(
        get_route_result(&first, REPO, "1")
            .await
            .unwrap()
            .strict_profile,
        route.strict_profile
    );
}

#[tokio::test]
#[serial]
async fn strict_preparation_requires_backend_and_checks_held_before_worktree() {
    let _env = TestEnv::new();
    let (store, _, mut t, _) = routed().await;
    let dir = tempfile::tempdir().unwrap();
    let error = crate::engine::runner::task_init::prepare_task(
        "1",
        Some("codex"),
        Some("model-one"),
        None,
        REPO,
        dir.path(),
        0,
        &Some(store.clone()),
    )
    .await
    .err()
    .unwrap();
    assert!(error.to_string().contains("fresh authorization backend"));
    t.labels.push("execution:held".into());
    let backend = SnapshotBackend::new(t);
    let error = crate::engine::runner::task_init::prepare_task(
        "1",
        Some("codex"),
        Some("model-one"),
        Some(&backend),
        REPO,
        dir.path(),
        0,
        &Some(store),
    )
    .await
    .err()
    .unwrap();
    assert!(error.to_string().contains("execution:held"));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[tokio::test]
#[serial]
async fn native_review_entry_does_not_record_a_session_or_fallback() {
    let _env = TestEnv::new();
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mut t = task("designer");
    t.labels.push("reviewer:implementer".into());
    let id = store.ensure_external_task(REPO, &t).await.unwrap();
    let mut router = Router::new(RouterConfig::default());
    router.available_agents = vec!["codex".into()];
    let route = router.route(&t, &store, REPO).await.unwrap();
    router
        .store_route_result("1", &route, &store, REPO)
        .await
        .unwrap();
    let backend: Arc<dyn ExternalBackend> = Arc::new(SnapshotBackend::new(t.clone()));
    let manager = Arc::new(crate::engine::tasks::TaskManager::with_store(
        backend.clone(),
        store.clone(),
        REPO.into(),
    ));
    let router = Arc::new(tokio::sync::RwLock::new(router));
    let tmux = Arc::new(crate::tmux::TmuxManager::new());
    let error = crate::engine::review::review_and_merge(
        &t, &backend, &tmux, REPO, &router, &manager, &store,
    )
    .await
    .err()
    .unwrap();
    assert!(error.to_string().contains("live execution is disabled"));
    let cached = store.get(id).await.unwrap();
    assert!(!cached.review_session_expected);
    assert_eq!(cached.review_invocations, 0);
    assert_eq!(
        stored_pin(&store, REPO, "1").await.unwrap(),
        route.strict_profile
    );
}

#[tokio::test]
#[serial]
async fn repersisting_a_pin_cannot_silently_repair_erased_selection() {
    let _env = TestEnv::new();
    let (store, router, t, pin) = routed().await;
    let route = get_route_result(&store, REPO, "1").await.unwrap();
    let id = store.resolve_task_id(REPO, "1").await.unwrap().unwrap();
    store
        .set_fields(id, &[("agent", Value::Null), ("model", Value::Null)])
        .await
        .unwrap();
    assert!(router
        .store_route_result(&t.id.0, &route, &store, REPO)
        .await
        .is_err());
    let task = store.get(id).await.unwrap();
    assert!(task.agent.is_none());
    assert!(task.model.is_none());
    assert_eq!(stored_pin(&store, REPO, "1").await.unwrap(), Some(pin));
}

#[tokio::test]
#[serial]
async fn native_routing_tick_cannot_restore_stale_labels_after_concurrent_hold() {
    let _env = TestEnv::new();
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mut t = task("designer");
    t.labels.push("reviewer:implementer".into());
    let id = store.ensure_external_task(REPO, &t).await.unwrap();
    let mut snapshot = SnapshotBackend::new(t);
    snapshot.on_status = Some(store.clone());
    let backend: Arc<dyn ExternalBackend> = Arc::new(snapshot);
    let manager = Arc::new(crate::engine::tasks::TaskManager::with_store(
        backend.clone(),
        store.clone(),
        REPO.into(),
    ));
    let mut router = Router::new(RouterConfig::default());
    router.available_agents = vec!["codex".into()];
    let mut quota = crate::engine::tick::RoutingTickQuota::new(1);
    crate::engine::tick::tick_route_tasks(
        &backend,
        &manager,
        &mut router,
        &store,
        REPO,
        &mut quota,
    )
    .await
    .unwrap();
    let cached = store.get(id).await.unwrap();
    for label in ["execution:held", "status:archived", "concurrent-marker"] {
        assert!(cached.labels.iter().any(|v| v == label), "lost {label}");
    }
    let pin = stored_pin(&store, REPO, "1").await.unwrap().unwrap();
    assert_eq!(pin.reviewer.unwrap().persona, "implementer");
    assert!(get_route_result(&store, REPO, "1").await.is_err());
}

#[tokio::test]
#[serial]
async fn native_dispatch_after_policy_removal_blocks_before_in_progress_or_review() {
    let _env = TestEnv::new();
    let (store, _, t, pin) = routed().await;
    let id = store.resolve_task_id(REPO, "1").await.unwrap().unwrap();
    store
        .update_status(id, crate::store::TaskStatus::Routed)
        .await
        .unwrap();
    std::env::remove_var(REGISTRY_ENV);
    let snapshot = Arc::new(SnapshotBackend::new(t));
    let backend: Arc<dyn ExternalBackend> = snapshot.clone();
    let (event_tx, mut events) = tokio::sync::broadcast::channel(4);
    let manager = Arc::new(crate::engine::tasks::TaskManager::with_events(
        backend.clone(),
        store.clone(),
        REPO.into(),
        event_tx,
    ));
    let tmux = Arc::new(crate::tmux::TmuxManager::new());
    let runner =
        Arc::new(crate::engine::runner::TaskRunner::new(REPO.into()).with_store(store.clone()));
    let capture = Arc::new(crate::channels::capture::CaptureService::new(Arc::new(
        crate::channels::transport::Transport::new(),
    )));
    let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
    let (weights, mut signals) = tokio::sync::mpsc::channel(1);
    let dispatching = Arc::new(dashmap::DashMap::new());
    crate::engine::tick::tick_dispatch_tasks(
        &backend,
        &tmux,
        REPO,
        &runner,
        &capture,
        &semaphore,
        &manager,
        &weights,
        crate::engine::tick::DispatchMode {
            is_degraded: false,
            healthy_agents: 1,
            threshold: 1,
        },
        &dispatching,
        &store,
        &std::collections::HashMap::new(),
    )
    .await
    .unwrap();
    let cached = store.get(id).await.unwrap();
    assert_eq!(cached.status, crate::store::TaskStatus::Blocked);
    assert!(cached
        .block_reason
        .unwrap()
        .contains("live execution is disabled"));
    assert!(!cached.review_session_expected);
    assert_eq!(cached.attempts, 0);
    assert_eq!(semaphore.available_permits(), 1);
    assert!(dispatching.is_empty());
    assert!(signals.try_recv().is_err());
    assert_eq!(
        *snapshot.updates.lock().unwrap(),
        vec![crate::backends::Status::Blocked]
    );
    let event = events.try_recv().unwrap();
    assert_eq!(event.old_status, "routed");
    assert_eq!(event.new_status, "blocked");
    assert!(events.try_recv().is_err());
    assert_eq!(stored_pin(&store, REPO, "1").await.unwrap(), Some(pin));
}

#[tokio::test]
#[serial]
async fn direct_context_runner_returns_blocked_signal_for_persisted_pin_without_policy() {
    let _env = TestEnv::new();
    let (store, _, t, pin) = routed().await;
    std::env::remove_var(REGISTRY_ENV);
    let backend: Arc<dyn ExternalBackend> = Arc::new(SnapshotBackend::new(t.clone()));
    let runner = crate::engine::runner::TaskRunner::new(REPO.into()).with_store(store.clone());
    let signal = runner
        .run_with_context(
            &t,
            &backend,
            &Arc::new(crate::tmux::TmuxManager::new()),
            None,
        )
        .await
        .unwrap();
    assert!(matches!(
        signal,
        crate::engine::runner::WeightSignal::Blocked
    ));
    assert_eq!(
        store
            .get_by_external_id(REPO, "1")
            .await
            .unwrap()
            .unwrap()
            .status,
        crate::store::TaskStatus::Blocked
    );
    assert_eq!(stored_pin(&store, REPO, "1").await.unwrap(), Some(pin));
}

#[tokio::test]
#[serial]
async fn internal_task_alias_preserves_pin_and_cached_revocations() {
    let _env = TestEnv::new();
    let (store, _, _, pin) = routed().await;
    let id = store.resolve_task_id(REPO, "1").await.unwrap().unwrap();
    let alias = format!("internal:{id}");
    assert_eq!(
        get_route_result(&store, REPO, &alias)
            .await
            .unwrap()
            .strict_profile,
        Some(pin.clone())
    );
    assert_eq!(stored_pin(&store, REPO, &alias).await.unwrap(), Some(pin));
    // An internal row number from another repository cannot reveal this pin.
    assert!(stored_pin(&store, "example/other", &alias)
        .await
        .unwrap()
        .is_none());
    store
        .set_fields(
            id,
            &[("labels", json!(["execution:held", "agent:designer"]))],
        )
        .await
        .unwrap();
    assert!(get_route_result(&store, REPO, &alias).await.is_err());
}

#[tokio::test]
#[serial]
async fn internal_task_alias_cannot_escape_strict_runner_after_policy_removal() {
    let _env = TestEnv::new();
    let (store, _, mut t, pin) = routed().await;
    let id = store.resolve_task_id(REPO, "1").await.unwrap().unwrap();
    t.id = ExternalId(format!("internal:{id}"));
    std::env::remove_var(REGISTRY_ENV);
    assert!(task_is_strict(&Some(store.clone()), REPO, &t.id.0)
        .await
        .unwrap());
    assert!(get_route_result(&store, REPO, &t.id.0).await.is_err());
    assert!(block_no_fallback(&Some(store.clone()), REPO, &t.id.0)
        .await
        .unwrap());
    let backend: Arc<dyn ExternalBackend> = Arc::new(SnapshotBackend::new(t.clone()));
    let runner = crate::engine::runner::TaskRunner::new(REPO.into()).with_store(store.clone());
    let signal = runner
        .run_with_context(
            &t,
            &backend,
            &Arc::new(crate::tmux::TmuxManager::new()),
            None,
        )
        .await
        .unwrap();
    assert!(matches!(
        signal,
        crate::engine::runner::WeightSignal::Blocked
    ));
    let stored = store.get(id).await.unwrap();
    assert_eq!(stored.status, crate::store::TaskStatus::Blocked);
    assert_eq!(stored.agent.as_deref(), Some(pin.cli.as_str()));
    assert_eq!(stored.model.as_deref(), Some(pin.model.as_str()));
    assert_eq!(stored_pin(&store, REPO, "1").await.unwrap(), Some(pin));
}

#[tokio::test]
#[serial]
async fn conflicting_alias_pin_fails_closed_instead_of_selecting_a_route() {
    let _env = TestEnv::new();
    let (store, _, _, pin) = routed().await;
    let id = store.resolve_task_id(REPO, "1").await.unwrap().unwrap();
    let alias = format!("internal:{id}");
    let mut other = pin;
    other.issue_id = alias.clone();
    other.effort = "medium".into();
    store
        .kv_set(&key(REPO, &alias), &serde_json::to_string(&other).unwrap())
        .await
        .unwrap();
    std::env::remove_var(REGISTRY_ENV);
    assert!(stored_pin(&store, REPO, &alias).await.is_err());
    assert!(get_route_result(&store, REPO, &alias).await.is_err());
    assert!(task_is_strict(&Some(store), REPO, &alias).await.is_err());
}
