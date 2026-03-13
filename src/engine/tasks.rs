use crate::backends::{ExternalBackend, ExternalId, ExternalTask, Status};
use crate::db::{Db, InternalTask, TaskStatus};
use crate::store::TaskStore;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Returns true if the given task ID refers to an internal (SQLite) task.
pub fn is_internal_id(id: &str) -> bool {
    id.starts_with("internal:")
}

/// Parse an internal task ID string (e.g. "internal:8") to its numeric SQLite id.
pub fn parse_internal_id(id: &str) -> Option<i64> {
    id.strip_prefix("internal:")?.parse().ok()
}

/// Map a backend `Status` to a DB `TaskStatus`.
pub fn status_to_task_status(status: Status) -> TaskStatus {
    match status {
        Status::New => TaskStatus::New,
        Status::Routed => TaskStatus::Routed,
        Status::InProgress => TaskStatus::InProgress,
        Status::Done => TaskStatus::Done,
        Status::Blocked => TaskStatus::Blocked,
        Status::InReview => TaskStatus::InReview,
        Status::NeedsReview => TaskStatus::NeedsReview,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskType {
    External,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTaskRequest {
    pub title: String,
    pub body: String,
    pub task_type: TaskType,
    pub labels: Vec<String>,
    pub source: String,
    pub source_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskFilter {
    pub status: Option<String>,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Task {
    External(ExternalTask),
    Internal(InternalTask),
}

pub struct TaskManager {
    pub(crate) db: Arc<Db>,
    backend: Arc<dyn ExternalBackend>,
    /// Unified task store (Phase 2+). Optional for backward compat with tests.
    store: Option<Arc<TaskStore>>,
    /// Repo identifier for store operations (e.g. "owner/repo").
    repo: String,
}

impl Clone for TaskManager {
    fn clone(&self) -> Self {
        Self {
            db: self.db.clone(),
            backend: self.backend.clone(),
            store: self.store.clone(),
            repo: self.repo.clone(),
        }
    }
}

impl TaskManager {
    #[allow(dead_code)] // Used in tests
    pub fn new(db: Arc<Db>, backend: Arc<dyn ExternalBackend>) -> Self {
        Self {
            db,
            backend,
            store: None,
            repo: String::new(),
        }
    }

    /// Create a TaskManager with the unified store for dual-write support.
    pub fn with_store(
        db: Arc<Db>,
        backend: Arc<dyn ExternalBackend>,
        store: Arc<TaskStore>,
        repo: String,
    ) -> Self {
        Self {
            db,
            backend,
            store: Some(store),
            repo,
        }
    }

    pub async fn create_task(&self, req: CreateTaskRequest) -> anyhow::Result<Task> {
        match req.task_type {
            TaskType::Internal => {
                let id = self
                    .db
                    .create_internal_task(&req.title, &req.body, &req.source, &req.source_id)
                    .await?;
                let task = self.db.get_internal_task(id).await?;
                Ok(Task::Internal(task))
            }
            TaskType::External => {
                let ext_id = self
                    .backend
                    .create_task(&req.title, &req.body, &req.labels)
                    .await?;
                let task = self.backend.get_task(&ext_id).await?;
                Ok(Task::External(task))
            }
        }
    }

    pub async fn get_task(&self, id: i64) -> anyhow::Result<Task> {
        match self.db.get_internal_task(id).await {
            Ok(internal) => Ok(Task::Internal(internal)),
            Err(internal_err) => {
                let ext_id = ExternalId(id.to_string());
                match self.backend.get_task(&ext_id).await {
                    Ok(external) => Ok(Task::External(external)),
                    Err(external_err) => Err(internal_err.context(format!(
                        "task {id} not found internally or externally (external: {external_err})"
                    ))),
                }
            }
        }
    }

    /// List tasks by status, source, or both.
    /// Returns both internal (SQLite) and external (GitHub) tasks.
    pub async fn list_tasks(&self, filter: TaskFilter) -> anyhow::Result<Vec<Task>> {
        let mut tasks = Vec::new();

        if let Some(status_str) = &filter.status {
            // Map string status to TaskStatus and Status
            let task_status = TaskStatus::from_str(status_str).unwrap_or(TaskStatus::New);
            let backend_status = match status_str.as_str() {
                "new" => Status::New,
                "routed" => Status::Routed,
                "in_progress" => Status::InProgress,
                "done" => Status::Done,
                "blocked" => Status::Blocked,
                "in_review" => Status::InReview,
                "needs_review" => Status::NeedsReview,
                _ => Status::New,
            };

            // Get internal tasks with this status
            let internal_tasks = self.db.list_internal_tasks_by_status(task_status).await?;

            // Apply source filter if specified
            for t in internal_tasks {
                if let Some(ref source) = filter.source {
                    if t.source != *source {
                        continue;
                    }
                }
                tasks.push(Task::Internal(t));
            }

            // Get external tasks with this status
            let external_tasks = self.backend.list_by_status(backend_status).await?;
            for t in external_tasks {
                tasks.push(Task::External(t));
            }
        } else if let Some(source) = &filter.source {
            // Only source filter — query all internal tasks across all statuses
            let all_internal = self.db.list_all_internal_tasks().await?;
            for t in all_internal {
                if t.source == *source {
                    tasks.push(Task::Internal(t));
                }
            }
        } else {
            // No filters — return all internal tasks + new external tasks
            let internal_tasks = self.db.list_all_internal_tasks().await?;
            for t in internal_tasks {
                tasks.push(Task::Internal(t));
            }
            let external_tasks = self.backend.list_by_status(Status::New).await?;
            for t in external_tasks {
                tasks.push(Task::External(t));
            }
        }

        Ok(tasks)
    }

    /// Get all external tasks across all statuses (for status summary).
    pub async fn list_all_external_tasks(&self) -> anyhow::Result<Vec<ExternalTask>> {
        self.backend.list_all_tasks().await
    }

    /// Get external tasks by status (for engine use)
    pub async fn list_external_by_status(
        &self,
        status: Status,
    ) -> anyhow::Result<Vec<ExternalTask>> {
        self.backend.list_by_status(status).await
    }

    /// Get open tasks that are routable (no status:* label or status:new).
    /// Includes both external (GitHub) tasks and internal (SQLite) tasks in New status.
    pub async fn list_routable(&self) -> anyhow::Result<Vec<ExternalTask>> {
        let mut tasks = self.backend.list_routable().await?;

        // Include internal tasks with New status so the engine can dispatch them.
        let internal_new = self
            .db
            .list_internal_tasks_by_status(TaskStatus::New)
            .await?;
        for t in internal_new {
            tasks.push(ExternalTask {
                id: ExternalId(format!("internal:{}", t.id)),
                title: t.title,
                body: t.body,
                state: "open".to_string(),
                labels: vec!["status:new".to_string()],
                author: t.source,
                created_at: t.created_at.to_rfc3339(),
                updated_at: t.updated_at.to_rfc3339(),
                url: String::new(),
            });
        }

        Ok(tasks)
    }

    /// Update the status of an internal or external task by its string ID.
    /// For `"internal:{n}"` IDs, updates SQLite. For all others, calls the backend.
    /// Also dual-writes to the unified store if available.
    pub async fn update_task_status(&self, id: &ExternalId, status: Status) -> anyhow::Result<()> {
        let result = if let Some(internal_id) = parse_internal_id(&id.0) {
            self.db
                .update_internal_task_status(internal_id, status_to_task_status(status))
                .await
        } else {
            self.backend.update_status(id, status).await
        };

        // Dual-write: mirror status to unified store (best-effort)
        if result.is_ok() {
            if let Some(ref store) = self.store {
                if let Ok(Some(store_id)) = store.resolve_task_id(&self.repo, &id.0).await {
                    let _ = store
                        .update_status(store_id, status_to_task_status(status))
                        .await;
                }
            }
        }

        result
    }

    /// List internal tasks by DB status (used by the dispatch phase).
    pub async fn db_list_internal_by_status(
        &self,
        status: TaskStatus,
    ) -> anyhow::Result<Vec<InternalTask>> {
        self.db.list_internal_tasks_by_status(status).await
    }

    pub async fn publish_task(&self, id: i64, labels: &[String]) -> anyhow::Result<ExternalId> {
        let internal = self.db.get_internal_task(id).await?;
        let ext_id = self
            .backend
            .create_task(&internal.title, &internal.body, labels)
            .await?;
        self.db
            .update_internal_task_status(id, TaskStatus::Done)
            .await?;
        Ok(ext_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::{ExternalId, ExternalTask, Mention};
    use crate::store::{NewTask, TaskStore};
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    /// Mock backend that records update_status calls.
    struct MockBackend {
        status_updates: Arc<Mutex<Vec<(String, Status)>>>,
    }

    impl MockBackend {
        fn new() -> Self {
            Self {
                status_updates: Arc::new(Mutex::new(vec![])),
            }
        }
    }

    #[async_trait]
    impl ExternalBackend for MockBackend {
        fn name(&self) -> &str {
            "mock"
        }
        async fn create_task(
            &self,
            _t: &str,
            _b: &str,
            _l: &[String],
        ) -> anyhow::Result<ExternalId> {
            Ok(ExternalId("new".to_string()))
        }
        async fn get_task(&self, id: &ExternalId) -> anyhow::Result<ExternalTask> {
            Ok(ExternalTask {
                id: id.clone(),
                title: "Mock".to_string(),
                body: "".to_string(),
                state: "open".to_string(),
                labels: vec![],
                author: "bot".to_string(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                url: "".to_string(),
            })
        }
        async fn list_by_status(&self, _s: Status) -> anyhow::Result<Vec<ExternalTask>> {
            Ok(vec![])
        }
        async fn list_routable(&self) -> anyhow::Result<Vec<ExternalTask>> {
            Ok(vec![])
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
        let db = Arc::new(crate::db::Db::open_memory().unwrap());
        let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        let tm = TaskManager::with_store(db, backend, store.clone(), "owner/repo".to_string());

        assert!(tm.store.is_some(), "store should be set via with_store");
        assert_eq!(tm.repo, "owner/repo");
    }

    #[tokio::test]
    async fn update_status_dual_writes_to_store_for_external_task() {
        let db = Arc::new(crate::db::Db::open_memory().unwrap());
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

        let tm =
            TaskManager::with_store(db, backend.clone(), store.clone(), "owner/repo".to_string());

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
        let db = Arc::new(crate::db::Db::open_memory().unwrap());
        let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        let tm = TaskManager::with_store(db, backend, store.clone(), "owner/repo".to_string());

        // Update for a task not in the store — should not error
        let result = tm
            .update_task_status(&ExternalId("999".to_string()), Status::Done)
            .await;
        assert!(result.is_ok(), "should succeed even when task not in store");
    }

    #[tokio::test]
    async fn update_status_works_without_store() {
        let db = Arc::new(crate::db::Db::open_memory().unwrap());
        let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());

        // Use old constructor (no store)
        let tm = TaskManager::new(db, backend);

        let result = tm
            .update_task_status(&ExternalId("42".to_string()), Status::Routed)
            .await;
        assert!(
            result.is_ok(),
            "should work without store (backward compat)"
        );
    }

    #[tokio::test]
    async fn update_status_dual_writes_for_internal_task() {
        let db = Arc::new(crate::db::Db::open_memory().unwrap());
        db.migrate().await.unwrap();
        let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
        let store = Arc::new(TaskStore::open_memory().await.unwrap());

        // Create an internal task in the DB
        let internal_id = db
            .create_internal_task("Internal task", "body", "cron", "job:1")
            .await
            .unwrap();

        let tm =
            TaskManager::with_store(db.clone(), backend, store.clone(), "owner/repo".to_string());

        // Update the internal task status
        let id_str = format!("internal:{}", internal_id);
        tm.update_task_status(&ExternalId(id_str), Status::Routed)
            .await
            .unwrap();

        // Verify DB was updated
        let task = db.get_internal_task(internal_id).await.unwrap();
        assert_eq!(task.status, TaskStatus::Routed);

        // Store dual-write: internal tasks return None from resolve_task_id,
        // so store should NOT be updated (by design — internal tasks not yet in store)
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
}
