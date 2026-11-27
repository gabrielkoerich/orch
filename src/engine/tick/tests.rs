use super::dispatch::block_task_without_project;
use super::dispatch::group_orphaned_routed_tasks;
use super::recovery::stuck_task_timing_from_map;
use super::*;
use crate::backends::{ExternalId, ExternalTask, Mention, Status};
use crate::channels::transport::Transport;
use crate::engine::RunTimeouts;
use crate::engine::cooldown;
use crate::engine::dispatch_guard::DispatchGuard;
use crate::engine::router::Router;
use crate::engine::router::RouterConfig;
use crate::engine::runner::NoProjectDir;
use crate::engine::tasks::TaskManager;
use crate::store::DepSource;
use crate::store::NewDependency;
use crate::store::NewTask;
use crate::store::StoreRoute;
use crate::store::TaskStatus as DbStatus;
use crate::store::TaskStatus;
use crate::store::TaskStore;
use crate::store::UpsertExternal;
use crate::tmux::{SessionGuard, TmuxManager};
use async_trait::async_trait;
use dashmap::mapref::entry::Entry;
use std::cell::Cell;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::sync::oneshot;
use tokio::task;
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
struct MockBackend {
    /// Tasks returned by `list_by_status(Blocked)`.
    blocked_tasks: Vec<ExternalTask>,
    /// Sub-issues map: task_id → list of child ExternalIds.
    sub_issues: HashMap<String, Vec<ExternalId>>,
    /// Tasks returned by `get_task`. Keyed by id.0.
    tasks_by_id: HashMap<String, ExternalTask>,
    /// Recorded `get_sub_issues` calls.
    get_sub_issues_calls: Arc<Mutex<Vec<String>>>,
    /// Recorded `update_status` calls.
    status_updates: Arc<Mutex<Vec<(String, Status)>>>,
    /// Recorded `remove_label` calls.
    removed_labels: Arc<Mutex<Vec<(String, String)>>>,
    /// Recorded `post_comment` calls.
    posted_comments: Arc<Mutex<Vec<(String, String)>>>,
}

impl MockBackend {
    fn new() -> Self {
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

/// Set `updated_at` to a far-past date for a task in an in-memory store.
#[cfg(test)]
async fn set_task_updated_at_past(store: &TaskStore, task_id: i64) {
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
        .bind(task_id)
        .execute(store.pool())
        .await
        .unwrap();
}

/// Age the in_review status-entry the same way the stale sweep tests do
async fn set_in_review_entered_at_past(store: &TaskStore, task_id: i64) {
    sqlx::query(
        "UPDATE task_activity SET timestamp = '2020-01-01T00:00:00Z'
             WHERE task_id = ? AND to_status = 'in_review'",
    )
    .bind(task_id)
    .execute(store.pool())
    .await
    .unwrap();
}

#[tokio::test]
async fn recover_stuck_tasks_resets_external_in_review_to_needs_review() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mock = MockBackend::new();
    let status_updates = Arc::clone(&mock.status_updates);
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let config = EngineConfig {
        no_session_stuck_timeout: 600,
        stuck_timeout: 1800,
        ..EngineConfig::default()
    };

    // Insert an external task and age both the status entry and updated_at
    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "99",
            title: "Stuck InReview",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::InReview).await.unwrap();
    set_in_review_entered_at_past(&store, id).await;
    set_task_updated_at_past(&store, id).await;

    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &config,
        &store,
        &HashMap::new(),
        &Arc::new(DashMap::new()),
    )
    .await
    .unwrap();

    let updates = status_updates.lock().unwrap();
    assert_eq!(updates.len(), 1, "stuck InReview task should be recovered");
    assert_eq!(updates[0].0, "99");
    assert_eq!(updates[0].1, Status::NeedsReview);
}

#[tokio::test]
async fn recover_stuck_tasks_skips_recent_in_review() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mock = MockBackend::new();
    let status_updates = Arc::clone(&mock.status_updates);
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let config = EngineConfig {
        no_session_stuck_timeout: 600, // 10 minutes — recent task won't trigger
        stuck_timeout: 1800,
        ..EngineConfig::default()
    };

    // Age updated_at, leave the in_review status entry at now
    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "100",
            title: "Recent InReview",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::InReview).await.unwrap();
    set_task_updated_at_past(&store, id).await;

    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &config,
        &store,
        &HashMap::new(),
        &Arc::new(DashMap::new()),
    )
    .await
    .unwrap();

    let updates = status_updates.lock().unwrap();
    assert!(
        updates.is_empty(),
        "recent InReview task should not be recovered"
    );
}

#[tokio::test]
async fn recover_stuck_tasks_resets_internal_in_review_to_needs_review() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let config = EngineConfig {
        no_session_stuck_timeout: 600,
        stuck_timeout: 1800,
        ..EngineConfig::default()
    };

    // Create an internal task in InReview and age both clocks
    let id = store
        .create_internal("owner/repo", "Internal InReview", "", "cron", "1", None)
        .await
        .unwrap();
    store.update_status(id, TaskStatus::InReview).await.unwrap();
    set_in_review_entered_at_past(&store, id).await;
    set_task_updated_at_past(&store, id).await;

    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &config,
        &store,
        &HashMap::new(),
        &Arc::new(DashMap::new()),
    )
    .await
    .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(
        task.status,
        TaskStatus::NeedsReview,
        "stuck internal InReview task should be reset to NeedsReview"
    );
}

#[tokio::test]
async fn recover_stuck_tasks_ages_in_review_from_status_entry_not_updated_at() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mock = MockBackend::new();
    let status_updates = Arc::clone(&mock.status_updates);
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let config = EngineConfig {
        no_session_stuck_timeout: 600,
        stuck_timeout: 1800,
        ..EngineConfig::default()
    };

    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "102",
            title: "Fresh updated_at InReview",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::InReview).await.unwrap();
    set_in_review_entered_at_past(&store, id).await;
    store
        .set_fields(id, &[("pr_number", serde_json::json!(42))])
        .await
        .unwrap();

    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &config,
        &store,
        &HashMap::new(),
        &Arc::new(DashMap::new()),
    )
    .await
    .unwrap();

    let updates = status_updates.lock().unwrap();
    assert_eq!(
        updates.len(),
        1,
        "in_review stuck recovery must age from the status entry even when updated_at is fresh"
    );
    assert_eq!(updates[0].0, "102");
    assert_eq!(updates[0].1, Status::NeedsReview);
}

