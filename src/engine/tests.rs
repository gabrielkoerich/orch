use super::serve::RESTART_ONLY_KEYS;
use super::serve::log_restart_only_changes;
use super::serve::resize_dispatch_pool;
use std::env;
use std::fs;
use std::path::Path;
use std::process;
use std::slice;
use std::sync::Arc;

use super::*;
use crate::backends::ExternalBackend;
use crate::backends::test_helpers::NoopBackend;
use crate::channels::capture::CaptureService;
use crate::channels::transport::Transport;
use crate::engine::git::git_ops;
use crate::store::UpsertExternal;
use crate::tmux::SessionGuard;
use crate::tmux::TmuxManager;

async fn in_review_task(store: &TaskStore, number: &str) -> i64 {
    let id = store
        .upsert_external(&UpsertExternal {
            repo: "owner/repo",
            ext_id: number,
            title: "Under review",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::InReview).await.unwrap();
    id
}

#[tokio::test]
async fn a_review_without_a_session_goes_back_to_needs_review_and_a_live_one_stays() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = Arc::new(NoopBackend);
    let task_manager =
        TaskManager::with_store(backend, Arc::clone(&store), "owner/repo".to_string());
    let tmux = TmuxManager::isolated();
    let orphaned = in_review_task(&store, "11").await;
    let live = in_review_task(&store, "12").await;
    let session = tmux
        .create_session("owner/repo", "12-review", "/tmp", "sleep 30", &[])
        .await
        .unwrap();
    let _guard = SessionGuard::new(&tmux, session);

    let reset = reset_orphaned_reviews(&task_manager, &store, "owner/repo", &tmux).await;

    assert_eq!(reset, 1);
    assert_eq!(
        store.get(orphaned).await.unwrap().status,
        TaskStatus::NeedsReview
    );
    assert_eq!(store.get(live).await.unwrap().status, TaskStatus::InReview);
}

#[test]
fn growing_max_parallel_lets_more_tasks_run_without_a_restart() {
    let semaphore = Arc::new(Semaphore::new(4));
    let held: Vec<_> = (0..4)
        .map(|_| Arc::clone(&semaphore).try_acquire_owned().unwrap())
        .collect();
    assert!(
        Arc::clone(&semaphore).try_acquire_owned().is_err(),
        "pool is full"
    );

    let in_force = resize_dispatch_pool(&semaphore, 4, 8);
    assert_eq!(in_force, 8);

    let grown: Vec<_> = (0..4)
        .map(|_| Arc::clone(&semaphore).try_acquire_owned().unwrap())
        .collect();
    assert_eq!(held.len() + grown.len(), 8, "8 tasks run at once");
    assert!(
        Arc::clone(&semaphore).try_acquire_owned().is_err(),
        "pool is full"
    );
}

#[test]
fn shrinking_max_parallel_never_cancels_a_running_task() {
    let semaphore = Arc::new(Semaphore::new(8));
    let mut held: Vec<_> = (0..8)
        .map(|_| Arc::clone(&semaphore).try_acquire_owned().unwrap())
        .collect();

    // Nothing is free, so all 8 tasks keep their permits and keep running.
    let mut in_force = resize_dispatch_pool(&semaphore, 8, 4);
    assert_eq!(in_force, 8);
    assert_eq!(held.len(), 8);

    // Three finish. Their permits are taken back instead of handed out again.
    held.truncate(5);
    in_force = resize_dispatch_pool(&semaphore, in_force, 4);
    assert_eq!(in_force, 5);
    assert!(Arc::clone(&semaphore).try_acquire_owned().is_err());

    // One more finishes and the pool settles at the new limit.
    held.truncate(4);
    in_force = resize_dispatch_pool(&semaphore, in_force, 4);
    assert_eq!(in_force, 4);
    assert!(Arc::clone(&semaphore).try_acquire_owned().is_err());

    // A slot freed after that is handed out, since 4 is now the real limit.
    held.truncate(3);
    assert!(Arc::clone(&semaphore).try_acquire_owned().is_ok());
}

