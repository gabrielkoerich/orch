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