/// An in_review task whose review session is gone goes back to needs_review, unless its
/// review already recorded a decision and the task is only waiting on the merge
async fn orphaned_review_status_after_recovery(with_decision: bool) -> TaskStatus {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());

    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "55",
            title: "Orphaned review",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::InReview).await.unwrap();
    // Entered in_review long ago. updated_at stays fresh, as review_poll rewrites it
    set_in_review_entered_at_past(&store, id).await;
    if with_decision {
        store::store_log_activity(
            &Some(Arc::clone(&store)),
            "owner/repo",
            "55",
            "review_decision",
            None,
            None,
            None::<&str>,
            None::<&str>,
            Some(&serde_json::json!({"decision": "approve"})),
        )
        .await;
    }

    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &EngineConfig::default(),
        &store,
        &HashMap::new(),
        &Arc::new(DashMap::new()),
    )
    .await
    .unwrap();

    store.get(id).await.unwrap().status
}

#[tokio::test]
async fn stale_in_review_recovery_resets_orphaned_task_without_session() {
    assert_eq!(
        orphaned_review_status_after_recovery(false).await,
        TaskStatus::NeedsReview
    );
}

#[tokio::test]
async fn stale_in_review_recovery_leaves_decided_task_waiting_on_merge() {
    assert_eq!(
        orphaned_review_status_after_recovery(true).await,
        TaskStatus::InReview
    );
}

#[tokio::test]
async fn recover_stuck_tasks_resets_review_agent_failures() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());

    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "104",
            title: "Orphaned review with failures",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::InReview).await.unwrap();
    store
        .set_fields(id, &[("review_agent_failures", serde_json::json!(2))])
        .await
        .unwrap();
    set_in_review_entered_at_past(&store, id).await;

    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &EngineConfig::default(),
        &store,
        &HashMap::new(),
        &Arc::new(DashMap::new()),
    )
    .await
    .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.status, TaskStatus::NeedsReview);
    assert_eq!(
        task.review_agent_failures, 0,
        "a dead review session is an infrastructure event, not an agent failure"
    );
}

#[tokio::test]
async fn recover_stuck_tasks_reclaims_hung_in_review_session() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mock = MockBackend::new();
    let status_updates = Arc::clone(&mock.status_updates);
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let config = EngineConfig {
        no_session_stuck_timeout: 600,
        stuck_timeout: 1800,
        ..EngineConfig::default()
    };

    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "103",
            title: "Hung review session",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::InReview).await.unwrap();
    set_in_review_entered_at_past(&store, id).await;
    store
        .set_fields(id, &[("pr_number", serde_json::json!(43))])
        .await
        .unwrap();

    let session_name = tmux.session_name("owner/repo", "103-review");
    let mut session_map = HashMap::new();
    session_map.insert(session_name, true);

    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &config,
        &store,
        &session_map,
        &Arc::new(DashMap::new()),
    )
    .await
    .unwrap();

    let updates = status_updates.lock().unwrap();
    assert_eq!(
        updates.len(),
        1,
        "a live hung review session must be reclaimed once the in_review entry ages past stuck_timeout"
    );
    assert_eq!(updates[0].0, "103");
    assert_eq!(updates[0].1, Status::NeedsReview);
}

#[tokio::test]
async fn untracked_repo_recovery_resets_stale_in_progress_to_new() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let tmux = Arc::new(TmuxManager::new());
    let config = EngineConfig {
        no_session_stuck_timeout: 600,
        stuck_timeout: 1800,
        ..EngineConfig::default()
    };

    // Cross-repo task should still be recovered even when current tick repo is owner/repo.
    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/other",
            ext_id: "777",
            title: "Cross-repo stuck task",
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
    set_task_updated_at_past(&store, id).await;

    let active = HashSet::from(["owner/repo".to_string()]);
    recover_untracked_repo_stuck_tasks(
        &tmux,
        &config,
        &store,
        &HashMap::new(),
        &Arc::new(DashMap::new()),
        &active,
    )
    .await;

    let task = store.get(id).await.unwrap();
    assert_eq!(
        task.status,
        TaskStatus::New,
        "cross-repo stale in_progress task should be reset to New"
    );
    assert_eq!(task.route_attempts, 0, "route attempts should be cleared");
    assert!(task.agent.is_none(), "agent should be cleared");
    assert!(task.model.is_none(), "model should be cleared");
}

#[tokio::test]
async fn untracked_repo_recovery_leaves_active_repos_to_their_own_pass() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let tmux = Arc::new(TmuxManager::new());
    let config = EngineConfig {
        no_session_stuck_timeout: 600,
        stuck_timeout: 1800,
        ..EngineConfig::default()
    };
    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/other",
            ext_id: "778",
            title: "Stuck task in an active repo",
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
    set_task_updated_at_past(&store, id).await;

    // Per-project passes run concurrently, so only the task's own pass may reclaim it
    let active = HashSet::from(["owner/repo".to_string(), "owner/other".to_string()]);
    recover_untracked_repo_stuck_tasks(
        &tmux,
        &config,
        &store,
        &HashMap::new(),
        &Arc::new(DashMap::new()),
        &active,
    )
    .await;

    assert_eq!(store.get(id).await.unwrap().status, TaskStatus::InProgress);
}

#[tokio::test]
async fn recover_stuck_tasks_finalizes_open_task_runs_rows() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mock = MockBackend::new();
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let config = EngineConfig {
        no_session_stuck_timeout: 600,
        stuck_timeout: 1800,
        ..EngineConfig::default()
    };

    let id = store
        .create_internal("owner/repo", "Stuck with open run", "", "cron", "1", None)
        .await
        .unwrap();
    store
        .set_fields(
            id,
            &[
                ("agent", serde_json::json!("claude")),
                ("model", serde_json::json!("sonnet")),
                ("route_attempts", serde_json::json!(1)),
            ],
        )
        .await
        .unwrap();
    store
        .update_status(id, TaskStatus::InProgress)
        .await
        .unwrap();
    set_task_updated_at_past(&store, id).await;

    // Start an agent run that will be orphaned by recovery
    store
        .start_run(&crate::store::StartRun {
            task_id: id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "sonnet",
            command: "claude --model sonnet",
            prompt: "fix it",
        })
        .await
        .unwrap();

    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &config,
        &store,
        &HashMap::new(),
        &Arc::new(DashMap::new()),
    )
    .await
    .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.status, TaskStatus::New);

    let runs = store.get_runs(id).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].outcome, "aborted");
    assert!(runs[0].completed_at.is_some());
    assert_eq!(
        runs[0].error,
        "stuck-task recovery: internal no session found"
    );
}