#[test]
fn a_project_added_to_the_config_is_picked_up_and_running_ones_are_not_respawned() {
    let configured = vec![
        ("owner/one".to_string(), PathBuf::from("/srv/one")),
        ("owner/two".to_string(), PathBuf::from("/srv/two")),
    ];
    let active: HashSet<String> = ["owner/one".to_string()].into_iter().collect();

    let (added, removed) = diff_configured_projects(&configured, &active);
    assert_eq!(
        added,
        vec![("owner/two".to_string(), PathBuf::from("/srv/two"))],
        "only the new project is started, so owner/one keeps its single set of subscribers"
    );
    assert!(removed.is_empty(), "{removed:?}");
}

#[test]
fn a_project_dropped_from_the_config_is_reported_and_not_restarted() {
    let configured = vec![("owner/one".to_string(), PathBuf::from("/srv/one"))];
    let active: HashSet<String> = ["owner/one".to_string(), "owner/gone".to_string()]
        .into_iter()
        .collect();

    let (added, removed) = diff_configured_projects(&configured, &active);
    assert!(added.is_empty(), "nothing new to start");
    assert_eq!(removed, vec!["owner/gone".to_string()]);
}

#[test]
fn an_unchanged_project_list_starts_nothing() {
    let configured = vec![("owner/one".to_string(), PathBuf::from("/srv/one"))];
    let active: HashSet<String> = ["owner/one".to_string()].into_iter().collect();

    assert_eq!(
        diff_configured_projects(&configured, &active),
        (vec![], vec![])
    );
}

#[test]
fn restart_only_keys_are_the_settings_read_once_at_startup() {
    // Guards the docs table: every key listed here is documented as needing
    // `orch service restart`, and nothing reloadable may be added to it.
    assert_eq!(RESTART_ONLY_KEYS, ["channels", "webhook", "relay"]);
}

#[test]
fn a_changed_restart_only_key_is_reported_and_an_unchanged_one_is_not() {
    let before = vec![Some("bot_token: a\n".to_string()), None];

    assert!(
        log_restart_only_changes(&before, &before.clone()).is_empty(),
        "a reload that did not touch these keys must stay quiet"
    );

    // An edited token and a newly added webhook block are both changes
    let after = vec![
        Some("bot_token: b\n".to_string()),
        Some("port: 8080\n".to_string()),
    ];
    assert_eq!(
        log_restart_only_changes(&before, &after),
        ["channels", "webhook"]
    );

    // Only the edited key is reported when the other is untouched
    let token_only = vec![Some("bot_token: b\n".to_string()), None];
    assert_eq!(log_restart_only_changes(&before, &token_only), ["channels"]);
}

#[test]
fn resize_dispatch_pool_is_a_noop_at_the_current_limit() {
    let semaphore = Arc::new(Semaphore::new(4));
    assert_eq!(resize_dispatch_pool(&semaphore, 4, 4), 4);
    assert_eq!(semaphore.available_permits(), 4);
}

#[test]
fn live_session_stuck_limit_follows_the_run_timeout() {
    let config = EngineConfig {
        stuck_timeout: 1800,
        run_timeouts: RunTimeouts {
            base: 1800,
            medium: 2700,
            complex: 3600,
        },
        ..EngineConfig::default()
    };
    assert_eq!(
        config.live_session_stuck_limit(Some("simple")),
        1800 + RUN_TIMEOUT_GRACE_SECS
    );
    assert_eq!(
        config.live_session_stuck_limit(Some("medium")),
        2700 + RUN_TIMEOUT_GRACE_SECS
    );
    assert_eq!(
        config.live_session_stuck_limit(Some("complex")),
        3600 + RUN_TIMEOUT_GRACE_SECS
    );

    let long_stuck = EngineConfig {
        stuck_timeout: 7200,
        ..config
    };
    assert_eq!(long_stuck.live_session_stuck_limit(Some("complex")), 7200);
}

