use crate::backends::{ExternalBackend, ExternalId, ExternalTask, Status};
use crate::store::{self, TaskStatus, TaskStore};
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
#[allow(clippy::large_enum_variant)]
pub enum Task {
    External(ExternalTask),
    Internal(store::Task),
}

/// Pre-update snapshot of a task's state, used to enrich events with
/// the old status and context fields (agent, model, branch, etc.).
#[derive(Debug, Clone, Default)]
struct TaskSnapshot {
    old_status: Option<String>,
    agent: Option<String>,
    model: Option<String>,
    branch: Option<String>,
    pr_number: Option<String>,
    error: Option<String>,
    title: Option<String>,
    summary: Option<String>,
    duration_seconds: Option<f64>,
}

pub struct TaskManager {
    backend: Arc<dyn ExternalBackend>,
    /// Unified task store (Phase 2+). Optional for backward compat with tests.
    store: Option<Arc<TaskStore>>,
    /// Repo identifier for store operations (e.g. "owner/repo").
    repo: String,
    /// Event bus sender — publishes TaskEvent on every status change.
    event_tx: Option<tokio::sync::broadcast::Sender<crate::engine::events::TaskEvent>>,
}

impl Clone for TaskManager {
    fn clone(&self) -> Self {
        Self {
            backend: self.backend.clone(),
            store: self.store.clone(),
            repo: self.repo.clone(),
            event_tx: self.event_tx.clone(),
        }
    }
}

impl TaskManager {
    #[allow(dead_code)] // Used in tests
    pub fn new(backend: Arc<dyn ExternalBackend>) -> Self {
        Self {
            backend,
            store: None,
            repo: String::new(),
            event_tx: None,
        }
    }

    /// Get reference to the task store (if available).
    #[allow(dead_code)]
    pub fn store(&self) -> Option<&Arc<TaskStore>> {
        self.store.as_ref()
    }

    /// Get reference to the repo identifier.
    #[allow(dead_code)]
    pub fn repo(&self) -> &str {
        &self.repo
    }

    /// Create a TaskManager with the unified store for dual-write support.
    pub fn with_store(
        backend: Arc<dyn ExternalBackend>,
        store: Arc<TaskStore>,
        repo: String,
    ) -> Self {
        Self {
            backend,
            store: Some(store),
            repo,
            event_tx: None,
        }
    }

    /// Create a TaskManager with event bus support.
    pub fn with_events(
        backend: Arc<dyn ExternalBackend>,
        store: Arc<TaskStore>,
        repo: String,
        event_tx: tokio::sync::broadcast::Sender<crate::engine::events::TaskEvent>,
    ) -> Self {
        Self {
            backend,
            store: Some(store),
            repo,
            event_tx: Some(event_tx),
        }
    }