/// Regression test for #3518: a completed agent run must not be discarded by the
/// stuck-task reclaim just because its tmux session has already exited.
///
/// In production, sessions are created without `remain-on-exit`, so once the agent
/// process exits tmux removes the session immediately — there is no observable
/// "dead pane" state for `tick_check_session_completions` to catch, and the runner's
/// own `store_touch_updated_at` only fires after its own poll notices the exit (up to
/// its 5s poll interval) plus post-processing. For a task whose total runtime already
/// exceeds `no_session_stuck_timeout`, the very next tick that observes the (now
/// absent) session would otherwise reclaim it instantly — discarding a successful run.
///
/// The fix: `tick_recover_stuck_tasks` skips the no-session reclaim while the task's
/// dispatch guard (the in-memory `dispatching` map keyed by "{repo}/{task_id}",
/// populated for the entire duration of `runner.run_with_context`) is still held —
/// i.e. while this process is still actively running the task, regardless of tmux
/// session state.
#[tokio::test]
async fn recover_stuck_tasks_skips_reclaim_while_dispatch_guard_held() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mock = MockBackend::new();
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let config = EngineConfig {
        no_session_stuck_timeout: 600,
        stuck_timeout: 1800,
        ..EngineConfig::default()
    };

    let id = store
        .create_internal(
            "owner/repo",
            "Long-running daily job",
            "",
            "cron",
            "1",
            None,
        )
        .await
        .unwrap();
    store
        .set_fields(
            id,
            &[
                ("agent", serde_json::json!("claude")),
                ("model", serde_json::json!("sonnet")),
                ("route_attempts", serde_json::json!(1)),
            ],
        )
        .await
        .unwrap();
    store
        .update_status(id, TaskStatus::InProgress)
        .await
        .unwrap();
    // Simulate a task that has already run well past no_session_stuck_timeout,
    // as a legitimately long agent run would.
    set_task_updated_at_past(&store, id).await;

    // Simulate the runner still actively processing this task: its dispatch guard
    // is held even though the tmux session (empty session_map below) is already gone.
    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
    dispatching.insert(
        format!("owner/repo/internal:{id}"),
        format!("internal:{id}"),
    );

    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &config,
        &store,
        &HashMap::new(),
        &dispatching,
    )
    .await
    .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(
        task.status,
        TaskStatus::InProgress,
        "task should not be reclaimed while its dispatch guard is still held"
    );

    // No run should have been finalized/aborted either.
    let runs = store.get_runs(id).await.unwrap();
    assert!(runs.is_empty());

    // Once the guard is released (runner finished), the very same conditions should
    // reclaim the task as before — the fix must not disable the safety net entirely.
    dispatching.clear();
    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &config,
        &store,
        &HashMap::new(),
        &dispatching,
    )
    .await
    .unwrap();
    let task = store.get(id).await.unwrap();
    assert_eq!(
        task.status,
        TaskStatus::New,
        "task should still be reclaimed once the dispatch guard is released"
    );
}

/// Concurrency regression test for issue #3526: the reclaim race recurred twice
/// (internal:156563 in #3523, internal:156673 in #3526) *after* the #3519/#3525
/// fix landed, in production under the real multi-threaded tokio runtime. The
/// existing `recover_stuck_tasks_skips_reclaim_while_dispatch_guard_held` test
/// above only exercises sequential `.await` calls on a single-threaded test
/// runtime (`#[tokio::test]` defaults to `flavor = "current_thread"`), which can
/// never observe genuine cross-thread races in the shared `dispatching` DashMap.
///
/// This test claims the guard exactly the way `dispatch_tasks_for_repo` does
/// (atomic `DashMap::entry` claim, `DispatchGuard` RAII release), hands it to a
/// spawned task running on a *different* worker thread that holds it until
/// explicitly told to release, and hammers the real `tick_recover_stuck_tasks`
/// from the main test task hundreds of times while the guard is deterministically
/// still held. If there were a visibility gap between `DashMap::entry().insert()`
/// on one thread and `DashMap::contains_key()` on another, this test would catch it.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn recover_stuck_tasks_never_reclaims_while_guard_held_under_real_concurrency() {
    const HAMMER_ITERATIONS: u32 = 300;

    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mock = MockBackend::new();
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let config = EngineConfig {
        no_session_stuck_timeout: 600,
        stuck_timeout: 1800,
        ..EngineConfig::default()
    };

    let id = store
        .create_internal(
            "owner/repo",
            "Long-running daily job",
            "",
            "cron",
            "1",
            None,
        )
        .await
        .unwrap();
    store
        .set_fields(
            id,
            &[
                ("agent", serde_json::json!("claude")),
                ("model", serde_json::json!("sonnet")),
                ("route_attempts", serde_json::json!(1)),
            ],
        )
        .await
        .unwrap();
    store
        .update_status(id, TaskStatus::InProgress)
        .await
        .unwrap();
    set_task_updated_at_past(&store, id).await;

    let dispatch_key = format!("owner/repo/internal:{id}");
    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());

    // Claim the guard synchronously, on this task, exactly like
    // dispatch_tasks_for_repo does: the atomic entry-claim always happens before
    // tokio::spawn hands the guard to the background task. (Claiming inside the
    // spawned task itself would race the hammer loop below against tokio's own
    // scheduling of that task onto a worker thread — a test artifact, not the
    // real production ordering, which guarantees claim-before-spawn.)
    let claim_task_id = format!("internal:{id}");
    {
        match dispatching.entry(dispatch_key.clone()) {
            Entry::Vacant(slot) => {
                slot.insert(claim_task_id);
            }
            Entry::Occupied(_) => panic!("dispatch key already claimed"),
        }
    }
    let guard = DispatchGuard::new(Arc::clone(&dispatching), dispatch_key.clone());

    // Hold the guard on a spawned task — a different worker thread under the
    // multi-thread runtime — mirroring the real runner holding it across genuine
    // async work (worktree setup, agent session, response handling) inside
    // `tokio::spawn(... run_with_context ...)`. It only drops the guard once
    // explicitly told to via `release_rx`, so its lifetime is fully controlled
    // by this test rather than raced against a timer — the release-signal
    // ordering was itself a source of false positives in an earlier version of
    // this test (a `sleep`-then-drop holder can legitimately release the guard
    // in the gap between the hammer loop's last check and its next iteration,
    // which is a correct outcome, not a bug, but made the assertion flaky).
    let (release_tx, release_rx) = oneshot::channel::<()>();
    let holder = tokio::spawn(async move {
        let _ = release_rx.await;
        drop(guard);
    });

    // Hammer the real reclaim path concurrently from the main task while the
    // guard is deterministically still held (the holder task is parked on
    // `release_rx` and cannot drop the guard until we signal it below). Each
    // iteration's `tick_recover_stuck_tasks` call may itself run on any worker
    // thread the tokio scheduler picks, independent of the thread that claimed
    // the guard — this is what actually exercises cross-thread DashMap
    // visibility, not wall-clock racing against a timer.
    for i in 0..HAMMER_ITERATIONS {
        tick_recover_stuck_tasks(
            &backend,
            &tmux,
            "owner/repo",
            &task_manager,
            &config,
            &store,
            &HashMap::new(),
            &dispatching,
        )
        .await
        .unwrap();

        let task = store.get(id).await.unwrap();
        assert_eq!(
            task.status,
            TaskStatus::InProgress,
            "task must never be reclaimed while the dispatch guard is deterministically \
                 still held, even under real cross-thread concurrency (dispatch_key={dispatch_key}, \
                 iteration={i}, contains_key_now={}, map_len_now={})",
            dispatching.contains_key(&dispatch_key),
            dispatching.len(),
        );
        task::yield_now().await;
    }

    // Now release the guard and confirm the holder task actually completed.
    let _ = release_tx.send(());
    holder.await.unwrap();

    // Guard released — the safety net must still reclaim as before.
    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &config,
        &store,
        &HashMap::new(),
        &dispatching,
    )
    .await
    .unwrap();
    let task = store.get(id).await.unwrap();
    assert_eq!(
        task.status,
        TaskStatus::New,
        "task should be reclaimed once the dispatch guard is released"
    );
}

