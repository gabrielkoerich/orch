use super::*;
use crate::backends::{ExternalId, ExternalTask, Mention};
use crate::store::{NewTask, TaskStore};
use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

type StatusTaskMap = Arc<Mutex<Vec<(Status, Vec<ExternalTask>)>>>;

/// Mock backend that records update_status calls.
struct MockBackend {
    status_updates: Arc<Mutex<Vec<(String, Status)>>>,
    status_tasks: StatusTaskMap,
    routable_tasks: Arc<Mutex<Vec<ExternalTask>>>,
}

impl MockBackend {
    fn new() -> Self {
        Self {
            status_updates: Arc::new(Mutex::new(vec![])),
            status_tasks: Arc::new(Mutex::new(vec![])),
            routable_tasks: Arc::new(Mutex::new(vec![])),
        }
    }

    fn with_status_tasks(self, status: Status, tasks: Vec<ExternalTask>) -> Self {
        self.status_tasks.lock().unwrap().push((status, tasks));
        self
    }

    fn with_routable_tasks(self, tasks: Vec<ExternalTask>) -> Self {
        *self.routable_tasks.lock().unwrap() = tasks;
        self
    }
}

#[async_trait]
impl ExternalBackend for MockBackend {
    fn name(&self) -> &'static str {
        "mock"
    }
    async fn create_task(&self, _t: &str, _b: &str, _l: &[String]) -> anyhow::Result<ExternalId> {
        Ok(ExternalId("new".to_string()))
    }
    async fn get_task(&self, id: &ExternalId) -> anyhow::Result<ExternalTask> {
        Ok(ExternalTask {
            id: id.clone(),
            title: "Mock".to_string(),
            body: String::new(),
            state: "open".to_string(),
            labels: vec![],
            author: "bot".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: String::new(),
        })
    }
    async fn list_by_status(&self, status: Status) -> anyhow::Result<Vec<ExternalTask>> {
        Ok(self
            .status_tasks
            .lock()
            .unwrap()
            .iter()
            .find(|(s, _)| *s == status)
            .map(|(_, tasks)| tasks.clone())
            .unwrap_or_default())
    }
    async fn list_routable(&self) -> anyhow::Result<Vec<ExternalTask>> {
        Ok(self.routable_tasks.lock().unwrap().clone())
    }
    async fn post_comment(&self, _id: &ExternalId, _b: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn set_labels(&self, _id: &ExternalId, _l: &[String]) -> anyhow::Result<()> {
        Ok(())
    }
    async fn remove_label(&self, _id: &ExternalId, _l: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn get_sub_issues(&self, _id: &ExternalId) -> anyhow::Result<Vec<ExternalId>> {
        Ok(vec![])
    }
    async fn create_sub_task(
        &self,
        _p: &ExternalId,
        _t: &str,
        _b: &str,
        _l: &[String],
    ) -> anyhow::Result<ExternalId> {
        Ok(ExternalId("child".to_string()))
    }
    async fn ensure_status_label(&self, _l: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn has_open_issue_with_title(&self, _t: &str, _l: &str) -> anyhow::Result<bool> {
        Ok(false)
    }
    async fn health_check(&self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn is_pr_merged(&self, _b: &str) -> anyhow::Result<bool> {
        Ok(false)
    }
    async fn get_authenticated_user(&self) -> anyhow::Result<Option<String>> {
        Ok(Some("testbot".to_string()))
    }
    async fn get_mentions(&self, _s: &str) -> anyhow::Result<Vec<Mention>> {
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

#[tokio::test]
async fn with_store_constructor_enables_dual_write() {
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    let tm = TaskManager::with_store(backend, Arc::clone(&store), "owner/repo".to_string());

    assert!(tm.store.is_some(), "store should be set via with_store");
    assert_eq!(tm.repo, "owner/repo");
}

#[tokio::test]
async fn update_status_dual_writes_to_store_for_external_task() {
    let mock = MockBackend::new();
    let backend: Arc<dyn ExternalBackend> = Arc::new(mock);
    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    // Pre-create the task in the store so resolve_task_id finds it
    store
        .create(&NewTask {
            external_id: Some("42".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Test".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let tm = TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    );

    // Update status — should write to both backend and store
    tm.update_task_status(&ExternalId("42".to_string()), Status::InProgress)
        .await
        .unwrap();

    // Verify the store side was updated
    let task = store.get(1).await.unwrap();
    assert_eq!(
        task.status,
        TaskStatus::InProgress,
        "store should have updated status"
    );
}

#[tokio::test]
async fn update_status_skips_store_when_task_not_in_store() {
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    let tm = TaskManager::with_store(backend, Arc::clone(&store), "owner/repo".to_string());

    // Update for a task not in the store — should not error
    let result = tm
        .update_task_status(&ExternalId("999".to_string()), Status::Done)
        .await;
    assert!(result.is_ok(), "should succeed even when task not in store");
}

#[tokio::test]
async fn update_status_works_without_store() {
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());

    // Use old constructor (no store)
    let tm = TaskManager::new(backend);

    let result = tm
        .update_task_status(&ExternalId("42".to_string()), Status::Routed)
        .await;
    assert!(
        result.is_ok(),
        "should work without store (backward compat)"
    );
}

#[tokio::test]
async fn update_status_writes_to_store_for_internal_task() {
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    // Create an internal task in the store
    let internal_id = store
        .create_internal("owner/repo", "Internal task", "body", "cron", "job:1", None)
        .await
        .unwrap();

    let tm = TaskManager::with_store(backend, Arc::clone(&store), "owner/repo".to_string());

    // Update the internal task status via TaskManager
    let id_str = format!("internal:{internal_id}");
    tm.update_task_status(&ExternalId(id_str), Status::Routed)
        .await
        .unwrap();

    // Verify the store was updated
    let task = store.get(internal_id).await.unwrap();
    assert_eq!(task.status, TaskStatus::Routed);
}

#[tokio::test]
async fn status_to_task_status_maps_all_variants() {
    assert_eq!(status_to_task_status(Status::New), TaskStatus::New);
    assert_eq!(status_to_task_status(Status::Routed), TaskStatus::Routed);
    assert_eq!(
        status_to_task_status(Status::InProgress),
        TaskStatus::InProgress
    );
    assert_eq!(status_to_task_status(Status::Done), TaskStatus::Done);
    assert_eq!(status_to_task_status(Status::Blocked), TaskStatus::Blocked);
    assert_eq!(
        status_to_task_status(Status::InReview),
        TaskStatus::InReview
    );
    assert_eq!(
        status_to_task_status(Status::NeedsReview),
        TaskStatus::NeedsReview
    );
    assert_eq!(
        status_to_task_status(Status::Approved),
        TaskStatus::Approved
    );
    assert_eq!(status_to_task_status(Status::Waiting), TaskStatus::Waiting);
}

#[test]
fn is_internal_id_detects_prefix() {
    assert!(is_internal_id("internal:5"));
    assert!(is_internal_id("internal:0"));
    assert!(!is_internal_id("42"));
    assert!(!is_internal_id(""));
    assert!(!is_internal_id("internal"));
}

#[test]
fn parse_internal_id_extracts_number() {
    assert_eq!(parse_internal_id("internal:5"), Some(5));
    assert_eq!(parse_internal_id("internal:0"), Some(0));
    assert_eq!(parse_internal_id("internal:abc"), None);
    assert_eq!(parse_internal_id("42"), None);
    assert_eq!(parse_internal_id(""), None);
}

#[tokio::test]
async fn create_internal_task_via_task_manager() {
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let tm = TaskManager::with_store(backend, Arc::clone(&store), "owner/repo".to_string());

    let task = tm
        .create_task(CreateTaskRequest {
            title: "Internal task".to_string(),
            body: "Do something".to_string(),
            task_type: TaskType::Internal,
            labels: vec![],
            source: "cron".to_string(),
            source_id: "job:daily".to_string(),
        })
        .await
        .unwrap();

    match task {
        Task::Internal(t) => {
            assert_eq!(t.title, "Internal task");
            assert_eq!(t.origin, "internal");
            assert!(
                t.external_id
                    .as_deref()
                    .is_some_and(|s| s.starts_with("internal:"))
            );
        }
        Task::External(_) => panic!("expected Internal variant"),
    }
}

#[tokio::test]
async fn create_internal_task_fails_without_store() {
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let tm = TaskManager::new(backend);

    let result = tm
        .create_task(CreateTaskRequest {
            title: "No store".to_string(),
            body: String::new(),
            task_type: TaskType::Internal,
            labels: vec![],
            source: "manual".to_string(),
            source_id: String::new(),
        })
        .await;

    assert!(result.is_err(), "should fail without store");
}

/// Regression test for the phantom-event bug: when an internal task ID is not found
/// in the store, update_task_status must return Err and must NOT publish any event.
#[tokio::test]
async fn update_status_internal_task_not_found_returns_err_and_no_event() {
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    let (event_tx, mut event_rx) = broadcast::channel::<TaskEvent>(16);

    let tm = TaskManager::with_events(
        backend,
        Arc::clone(&store),
        "owner/repo".to_string(),
        event_tx,
    );

    // "internal:999" does not exist in the (empty) store
    let result = tm
        .update_task_status(&ExternalId("internal:999".to_string()), Status::Done)
        .await;

    assert!(
        result.is_err(),
        "should return Err when internal task is not found in store"
    );

    // No event must have been published
    assert!(
        event_rx.try_recv().is_err(),
        "no event should be published for a not-found internal task"
    );
}

/// Regression test for #3437: `update_task_status` resolves by `self.repo`, so a
/// `TaskManager` scoped to one repo cannot mark Done a task that belongs to a
/// different repo — exactly the case the global cross-repo CI-failure unblock sweep
/// hits. `mark_task_done_by_store_id` bypasses that repo-scoped resolution and must
/// succeed where `update_task_status` fails.
#[tokio::test]
async fn mark_task_done_by_store_id_succeeds_across_repos() {
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let store = Arc::new(TaskStore::open_memory().await.unwrap());

    let store_id = store
        .create_internal(
            "other/repo",
            "Cross-repo task",
            "body",
            "cron",
            "job:1",
            None,
        )
        .await
        .unwrap();
    let ext_id = ExternalId(format!("internal:{store_id}"));

    // TaskManager scoped to a different repo than the task belongs to.
    let tm = TaskManager::with_store(backend, Arc::clone(&store), "active/repo".to_string());

    // The repo-scoped path fails to even find the task.
    let resolve_result = tm.update_task_status(&ext_id, Status::Done).await;
    assert!(
        resolve_result.is_err(),
        "repo-scoped update_task_status must fail for a cross-repo task"
    );

    // The bypass path succeeds using the already-resolved store id + owning repo.
    tm.mark_task_done_by_store_id(store_id, "other/repo", &ext_id)
        .await
        .unwrap();

    let task = store.get(store_id).await.unwrap();
    assert_eq!(task.status, TaskStatus::Done);
}

#[tokio::test]
async fn get_task_returns_internal_from_store() {
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let tm = TaskManager::with_store(backend, Arc::clone(&store), "owner/repo".to_string());

    let id = store
        .create_internal("owner/repo", "Fetch me", "body", "cron", "job:1", None)
        .await
        .unwrap();

    let task = tm.get_task(id).await.unwrap();
    match task {
        Task::Internal(t) => assert_eq!(t.title, "Fetch me"),
        Task::External(_) => panic!("expected Internal variant"),
    }
}

#[tokio::test]
async fn list_routable_includes_internal_new_tasks() {
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let tm = TaskManager::with_store(backend, Arc::clone(&store), "owner/repo".to_string());

    // Create internal task (starts as New)
    store
        .create_internal("owner/repo", "Route me", "", "cron", "job:2", None)
        .await
        .unwrap();

    let routable = tm.list_routable().await.unwrap();
    assert_eq!(routable.len(), 1);
    assert!(routable[0].id.0.starts_with("internal:"));
    assert_eq!(routable[0].title, "Route me");
}

#[tokio::test]
async fn list_routable_excludes_non_new_internal_tasks() {
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let tm = TaskManager::with_store(backend, Arc::clone(&store), "owner/repo".to_string());

    let id = store
        .create_internal("owner/repo", "Already routed", "", "cron", "job:3", None)
        .await
        .unwrap();
    store
        .update_status(id, TaskStatus::InProgress)
        .await
        .unwrap();

    let routable = tm.list_routable().await.unwrap();
    assert!(
        routable.is_empty(),
        "in_progress tasks should not be routable"
    );
}

#[tokio::test]
async fn list_internal_by_status_returns_external_tasks() {
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let tm = TaskManager::with_store(backend, Arc::clone(&store), "owner/repo".to_string());

    store
        .create_internal("owner/repo", "Task A", "", "cron", "a", None)
        .await
        .unwrap();
    let id_b = store
        .create_internal("owner/repo", "Task B", "", "cron", "b", None)
        .await
        .unwrap();
    store.update_status(id_b, TaskStatus::Done).await.unwrap();

    let new_tasks = tm.list_internal_by_status(TaskStatus::New).await.unwrap();
    assert_eq!(new_tasks.len(), 1);
    assert_eq!(new_tasks[0].title, "Task A");
    // Verify it's wrapped as ExternalTask with status label
    assert!(new_tasks[0].labels.contains(&"status:new".to_string()));
}

#[tokio::test]
async fn store_task_to_external_maps_fields_correctly() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create_internal(
            "owner/repo",
            "Convert me",
            "body text",
            "manual",
            "m:1",
            None,
        )
        .await
        .unwrap();
    let task = store.get(id).await.unwrap();
    let ext = store_task_to_external(&task);

    assert!(ext.id.0.starts_with("internal:"));
    assert_eq!(ext.title, "Convert me");
    assert_eq!(ext.body, "body text");
    assert_eq!(ext.state, "open");
    assert!(ext.labels.contains(&"status:new".to_string()));
    assert_eq!(ext.author, "manual"); // source maps to author
}

#[tokio::test]
async fn list_all_internal_returns_store_tasks() {
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let tm = TaskManager::with_store(backend, Arc::clone(&store), "owner/repo".to_string());

    store
        .create_internal("owner/repo", "T1", "", "cron", "1", None)
        .await
        .unwrap();
    store
        .create_internal("owner/repo", "T2", "", "cron", "2", None)
        .await
        .unwrap();
    // Different repo — should not appear
    store
        .create_internal("other/repo", "T3", "", "cron", "3", None)
        .await
        .unwrap();

    let tasks = tm.list_all_internal().await.unwrap();
    assert_eq!(tasks.len(), 2);
}

// ── Phase 4: store-first reads ───────────────────────────────────

#[tokio::test]
async fn list_external_by_status_reads_from_store() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend = Arc::new(MockBackend::new());
    let tm = TaskManager::with_store(backend, Arc::clone(&store), "owner/repo".to_string());

    // Create an external task in the store with status Routed
    let id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "42",
            title: "Store task",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Routed).await.unwrap();

    let routed = tm.list_external_by_status(Status::Routed).await.unwrap();
    assert_eq!(routed.len(), 1);
    assert_eq!(routed[0].id.0, "42");
    assert_eq!(routed[0].title, "Store task");
}

#[tokio::test]
async fn list_external_by_status_excludes_internal_tasks() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend = Arc::new(MockBackend::new());
    let tm = TaskManager::with_store(backend, Arc::clone(&store), "owner/repo".to_string());

    // Create an internal task with Routed status
    let id = store
        .create_internal("owner/repo", "Internal task", "", "cron", "1", None)
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Routed).await.unwrap();

    // Also create an external task to populate the store
    store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "99",
            title: "External",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();

    let routed = tm.list_external_by_status(Status::Routed).await.unwrap();
    // Should only include the internal task since it's status Routed, but filtered to external only
    assert_eq!(routed.len(), 0);
}

#[tokio::test]
async fn list_external_by_status_falls_back_when_store_has_only_internal_tasks() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend = Arc::new(MockBackend::new().with_status_tasks(
        Status::Routed,
        vec![ExternalTask {
            id: ExternalId("123".to_string()),
            title: "Backend routed".to_string(),
            body: String::new(),
            state: "open".to_string(),
            labels: vec!["status:routed".to_string()],
            author: "bot".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: String::new(),
        }],
    ));
    let tm = TaskManager::with_store(backend, Arc::clone(&store), "owner/repo".to_string());

    let id = store
        .create_internal("owner/repo", "Internal task", "", "cron", "1", None)
        .await
        .unwrap();
    store.update_status(id, TaskStatus::Routed).await.unwrap();

    let routed = tm.list_external_by_status(Status::Routed).await.unwrap();
    assert_eq!(routed.len(), 1);
    assert_eq!(routed[0].id.0, "123");
}

#[tokio::test]
async fn list_routable_reads_from_store_when_populated() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend = Arc::new(MockBackend::new());
    let tm = TaskManager::with_store(backend, Arc::clone(&store), "owner/repo".to_string());

    // Create a new external task
    store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "10",
            title: "New external",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    // Also a new internal task
    store
        .create_internal("owner/repo", "New internal", "", "cron", "1", None)
        .await
        .unwrap();

    let routable = tm.list_routable().await.unwrap();
    // Both should appear (both are status=new)
    assert_eq!(routable.len(), 2);
}

