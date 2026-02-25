use crate::backends::{ExternalBackend, ExternalId, ExternalTask, Status};
use crate::db::{Db, InternalTask, TaskStatus};
use chrono;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    db: Arc<Db>,
    backend: Arc<dyn ExternalBackend>,
}

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
            // Only source filter, no status filter
            // For now, only internal tasks have a source field
            let all_internal = self
                .db
                .list_internal_tasks_by_status(TaskStatus::New)
                .await?;
            for t in all_internal {
                if t.source == *source {
                    tasks.push(Task::Internal(t));
                }
            }
        } else {
            // No filters - return all new tasks (both internal and external)
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

    /// Get external tasks by status (for engine use)
    pub async fn list_external_by_status(
        &self,
        status: Status,
    ) -> anyhow::Result<Vec<ExternalTask>> {
        self.backend.list_by_status(status).await
    }

    /// Get internal tasks by status (for engine use)
    pub async fn list_internal_by_status(
        &self,
        status: TaskStatus,
    ) -> anyhow::Result<Vec<InternalTask>> {
        self.db.list_internal_tasks_by_status(status).await
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
                    Status::InReview => TaskStatus::Blocked,
                    Status::NeedsReview => TaskStatus::NeedsReview,
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

    /// Assign an agent to a task (route the task to a specific agent)
    pub async fn route_task(&self, id: i64, agent: &str) -> anyhow::Result<()> {
        match self.db.get_internal_task(id).await {
            Ok(_) => {
                // Internal task: set the agent field and update status to routed
                self.db.set_internal_task_agent(id, Some(agent)).await?;
                self.db
                    .update_internal_task_status(id, TaskStatus::Routed)
                    .await?;
                Ok(())
            }
            Err(_) => {
                // External task: use backend to add the agent label
                let ext_id = ExternalId(id.to_string());
                // Add agent label (e.g., "agent:claude")
                let agent_label = format!("agent:{}", agent);
                self.backend.set_labels(&ext_id, &[agent_label]).await?;
                // Update status to routed
                self.backend.update_status(&ext_id, Status::Routed).await
            }
        }
    }

    /// Mark a task as blocked with a reason
    pub async fn block_task(&self, id: i64, reason: &str) -> anyhow::Result<()> {
        match self.db.get_internal_task(id).await {
            Ok(_) => {
                // Internal task: set block reason and update status
                self.db
                    .set_internal_task_block_reason(id, Some(reason))
                    .await?;
                self.db
                    .update_internal_task_status(id, TaskStatus::Blocked)
                    .await
            }
            Err(_) => {
                // External task: post a comment with the reason and update status
                let ext_id = ExternalId(id.to_string());
                let comment = format!("Task blocked: {}", reason);
                self.backend.post_comment(&ext_id, &comment).await?;
                self.backend.update_status(&ext_id, Status::Blocked).await
            }
        }
    }

    /// Unblock a task (check if all children are done first)
    pub async fn unblock_task(&self, id: i64) -> anyhow::Result<()> {
        match self.db.get_internal_task(id).await {
            Ok(_) => {
                // Internal task: check if children are done, then unblock
                let all_children_done = self.db.are_all_children_done(id).await?;
                if !all_children_done {
                    anyhow::bail!("Cannot unblock: not all child tasks are done");
                }
                self.db.set_internal_task_block_reason(id, None).await?;
                self.db
                    .update_internal_task_status(id, TaskStatus::New)
                    .await
            }
            Err(_) => {
                // External task: unblock directly
                let ext_id = ExternalId(id.to_string());
                self.backend.update_status(&ext_id, Status::New).await
            }
        }
    }
}