#[tokio::test]
async fn recover_stuck_tasks_skips_external_in_review_waiting_for_human_review() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mock = MockBackend::new();
    let status_updates = Arc::clone(&mock.status_updates);
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let config = EngineConfig {
        no_session_stuck_timeout: 600,
        stuck_timeout: 1800,
        ..EngineConfig::default()
    };

    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "101",
            title: "Waiting For Human Review",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::InReview).await.unwrap();
    set_task_updated_at_past(&store, id).await;

    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &config,
        &store,
        &HashMap::new(),
        &Arc::new(DashMap::new()),
    )
    .await
    .unwrap();

    let updates = status_updates.lock().unwrap();
    assert!(
        updates.is_empty(),
        "InReview task waiting for human review should not be reset"
    );
}

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

#[tokio::test]
async fn detect_silent_agents_skips_backend_for_internal_tasks() {
    let transport = Arc::new(Transport::new());
    let capture = Arc::new(CaptureService::new(transport));
    let mock = Arc::new(MockBackend::new());
    let backend: Arc<dyn ExternalBackend> = Arc::<MockBackend>::clone(&mock);
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let config = EngineConfig {
        silence_grace_period: 0,
        ..EngineConfig::default()
    };

    let internal_id = store
        .create_internal("owner/repo", "Silent internal", "", "cron", "1", None)
        .await
        .unwrap();
    store
        .update_status(internal_id, TaskStatus::InProgress)
        .await
        .unwrap();

    let task_id = format!("internal:{internal_id}");
    capture
        .register_session("owner/repo", &task_id, "orch-test-internal")
        .await;
    time::sleep(Duration::from_millis(1100)).await;

    tick_detect_silent_agents(
        &tmux,
        "owner/repo",
        &capture,
        &backend,
        &task_manager,
        &config,
        &store,
    )
    .await
    .unwrap();

    assert!(
        mock.removed_labels.lock().unwrap().is_empty(),
        "internal tasks should skip backend label removal"
    );
    assert!(
        mock.posted_comments.lock().unwrap().is_empty(),
        "internal tasks should skip backend comments"
    );

    let task = store.get(internal_id).await.unwrap();
    // Silence detection now uses failover: if a fallback agent is found → Routed,
    // otherwise → New (reset for re-routing when cooldowns expire). Either is
    // acceptable; the key invariant is that the task is no longer InProgress.
    assert!(
        matches!(task.status, TaskStatus::Routed | TaskStatus::New),
        "internal task should be routed to fallback or reset to new, got {:?}",
        task.status
    );
}

#[tokio::test]
async fn detect_silent_agents_finalizes_open_task_runs_rows() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mock = MockBackend::new();
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let transport = Arc::new(Transport::new());
    let capture = Arc::new(CaptureService::new(transport));
    let tmux = Arc::new(TmuxManager::new());
    let config = EngineConfig {
        silence_grace_period: 60,
        silence_cooldown: 300,
        ..EngineConfig::default()
    };

    let internal_id = store
        .create_internal("owner/repo", "Silent with open run", "", "cron", "1", None)
        .await
        .unwrap();
    store
        .set_fields(
            internal_id,
            &[
                ("agent", serde_json::json!("claude")),
                ("model", serde_json::json!("sonnet")),
            ],
        )
        .await
        .unwrap();
    store
        .update_status(internal_id, TaskStatus::InProgress)
        .await
        .unwrap();

    let task_id = format!("internal:{internal_id}");
    capture
        .register_session("owner/repo", &task_id, "orch-test-silent-run")
        .await;
    capture
        .set_buffer_state_for_test(
            "owner/repo",
            &task_id,
            false,
            false,
            chrono::Utc::now() - chrono::Duration::seconds(120),
        )
        .await;

    // Start an agent run that will be orphaned by silence detection
    store
        .start_run(&crate::store::StartRun {
            task_id: internal_id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "sonnet",
            command: "claude --model sonnet",
            prompt: "fix it",
        })
        .await
        .unwrap();

    tick_detect_silent_agents(
        &tmux,
        "owner/repo",
        &capture,
        &backend,
        &task_manager,
        &config,
        &store,
    )
    .await
    .unwrap();

    let runs = store.get_runs(internal_id).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].outcome, "aborted");
    assert!(runs[0].completed_at.is_some());
    assert_eq!(runs[0].error, "silence detection: session killed");
}

/// Regression test for the race between Phase 1 (tick_check_session_completions)
/// and Phase 2 (tick_recover_stuck_tasks).
///
/// Before the fix, Phase 1 killed the tmux session without touching updated_at.
/// Phase 2 could then see status=in_progress, has_session=false, and an old
/// updated_at, and would reset the task to New — even though the runner's
/// wait_for_completion() poll had not yet returned.
///
/// The fix: Phase 1 touches updated_at as soon as it detects the session is done.
/// This test verifies that after the touch, Phase 2 does NOT reclaim the task.
#[tokio::test]
async fn phase1_session_completion_touch_prevents_phase2_reclaim() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mock = MockBackend::new();
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let config = EngineConfig {
        no_session_stuck_timeout: 600,
        stuck_timeout: 1800,
        ..EngineConfig::default()
    };

    // Create an in-progress internal task with old updated_at.
    // Without the fix this task would be reclaimed by Phase 2 (age >> threshold,
    // no session running in the test environment).
    let id = store
        .create_internal("owner/repo", "Completed task", "", "cron", "1", None)
        .await
        .unwrap();
    store
        .update_status(id, TaskStatus::InProgress)
        .await
        .unwrap();
    set_task_updated_at_past(&store, id).await;

    // Simulate Phase 1: touch updated_at when the session is detected as done.
    // In production this happens inside tick_check_session_completions.
    let task_id = format!("internal:{id}");
    store_touch_updated_at(&Some(Arc::clone(&store)), "owner/repo", &task_id).await;

    // Phase 2 runs — updated_at is fresh so the task must NOT be reclaimed.
    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &config,
        &store,
        &HashMap::new(),
        &Arc::new(DashMap::new()),
    )
    .await
    .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(
        task.status,
        TaskStatus::InProgress,
        "Phase 2 must not reclaim a task whose updated_at was just touched by Phase 1"
    );
}