#[tokio::test]
async fn list_routable_falls_back_when_store_has_only_internal_tasks() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend = Arc::new(MockBackend::new().with_routable_tasks(vec![ExternalTask {
        id: ExternalId("55".to_string()),
        title: "Backend new".to_string(),
        body: String::new(),
        state: "open".to_string(),
        labels: vec!["status:new".to_string()],
        author: "bot".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        url: String::new(),
    }]));
    let tm = TaskManager::with_store(backend, Arc::clone(&store), "owner/repo".to_string());

    store
        .create_internal("owner/repo", "Internal new", "", "cron", "1", None)
        .await
        .unwrap();

    let routable = tm.list_routable().await.unwrap();
    assert_eq!(routable.len(), 2);
    assert!(routable.iter().any(|task| task.id.0 == "55"));
    assert!(
        routable
            .iter()
            .any(|task| task.id.0.starts_with("internal:"))
    );
}

#[tokio::test]
async fn list_all_external_tasks_reads_from_store() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend = Arc::new(MockBackend::new());
    let tm = TaskManager::with_store(backend, Arc::clone(&store), "owner/repo".to_string());

    // Create tasks with different statuses
    let id1 = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "1",
            title: "Task 1",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    let id2 = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "2",
            title: "Task 2",
            body: "",
            author: "",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    store.update_status(id1, TaskStatus::Done).await.unwrap();
    store
        .update_status(id2, TaskStatus::InProgress)
        .await
        .unwrap();

    // Internal tasks should be excluded
    store
        .create_internal("owner/repo", "Internal", "", "cron", "1", None)
        .await
        .unwrap();

    let all = tm.list_all_external_tasks().await.unwrap();
    assert_eq!(all.len(), 2);
}

