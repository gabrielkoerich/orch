use crate::backends::{ExternalBackend, ExternalId, ExternalTask, Status};
use crate::db::{Db, InternalTask, TaskStatus};
use chrono;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub enum TaskType {
    External,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct CreateTaskRequest {
    pub title: String,
    pub body: String,
    pub task_type: TaskType,
    pub labels: Vec<String>,
    pub source: String,
    pub source_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct TaskFilter {
    pub status: Option<String>,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
#[allow(dead_code)]
pub enum Task {
    External(ExternalTask),
    Internal(InternalTask),
}

#[allow(dead_code)]
pub struct TaskManager {
    db: Arc<Db>,
    backend: Arc<dyn ExternalBackend>,
}

#[allow(dead_code)]
impl TaskManager {
    pub fn new(db: Arc<Db>, backend: Arc<dyn ExternalBackend>) -> Self {
        Self { db, backend }
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
            Err(_) => {
                let ext_id = ExternalId(id.to_string());
                let external = self.backend.get_task(&ext_id).await?;
                Ok(Task::External(external))
            }
        }
    }

    pub async fn list_tasks(&self, filter: TaskFilter) -> anyhow::Result<Vec<Task>> {
        let mut tasks = Vec::new();

        if let Some(status) = &filter.status {
            let internal_tasks = self
                .db
                .list_internal_tasks_by_status(
                    TaskStatus::from_str(status).unwrap_or(TaskStatus::New),
                )
                .await?;
            for t in internal_tasks {
                tasks.push(Task::Internal(t));
            }
        }

        if let Some(source) = &filter.source {
            let all_internal = self
                .db
                .list_internal_tasks_by_status(TaskStatus::New)
                .await?;
            let filtered: Vec<InternalTask> = all_internal
                .into_iter()
                .filter(|t| t.source == *source)
                .collect();
            for t in filtered {
                tasks.push(Task::Internal(t));
            }
        }

        if filter.status.is_none() && filter.source.is_none() {
            let internal_tasks = self
                .db
                .list_internal_tasks_by_status(TaskStatus::New)
                .await?;
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

    pub async fn update_status(&self, id: i64, status: Status) -> anyhow::Result<()> {
        match self.db.get_internal_task(id).await {
            Ok(_) => {
                let task_status = match status {
                    Status::New => TaskStatus::New,
                    Status::Routed => TaskStatus::Routed,
                    Status::InProgress => TaskStatus::InProgress,
                    Status::Done => TaskStatus::Done,
                    Status::Blocked => TaskStatus::Blocked,
                    Status::InReview | Status::NeedsReview => TaskStatus::Blocked,
                };
                self.db.update_internal_task_status(id, task_status).await
            }
            Err(_) => {
                let ext_id = ExternalId(id.to_string());
                self.backend.update_status(&ext_id, status).await
            }
        }
    }

    pub async fn delete_task(&self, id: i64) -> anyhow::Result<()> {
        match self.db.get_internal_task(id).await {
            Ok(_) => self.db.delete_internal_task(id).await,
            Err(_) => {
                tracing::warn!(id, "cannot delete external tasks via TaskManager");
                Ok(())
            }
        }
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

    /// Unblock parent tasks whose children are all done.
    ///
    /// Returns the list of task IDs that were unblocked.
    pub async fn unblock_parents(&self) -> anyhow::Result<Vec<i64>> {
        let blocked = self.backend.list_by_status(Status::Blocked).await?;
        let mut unblocked = Vec::new();

        for task in blocked {
            // Get sub-issues (children)
            let children = match self.backend.get_sub_issues(&task.id).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::debug!(task_id = task.id.0, ?e, "get_sub_issues failed, skipping");
                    continue;
                }
            };

            if children.is_empty() {
                tracing::debug!(task_id = task.id.0, "no sub-issues found, skipping unblock");
                continue;
            }

            // Check if all children are done
            let mut all_done = true;
            for child_id in &children {
                match self.backend.get_task(child_id).await {
                    Ok(child_task) => {
                        if !child_task.labels.iter().any(|l| l == "status:done") {
                            all_done = false;
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::debug!(child_id = child_id.0, ?e, "get_task failed for child");
                        all_done = false;
                        break;
                    }
                }
            }

            if all_done {
                tracing::info!(
                    task_id = task.id.0,
                    "all sub-tasks completed, unblocking parent"
                );
                self.backend.update_status(&task.id, Status::New).await?;
                self.backend
                    .post_comment(
                        &task.id,
                        &format!(
                            "[{}] All sub-tasks completed. Unblocking parent task.",
                            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ")
                        ),
                    )
                    .await?;

                // Parse the ID as i64 for the return list
                if let Ok(id_num) = task.id.0.parse::<i64>() {
                    unblocked.push(id_num);
                }
            }
        }

        Ok(unblocked)
    }
}