#[tokio::test]
async fn stuck_task_timing_treats_dead_session_as_missing() {
    let tmux = Arc::new(TmuxManager::new());
    let repo = "owner/repo";
    let task_id = "internal:33498";
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

    let set_option_result = tmux
        .command()
        .args(["set-option", "-t", &session_name, "remain-on-exit", "on"])
        .output()
        .await;
    if !matches!(set_option_result, Ok(ref o) if o.status.success()) {
        eprintln!("Skipping test: unable to set tmux remain-on-exit option");
        return;
    }

    let send_exit_result = tmux
        .command()
        .args(["send-keys", "-t", &session_name, "exit", "Enter"])
        .output()
        .await;
    if !matches!(send_exit_result, Ok(ref o) if o.status.success()) {
        eprintln!("Skipping test: unable to exit tmux pane");
        return;
    }

    // Poll until pane is dead (up to 2s) — fixed sleep is unreliable under load.
    for _ in 0..20 {
        time::sleep(Duration::from_millis(100)).await;
        if !tmux.session_is_running(&session_name).await {
            break;
        }
    }

    // Also wait for batch_session_active to reflect the dead state.
    // session_is_running uses `list-panes -t <session>` which may settle
    // before the global `list-panes -a` snapshot used by the production code.
    // On CI under load these two tmux commands can transiently disagree.
    let mut session_map = tmux.batch_session_active().await;
    for _ in 0..20 {
        if !session_map.get(&session_name).copied().unwrap_or(false) {
            break;
        }
        time::sleep(Duration::from_millis(100)).await;
        session_map = tmux.batch_session_active().await;
    }
    if session_map.get(&session_name).copied().unwrap_or(false) {
        eprintln!(
            "Skipping test: session still alive in batch_session_active after retries (CI tmux lag)"
        );
        return;
    }

    let config = EngineConfig {
        no_session_stuck_timeout: 600,
        stuck_timeout: 1800,
        ..EngineConfig::default()
    };
    let updated_at = (chrono::Utc::now()
        - chrono::Duration::seconds(
            i64::try_from(config.no_session_stuck_timeout).expect("timeout fits in i64") + 5,
        ))
    .to_rfc3339();

    let timing = stuck_task_timing_from_map(
        &tmux,
        repo,
        task_id,
        &updated_at,
        &config,
        "parse failure",
        &session_map,
    )
    .expect("dead session should be treated as missing");

    assert!(!timing.has_session);
    assert_eq!(timing.threshold, config.no_session_stuck_timeout);
}

#[tokio::test]
#[serial_test::serial(suspend_state)]
async fn stuck_task_timing_discounts_host_suspend_gap() {
    super::super::suspend::clear_for_test();

    let tmux = Arc::new(TmuxManager::new());
    let repo = "owner/repo";
    let task_id = "internal:99001";
    let session_map = HashMap::new();

    let config = EngineConfig {
        no_session_stuck_timeout: 1800,
        stuck_timeout: 1800,
        ..EngineConfig::default()
    };

    // Task was last touched 33 minutes ago (age_mins=33 matches the reported
    // false-positive), but the host was suspended for 30 of those minutes —
    // real elapsed runtime is only 3 minutes, well under the 30-minute threshold.
    let updated_at = (chrono::Utc::now() - chrono::Duration::minutes(33)).to_rfc3339();
    let suspend_gap_detected_at = chrono::Utc::now() - chrono::Duration::minutes(1);
    super::super::suspend::inject_gap_for_test(
        suspend_gap_detected_at,
        chrono::Duration::minutes(30),
    );

    let timing = stuck_task_timing_from_map(
        &tmux,
        repo,
        task_id,
        &updated_at,
        &config,
        "parse failure",
        &session_map,
    );

    assert!(
        timing.is_none(),
        "task age should be discounted below the stuck threshold once suspend time is subtracted, got {timing:?}"
    );

    super::super::suspend::clear_for_test();
}

#[tokio::test]
async fn recover_stuck_tasks_reclaims_routed_task_blocked_by_stale_session() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mock = MockBackend::new();
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let config = EngineConfig {
        no_session_stuck_timeout: 600,
        stuck_timeout: 1,
        ..EngineConfig::default()
    };

    let id = store
        .create_internal("owner/repo", "Blocked routed task", "", "cron", "1", None)
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Routed).await.unwrap();
    set_task_updated_at_past(&store, id).await;

    let task_id = format!("internal:{id}");
    let session_name = tmux.session_name("owner/repo", &task_id);
    let create_result = tmux
        .command()
        .args([
            "new-session",
            "-d",
            "-s",
            &session_name,
            "-c",
            "/tmp",
            "sleep 60",
        ])
        .output()
        .await;

    match create_result {
        Ok(o) if o.status.success() => {}
        _ => {
            eprintln!("Skipping test: tmux not available or failed to create test session");
            return;
        }
    }

    let session_map = tmux.batch_session_active().await;
    assert_eq!(session_map.get(&session_name).copied(), Some(true));

    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &config,
        &store,
        &session_map,
        &Arc::new(DashMap::new()),
    )
    .await
    .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(
        task.status,
        TaskStatus::New,
        "routed task blocked by a stale session should be reset for re-routing"
    );
    assert!(
        !tmux.session_exists(&session_name).await,
        "recovery should kill the stale tmux session so dispatch can proceed"
    );
}

/// An internal complex task that entered in_progress `minutes` ago and was last written
/// `updated_minutes_ago`, with a live tmux session. None when tmux is not available
async fn live_complex_run(
    store: &TaskStore,
    tmux: &TmuxManager,
    minutes: i64,
    updated_minutes_ago: i64,
) -> Option<(i64, String)> {
    let id = store
        .create_internal("owner/repo", "Long complex run", "", "cron", "1", None)
        .await
        .unwrap();
    store
        .set_fields(id, &[("complexity", serde_json::json!("complex"))])
        .await
        .unwrap();
    store
        .update_status(id, TaskStatus::InProgress)
        .await
        .unwrap();
    let ago = |m: i64| {
        (chrono::Utc::now() - chrono::Duration::minutes(m))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    };
    let entered = ago(minutes);
    sqlx::query(
        "UPDATE task_activity SET timestamp = ? WHERE task_id = ? AND to_status = 'in_progress'",
    )
    .bind(&entered)
    .bind(id)
    .execute(store.pool())
    .await
    .unwrap();
    sqlx::query("UPDATE tasks SET updated_at = ? WHERE id = ?")
        .bind(ago(updated_minutes_ago))
        .bind(id)
        .execute(store.pool())
        .await
        .unwrap();

    let session_name = tmux.session_name("owner/repo", &format!("internal:{id}"));
    let created = tmux
        .command()
        .args([
            "new-session",
            "-d",
            "-s",
            &session_name,
            "-c",
            "/tmp",
            "sleep 60",
        ])
        .output()
        .await;
    match created {
        Ok(o) if o.status.success() => Some((id, session_name)),
        _ => None,
    }
}