#[tokio::test]
async fn list_all_external_tasks_falls_back_when_store_has_only_internal_tasks() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend = Arc::new(MockBackend::new().with_status_tasks(
        Status::Done,
        vec![ExternalTask {
            id: ExternalId("200".to_string()),
            title: "Backend done".to_string(),
            body: String::new(),
            state: "closed".to_string(),
            labels: vec!["status:done".to_string()],
            author: "bot".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: String::new(),
        }],
    ));
    let tm = TaskManager::with_store(backend, Arc::clone(&store), "owner/repo".to_string());

    store
        .create_internal("owner/repo", "Internal", "", "cron", "1", None)
        .await
        .unwrap();

    let all = tm.list_all_external_tasks().await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id.0, "200");
}

#[tokio::test]
async fn list_tasks_without_status_merges_backend_external_when_store_only_has_internal() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend = Arc::new(MockBackend::new().with_status_tasks(
        Status::New,
        vec![ExternalTask {
            id: ExternalId("301".to_string()),
            title: "Backend new".to_string(),
            body: String::new(),
            state: "open".to_string(),
            labels: vec!["status:new".to_string()],
            author: "bot".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: String::new(),
        }],
    ));
    let tm = TaskManager::with_store(backend, Arc::clone(&store), "owner/repo".to_string());

    let internal_id = store
        .create_internal("owner/repo", "Internal task", "", "cron", "1", None)
        .await
        .unwrap();

    let listed = tm.list_tasks(TaskFilter::default()).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().any(|task| matches!(
        task,
        Task::External(external) if external.id.0 == "301"
    )));
    assert!(listed.iter().any(|task| matches!(
        task,
        Task::Internal(internal) if internal.id == internal_id
    )));
}