    pub async fn create_task(&self, req: CreateTaskRequest) -> anyhow::Result<Task> {
        match req.task_type {
            TaskType::Internal => {
                let store = self
                    .store
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("store required for internal tasks"))?;
                let id = store
                    .create_internal(
                        &self.repo,
                        &req.title,
                        &req.body,
                        &req.source,
                        &req.source_id,
                    )
                    .await?;
                let task = store.get(id).await?;
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
        // Try store first (covers both internal and external tasks)
        if let Some(ref store) = self.store {
            if let Ok(task) = store.get(id).await {
                if task.origin == "internal" {
                    return Ok(Task::Internal(task));
                }
                // External task found in store — still return as External from backend
                // for compatibility with callers that expect ExternalTask fields
            }
        }
        // Fall back to backend for external tasks
        let ext_id = ExternalId(id.to_string());
        match self.backend.get_task(&ext_id).await {
            Ok(external) => Ok(Task::External(external)),
            Err(external_err) => Err(anyhow::anyhow!(
                "task {id} not found in store or externally (external: {external_err})"
            )),
        }
    }

    /// List tasks by status, source, or both.
    /// Returns both internal (store) and external (GitHub) tasks.
    pub async fn list_tasks(&self, filter: TaskFilter) -> anyhow::Result<Vec<Task>> {
        let mut tasks = Vec::new();

        if let Some(status_str) = &filter.status {
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

            // Get internal tasks from store
            if let Some(ref store) = self.store {
                let internal_tasks = store
                    .list_internal_by_status(&self.repo, task_status)
                    .await?;
                for t in internal_tasks {
                    if let Some(ref source) = filter.source {
                        if t.source != *source {
                            continue;
                        }
                    }
                    tasks.push(Task::Internal(t));
                }
            }

            // Get external tasks with this status (store-first, same pattern as list_external_by_status)
            let external_tasks = self.list_external_by_status(backend_status).await?;
            for t in external_tasks {
                tasks.push(Task::External(t));
            }
        } else if let Some(source) = &filter.source {
            // Only source filter — query internal tasks by source directly in SQL
            if let Some(ref store) = self.store {
                let by_source = store.list_internal_by_source(&self.repo, source).await?;
                for t in by_source {
                    tasks.push(Task::Internal(t));
                }
            }
        } else {
            // No filters — return all active tasks (exclude done) via SQL WHERE
            if let Some(ref store) = self.store {
                let active_tasks = store.list_active(&self.repo).await?;
                for t in active_tasks {
                    if t.origin == "internal" {
                        tasks.push(Task::Internal(t));
                    } else {
                        tasks.push(Task::External(store_task_to_external(&t)));
                    }
                }
            } else {
                // No store — fall back to backend for external tasks
                let external_tasks = self.backend.list_all_tasks().await?;
                for t in external_tasks {
                    tasks.push(Task::External(t));
                }
            }
        }

        Ok(tasks)
    }

    /// Get all external tasks across all statuses (for status summary).
    /// Reads from the store when available, falling back to the backend.
    pub async fn list_all_external_tasks(&self) -> anyhow::Result<Vec<ExternalTask>> {
        if let Some(ref store) = self.store {
            let tasks = store.list_all_external(&self.repo).await?;
            if !tasks.is_empty() {
                return Ok(tasks.iter().map(store_task_to_external).collect());
            }
        }
        self.backend.list_all_tasks().await
    }

    /// Get external tasks by status (for engine use).
    /// Store-first: reads from the store when available, falls back to backend
    /// if the store has no tasks (e.g. before first sync).
    pub async fn list_external_by_status(
        &self,
        status: Status,
    ) -> anyhow::Result<Vec<ExternalTask>> {
        if let Some(ref store) = self.store {
            let db_status = status_to_task_status(status);
            let external = store.list_external_by_status(&self.repo, db_status).await?;
            // Use the fetched results when non-empty; only check the sentinel when empty
            // (empty could mean "no tasks with this status" OR "store not yet synced").
            if !external.is_empty() || store.has_external_tasks(&self.repo).await {
                let external: Vec<ExternalTask> =
                    external.iter().map(store_task_to_external).collect();
                return Ok(external);
            }
        }
        self.backend.list_by_status(status).await
    }

    /// Get all tasks (external + internal) by status.
    /// Store-first for external tasks: reads from the store when available,
    /// falls back to backend if the store has no tasks.
    /// Always includes internal tasks from the store when available.
    pub async fn list_all_by_status(&self, status: Status) -> anyhow::Result<Vec<ExternalTask>> {
        let mut tasks = Vec::new();

        // Get external tasks with store-first pattern
        if let Some(ref store) = self.store {
            let db_status = status_to_task_status(status);
            let external = store.list_external_by_status(&self.repo, db_status).await?;
            // Use the fetched results when non-empty; only check the sentinel when empty
            // (empty could mean "no tasks with this status" OR "store not yet synced").
            if !external.is_empty() || store.has_external_tasks(&self.repo).await {
                tasks.extend(external.into_iter().map(|t| store_task_to_external(&t)));
            }
        } else {
            // No store - get from backend
            tasks.extend(self.backend.list_by_status(status).await?);
        }

        // Always include internal tasks from store when available
        if self.store.is_some() {
            if let Ok(internal) = self
                .list_internal_by_status(status_to_task_status(status))
                .await
            {
                tasks.extend(internal);
            }
        }

        Ok(tasks)
    }

    /// Get open tasks that are routable (status = new).
    /// Store-first: reads all New tasks (external + internal) from the store,
    /// falling back to the backend for external tasks before first sync.
    pub async fn list_routable(&self) -> anyhow::Result<Vec<ExternalTask>> {
        if let Some(ref store) = self.store {
            let all_new = store.list_routable(&self.repo).await?;
            // Check if any external (non-internal) tasks are in the result — if so
            // the store has external tasks and we can skip the has_external_tasks query.
            let has_external_new = all_new.iter().any(|t| t.origin != "internal");
            let tasks: Vec<ExternalTask> = all_new.iter().map(store_task_to_external).collect();
            if has_external_new || store.has_external_tasks(&self.repo).await {
                return Ok(tasks);
            }
        }

        // Fallback: backend for external + store for internal
        let mut tasks = self.backend.list_routable().await?;
        if let Some(ref store) = self.store {
            let internal_new = store
                .list_internal_by_status(&self.repo, TaskStatus::New)
                .await?;
            for t in internal_new {
                tasks.push(store_task_to_external(&t));
            }
        }
        Ok(tasks)
    }

    /// Update the status of an internal or external task by its string ID.
    /// For `"internal:{n}"` IDs, updates the store. For all others, calls the backend.
    /// Store is always updated when available.
    pub async fn update_task_status(&self, id: &ExternalId, status: Status) -> anyhow::Result<()> {
        let task_status = status_to_task_status(status);

        // Read pre-update snapshot from the store (old_status + context fields).
        // This must happen BEFORE any store.update_status() call so we capture
        // the previous state for the event.
        let (pre_snapshot, snapshot_store_id) = self.read_task_snapshot(&id.0).await;

        if is_internal_id(&id.0) {
            // Internal tasks: store is the single source of truth
            let store = self
                .store
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("store required for internal task status update"))?;
            let store_id = snapshot_store_id
                .ok_or_else(|| anyhow::anyhow!("internal task {} not found in store", id.0))?;
            if task_status != TaskStatus::Blocked {
                store.set_block_reason(store_id, None).await?;
            }
            store.update_status(store_id, task_status).await?;
            // Publish event to bus only after confirmed update
            self.publish_event(id, status, &pre_snapshot, None);
            return Ok(());
        }

        // External tasks: store-first, then mirror to backend.
        // SQLite is the source of truth; backend sync is best-effort.
        if let Some(ref store) = self.store {
            if let Some(store_id) = snapshot_store_id {
                if task_status != TaskStatus::Blocked {
                    store.set_block_reason(store_id, None).await?;
                }
                store.update_status(store_id, task_status).await?;
            }
        }

        // Mirror to backend (GitHub labels). Log failure but don't fail the operation
        // since the store already has the correct status.
        if let Err(e) = self.backend.update_status(id, status).await {
            tracing::warn!(
                task_id = id.0,
                ?status,
                err = %e,
                "failed to mirror status to backend — store is authoritative"
            );
        }

        // Publish event to bus
        self.publish_event(id, status, &pre_snapshot, None);

        Ok(())
    }