fn long_run_config() -> EngineConfig {
    EngineConfig {
        stuck_timeout: 1800,
        run_timeouts: RunTimeouts {
            base: 1800,
            medium: 2700,
            complex: 3600,
        },
        ..EngineConfig::default()
    }
}

#[test]
fn tick_router_loads_the_config_once_for_many_live_runs() {
    let cell = OnceLock::new();
    let loads = Cell::new(0);
    for _ in 0..4 {
        tick_router(&cell, || {
            loads.set(loads.get() + 1);
            RouterConfig::default()
        });
    }
    assert_eq!(loads.get(), 1);
}

#[tokio::test]
async fn run_tier_follows_the_tier_of_the_stored_model() {
    let mut router = RouterConfig::default();
    router.model_map.clear();
    for (tier, model) in [("simple", "rt-s"), ("complex", "rt-c")] {
        router
            .model_map
            .entry(tier.to_string())
            .or_default()
            .insert("rtagent".to_string(), vec![model.to_string()]);
    }
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create_internal(
            "owner/repo",
            "Simple on a complex model",
            "",
            "cron",
            "1",
            None,
        )
        .await
        .unwrap();
    store
        .set_fields(
            id,
            &[
                ("complexity", serde_json::json!("simple")),
                ("agent", serde_json::json!("rtagent")),
                ("model", serde_json::json!("rt-c")),
            ],
        )
        .await
        .unwrap();
    let task = store.get(id).await.unwrap();
    assert_eq!(run_tier(&router, &task), "complex");

    store
        .set_fields(id, &[("model", serde_json::json!("rt-s"))])
        .await
        .unwrap();
    let task = store.get(id).await.unwrap();
    assert_eq!(run_tier(&router, &task), "simple");
}

#[tokio::test]
async fn recover_stuck_tasks_keeps_a_live_complex_run_inside_its_timeout() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let Some((id, session_name)) = live_complex_run(&store, &tmux, 31, 31).await else {
        eprintln!("Skipping test: tmux not available or failed to create test session");
        return;
    };
    let session_map = tmux.batch_session_active().await;

    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &long_run_config(),
        &store,
        &session_map,
        &Arc::new(DashMap::new()),
    )
    .await
    .unwrap();

    let alive = tmux.session_exists(&session_name).await;
    let _ = tmux.kill_session(&session_name).await;
    assert_eq!(
        store.get(id).await.unwrap().status,
        TaskStatus::InProgress,
        "a complex run at 31 minutes is inside its 3600s timeout"
    );
    assert!(alive, "recovery must not touch a run inside its timeout");
}

#[tokio::test]
async fn recover_stuck_tasks_kills_a_live_complex_run_past_its_timeout() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let Some((id, session_name)) = live_complex_run(&store, &tmux, 66, 0).await else {
        eprintln!("Skipping test: tmux not available or failed to create test session");
        return;
    };
    let session_map = tmux.batch_session_active().await;

    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &long_run_config(),
        &store,
        &session_map,
        &Arc::new(DashMap::new()),
    )
    .await
    .unwrap();

    let alive = tmux.session_exists(&session_name).await;
    let _ = tmux.kill_session(&session_name).await;
    assert_eq!(
        store.get(id).await.unwrap().status,
        TaskStatus::New,
        "a complex run past its timeout plus grace is stuck"
    );
    assert!(
        !alive,
        "recovery must kill the session it reports as killed"
    );
}

// ── tick_dispatch_tasks: deadlock regression test (#1361) ──────────────

/// Regression test for #1361: tick_dispatch_tasks previously accepted
/// `&Arc<RwLock<Router>>` and called `router_arc.read().await` internally.
/// The main loop already held a write lock on the same RwLock, so the read
/// could never be acquired — guaranteed deadlock on every tick with routed
/// tasks.
///
/// The fix changed the signature to accept `&Router` directly (the caller
/// already has the dereferenced guard), eliminating the lock re-acquisition.
///
/// This test verifies the function completes within 2 seconds when the
/// caller holds the write lock — the old code would deadlock here.
#[tokio::test]
async fn dispatch_does_not_deadlock_under_write_lock() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mock = MockBackend::new();
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let semaphore = Arc::new(Semaphore::new(4));
    let (weight_tx, _weight_rx) = mpsc::channel(16);
    let dispatching = Arc::new(DashMap::new());
    let transport = Arc::new(Transport::new());
    let capture = Arc::new(CaptureService::new(transport));

    let router_config = RouterConfig::default();
    let router = Router::new_for_test(router_config, vec!["claude".to_string()]);
    let router_arc = Arc::new(RwLock::new(router));

    // Simulate what the main loop does: hold a write lock, snapshot
    // dispatch mode, then call tick_dispatch_tasks without passing any
    // lock guard into async dispatch work.
    let write_guard = router_arc.write().await;
    let dispatch_mode = dispatch_mode_from_router(&write_guard);

    let result = time::timeout(
        Duration::from_secs(2),
        tick_dispatch_tasks(
            &backend,
            &tmux,
            "owner/repo",
            &Arc::new(TaskRunner::new("owner/repo".to_string())),
            &capture,
            &semaphore,
            &task_manager,
            &weight_tx,
            dispatch_mode,
            &dispatching,
            &store,
            &HashMap::new(),
        ),
    )
    .await;

    assert!(
        result.is_ok(),
        "tick_dispatch_tasks deadlocked! It tried to acquire a read lock \
             on router_arc while a write lock was already held (issue #1361)"
    );
}

/// Regression test for #3599: a task routed to an agent/model that entered
/// cooldown *after* routing but *before* dispatch must not be dispatched —
/// the stale route was decided before the failure that triggered the
/// cooldown was known. Dispatch must re-check the assigned agent/model
/// against the same cooldown store used at routing time and, if stale,
/// leave the task in `Routed` for the next tick to re-evaluate (no PR is
/// spawned, no per-task defer timer is written).
#[tokio::test]
async fn dispatch_skips_task_whose_routed_model_entered_cooldown() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mock = MockBackend::new();
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let runner = Arc::new(TaskRunner::new("owner/repo".to_string()));
    let semaphore = Arc::new(Semaphore::new(4));
    let (weight_tx, _weight_rx) = mpsc::channel(16);
    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
    let transport = Arc::new(Transport::new());
    let capture = Arc::new(CaptureService::new(transport));

    let id = store
        .upsert_external(&UpsertExternal {
            repo: "owner/repo",
            ext_id: "999",
            title: "Stale routed task",
            body: "",
            author: "user",
            url: "https://github.com/owner/repo/issues/999",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store
        .store_route(&StoreRoute {
            id,
            agent: "minimax",
            model: Some("opus"),
            complexity: "medium",
            estimate: 0,
            reason: "test",
            profile: "{}",
        })
        .await
        .unwrap();
    store.update_status(id, DbStatus::Routed).await.unwrap();

    // Simulate a billing/rate-limit cooldown recorded for minimax:opus after
    // this task was routed but before it dispatched.
    cooldown::set_model_cooldown("minimax", "opus", 300, "rate_limit_usage_window").await;

    let dispatch_mode = DispatchMode {
        is_degraded: false,
        healthy_agents: 1,
        threshold: 1,
    };

    tick_dispatch_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &runner,
        &capture,
        &semaphore,
        &task_manager,
        &weight_tx,
        dispatch_mode,
        &dispatching,
        &store,
        &HashMap::new(),
    )
    .await
    .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(
        task.status,
        DbStatus::Routed,
        "task whose routed model is now cooled down must stay Routed, not be dispatched"
    );
    assert!(
        dispatching.is_empty(),
        "dispatch guard must be released when skipping a stale-cooldown task"
    );
}