#[tokio::test]
async fn list_tasks_status_all_merges_backend_external_when_store_only_has_internal() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend = Arc::new(MockBackend::new().with_status_tasks(
        Status::Done,
        vec![ExternalTask {
            id: ExternalId("302".to_string()),
            title: "Backend done".to_string(),
            body: String::new(),
            state: "closed".to_string(),
            labels: vec!["status:done".to_string()],
            author: "bot".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: String::new(),
        }],
    ));
    let tm = TaskManager::with_store(backend, Arc::clone(&store), "owner/repo".to_string());

    let internal_id = store
        .create_internal("owner/repo", "Internal task", "", "cron", "1", None)
        .await
        .unwrap();

    let listed = tm
        .list_tasks(TaskFilter {
            status: Some("all".to_string()),
            source: None,
        })
        .await
        .unwrap();
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().any(|task| matches!(
        task,
        Task::External(external) if external.id.0 == "302"
    )));
    assert!(listed.iter().any(|task| matches!(
        task,
        Task::Internal(internal) if internal.id == internal_id
    )));
}

// ── store-first update_task_status ──────────────────────────────

#[tokio::test]
async fn update_task_status_writes_store_first_for_external() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let tm = TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    );

    // Upsert an external task
    let store_id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "42",
            title: "Test",
            body: "",
            author: "user",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    assert_eq!(store.get(store_id).await.unwrap().status, TaskStatus::New);

    // Update via task_manager — store should update first
    let ext_id = ExternalId("42".to_string());
    tm.update_task_status(&ext_id, Status::Routed)
        .await
        .unwrap();

    // Store should now have Routed
    let task = store.get(store_id).await.unwrap();
    assert_eq!(task.status, TaskStatus::Routed);
}