    pub async fn update_task_status_and_result(
        &self,
        id: &ExternalId,
        status: Status,
        updates: &[(&str, serde_json::Value)],
    ) -> anyhow::Result<()> {
        let task_status = status_to_task_status(status);

        // Read pre-update snapshot from the store (old_status + context fields).
        let (pre_snapshot, snapshot_store_id) = self.read_task_snapshot(&id.0).await;

        if is_internal_id(&id.0) {
            // Internal tasks: store is the single source of truth
            let store = self
                .store
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("store required for internal task status update"))?;
            let store_id = snapshot_store_id
                .ok_or_else(|| anyhow::anyhow!("internal task {} not found in store", id.0))?;

            store
                .update_status_and_fields(store_id, task_status, updates)
                .await?;
            // Publish event to bus only after confirmed update
            self.publish_event(id, status, &pre_snapshot, None);
            return Ok(());
        }

        // External tasks: store-first, then mirror to backend.
        if let Some(ref store) = self.store {
            if let Some(store_id) = snapshot_store_id {
                store
                    .update_status_and_fields(store_id, task_status, updates)
                    .await?;
            }
        }

        // Mirror to backend (GitHub labels).
        if let Err(e) = self.backend.update_status(id, status).await {
            tracing::warn!(
                task_id = id.0,
                ?status,
                err = %e,
                "failed to mirror status to backend — store is authoritative"
            );
        }

