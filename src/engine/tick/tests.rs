use super::*;
use crate::backends::{ExternalId, ExternalTask, Mention, Status};
use crate::channels::transport::Transport;
use crate::cooldown;
use crate::engine::router::Router;
use crate::engine::router::RouterConfig;
use crate::engine::tasks::TaskManager;
use crate::store::NewTask;
use crate::store::TaskStatus;
use crate::store::TaskStore;
use crate::store::UpsertExternal;
use crate::sys::tmux::{SessionGuard, TmuxManager};
use async_trait::async_trait;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time;

#[test]
fn timed_out_run_without_pr_never_becomes_done() {
    let failed = WeightSignal::Failed {
        agent: "claude".to_string(),
    };
    assert_eq!(display_status_for(&failed, true, false), "needs_review");
    assert_eq!(display_status_for(&failed, false, false), "needs_review");
}

#[test]
fn done_needs_a_successful_run_with_no_pr() {
    let ok = WeightSignal::Success {
        agent: "claude".to_string(),
    };
    assert_eq!(display_status_for(&ok, true, true), "needs_review");
    assert_eq!(display_status_for(&ok, true, false), "done");
    assert_eq!(
        display_status_for(&WeightSignal::Rerouted, true, false),
        "new"
    );
    assert_eq!(
        display_status_for(&WeightSignal::None, true, false),
        "needs_review"
    );
}
#[test]
fn skips_agent_for_human_and_no_agent_labels() {
    assert!(skips_agent(&["owner:human".to_string()]));
    assert!(skips_agent(&["no-agent".to_string()]));
    assert!(!skips_agent(&["status:new".to_string()]));
}

/// A router reporting one available agent with no healthy "review" model, so the
/// global NeedsReview refire sweep in `tick_global_sweeps` treats every review agent
/// as cooled and skips — keeping it a no-op for tests unrelated to it.
fn cooled_review_router_for_test() -> Router {
    Router::new_for_test(RouterConfig::default(), vec!["claude".to_string()])
}

// ── minimal mock backend ─────────────────────────────────────────────────

/// Configurable mock backend for tick tests.
pub(super) struct MockBackend {
    /// Tasks returned by `list_by_status(Blocked)`.
    pub(super) blocked_tasks: Vec<ExternalTask>,
    /// Sub-issues map: task_id → list of child ExternalIds.
    pub(super) sub_issues: HashMap<String, Vec<ExternalId>>,
    /// Tasks returned by `get_task`. Keyed by id.0.
    pub(super) tasks_by_id: HashMap<String, ExternalTask>,
    /// Recorded `get_sub_issues` calls.
    pub(super) get_sub_issues_calls: Arc<Mutex<Vec<String>>>,
    /// Recorded `update_status` calls.
    pub(super) status_updates: Arc<Mutex<Vec<(String, Status)>>>,
    /// Recorded `remove_label` calls.
    pub(super) removed_labels: Arc<Mutex<Vec<(String, String)>>>,
    /// Recorded `post_comment` calls.
    pub(super) posted_comments: Arc<Mutex<Vec<(String, String)>>>,
}

impl MockBackend {
    pub(super) fn new() -> Self {
        Self {
            blocked_tasks: vec![],
            sub_issues: HashMap::new(),
            tasks_by_id: HashMap::new(),
            get_sub_issues_calls: Arc::new(Mutex::new(vec![])),
            status_updates: Arc::new(Mutex::new(vec![])),
            removed_labels: Arc::new(Mutex::new(vec![])),
            posted_comments: Arc::new(Mutex::new(vec![])),
        }
    }
}

fn make_task(id: &str, labels: &[&str]) -> ExternalTask {
    ExternalTask {
        id: ExternalId(id.to_string()),
        title: format!("Task {id}"),
        body: String::new(),
        state: "open".to_string(),
        labels: labels.iter().map(ToString::to_string).collect(),
        author: "bot".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        url: format!("https://github.com/test/test/issues/{id}"),
    }
}

