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
    let (store, _, _, _) = routed().await;
    let project = tempfile::tempdir().unwrap();
    let error = crate::engine::runner::task_init::prepare_task(
        "1",
        Some("codex"),
        Some("model-one"),
        None,
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