#[test]
fn engine_config_defaults() {
    let config = EngineConfig::default();
    assert_eq!(config.tick_interval, Duration::from_secs(10));
    assert_eq!(config.sync_interval, Duration::from_secs(45));
    assert_eq!(config.max_parallel, 4);
    assert_eq!(config.stuck_timeout, 1800);
    assert_eq!(config.no_session_stuck_timeout, 600);
    assert!(
        config.no_session_stuck_timeout < config.stuck_timeout,
        "no_session_stuck_timeout must be shorter than stuck_timeout"
    );
    assert_eq!(config.graceful_shutdown_timeout, Duration::from_secs(600));
    assert_eq!(config.upgrade_check_interval, 3600);
}

#[test]
fn engine_config_from_config_uses_defaults_when_no_config() {
    let config = EngineConfig::from_config();
    assert_eq!(config.tick_interval, Duration::from_secs(10));
    assert_eq!(config.sync_interval, Duration::from_secs(45));
    // max_parallel may be overridden by user config; just check it's reasonable
    assert!(config.max_parallel >= 1 && config.max_parallel <= 64);
    assert_eq!(config.stuck_timeout, 1800);
    assert_eq!(config.no_session_stuck_timeout, 600);
    assert_eq!(config.graceful_shutdown_timeout, Duration::from_secs(600));
    assert_eq!(config.upgrade_check_interval, 3600);
}

#[test]
fn engine_config_no_session_timeout_is_under_15_minutes() {
    let config = EngineConfig::default();
    assert!(
        config.no_session_stuck_timeout <= 900,
        "no_session_stuck_timeout ({}) exceeds 15 minutes",
        config.no_session_stuck_timeout
    );
}

#[test]
fn upgrade_check_runs_immediately_when_never_checked() {
    assert!(should_run_upgrade_check(None, 3600));
}

#[test]
fn upgrade_check_waits_for_interval_after_first_run() {
    assert!(!should_run_upgrade_check(Some(Instant::now()), 3600));
}

#[test]
fn upgrade_check_can_be_disabled() {
    assert!(!should_run_upgrade_check(None, 0));
}

#[test]
fn configured_agents_parses_yaml_block_array() {
    // config::get returns serde_norway::to_string for arrays, which is YAML block format
    let yaml_str = "- claude\n- codex\n- opencode\n- olm\n";
    let parsed: Vec<String> = serde_norway::from_str(yaml_str).unwrap();
    assert_eq!(parsed, vec!["claude", "codex", "opencode", "olm"]);
}

#[test]
fn configured_agents_parses_json_inline_array() {
    let json_str = r#"["claude","codex","olm"]"#;
    let parsed: Vec<String> = serde_json::from_str(json_str).unwrap();
    assert_eq!(parsed, vec!["claude", "codex", "olm"]);
}

#[test]
fn configured_agents_fallback_returns_default_agents() {
    // When no config is present, configured_agents() returns DEFAULT_AGENTS
    let defaults = configured_agents();
    assert!(defaults.contains(&"claude".to_string()));
    assert!(defaults.contains(&"codex".to_string()));
    assert!(defaults.contains(&"opencode".to_string()));
}