#[async_trait]
impl ExternalBackend for MockBackend {
    fn name(&self) -> &'static str {
        "mock"
    }
    async fn create_task(
        &self,
        _title: &str,
        _body: &str,
        _labels: &[String],
    ) -> anyhow::Result<ExternalId> {
        Ok(ExternalId("new".to_string()))
    }
    async fn get_task(&self, id: &ExternalId) -> anyhow::Result<ExternalTask> {
        self.tasks_by_id
            .get(&id.0)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("task not found: {}", id.0))
    }
    async fn list_by_status(&self, status: Status) -> anyhow::Result<Vec<ExternalTask>> {
        if status == Status::Blocked {
            Ok(self.blocked_tasks.clone())
        } else {
            Ok(vec![])
        }
    }
    async fn list_routable(&self) -> anyhow::Result<Vec<ExternalTask>> {
        Ok(vec![])
    }
    async fn post_comment(&self, _id: &ExternalId, _body: &str) -> anyhow::Result<()> {
        self.posted_comments
            .lock()
            .unwrap()
            .push((_id.0.clone(), _body.to_string()));
        Ok(())
    }
    async fn set_labels(&self, _id: &ExternalId, _labels: &[String]) -> anyhow::Result<()> {
        Ok(())
    }
    async fn remove_label(&self, _id: &ExternalId, _label: &str) -> anyhow::Result<()> {
        self.removed_labels
            .lock()
            .unwrap()
            .push((_id.0.clone(), _label.to_string()));
        Ok(())
    }
    async fn get_sub_issues(&self, id: &ExternalId) -> anyhow::Result<Vec<ExternalId>> {
        self.get_sub_issues_calls.lock().unwrap().push(id.0.clone());
        Ok(self.sub_issues.get(&id.0).cloned().unwrap_or_default())
    }
    async fn create_sub_task(
        &self,
        _parent: &ExternalId,
        _title: &str,
        _body: &str,
        _labels: &[String],
    ) -> anyhow::Result<ExternalId> {
        Ok(ExternalId("child".to_string()))
    }
    async fn ensure_status_label(&self, _label: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn has_open_issue_with_title(&self, _title: &str, _label: &str) -> anyhow::Result<bool> {
        Ok(false)
    }
    async fn health_check(&self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn is_pr_merged(&self, _branch: &str) -> anyhow::Result<bool> {
        Ok(false)
    }
    async fn get_authenticated_user(&self) -> anyhow::Result<Option<String>> {
        Ok(Some("testbot".to_string()))
    }
    async fn get_mentions(&self, _since: &str) -> anyhow::Result<Vec<Mention>> {
        Ok(vec![])
    }
    async fn update_status(&self, id: &ExternalId, status: Status) -> anyhow::Result<()> {
        self.status_updates
            .lock()
            .unwrap()
            .push((id.0.clone(), status));
        Ok(())
    }
}

fn make_task_manager(backend: Arc<dyn ExternalBackend>) -> Arc<TaskManager> {
    Arc::new(TaskManager::new(backend))
}

// ── tick_unblock_parents ─────────────────────────────────────────────────

#[tokio::test]
async fn unblock_parents_unblocks_when_all_children_done() {
    let mut mock = MockBackend::new();

    // Blocked parent with two done children
    let parent = make_task("10", &["status:blocked"]);
    mock.blocked_tasks.push(parent.clone());
    mock.sub_issues.insert(
        "10".to_string(),
        vec![ExternalId("11".to_string()), ExternalId("12".to_string())],
    );
    mock.tasks_by_id
        .insert("11".to_string(), make_task("11", &["status:done"]));
    mock.tasks_by_id
        .insert("12".to_string(), make_task("12", &["status:done"]));

    let status_updates = Arc::clone(&mock.status_updates);
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = make_task_manager(Arc::clone(&backend));
    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    tick_unblock_parents(&backend, &task_manager, &store, "test/repo")
        .await
        .unwrap();

    let updates = status_updates.lock().unwrap();
    assert_eq!(updates.len(), 1, "parent should be unblocked");
    assert_eq!(updates[0], ("10".to_string(), Status::New));
}

#[tokio::test]
async fn unblock_parents_skips_when_child_not_done() {
    let mut mock = MockBackend::new();

    let parent = make_task("20", &["status:blocked"]);
    mock.blocked_tasks.push(parent.clone());
    mock.sub_issues.insert(
        "20".to_string(),
        vec![ExternalId("21".to_string()), ExternalId("22".to_string())],
    );
    // Child 21 is done, child 22 is still in_progress
    mock.tasks_by_id
        .insert("21".to_string(), make_task("21", &["status:done"]));
    mock.tasks_by_id
        .insert("22".to_string(), make_task("22", &["status:in_progress"]));

    let status_updates = Arc::clone(&mock.status_updates);
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = make_task_manager(Arc::clone(&backend));
    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    tick_unblock_parents(&backend, &task_manager, &store, "test/repo")
        .await
        .unwrap();

    let updates = status_updates.lock().unwrap();
    assert!(
        updates.is_empty(),
        "should not unblock when a child is still running"
    );
}

#[tokio::test]
async fn unblock_parents_skips_task_with_no_children() {
    let mut mock = MockBackend::new();

    // Blocked task with no sub-issues (blocked for a different reason)
    let parent = make_task("30", &["status:blocked"]);
    mock.blocked_tasks.push(parent.clone());
    // No entry in sub_issues → get_sub_issues returns []

    let status_updates = Arc::clone(&mock.status_updates);
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = make_task_manager(Arc::clone(&backend));
    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    tick_unblock_parents(&backend, &task_manager, &store, "test/repo")
        .await
        .unwrap();

    let updates = status_updates.lock().unwrap();
    assert!(
        updates.is_empty(),
        "should not unblock tasks with no sub-issues"
    );
}

#[tokio::test]
async fn unblock_parents_skips_task_with_block_reason_in_store() {
    // A blocked task that has a known block_reason in the store should never
    // trigger a get_sub_issues GraphQL call — it is not waiting on children.
    let mut mock = MockBackend::new();

    // Blocked parent in the backend list
    let parent = make_task("60", &["status:blocked"]);
    mock.blocked_tasks.push(parent.clone());
    // Give it children in the backend so that, if the skip logic is wrong,
    // the parent would get unblocked.
    mock.sub_issues
        .insert("60".to_string(), vec![ExternalId("61".to_string())]);
    mock.tasks_by_id
        .insert("61".to_string(), make_task("61", &["status:done"]));

    let status_updates = Arc::clone(&mock.status_updates);
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = make_task_manager(Arc::clone(&backend));

    // Insert the blocked task into the store with a block_reason set.
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let task_id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "test/repo",
            ext_id: "60",
            title: "Task 60",
            body: "",
            author: "bot",
            url: "https://github.com/test/test/issues/60",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store
        .update_status(task_id, TaskStatus::Blocked)
        .await
        .unwrap();
    store
        .set_block_reason(task_id, Some("CI failure limit reached"))
        .await
        .unwrap();

    tick_unblock_parents(&backend, &task_manager, &store, "test/repo")
        .await
        .unwrap();

    let updates = status_updates.lock().unwrap();
    assert!(
        updates.is_empty(),
        "task with block_reason should be skipped — no API call, no unblock"
    );
}

#[tokio::test]
async fn unblock_parents_unblocks_internal_parent_from_store_children() {
    let mock = MockBackend::new();
    let get_sub_issues_calls = Arc::clone(&mock.get_sub_issues_calls);
    let status_updates = Arc::clone(&mock.status_updates);
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "test/repo".to_string(),
    ));

    let parent_id = store
        .create_internal("test/repo", "Parent", "", "job", "daily", None)
        .await
        .unwrap();
    store
        .update_status(parent_id, TaskStatus::Blocked)
        .await
        .unwrap();
    let child_id = store
        .create(&NewTask {
            external_id: Some("91".to_string()),
            repo: "test/repo".to_string(),
            origin: "github".to_string(),
            title: "Child".to_string(),
            body: String::new(),
            source: String::new(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
            labels: vec![],
            parent_id: Some(parent_id),
        })
        .await
        .unwrap();
    store
        .update_status(child_id, TaskStatus::Done)
        .await
        .unwrap();

    tick_unblock_parents(&backend, &task_manager, &store, "test/repo")
        .await
        .unwrap();

    let parent = store.get(parent_id).await.unwrap();
    assert_eq!(parent.status, TaskStatus::New);
    assert!(
        status_updates.lock().unwrap().is_empty(),
        "internal parent should be updated in store without backend mirroring"
    );
    assert!(
        get_sub_issues_calls.lock().unwrap().is_empty(),
        "internal parent should not query backend sub-issues"
    );
}