#[tokio::test]
async fn update_task_status_handles_internal_tasks() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let tm = TaskManager::with_store(
        Arc::clone(&backend),
        Arc::clone(&store),
        "owner/repo".to_string(),
    );

    // Create an internal task
    let store_id = store
        .create_internal("owner/repo", "task", "body", "manual", "", None)
        .await
        .unwrap();

    let internal_id = ExternalId(format!("internal:{store_id}"));
    tm.update_task_status(&internal_id, Status::InProgress)
        .await
        .unwrap();

    let task = store.get(store_id).await.unwrap();
    assert_eq!(task.status, TaskStatus::InProgress);
}

#[tokio::test]
async fn update_task_status_publishes_event() {
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let (tx, mut rx) = broadcast::channel::<TaskEvent>(16);
    let tm = TaskManager {
        backend,
        store: None,
        repo: "owner/repo".to_string(),
        event_tx: Some(tx),
    };
    // External task — backend mock accepts any status update
    let id = ExternalId("42".to_string());
    tm.update_task_status(&id, Status::Routed).await.unwrap();
    let event = rx.try_recv().unwrap();
    assert_eq!(event.task_id, "42");
    assert_eq!(event.new_status, "routed");
    assert_eq!(event.repo, "owner/repo");
}

#[tokio::test]
async fn event_includes_old_status_and_context() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let (tx, mut rx) = broadcast::channel::<TaskEvent>(16);

    // Upsert an external task (starts as New)
    let store_id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "77",
            title: "Context test",
            body: "",
            author: "user",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();

    // Set some context fields on the task
    store
        .set_fields(
            store_id,
            &[
                ("agent", serde_json::Value::String("claude".to_string())),
                ("model", serde_json::Value::String("sonnet".to_string())),
                ("branch", serde_json::Value::String("feat-77".to_string())),
                ("pr_number", serde_json::Value::Number(42.into())),
                (
                    "last_error",
                    serde_json::Value::String("timeout".to_string()),
                ),
            ],
        )
        .await
        .unwrap();

    let tm = TaskManager::with_events(backend, Arc::clone(&store), "owner/repo".to_string(), tx);

    // Update New → Routed
    let ext_id = ExternalId("77".to_string());
    tm.update_task_status(&ext_id, Status::Routed)
        .await
        .unwrap();

    let event = rx.try_recv().unwrap();
    assert_eq!(event.task_id, "77");
    assert_eq!(event.old_status, "new");
    assert_eq!(event.new_status, "routed");
    assert_eq!(event.agent.as_deref(), Some("claude"));
    assert_eq!(event.model.as_deref(), Some("sonnet"));
    assert_eq!(event.branch.as_deref(), Some("feat-77"));
    assert_eq!(event.pr_number.as_deref(), Some("42"));
    assert_eq!(event.error.as_deref(), Some("timeout"));
}