        // Publish event to bus
        self.publish_event(id, status, &pre_snapshot, None);

        Ok(())
    }

    /// Update the status of a task only if it is currently in `expected_status`.
    ///
    /// Returns `Ok(true)` if the update was applied, `Ok(false)` if the task had
    /// already transitioned to a different status (TOCTOU-safe — no-op on race).
    pub async fn update_task_status_if(
        &self,
        id: &ExternalId,
        status: Status,
        expected_status: Status,
    ) -> anyhow::Result<bool> {
        let task_status = status_to_task_status(status);
        let expected_task_status = status_to_task_status(expected_status);

        let (pre_snapshot, snapshot_store_id) = self.read_task_snapshot(&id.0).await;

        if is_internal_id(&id.0) {
            let store = self
                .store
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("store required for internal task status update"))?;
            let store_id = snapshot_store_id
                .ok_or_else(|| anyhow::anyhow!("internal task {} not found in store", id.0))?;
            if task_status != crate::store::TaskStatus::Blocked {
                store.set_block_reason(store_id, None).await?;
            }
            let updated = store
                .update_status_if(store_id, task_status, expected_task_status)
                .await?;
            if updated {
                self.publish_event(id, status, &pre_snapshot, None);
            }
            return Ok(updated);
        }

        // External tasks: conditional store update, then mirror to backend.
        // The conditional guarantee (update only when current == expected) relies on
        // having a local store record. If the unified store isn't available or the
        // task has no store record (e.g. not yet synced), we cannot safely perform
        // the conditional update — return false and warn so callers treat this as
        // a no-op instead of assuming the update succeeded.
        if let Some(ref store) = self.store {
            if let Some(store_id) = snapshot_store_id {
                if task_status != crate::store::TaskStatus::Blocked {
                    store.set_block_reason(store_id, None).await?;
                }
                let updated = store
                    .update_status_if(store_id, task_status, expected_task_status)
                    .await?;
                if !updated {
                    return Ok(false);
                }
            } else {
                tracing::warn!(
                    task_id = id.0,
                    ?expected_task_status,
                    "update_task_status_if: external task has no store record — cannot perform conditional update"
                );
                return Ok(false);
            }
        } else {
            tracing::warn!(
                task_id = id.0,
                ?expected_task_status,
                "update_task_status_if: no store available — cannot perform conditional update for external task"
            );
            return Ok(false);
        }

        if let Err(e) = self.backend.update_status(id, status).await {
            tracing::warn!(
                task_id = id.0,
                ?status,
                err = %e,
                "failed to mirror status to backend — store is authoritative"
            );
        }

        self.publish_event(id, status, &pre_snapshot, None);
        Ok(true)
    }

    /// Update the status of a task and include elapsed duration in the event.
    ///
    /// Use this at task completion points (success or failure) so the notify
    /// subscriber can include accurate timing in channel messages instead of 0.0.
    pub async fn update_task_status_with_duration(
        &self,
        id: &ExternalId,
        status: Status,
        duration_seconds: Option<f64>,
    ) -> anyhow::Result<()> {
        let task_status = status_to_task_status(status);
        let (pre_snapshot, snapshot_store_id) = self.read_task_snapshot(&id.0).await;

        if is_internal_id(&id.0) {
            let store = self
                .store
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("store required for internal task status update"))?;
            let store_id = snapshot_store_id
                .ok_or_else(|| anyhow::anyhow!("internal task {} not found in store", id.0))?;
            if task_status != TaskStatus::Blocked {
                store.set_block_reason(store_id, None).await?;
            }
            store.update_status(store_id, task_status).await?;
            self.publish_event(id, status, &pre_snapshot, duration_seconds);
            return Ok(());
        }

        if let Some(ref store) = self.store {
            if let Some(store_id) = snapshot_store_id {
                if task_status != TaskStatus::Blocked {
                    store.set_block_reason(store_id, None).await?;
                }
                store.update_status(store_id, task_status).await?;
            }
        }

        if let Err(e) = self.backend.update_status(id, status).await {
            tracing::warn!(
                task_id = id.0,
                ?status,
                err = %e,
                "failed to mirror status to backend — store is authoritative"
            );
        }

        self.publish_event(id, status, &pre_snapshot, duration_seconds);

        Ok(())
    }

    /// Read the current task snapshot from the store for event enrichment.
    /// Returns `(snapshot, store_id)` — store_id is `Some` when the task was found,
    /// so callers can reuse it for subsequent store operations without a second lookup.
    async fn read_task_snapshot(&self, task_id: &str) -> (TaskSnapshot, Option<i64>) {
        let Some(ref store) = self.store else {
            return (TaskSnapshot::default(), None);
        };
        let store_id = match store.resolve_task_id(&self.repo, task_id).await {
            Ok(Some(id)) => id,
            _ => return (TaskSnapshot::default(), None),
        };
        match store.get(store_id).await {
            Ok(task) => {
                let duration_seconds = store.latest_task_metric_duration(task_id).await;
                let snapshot = TaskSnapshot {
                    old_status: Some(task.status.as_str().to_string()),
                    agent: task.agent.clone(),
                    model: task.model.clone(),
                    branch: if task.branch.is_empty() {
                        None
                    } else {
                        Some(task.branch.clone())
                    },
                    pr_number: task.pr_number.map(|n| n.to_string()),
                    error: if task.last_error.is_empty() {
                        None
                    } else {
                        Some(task.last_error.clone())
                    },
                    title: if task.title.is_empty() {
                        None
                    } else {
                        Some(task.title.clone())
                    },
                    summary: if task.summary.is_empty() {
                        None
                    } else {
                        Some(task.summary.clone())
                    },
                    duration_seconds,
                };
                (snapshot, Some(store_id))
            }
            Err(_) => (TaskSnapshot::default(), None),
        }
    }

    /// Publish a TaskEvent to the event bus (if wired).
    fn publish_event(
        &self,
        id: &ExternalId,
        status: Status,
        snapshot: &TaskSnapshot,
        duration_seconds: Option<f64>,
    ) {
        if let Some(ref tx) = self.event_tx {
            let event = crate::engine::events::TaskEvent {
                task_id: id.0.clone(),
                repo: self.repo.clone(),
                old_status: snapshot.old_status.clone().unwrap_or_default(),
                new_status: status.as_label().trim_start_matches("status:").to_string(),
                agent: snapshot.agent.clone(),
                model: snapshot.model.clone(),
                pr_number: snapshot.pr_number.clone(),
                branch: snapshot.branch.clone(),
                review_context: None,
                error: snapshot.error.clone(),
                timestamp: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
                title: snapshot.title.clone(),
                summary: snapshot.summary.clone(),
                duration_seconds: duration_seconds.or(snapshot.duration_seconds),
            };
            let _ = tx.send(event);
        }
    }

    /// List internal tasks by status from the store.
    /// Returns store::Task items converted to ExternalTask for backward compatibility.
    pub async fn list_internal_by_status(
        &self,
        status: TaskStatus,
    ) -> anyhow::Result<Vec<ExternalTask>> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("store required"))?;
        let tasks = store.list_internal_by_status(&self.repo, status).await?;
        Ok(tasks
            .into_iter()
            .map(|t| store_task_to_external(&t))
            .collect())
    }

    /// List all internal tasks from the store.
    pub async fn list_all_internal(&self) -> anyhow::Result<Vec<store::Task>> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("store required"))?;
        store.list_all_internal(&self.repo).await
    }

    /// Check whether a child task (identified by its external ID) is done,
    /// using the local store when available and falling back to the GitHub backend.
    ///
    /// Returns `true` if the task is done, `false` if it is not done or cannot be
    /// determined (caller should treat unknown as not-done).
    pub async fn is_child_done(&self, child_id: &ExternalId) -> bool {
        if let Some(ref store) = self.store {
            match store.get_by_external_id(&self.repo, &child_id.0).await {
                Ok(Some(task)) => return task.status == TaskStatus::Done,
                Ok(None) => {
                    // Not in local store yet — fall through to backend
                }
                Err(e) => {
                    tracing::debug!(child = child_id.0, ?e, "store lookup failed for child task");
                    // Fall through to backend
                }
            }
        }
        // Fallback: ask the backend (also handles the no-store case)
        match self.backend.get_task(child_id).await {
            Ok(child) => child.labels.iter().any(|l| l == Status::Done.as_label()),
            Err(e) => {
                tracing::debug!(
                    child = child_id.0,
                    ?e,
                    "failed to fetch child task from backend"
                );
                false
            }
        }
    }

    pub async fn publish_task(&self, id: i64, labels: &[String]) -> anyhow::Result<ExternalId> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("store required for publish_task"))?;
        let task = store.get(id).await?;
        let ext_id = self
            .backend
            .create_task(&task.title, &task.body, labels)
            .await?;
        store.update_external_id(id, &ext_id.0).await?;
        store.update_status(id, TaskStatus::New).await?;
        Ok(ext_id)
    }
}