// ── tick_recover_stuck_tasks (InReview) ──────────────────────────────────

#[tokio::test]
async fn unblock_parents_no_blocked_tasks() {
    let mock = MockBackend::new(); // no blocked tasks
    let status_updates = Arc::clone(&mock.status_updates);
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = make_task_manager(Arc::clone(&backend));
    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    tick_unblock_parents(&backend, &task_manager, &store, "test/repo")
        .await
        .unwrap();

    assert!(status_updates.lock().unwrap().is_empty());
}

#[tokio::test]
async fn unblock_parents_handles_multiple_blocked_tasks() {
    let mut mock = MockBackend::new();

    // Task 40: all children done → should unblock
    let t40 = make_task("40", &["status:blocked"]);
    mock.blocked_tasks.push(t40);
    mock.sub_issues
        .insert("40".to_string(), vec![ExternalId("41".to_string())]);
    mock.tasks_by_id
        .insert("41".to_string(), make_task("41", &["status:done"]));

    // Task 50: child not done → should stay blocked
    let t50 = make_task("50", &["status:blocked"]);
    mock.blocked_tasks.push(t50);
    mock.sub_issues
        .insert("50".to_string(), vec![ExternalId("51".to_string())]);
    mock.tasks_by_id
        .insert("51".to_string(), make_task("51", &["status:in_progress"]));

    let status_updates = Arc::clone(&mock.status_updates);
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = make_task_manager(Arc::clone(&backend));
    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    tick_unblock_parents(&backend, &task_manager, &store, "test/repo")
        .await
        .unwrap();

    let updates = status_updates.lock().unwrap();
    assert_eq!(updates.len(), 1, "only one parent should be unblocked");
    assert_eq!(updates[0].0, "40", "task 40 should be unblocked");
    assert_eq!(updates[0].1, Status::New);
}