/// Integration test that requires a fully working tmux environment with
/// reliable capture-pane timing. Runs locally but not in CI (GitHub Actions
/// has tmux installed but capture-pane timing is unreliable).
///
/// DO NOT REMOVE #[ignore] — this test was un-ignored in PR #608 which
/// broke CI. The `tmux -V` skip check is insufficient because tmux CAN
/// create sessions on CI runners, but capture-pane output is empty or
/// delayed, causing the assertion to fail after a 10s timeout.
#[tokio::test]
#[ignore = "capture-pane timing is unreliable on CI runners"]
async fn integration_channel_to_tmux_to_capture() {
    // Create transport and capture service
    let transport = Arc::new(Transport::new());
    let capture = Arc::new(CaptureService::new(Arc::clone(&transport)));
    let tmux = TmuxManager::new();

    let task_id = "testtask1".to_string();
    let session_name = format!("orch-test-{task_id}");

    // Start a detached tmux session
    let _ = tmux
        .command()
        .args(["new-session", "-d", "-s", &session_name])
        .output()
        .await
        .expect("failed to start tmux session");

    // Guard ensures session cleanup even on panic
    let _guard = SessionGuard::new(&tmux, session_name.clone());

    // Register session with capture service and transport binding
    capture
        .register_session("owner/repo", &task_id, &session_name)
        .await;
    transport
        .bind(
            "owner/repo",
            &task_id,
            &session_name,
            "telegram",
            "12345",
            None,
        )
        .await;

    // Spawn capture.run() which exits when session is unregistered
    let capture_clone = Arc::clone(&capture);
    let capture_handle = tokio::spawn(async move { capture_clone.run().await });

    // Subscribe to transport output for this task
    let mut rx = transport
        .subscribe("owner/repo", &task_id)
        .await
        .expect("no subscription");

    // Send a message into the tmux session via tmux send-keys (simulate channel input)
    let send = tmux
        .command()
        .args([
            "send-keys",
            "-t",
            &session_name,
            "echo from-channel",
            "Enter",
        ])
        .output()
        .await
        .expect("failed to send keys");
    assert!(send.status.success());

    // Wait for an output chunk from capture -> transport
    let mut got = false;
    for _ in 0..20 {
        if let Ok(Ok(c)) = time::timeout(Duration::from_millis(500), rx.recv()).await
            && c.content.contains("from-channel")
        {
            got = true;
            break;
        }
    }

    // Clean up: unregister session (tmux kill handled by SessionGuard on drop)
    capture.unregister_session("owner/repo", &task_id).await;

    // Wait for capture.run to finish
    let _ = capture_handle.await;

    assert!(got, "did not observe tmux output via capture/transport");
}

// ── read_project_channel_config tests ─────────────────────────────────

#[test]
fn read_project_channel_config_returns_defaults_when_no_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = read_project_channel_config(dir.path());
    assert!(cfg.telegram_topic_id.is_none());
    assert!(cfg.discord_channel_id.is_none());
}

#[test]
fn read_project_channel_config_parses_telegram_and_discord() {
    let dir = tempfile::tempdir().expect("tempdir");
    let orch_yml = dir.path().join(".orch.yml");
    fs::write(
        &orch_yml,
        "channels:\n  telegram:\n    topic_id: \"42\"\n  discord:\n    channel_id: \"9999\"\n",
    )
    .expect("write .orch.yml");
    let cfg = read_project_channel_config(dir.path());
    assert_eq!(cfg.telegram_topic_id.as_deref(), Some("42"));
    assert_eq!(cfg.discord_channel_id.as_deref(), Some("9999"));
}

#[test]
fn read_project_channel_config_handles_partial_channels() {
    let dir = tempfile::tempdir().expect("tempdir");
    let orch_yml = dir.path().join(".orch.yml");
    fs::write(&orch_yml, "channels:\n  telegram:\n    topic_id: \"7\"\n").expect("write .orch.yml");
    let cfg = read_project_channel_config(dir.path());
    assert_eq!(cfg.telegram_topic_id.as_deref(), Some("7"));
    assert!(cfg.discord_channel_id.is_none());
}

#[test]
fn read_project_channel_config_handles_malformed_yaml() {
    let dir = tempfile::tempdir().expect("tempdir");
    let orch_yml = dir.path().join(".orch.yml");
    fs::write(&orch_yml, "channels: [\nbad yaml").expect("write .orch.yml");
    let cfg = read_project_channel_config(dir.path());
    // Should return defaults (not panic) and emit a warn! log with the parse error.
    assert!(cfg.telegram_topic_id.is_none());
    assert!(cfg.discord_channel_id.is_none());
}

#[test]
#[cfg(unix)]
fn read_project_channel_config_handles_unreadable_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    // A directory fails to read for every user, root included, where chmod 000 does not
    fs::create_dir(dir.path().join(".orch.yml")).expect("create .orch.yml dir");
    let cfg = read_project_channel_config(dir.path());
    // Should return defaults (not panic) and emit a warn! log with the read error.
    assert!(cfg.telegram_topic_id.is_none());
    assert!(cfg.discord_channel_id.is_none());
}