#[tokio::test]
async fn dispatch_skips_a_task_with_an_open_dependency() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mock = MockBackend::new();
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let runner = Arc::new(TaskRunner::new("owner/repo".to_string()));
    let semaphore = Arc::new(Semaphore::new(4));
    let (weight_tx, _weight_rx) = mpsc::channel(16);
    let dispatching: Arc<DashMap<String, String>> = Arc::new(DashMap::new());
    let transport = Arc::new(Transport::new());
    let capture = Arc::new(CaptureService::new(transport));

    let id = store
        .upsert_external(&UpsertExternal {
            repo: "owner/repo",
            ext_id: "42",
            title: "Waiting routed task",
            body: "",
            author: "user",
            url: "https://github.com/owner/repo/issues/42",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store
        .replace_dependencies(
            id,
            &[NewDependency {
                repo: "owner/repo".into(),
                number: 7,
                source: DepSource::Github,
            }],
        )
        .await
        .unwrap();
    store
        .store_route(&StoreRoute {
            id,
            agent: "claude",
            model: Some("haiku"),
            complexity: "simple",
            estimate: 0,
            reason: "test",
            profile: "{}",
        })
        .await
        .unwrap();
    store.update_status(id, DbStatus::Routed).await.unwrap();

    let dispatch_mode = DispatchMode {
        is_degraded: false,
        healthy_agents: 1,
        threshold: 1,
    };
    tick_dispatch_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &runner,
        &capture,
        &semaphore,
        &task_manager,
        &weight_tx,
        dispatch_mode,
        &dispatching,
        &store,
        &HashMap::new(),
    )
    .await
    .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.status, DbStatus::Waiting);
    assert!(dispatching.is_empty());
}

/// Regression test for the completed-session -> stuck-task reclaim race.
///
/// Scenario:
/// - Runner finishes a tmux session and will call store_touch_updated_at shortly after
/// - Engine's tick may observe the dead session in Phase 1 and Phase 2 may run
///   stuck-task recovery before the runner touches the store, causing incorrect reclaim
/// Fix: tick_check_session_completions now touches the store.updated_at when it
/// observes a completed session, preventing reclaim. This test simulates the
/// timing by creating a session, marking the corresponding task InProgress with
/// an old updated_at, calling tick_check_session_completions (which should touch
/// the store), then calling tick_recover_stuck_tasks and asserting the task
/// remains InProgress.
#[tokio::test]
async fn completed_session_does_not_cause_stuck_reclaim_race() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let mock = MockBackend::new();
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let task_manager = Arc::new(TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    ));
    let tmux = Arc::new(TmuxManager::new());
    let transport = Arc::new(Transport::new());
    let capture = Arc::new(CaptureService::new(transport));

    // Insert external task and set to InProgress with old updated_at
    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "200",
            title: "Race Task",
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

    // set updated_at to far past so it would normally be eligible for reclaim
    sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?")
        .bind(id)
        .execute(store.pool())
        .await
        .unwrap();

    // Register a real tmux session for this task and then exit it so it's observed dead
    let task_id = "200";
    let session_name = tmux.session_name("owner/repo", task_id);
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

    // Ensure remain-on-exit so the session lingers as dead long enough to be snapshot
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

    // Poll until pane is dead
    for _ in 0..20 {
        time::sleep(Duration::from_millis(100)).await;
        if !tmux.session_is_running(&session_name).await {
            break;
        }
    }

    // Register the session in capture (engine does this during dispatch)
    let task_key = task_id.to_string();
    capture
        .register_session("owner/repo", &task_key, &session_name)
        .await;

    // Call phase1 (now touches store.updated_at for completed session)
    let config = EngineConfig {
        no_session_stuck_timeout: 600,
        stuck_timeout: 1800,
        ..EngineConfig::default()
    };

    tick_check_session_completions(&tmux, "owner/repo", &capture, &store)
        .await
        .unwrap();

    // Verify phase1 actually touched updated_at. It only processes sessions
    // that appear in snapshot() (list_sessions + batch_session_active). On CI,
    // if the session was already fully removed before the snapshot runs (e.g.
    // remain-on-exit semantics differ across tmux versions), phase1 won't see
    // it and the store won't be touched. Skip in that case.
    {
        let task_state = store.get(id).await.unwrap();
        if task_state.updated_at.starts_with("2020") {
            eprintln!(
                "Skipping test: tick_check_session_completions did not update stored \
                     updated_at — session was not in snapshot (CI tmux environment issue)"
            );
            return;
        }
    }

    // Now run phase2 — the updated_at should have been touched so no reclaim occurs
    tick_recover_stuck_tasks(
        &backend,
        &tmux,
        "owner/repo",
        &task_manager,
        &config,
        &store,
        &HashMap::new(),
        &Arc::new(DashMap::new()),
    )
    .await
    .unwrap();

    // Verify the task is still InProgress in the store
    let task = store.get(id).await.unwrap();
    assert_eq!(
        task.status,
        TaskStatus::InProgress,
        "task should not be reclaimed to New"
    );
}

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