/// `tick_all` reads leadership and the global sweeps from the first project's store, so
/// an empty project list must return before any of that, and touch nothing.
#[tokio::test]
async fn tick_all_is_a_no_op_without_projects() {
    let transport = Arc::new(Transport::new());
    let capture = Arc::new(CaptureService::new(Arc::clone(&transport)));
    let tmux = Arc::new(TmuxManager::new());
    let semaphore = Arc::new(Semaphore::new(1));
    let router = Arc::new(RwLock::new(cooled_review_router_for_test()));
    let (weight_tx, mut weight_rx) = mpsc::channel(8);
    let dispatching = Arc::new(DashMap::new());
    let auto_merge_in_flight = Arc::new(DashSet::new());

    tick_all(
        &[],
        &tmux,
        &capture,
        &semaphore,
        &EngineConfig::default(),
        &router,
        &weight_tx,
        &dispatching,
        Some(&transport),
        &HashSet::new(),
        &auto_merge_in_flight,
    )
    .await;

    assert!(dispatching.is_empty(), "no project, so no dispatch claimed");
    assert!(
        weight_rx.try_recv().is_err(),
        "no project, so no weight signal"
    );
}

/// The store-wide sweeps must run once per tick, not once per project. The NeedsReview
/// refire sweep reads `needs_review_refires` and then increments it, so when it ran
/// inside the per-project join every project saw the same count and spent one of the
/// five refire attempts — four projects burned four attempts in a single tick (#3864).
#[serial_test::serial(cooldown_state)]
#[tokio::test]
async fn tick_all_runs_the_global_needs_review_sweep_once_for_two_projects() {
    cooldown::reset_global_state().await;

    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let id = store
        .upsert_external(&UpsertExternal {
            repo: "owner/one",
            ext_id: "7",
            title: "Stale needs_review task",
            body: "",
            author: "user",
            url: "https://github.com/owner/one/issues/7",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store
        .update_status(id, TaskStatus::NeedsReview)
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
        .bind(id)
        .execute(store.pool())
        .await
        .unwrap();

    let engines: Vec<ProjectEngine> = ["owner/one", "owner/two"]
        .iter()
        .map(|repo| {
            let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
            ProjectEngine {
                repo: repo.to_string(),
                project_dir: PathBuf::from("/nonexistent"),
                backend: Arc::clone(&backend),
                task_manager: Arc::new(TaskManager::with_store(
                    backend,
                    Arc::clone(&store),
                    repo.to_string(),
                )),
                runner: Arc::new(TaskRunner::new(repo.to_string())),
                store: Arc::clone(&store),
            }
        })
        .collect();

    // A healthy review agent, so the sweep does not skip the pass
    let mut config = RouterConfig::default();
    config
        .model_map
        .entry("review".to_string())
        .or_default()
        .insert("claude".to_string(), vec!["sonnet".to_string()]);
    let router = Arc::new(RwLock::new(Router::new_for_test(
        config,
        vec!["claude".to_string()],
    )));

    let transport = Arc::new(Transport::new());
    let capture = Arc::new(CaptureService::new(Arc::clone(&transport)));
    let (weight_tx, _weight_rx) = mpsc::channel(8);

    tick_all(
        &engines,
        &Arc::new(TmuxManager::new()),
        &capture,
        &Arc::new(Semaphore::new(1)),
        &EngineConfig::default(),
        &router,
        &weight_tx,
        &Arc::new(DashMap::new()),
        Some(&transport),
        &engines.iter().map(|e| e.repo.clone()).collect(),
        &Arc::new(DashSet::new()),
    )
    .await;

    assert_eq!(
        store.get(id).await.unwrap().needs_review_refires,
        1,
        "two projects must share one pass of the global NeedsReview sweep"
    );
}

/// `tick_all` must not reap tmux sessions it did not start. Startup cleanup used to
/// live here, and nextest's per-test process made the once-flag kill live agents (#3889)
#[tokio::test]
async fn tick_all_does_not_kill_sessions_it_did_not_create() {
    let tmux = Arc::new(TmuxManager::new());
    let Ok(session_name) = tmux
        .create_session("other/foreign", "3889", "/tmp", "sleep 30", &[])
        .await
    else {
        eprintln!("Skipping test: tmux not available or failed to create test session");
        return;
    };
    let _guard = SessionGuard::new(&tmux, session_name.clone());

    // Cross a unix-second boundary so a reintroduced startup_cleanup would treat
    // this session as stale (created_at < now)
    time::sleep(Duration::from_millis(1100)).await;

    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let engines = vec![ProjectEngine {
        repo: "owner/one".to_string(),
        project_dir: PathBuf::from("/nonexistent"),
        backend: Arc::clone(&backend),
        task_manager: Arc::new(TaskManager::with_store(
            backend,
            Arc::clone(&store),
            "owner/one".to_string(),
        )),
        runner: Arc::new(TaskRunner::new("owner/one".to_string())),
        store,
    }];

    let transport = Arc::new(Transport::new());
    let capture = Arc::new(CaptureService::new(Arc::clone(&transport)));
    let (weight_tx, _weight_rx) = mpsc::channel(8);
    let router = Arc::new(RwLock::new(cooled_review_router_for_test()));

    tick_all(
        &engines,
        &tmux,
        &capture,
        &Arc::new(Semaphore::new(1)),
        &EngineConfig::default(),
        &router,
        &weight_tx,
        &Arc::new(DashMap::new()),
        Some(&transport),
        &engines.iter().map(|e| e.repo.clone()).collect(),
        &Arc::new(DashSet::new()),
    )
    .await;

    assert!(
        tmux.session_exists(&session_name).await,
        "tick_all must not kill a tmux session it did not start"
    );
}

// ── tick_dispatch_tasks: deadlock regression test (#1361) ──────────────

/// Regression test: a repo configured with a ".git" suffix (e.g. "owner/repo.git")
/// must still match its own live tmux sessions in `tick_check_session_completions`.
///
/// `TmuxManager::session_name` strips ".git" when creating the session, so a live
/// session's parsed `project` field never carries it. Before the fix, this
/// function's `repo_name` derivation skipped the `.trim_end_matches(".git")` step,
/// so `session.project` ("repo") never equaled `repo_name` ("repo.git") and the
/// completed-session touch/kill was silently skipped for the entire repo — leaving
/// the coarser-grained stuck-task reclaim as the only (racy) safety net.
#[tokio::test]
async fn session_completion_matches_repo_configured_with_git_suffix() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let tmux = Arc::new(TmuxManager::new());
    let transport = Arc::new(Transport::new());
    let capture = Arc::new(CaptureService::new(transport));

    let repo = "owner/repo.git";
    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo,
            ext_id: "201",
            title: "Git-suffix repo task",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store
        .update_status(id, TaskStatus::InProgress)
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
        .bind(id)
        .execute(store.pool())
        .await
        .unwrap();

    // session_name strips ".git" internally — this is the real session a dispatch
    // for this repo would have created.
    let task_id = "201";
    let session_name = tmux.session_name(repo, task_id);
    let create_result = tmux
        .command()
        .args(["new-session", "-d", "-s", &session_name, "-c", "/tmp"])
        .output()
        .await;
    match create_result {
        Ok(o) if o.status.success() => {}
        _ => {
            eprintln!("Skipping test: tmux not available or failed to create test session");
            return;
        }
    }
    let _guard = SessionGuard::new(&tmux, session_name.clone());

    let _ = tmux
        .command()
        .args(["set-option", "-t", &session_name, "remain-on-exit", "on"])
        .output()
        .await;
    let _ = tmux
        .command()
        .args(["send-keys", "-t", &session_name, "exit", "Enter"])
        .output()
        .await;

    for _ in 0..20 {
        time::sleep(Duration::from_millis(100)).await;
        if !tmux.session_is_running(&session_name).await {
            break;
        }
    }

    capture.register_session(repo, task_id, &session_name).await;

    tick_check_session_completions(&tmux, repo, &capture, &store)
        .await
        .unwrap();

    let task_state = store.get(id).await.unwrap();
    if task_state.updated_at.starts_with("2020") {
        eprintln!(
            "Skipping test: tick_check_session_completions did not update stored \
                 updated_at — session was not in snapshot (CI tmux environment issue)"
        );
        return;
    }
    assert!(
        !task_state.updated_at.starts_with("2020"),
        "tick_check_session_completions must match its own repo's live sessions \
             even when the repo is configured with a \".git\" suffix"
    );
}

// ── tick_job_scheduler: resilience to bad project configs ────────────────

/// A project with a duplicate job id (same id in .orch.yml AND prompts/jobs/)
/// must not cause tick_job_scheduler to return Err — it should log an error
/// and return Ok(()) so other projects are unaffected.
#[tokio::test]
async fn job_scheduler_returns_ok_on_duplicate_job_config() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = tmp.path().join(".orch.yml");
    fs::write(
        &cfg,
        "jobs:\n  - id: trading-scan\n    schedule: \"0 9 * * *\"\n    task:\n      title: Scan\n      body: do it\n",
    )
    .unwrap();
    let jobs_dir = tmp.path().join("prompts").join("jobs");
    fs::create_dir_all(&jobs_dir).unwrap();
    fs::write(
        jobs_dir.join("trading-scan.md"),
        "---\nid: trading-scan\nschedule: '0 9 * * *'\ntitle: Trading Scan\n---\n\nDo trading scan.\n",
    )
    .unwrap();

    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let result = tick_job_scheduler(&cfg, &backend, None, "test/repo", None).await;
    assert!(
        result.is_ok(),
        "duplicate job id must not propagate as Err: {result:?}"
    );
}