#[tokio::test]
async fn event_includes_old_status_for_internal_task() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let (tx, mut rx) = broadcast::channel::<TaskEvent>(16);

    // Create an internal task (starts as New)
    let store_id = store
        .create_internal(
            "owner/repo",
            "internal ctx test",
            "body",
            "manual",
            "",
            None,
        )
        .await
        .unwrap();

    let tm = TaskManager::with_events(backend, Arc::clone(&store), "owner/repo".to_string(), tx);

    // Update New → Routed
    let internal_id = ExternalId(format!("internal:{store_id}"));
    tm.update_task_status(&internal_id, Status::Routed)
        .await
        .unwrap();

    let event = rx.try_recv().unwrap();
    assert_eq!(event.old_status, "new");
    assert_eq!(event.new_status, "routed");

    // Update Routed → InProgress
    tm.update_task_status(&internal_id, Status::InProgress)
        .await
        .unwrap();

    let event2 = rx.try_recv().unwrap();
    assert_eq!(event2.old_status, "routed");
    assert_eq!(event2.new_status, "in_progress");
}

// ── update_task_status_if ────────────────────────────────────────

#[tokio::test]
async fn update_task_status_if_applies_when_status_matches() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let tm = TaskManager::with_store(backend, Arc::clone(&store), "owner/repo".to_string());

    let store_id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "10",
            title: "T",
            body: "",
            author: "u",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    // Task starts as New; move it to InReview first
    store
        .update_status(store_id, TaskStatus::InReview)
        .await
        .unwrap();

    let id = ExternalId("10".to_string());
    let updated = tm
        .update_task_status_if(&id, Status::NeedsReview, Status::InReview)
        .await
        .unwrap();
    assert!(updated, "should update when expected status matches");

    let task = store.get(store_id).await.unwrap();
    assert_eq!(task.status, TaskStatus::NeedsReview);
}