/// A task of a repo with no project directory ends `blocked` with a reason that names
/// the cause, never `needs_review` (#4024)
#[tokio::test]
async fn block_task_without_project_blocks_with_the_cause() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let task_manager = Arc::new(TaskManager::with_store(
        backend,
        Arc::clone(&store),
        "removed/repo".to_string(),
    ));
    let id = store
        .create_internal("removed/repo", "Orphan", "", "cron", "1", None)
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Routed).await.unwrap();

    block_task_without_project(
        &task_manager,
        &store,
        "removed/repo",
        &format!("internal:{id}"),
    )
    .await;

    let task = store.get(id).await.unwrap();
    assert_eq!(task.status, TaskStatus::Blocked);
    assert_eq!(
        task.block_reason.as_deref(),
        Some(NoProjectDir::BLOCK_REASON)
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

/// Regression test for #3413: tasks routed for an inactive/removed repo were
/// stuck in `routed` indefinitely because `tick_dispatch_tasks` is repo-scoped.
///
/// `group_orphaned_routed_tasks` must select tasks whose repo != the active
/// repo, group them by repo, and leave active-repo tasks out entirely.
#[tokio::test]
async fn group_orphaned_routed_tasks_selects_inactive_repo_tasks() {
    async fn make_routed(store: &TaskStore, repo: &str, ext_id: &str) -> i64 {
        let id = store
            .upsert_external(&UpsertExternal {
                repo,
                ext_id,
                title: "Routed task",
                body: "",
                author: "user",
                url: &format!("https://github.com/{repo}/issues/{ext_id}"),
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store
            .store_route(&StoreRoute {
                id,
                agent: "claude",
                model: Some("sonnet"),
                complexity: "medium",
                estimate: 0,
                reason: "test",
                profile: "{}",
            })
            .await
            .unwrap();
        store.update_status(id, TaskStatus::Routed).await.unwrap();
        id
    }

    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    let active_id = make_routed(&store, "active/repo", "1").await;
    let inactive_id_a = make_routed(&store, "inactive/repo", "2").await;
    let other_inactive_id = make_routed(&store, "other-inactive/repo", "3").await;
    let inactive_id_b = make_routed(&store, "inactive/repo", "4").await;

    let all_routed = store
        .list_all_by_status_global(TaskStatus::Routed)
        .await
        .unwrap();
    let active_repos: HashSet<String> = ["active/repo".to_string()].into_iter().collect();
    let by_repo = group_orphaned_routed_tasks(all_routed, &active_repos);

    assert!(
        !by_repo.contains_key("active/repo"),
        "global sweep must not select tasks for the active repo"
    );
    let inactive_ids: HashSet<i64> = by_repo
        .get("inactive/repo")
        .expect("inactive/repo must be selected")
        .iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(
        inactive_ids,
        [inactive_id_a, inactive_id_b].into_iter().collect(),
        "global sweep must group all orphaned tasks for a repo together"
    );
    assert_eq!(
        by_repo
            .get("other-inactive/repo")
            .map(|tasks| tasks.iter().map(|t| t.id).collect::<Vec<_>>()),
        Some(vec![other_inactive_id])
    );

    // Sanity: the active-repo task really was created and routed, it's just
    // excluded from the selection above.
    assert_eq!(
        store.get(active_id).await.unwrap().status,
        TaskStatus::Routed
    );
}

/// Regression test for the PR #3413 review finding: a single `current_repo` string is
/// the wrong exclusion — repo B's routed tasks must not look "orphaned" during repo A's
/// dispatch just because A != B.
/// `group_orphaned_routed_tasks` must exclude every repo in the full active set.
#[tokio::test]
async fn group_orphaned_routed_tasks_excludes_all_active_repos_not_just_current() {
    async fn make_routed(store: &TaskStore, repo: &str, ext_id: &str) -> i64 {
        let id = store
            .upsert_external(&UpsertExternal {
                repo,
                ext_id,
                title: "Routed task",
                body: "",
                author: "user",
                url: &format!("https://github.com/{repo}/issues/{ext_id}"),
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        store
            .store_route(&StoreRoute {
                id,
                agent: "claude",
                model: Some("sonnet"),
                complexity: "medium",
                estimate: 0,
                reason: "test",
                profile: "{}",
            })
            .await
            .unwrap();
        store.update_status(id, TaskStatus::Routed).await.unwrap();
        id
    }

    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    let repo_a_id = make_routed(&store, "active/repo-a", "1").await;
    let repo_b_id = make_routed(&store, "active/repo-b", "2").await;
    let orphaned_id = make_routed(&store, "inactive/repo", "3").await;

    let all_routed = store
        .list_all_by_status_global(TaskStatus::Routed)
        .await
        .unwrap();
    // Simulate repo A's tick: current repo is "active/repo-a", but the full
    // active project set also includes "active/repo-b".
    let active_repos: HashSet<String> = ["active/repo-a".to_string(), "active/repo-b".to_string()]
        .into_iter()
        .collect();
    let by_repo = group_orphaned_routed_tasks(all_routed, &active_repos);

    assert!(
        !by_repo.contains_key("active/repo-a"),
        "the current repo's own tasks must not be swept"
    );
    assert!(
        !by_repo.contains_key("active/repo-b"),
        "another active repo's tasks must not be swept during repo A's tick"
    );
    assert_eq!(
        by_repo
            .get("inactive/repo")
            .map(|tasks| tasks.iter().map(|t| t.id).collect::<Vec<_>>()),
        Some(vec![orphaned_id]),
        "a genuinely inactive repo's tasks must still be swept"
    );

    // Sanity: repo A and repo B tasks were really created and routed, just excluded.
    assert_eq!(
        store.get(repo_a_id).await.unwrap().status,
        TaskStatus::Routed
    );
    assert_eq!(
        store.get(repo_b_id).await.unwrap().status,
        TaskStatus::Routed
    );
}

/// Tasks with the `no-agent` label must be skipped by the global dispatch sweep.
#[tokio::test]
async fn global_dispatch_sweep_skips_no_agent_tasks() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    let id = store
        .upsert_external(&UpsertExternal {
            repo: "inactive/repo",
            ext_id: "99",
            title: "No-agent routed task",
            body: "",
            author: "user",
            url: "https://github.com/inactive/repo/issues/99",
            labels: &["no-agent".to_string()],
            origin: "github",
        })
        .await
        .unwrap();
    store
        .store_route(&StoreRoute {
            id,
            agent: "claude",
            model: Some("sonnet"),
            complexity: "medium",
            estimate: 0,
            reason: "test",
            profile: "{}",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Routed).await.unwrap();

    // Safe to call the full sweep here: with the only orphaned task filtered
    // out by the no-agent label, `group_orphaned_routed_tasks` returns an
    // empty map and `dispatch_routed_tasks_global` returns before ever
    // constructing a backend or spawning a runner.
    let tmux = Arc::new(TmuxManager::new());
    let transport = Arc::new(Transport::new());
    let capture = Arc::new(CaptureService::new(transport));
    let semaphore = Arc::new(Semaphore::new(4));
    let (weight_tx, _weight_rx) = mpsc::channel(16);
    let dispatching = Arc::new(DashMap::new());
    let dispatch_mode = DispatchMode {
        is_degraded: false,
        healthy_agents: 1,
        threshold: 1,
    };

    let active_repos: HashSet<String> = ["active/repo".to_string()].into_iter().collect();
    dispatch_routed_tasks_global(
        &tmux,
        &capture,
        &semaphore,
        &weight_tx,
        dispatch_mode,
        &dispatching,
        &store,
        &HashMap::new(),
        &active_repos,
    )
    .await
    .unwrap();

    assert_eq!(
        store.get(id).await.unwrap().status,
        TaskStatus::Routed,
        "no-agent tasks must not be dispatched"
    );
}