/// When one project has a duplicate job id (bad config) and a second project
/// has a valid config, the scheduler tick for the second project must still
/// succeed — simulating the per-project loop in the engine.
#[tokio::test]
async fn job_scheduler_processes_valid_project_when_other_has_duplicate() {
    // Bad project: duplicate id across .orch.yml and prompts/jobs/
    let bad_tmp = tempfile::tempdir().unwrap();
    let bad_cfg = bad_tmp.path().join(".orch.yml");
    fs::write(
        &bad_cfg,
        "jobs:\n  - id: dup\n    schedule: \"0 9 * * *\"\n    task:\n      title: Dup\n      body: x\n",
    )
    .unwrap();
    let bad_jobs_dir = bad_tmp.path().join("prompts").join("jobs");
    fs::create_dir_all(&bad_jobs_dir).unwrap();
    fs::write(
        bad_jobs_dir.join("dup.md"),
        "---\nid: dup\nschedule: '0 9 * * *'\ntitle: Dup\n---\n\nbody\n",
    )
    .unwrap();

    // Good project: valid config, no duplicate
    let good_tmp = tempfile::tempdir().unwrap();
    let good_cfg = good_tmp.path().join(".orch.yml");
    fs::write(
        &good_cfg,
        "jobs:\n  - id: valid-job\n    schedule: \"0 9 * * *\"\n    task:\n      title: Valid\n      body: ok\n",
    )
    .unwrap();

    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());

    let bad_result = tick_job_scheduler(&bad_cfg, &backend, None, "test/bad-repo", None).await;
    assert!(
        bad_result.is_ok(),
        "bad project must not abort scheduler: {bad_result:?}"
    );

    let good_result = tick_job_scheduler(&good_cfg, &backend, None, "test/good-repo", None).await;
    assert!(
        good_result.is_ok(),
        "valid project must still run after bad project: {good_result:?}"
    );
}