fn git(dir: &Path, args: &[&str]) {
    let mut cmd = process::Command::new("git");
    cmd.current_dir(dir);
    if args.first() == Some(&"commit") {
        cmd.args(["-c", "commit.gpgsign=false"]);
    }
    let out = cmd
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?} failed to start: {e}"));
    assert!(
        out.status.success(),
        "git {:?} failed:\nstdout: {}\nstderr: {}",
        args,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn restore_orch_home(prev: Option<String>) {
    match prev {
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        Some(v) => unsafe { env::set_var("ORCH_HOME", v) },
        // SAFETY: nextest runs each test in its own process, so no other test touches the environment
        None => unsafe { env::remove_var("ORCH_HOME") },
    }
}

struct StartupConflict {
    _tmp: tempfile::TempDir,
    _orch_home: tempfile::TempDir,
    prev_orch_home: Option<String>,
    project_dir: PathBuf,
    worktree_dir: PathBuf,
    branch: String,
    store: Arc<TaskStore>,
    store_id: i64,
    engine: ProjectEngine,
}

impl Drop for StartupConflict {
    fn drop(&mut self) {
        restore_orch_home(self.prev_orch_home.take());
    }
}

async fn startup_conflict_fixture(
    retries: i32,
    pr_number: Option<i32>,
    start_rebase: bool,
) -> StartupConflict {
    let orch_home = tempfile::tempdir().unwrap();
    let prev_orch_home = env::var("ORCH_HOME").ok();
    // SAFETY: nextest runs each test in its own process, so no other test touches the environment
    unsafe { env::set_var("ORCH_HOME", orch_home.path()) };

    let tmp = tempfile::tempdir().unwrap();
    let origin = tmp.path().join("origin.git");
    fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "--bare", "-b", "main"]);
    let project_dir = tmp.path().join("repo");
    git(
        tmp.path(),
        &[
            "clone",
            origin.to_str().unwrap(),
            project_dir.to_str().unwrap(),
        ],
    );
    git(&project_dir, &["config", "user.email", "t@t.com"]);
    git(&project_dir, &["config", "user.name", "T"]);
    fs::write(project_dir.join("f.txt"), "base\n").unwrap();
    git(&project_dir, &["add", "."]);
    git(&project_dir, &["commit", "-m", "base"]);
    git(&project_dir, &["push", "origin", "HEAD:main"]);
    git(&project_dir, &["remote", "set-head", "origin", "main"]);

    let branch = "gh-issue-4087-conflict".to_string();
    git(&project_dir, &["checkout", "-b", &branch]);
    fs::write(project_dir.join("f.txt"), "agent\n").unwrap();
    git(&project_dir, &["commit", "-am", "agent"]);
    git(&project_dir, &["checkout", "main"]);
    fs::write(project_dir.join("f.txt"), "main\n").unwrap();
    git(&project_dir, &["commit", "-am", "main"]);
    git(&project_dir, &["push", "origin", "HEAD:main"]);

    let wt_base = orch_home.path().join("worktrees").join("repo");
    fs::create_dir_all(&wt_base).unwrap();
    let worktree_dir = wt_base.join(&branch);
    git(
        &project_dir,
        &["worktree", "add", worktree_dir.to_str().unwrap(), &branch],
    );
    git(&worktree_dir, &["config", "user.email", "t@t.com"]);
    git(&worktree_dir, &["config", "user.name", "T"]);
    git(&worktree_dir, &["fetch", "origin"]);
    if start_rebase {
        let rebase = process::Command::new("git")
            .args(["rebase", "origin/main"])
            .current_dir(&worktree_dir)
            .output()
            .unwrap();
        assert!(!rebase.status.success(), "precondition: rebase conflicts");
    }

    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let store_id = store
        .upsert_external(&UpsertExternal {
            repo: "owner/repo",
            ext_id: "4087",
            title: "Conflict",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store
        .update_status(store_id, TaskStatus::Routed)
        .await
        .unwrap();
    let mut fields = vec![
        ("branch", serde_json::json!(branch)),
        (
            "worktree",
            serde_json::json!(worktree_dir.to_string_lossy().as_ref()),
        ),
        ("merge_conflict_retries", serde_json::json!(retries)),
    ];
    if let Some(pr) = pr_number {
        fields.push(("pr_number", serde_json::json!(pr)));
    }
    store.set_fields(store_id, &fields).await.unwrap();

    let backend: Arc<dyn ExternalBackend> = Arc::new(NoopBackend);
    let engine = ProjectEngine {
        repo: "owner/repo".to_string(),
        project_dir: project_dir.clone(),
        backend: Arc::clone(&backend),
        task_manager: Arc::new(TaskManager::with_store(
            backend,
            Arc::clone(&store),
            "owner/repo".to_string(),
        )),
        runner: Arc::new(
            runner::TaskRunner::new("owner/repo".to_string()).with_store(Arc::clone(&store)),
        ),
        store: Arc::clone(&store),
    };

    StartupConflict {
        _tmp: tmp,
        _orch_home: orch_home,
        prev_orch_home,
        project_dir,
        worktree_dir,
        branch,
        store,
        store_id,
        engine,
    }
}

#[tokio::test]
#[serial_test::serial(orch_home)]
async fn startup_keeps_a_content_conflict_when_the_task_retried_a_merge() {
    let fx = startup_conflict_fixture(2, Some(4043), false).await;
    reconcile_startup_worktrees(slice::from_ref(&fx.engine))
        .await
        .unwrap();

    assert!(fx.worktree_dir.exists(), "worktree must stay");
    assert!(
        git_ops::rebase_in_progress(&fx.worktree_dir).await,
        "the content conflict must stay for the agent"
    );
    let task = fx.store.get(fx.store_id).await.unwrap();
    assert_eq!(task.status, TaskStatus::Routed);
    assert_eq!(task.branch, fx.branch);
    assert!(!task.worktree.is_empty());
}

#[tokio::test]
#[serial_test::serial(orch_home)]
async fn startup_keeps_an_in_progress_rebase_when_the_task_has_a_pr() {
    let fx = startup_conflict_fixture(0, Some(12), true).await;
    reconcile_startup_worktrees(slice::from_ref(&fx.engine))
        .await
        .unwrap();

    assert!(fx.worktree_dir.exists(), "worktree must stay");
    assert!(
        git_ops::rebase_in_progress(&fx.worktree_dir).await,
        "an in-progress rebase for a PR task must stay"
    );
    let task = fx.store.get(fx.store_id).await.unwrap();
    assert_eq!(task.status, TaskStatus::Routed);
    assert_eq!(task.branch, fx.branch);
}

#[tokio::test]
#[serial_test::serial(orch_home)]
async fn startup_resets_a_conflict_without_retries_or_a_pr() {
    let fx = startup_conflict_fixture(0, None, false).await;
    reconcile_startup_worktrees(slice::from_ref(&fx.engine))
        .await
        .unwrap();

    assert!(
        !fx.worktree_dir.exists(),
        "worktree is removed when there is nothing to keep"
    );
    let task = fx.store.get(fx.store_id).await.unwrap();
    assert_eq!(task.status, TaskStatus::New);
}

#[tokio::test]
#[serial_test::serial(orch_home)]
async fn startup_reset_keeps_branch_when_rescue_cannot_push() {
    let fx = startup_conflict_fixture(0, None, false).await;
    git(
        &fx.project_dir,
        &["remote", "set-url", "origin", "/nonexistent/origin.git"],
    );
    reconcile_startup_worktrees(slice::from_ref(&fx.engine))
        .await
        .unwrap();

    let task = fx.store.get(fx.store_id).await.unwrap();
    assert_eq!(task.status, TaskStatus::New);
    assert_eq!(
        task.branch, fx.branch,
        "the task still names the local branch cleanup kept"
    );
    assert!(
        fx.worktree_dir.exists(),
        "rescue failure keeps the worktree on disk"
    );
}