#[tokio::test]
async fn update_task_status_if_is_noop_when_status_mismatches() {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let (tx, mut rx) = broadcast::channel::<TaskEvent>(16);
    let tm = TaskManager::with_events(backend, Arc::clone(&store), "owner/repo".to_string(), tx);

    let store_id = store
        .upsert_external(&crate::store::UpsertExternal {
            repo: "owner/repo",
            ext_id: "11",
            title: "T",
            body: "",
            author: "u",
            url: "",
            labels: &[],
            origin: "github",
        })
        .await
        .unwrap();
    // Simulate a concurrent transition: task already moved to Done
    store
        .update_status(store_id, TaskStatus::Done)
        .await
        .unwrap();

    let id = ExternalId("11".to_string());
    // Attempt to reset to NeedsReview expecting InReview — should be a no-op
    let updated = tm
        .update_task_status_if(&id, Status::NeedsReview, Status::InReview)
        .await
        .unwrap();
    assert!(
        !updated,
        "should not update when status has already changed"
    );

    // Status must remain Done
    let task = store.get(store_id).await.unwrap();
    assert_eq!(task.status, TaskStatus::Done);

    // No event must be published
    assert!(
        rx.try_recv().is_err(),
        "no event should be published on a no-op update"
    );
}
async fn duration_fixture(
    internal: bool,
) -> (
    TaskManager,
    Arc<TaskStore>,
    i64,
    ExternalId,
    broadcast::Receiver<TaskEvent>,
) {
    let store = Arc::new(TaskStore::open_memory().await.unwrap());
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let (tx, rx) = broadcast::channel::<TaskEvent>(16);
    let (store_id, id) = if internal {
        let sid = store
            .create_internal("owner/repo", "task", "body", "manual", "", None)
            .await
            .unwrap();
        (sid, ExternalId(format!("internal:{sid}")))
    } else {
        let sid = store
            .upsert_external(&crate::store::UpsertExternal {
                repo: "owner/repo",
                ext_id: "91",
                title: "t",
                body: "",
                author: "user",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();
        (sid, ExternalId("91".to_string()))
    };
    store
        .set_block_reason(store_id, Some("stale reason"))
        .await
        .unwrap();
    let tm = TaskManager::with_events(backend, Arc::clone(&store), "owner/repo".to_string(), tx);
    (tm, store, store_id, id, rx)
}

#[tokio::test]
async fn status_update_clears_block_reason_and_publishes_event_for_both_kinds() {
    for internal in [false, true] {
        let (tm, store, store_id, id, mut rx) = duration_fixture(internal).await;
        tm.update_task_status(&id, Status::Routed).await.unwrap();
        let task = store.get(store_id).await.unwrap();
        assert_eq!(task.status, TaskStatus::Routed);
        assert_eq!(task.block_reason, None);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.old_status, "new");
        assert_eq!(event.new_status, "routed");
        assert_eq!(event.duration_seconds, None);
    }
}

#[tokio::test]
async fn status_update_with_duration_clears_block_reason_and_carries_duration() {
    for internal in [false, true] {
        let (tm, store, store_id, id, mut rx) = duration_fixture(internal).await;
        tm.update_task_status_with_duration(&id, Status::Routed, Some(12.5))
            .await
            .unwrap();
        let task = store.get(store_id).await.unwrap();
        assert_eq!(task.status, TaskStatus::Routed);
        assert_eq!(task.block_reason, None);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.old_status, "new");
        assert_eq!(event.new_status, "routed");
        assert_eq!(event.duration_seconds, Some(12.5));
    }
}

#[tokio::test]
async fn status_update_to_blocked_keeps_block_reason() {
    for internal in [false, true] {
        let (tm, store, store_id, id, _rx) = duration_fixture(internal).await;
        tm.update_task_status(&id, Status::Blocked).await.unwrap();
        let task = store.get(store_id).await.unwrap();
        assert_eq!(task.status, TaskStatus::Blocked);
        assert_eq!(task.block_reason.as_deref(), Some("stale reason"));

        let (tm, store, store_id, id, _rx) = duration_fixture(internal).await;
        tm.update_task_status_with_duration(&id, Status::Blocked, None)
            .await
            .unwrap();
        let task = store.get(store_id).await.unwrap();
        assert_eq!(task.block_reason.as_deref(), Some("stale reason"));
    }
}

#[tokio::test]
async fn status_update_internal_without_store_or_row_errors() {
    let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
    let tm = TaskManager::new(Arc::clone(&backend));
    let id = ExternalId("internal:1".to_string());
    assert!(tm.update_task_status(&id, Status::Routed).await.is_err());
    assert!(
        tm.update_task_status_with_duration(&id, Status::Routed, None)
            .await
            .is_err()
    );
}