/// Convert a store::Task to an ExternalTask for backward compatibility with
/// code that expects ExternalTask (engine dispatch, review, etc.).
pub fn store_task_to_external(t: &store::Task) -> ExternalTask {
    let ext_id = t
        .external_id
        .clone()
        .unwrap_or_else(|| format!("internal:{}", t.id));
    ExternalTask {
        id: ExternalId(ext_id),
        title: t.title.clone(),
        body: t.body.clone(),
        state: "open".to_string(),
        labels: {
            let mut labels: Vec<String> = t
                .labels
                .iter()
                .filter(|l| !l.starts_with("status:"))
                .cloned()
                .collect();
            labels.push(format!("status:{}", t.status.as_str()));
            labels
        },
        author: t.source.clone(),
        created_at: t.created_at.clone(),
        updated_at: t.updated_at.clone(),
        url: t.url.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::{ExternalId, ExternalTask, Mention};
    use crate::store::{NewTask, TaskStore};
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

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

        let tm = TaskManager::with_store(backend, store.clone(), "owner/repo".to_string());

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

        let tm = TaskManager::with_store(backend.clone(), store.clone(), "owner/repo".to_string());

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

        let tm = TaskManager::with_store(backend, store.clone(), "owner/repo".to_string());

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
            .create_internal("owner/repo", "Internal task", "body", "cron", "job:1")
            .await
            .unwrap();

        let tm = TaskManager::with_store(backend, store.clone(), "owner/repo".to_string());

        // Update the internal task status via TaskManager
        let id_str = format!("internal:{}", internal_id);
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
        let tm = TaskManager::with_store(backend, store.clone(), "owner/repo".to_string());

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
                assert!(t
                    .external_id
                    .as_deref()
                    .map(|s| s.starts_with("internal:"))
                    .unwrap_or(false));
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
                body: "".to_string(),
                task_type: TaskType::Internal,
                labels: vec![],
                source: "manual".to_string(),
                source_id: "".to_string(),
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

        let (event_tx, mut event_rx) =
            tokio::sync::broadcast::channel::<crate::engine::events::TaskEvent>(16);

        let tm =
            TaskManager::with_events(backend, store.clone(), "owner/repo".to_string(), event_tx);

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

    #[tokio::test]
    async fn get_task_returns_internal_from_store() {
        let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let tm = TaskManager::with_store(backend, store.clone(), "owner/repo".to_string());

        let id = store
            .create_internal("owner/repo", "Fetch me", "body", "cron", "job:1")
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
        let tm = TaskManager::with_store(backend, store.clone(), "owner/repo".to_string());

        // Create internal task (starts as New)
        store
            .create_internal("owner/repo", "Route me", "", "cron", "job:2")
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
        let tm = TaskManager::with_store(backend, store.clone(), "owner/repo".to_string());

        let id = store
            .create_internal("owner/repo", "Already routed", "", "cron", "job:3")
            .await
            .unwrap();
        store
            .update_status(id, crate::store::TaskStatus::InProgress)
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
        let tm = TaskManager::with_store(backend, store.clone(), "owner/repo".to_string());

        store
            .create_internal("owner/repo", "Task A", "", "cron", "a")
            .await
            .unwrap();
        let id_b = store
            .create_internal("owner/repo", "Task B", "", "cron", "b")
            .await
            .unwrap();
        store
            .update_status(id_b, crate::store::TaskStatus::Done)
            .await
            .unwrap();

        let new_tasks = tm
            .list_internal_by_status(crate::store::TaskStatus::New)
            .await
            .unwrap();
        assert_eq!(new_tasks.len(), 1);
        assert_eq!(new_tasks[0].title, "Task A");
        // Verify it's wrapped as ExternalTask with status label
        assert!(new_tasks[0].labels.contains(&"status:new".to_string()));
    }

    #[tokio::test]
    async fn store_task_to_external_maps_fields_correctly() {
        let store = TaskStore::open_memory().await.unwrap();
        let id = store
            .create_internal("owner/repo", "Convert me", "body text", "manual", "m:1")
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
        let tm = TaskManager::with_store(backend, store.clone(), "owner/repo".to_string());

        store
            .create_internal("owner/repo", "T1", "", "cron", "1")
            .await
            .unwrap();
        store
            .create_internal("owner/repo", "T2", "", "cron", "2")
            .await
            .unwrap();
        // Different repo — should not appear
        store
            .create_internal("other/repo", "T3", "", "cron", "3")
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
        let tm = TaskManager::with_store(backend, store.clone(), "owner/repo".to_string());

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
        let tm = TaskManager::with_store(backend, store.clone(), "owner/repo".to_string());

        // Create an internal task with Routed status
        let id = store
            .create_internal("owner/repo", "Internal task", "", "cron", "1")
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
                body: "".to_string(),
                state: "open".to_string(),
                labels: vec!["status:routed".to_string()],
                author: "bot".to_string(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                url: "".to_string(),
            }],
        ));
        let tm = TaskManager::with_store(backend, store.clone(), "owner/repo".to_string());

        let id = store
            .create_internal("owner/repo", "Internal task", "", "cron", "1")
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
        let tm = TaskManager::with_store(backend, store.clone(), "owner/repo".to_string());

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
            .create_internal("owner/repo", "New internal", "", "cron", "1")
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
            body: "".to_string(),
            state: "open".to_string(),
            labels: vec!["status:new".to_string()],
            author: "bot".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: "".to_string(),
        }]));
        let tm = TaskManager::with_store(backend, store.clone(), "owner/repo".to_string());

        store
            .create_internal("owner/repo", "Internal new", "", "cron", "1")
            .await
            .unwrap();

        let routable = tm.list_routable().await.unwrap();
        assert_eq!(routable.len(), 2);
        assert!(routable.iter().any(|task| task.id.0 == "55"));
        assert!(routable
            .iter()
            .any(|task| task.id.0.starts_with("internal:")));
    }

    #[tokio::test]
    async fn list_all_external_tasks_reads_from_store() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend = Arc::new(MockBackend::new());
        let tm = TaskManager::with_store(backend, store.clone(), "owner/repo".to_string());

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
            .create_internal("owner/repo", "Internal", "", "cron", "1")
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
                body: "".to_string(),
                state: "closed".to_string(),
                labels: vec!["status:done".to_string()],
                author: "bot".to_string(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                url: "".to_string(),
            }],
        ));
        let tm = TaskManager::with_store(backend, store.clone(), "owner/repo".to_string());

        store
            .create_internal("owner/repo", "Internal", "", "cron", "1")
            .await
            .unwrap();

        let all = tm.list_all_external_tasks().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id.0, "200");
    }

    // ── store-first update_task_status ──────────────────────────────

    #[tokio::test]
    async fn update_task_status_writes_store_first_for_external() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
        let tm = TaskManager::with_store(backend.clone(), store.clone(), "owner/repo".to_string());

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
        let tm = TaskManager::with_store(backend.clone(), store.clone(), "owner/repo".to_string());

        // Create an internal task
        let store_id = store
            .create_internal("owner/repo", "task", "body", "manual", "")
            .await
            .unwrap();

        let internal_id = ExternalId(format!("internal:{}", store_id));
        tm.update_task_status(&internal_id, Status::InProgress)
            .await
            .unwrap();

        let task = store.get(store_id).await.unwrap();
        assert_eq!(task.status, TaskStatus::InProgress);
    }

    #[tokio::test]
    async fn update_task_status_publishes_event() {
        let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
        let (tx, mut rx) = tokio::sync::broadcast::channel::<crate::engine::events::TaskEvent>(16);
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
        let (tx, mut rx) = tokio::sync::broadcast::channel::<crate::engine::events::TaskEvent>(16);

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

        let tm = TaskManager::with_events(backend, store.clone(), "owner/repo".to_string(), tx);

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
        let (tx, mut rx) = tokio::sync::broadcast::channel::<crate::engine::events::TaskEvent>(16);

        // Create an internal task (starts as New)
        let store_id = store
            .create_internal("owner/repo", "internal ctx test", "body", "manual", "")
            .await
            .unwrap();

        let tm = TaskManager::with_events(backend, store.clone(), "owner/repo".to_string(), tx);

        // Update New → Routed
        let internal_id = ExternalId(format!("internal:{}", store_id));
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
        let tm = TaskManager::with_store(backend, store.clone(), "owner/repo".to_string());

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
            .update_status(store_id, crate::store::TaskStatus::InReview)
            .await
            .unwrap();

        let id = ExternalId("10".to_string());
        let updated = tm
            .update_task_status_if(&id, Status::NeedsReview, Status::InReview)
            .await
            .unwrap();
        assert!(updated, "should update when expected status matches");

        let task = store.get(store_id).await.unwrap();
        assert_eq!(task.status, crate::store::TaskStatus::NeedsReview);
    }

    #[tokio::test]
    async fn update_task_status_if_is_noop_when_status_mismatches() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let backend: Arc<dyn ExternalBackend> = Arc::new(MockBackend::new());
        let (tx, mut rx) = tokio::sync::broadcast::channel::<crate::engine::events::TaskEvent>(16);
        let tm = TaskManager::with_events(backend, store.clone(), "owner/repo".to_string(), tx);

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
            .update_status(store_id, crate::store::TaskStatus::Done)
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
        assert_eq!(task.status, crate::store::TaskStatus::Done);

        // No event must be published
        assert!(
            rx.try_recv().is_err(),
            "no event should be published on a no-op update"
        );
    }
}