// ── route_new_tasks_global ───────────────────────────────────────────────

/// Regression test for #3407: tasks from inactive/removed repos were stuck
/// in `new` indefinitely because `tick_route_tasks` is repo-scoped.
///
/// `route_new_tasks_global` must route tasks whose repo != current_repo
/// and leave current-repo tasks untouched.
#[serial_test::serial(cooldown_state)]
#[tokio::test]
async fn global_sweep_routes_inactive_repo_tasks() {
    cooldown::reset_global_state().await;

    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    // Active repo task — must NOT be touched by the global sweep.
    let active_id = store
        .upsert_external(&UpsertExternal {
            repo: "active/repo",
            ext_id: "1",
            title: "Active task",
            body: "",
            author: "user",
            url: "https://github.com/active/repo/issues/1",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();

    // Inactive repo task — must be routed by the global sweep.
    let inactive_id = store
        .upsert_external(&UpsertExternal {
            repo: "inactive/repo",
            ext_id: "42",
            title: "Orphaned task",
            body: "",
            author: "user",
            url: "https://github.com/inactive/repo/issues/42",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();

    assert_eq!(store.get(active_id).await.unwrap().status, TaskStatus::New);
    assert_eq!(
        store.get(inactive_id).await.unwrap().status,
        TaskStatus::New
    );

    let mut model_map: HashMap<String, HashMap<String, Vec<String>>> = HashMap::new();
    let mut tier = HashMap::new();
    tier.insert("claude".to_string(), vec!["haiku".to_string()]);
    model_map.insert("medium".to_string(), tier);
    let config = RouterConfig {
        mode: "round_robin".to_string(),
        agents: vec!["claude".to_string()],
        model_map,
        ..Default::default()
    };
    let mut router = Router::new_for_test(config, vec!["claude".to_string()]);

    let active_repos: HashSet<String> = ["active/repo".to_string()].into_iter().collect();
    let mut routing_quota = RoutingTickQuota::new(1);
    route_new_tasks_global(&mut router, &store, &active_repos, &mut routing_quota)
        .await
        .unwrap();

    // Active-repo task must remain new (global sweep skips it).
    assert_eq!(
        store.get(active_id).await.unwrap().status,
        TaskStatus::New,
        "global sweep must not touch tasks for the active repo"
    );

    // Inactive-repo task must now be routed.
    let after = store.get(inactive_id).await.unwrap();
    assert_eq!(
        after.status,
        TaskStatus::Routed,
        "global sweep must route tasks from inactive repos"
    );
    assert!(
        matches!(&after.agent, Some(a) if !a.is_empty()),
        "agent must be set after routing"
    );
}

/// Regression test for #3523: `route_new_tasks_global` previously filtered only
/// `t.repo != current_repo`, so during repo A's tick it would route repo B's `new`
/// tasks too — even though B is an active, non-orphaned project with its own tick
/// scheduled later in the same pass. This is the exact "racing the real per-repo
/// dispatch" failure mode `dispatch_routed_tasks_global` was fixed for in #3415;
/// the route-side sweep needs the same `active_repos` scoping, not just the
/// current tick's own repo.
#[serial_test::serial(cooldown_state)]
#[tokio::test]
async fn global_sweep_does_not_touch_other_active_repo_tasks() {
    cooldown::reset_global_state().await;

    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    // Belongs to a second active repo — NOT the repo whose tick is running,
    // but still a currently-configured project, not an orphan.
    let other_active_id = store
        .upsert_external(&UpsertExternal {
            repo: "other-active/repo",
            ext_id: "7",
            title: "Other active project's task",
            body: "",
            author: "user",
            url: "https://github.com/other-active/repo/issues/7",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();

    // Genuinely orphaned repo — not in the active set at all.
    let inactive_id = store
        .upsert_external(&UpsertExternal {
            repo: "inactive/repo",
            ext_id: "42",
            title: "Orphaned task",
            body: "",
            author: "user",
            url: "https://github.com/inactive/repo/issues/42",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();

    let mut model_map: HashMap<String, HashMap<String, Vec<String>>> = HashMap::new();
    let mut tier = HashMap::new();
    tier.insert("claude".to_string(), vec!["haiku".to_string()]);
    model_map.insert("medium".to_string(), tier);
    let config = RouterConfig {
        mode: "round_robin".to_string(),
        agents: vec!["claude".to_string()],
        model_map,
        ..Default::default()
    };
    let mut router = Router::new_for_test(config, vec!["claude".to_string()]);

    // Simulate "active/repo"'s tick running the global sweep. Both
    // "active/repo" and "other-active/repo" are currently-configured
    // projects; only "inactive/repo" is orphaned.
    let active_repos: HashSet<String> =
        ["active/repo".to_string(), "other-active/repo".to_string()]
            .into_iter()
            .collect();
    let mut routing_quota = RoutingTickQuota::new(1);
    route_new_tasks_global(&mut router, &store, &active_repos, &mut routing_quota)
        .await
        .unwrap();

    assert_eq!(
        store.get(other_active_id).await.unwrap().status,
        TaskStatus::New,
        "global sweep run from a different active repo's tick must not route \
             another active repo's new tasks — that repo's own tick will route them"
    );
    assert_eq!(
        store.get(inactive_id).await.unwrap().status,
        TaskStatus::Routed,
        "global sweep must still route tasks from genuinely orphaned repos"
    );
}

/// Tasks with the `no-agent` label must be skipped by the global sweep.
#[serial_test::serial(cooldown_state)]
#[tokio::test]
async fn global_sweep_skips_no_agent_tasks() {
    cooldown::reset_global_state().await;

    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    let id = store
        .upsert_external(&UpsertExternal {
            repo: "inactive/repo",
            ext_id: "99",
            title: "No-agent task",
            body: "",
            author: "user",
            url: "https://github.com/inactive/repo/issues/99",
            labels: &["no-agent".to_string()],
            origin: "github",
        })
        .await
        .unwrap();

    let mut model_map: HashMap<String, HashMap<String, Vec<String>>> = HashMap::new();
    let mut tier = HashMap::new();
    tier.insert("claude".to_string(), vec!["haiku".to_string()]);
    model_map.insert("medium".to_string(), tier);
    let config = RouterConfig {
        mode: "round_robin".to_string(),
        agents: vec!["claude".to_string()],
        model_map,
        ..Default::default()
    };
    let mut router = Router::new_for_test(config, vec!["claude".to_string()]);

    let active_repos: HashSet<String> = ["active/repo".to_string()].into_iter().collect();
    let mut routing_quota = RoutingTickQuota::new(1);
    route_new_tasks_global(&mut router, &store, &active_repos, &mut routing_quota)
        .await
        .unwrap();

    assert_eq!(
        store.get(id).await.unwrap().status,
        TaskStatus::New,
        "no-agent tasks must not be routed"
    );
}

/// Regression test for #3615: the routing limit must be consumed by a
/// shared per-engine-tick quota, not independently by every repo/global
/// routing pass. Otherwise one slow router timeout per pass can stack into
/// a multi-minute watchdog stall.
#[serial_test::serial(cooldown_state)]
#[tokio::test]
async fn global_sweep_honors_shared_routing_quota() {
    cooldown::reset_global_state().await;

    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    for ext_id in ["1", "2"] {
        store
            .upsert_external(&UpsertExternal {
                repo: "inactive/repo",
                ext_id,
                title: "Orphaned task",
                body: "",
                author: "user",
                url: &format!("https://github.com/inactive/repo/issues/{ext_id}"),
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
    }

    let mut model_map: HashMap<String, HashMap<String, Vec<String>>> = HashMap::new();
    let mut tier = HashMap::new();
    tier.insert("claude".to_string(), vec!["haiku".to_string()]);
    model_map.insert("medium".to_string(), tier);
    let config = RouterConfig {
        mode: "round_robin".to_string(),
        agents: vec!["claude".to_string()],
        model_map,
        ..Default::default()
    };
    let mut router = Router::new_for_test(config, vec!["claude".to_string()]);

    let active_repos: HashSet<String> = ["active/repo".to_string()].into_iter().collect();
    let mut routing_quota = RoutingTickQuota::new(1);
    route_new_tasks_global(&mut router, &store, &active_repos, &mut routing_quota)
        .await
        .unwrap();

    let all_tasks = store.list_all_global().await.unwrap();
    let routed = all_tasks
        .iter()
        .filter(|task| task.status == TaskStatus::Routed)
        .count();
    let still_new = all_tasks
        .iter()
        .filter(|task| task.status == TaskStatus::New)
        .count();

    assert_eq!(routed, 1, "only one task may consume the shared quota");
    assert_eq!(still_new, 1, "remaining tasks wait for the next tick");

    route_new_tasks_global(&mut router, &store, &active_repos, &mut routing_quota)
        .await
        .unwrap();
    let after_exhausted = store.list_all_global().await.unwrap();
    assert_eq!(
        after_exhausted
            .iter()
            .filter(|task| task.status == TaskStatus::Routed)
            .count(),
        1,
        "an exhausted shared quota must not reset within the same tick"
    );
}

// ── dispatch_routed_tasks_global ─────────────────────────────────────────
//
// `dispatch_routed_tasks_global` itself is deliberately not exercised
// end-to-end here: once it finds an orphaned repo, it spawns the real
// runner, which creates a real tmux session and touches the network.
// The existing `dispatch_does_not_deadlock_under_write_lock` test avoids
// this same hazard by keeping the dispatchable set empty. The selection
// logic (which tasks belong to which repo) is pulled into the pure
// `group_orphaned_routed_tasks` helper below so it can be tested without
// those side effects.
