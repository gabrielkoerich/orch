pub mod commands;
pub mod dependencies;
pub mod dispatch_guard;
pub mod events;
pub mod jobs;
pub mod priority;

use crate::backends::{ExternalBackend, ExternalId, ExternalTask, Status};
use crate::engine::tasks::events::TaskEvent;
use crate::store::{self, TaskStatus, TaskStore};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::sync::Arc;
use tokio::sync::broadcast::Sender;

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
        Status::Approved => TaskStatus::Approved,
        Status::NeedsReview => TaskStatus::NeedsReview,
        Status::Waiting => TaskStatus::Waiting,
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
#[expect(
    clippy::large_enum_variant,
    reason = "tasks are few and boxing would touch every match"
)]
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
    event_tx: Option<Sender<TaskEvent>>,
}

impl Debug for TaskManager {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskManager")
            .field("repo", &self.repo)
            .finish_non_exhaustive()
    }
}

impl Clone for TaskManager {
    fn clone(&self) -> Self {
        Self {
            backend: Arc::clone(&self.backend),
            store: self.store.clone(),
            repo: self.repo.clone(),
            event_tx: self.event_tx.clone(),
        }
    }
}

impl TaskManager {
    #[cfg(test)]
    pub fn new(backend: Arc<dyn ExternalBackend>) -> Self {
        Self {
            backend,
            store: None,
            repo: String::new(),
            event_tx: None,
        }
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
        event_tx: Sender<TaskEvent>,
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
                        None,
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
        if let Some(ref store) = self.store
            && let Ok(task) = store.get(id).await
            && task.origin == "internal"
        {
            return Ok(Task::Internal(task));
        }
        // External task found in store — still return as External from backend
        // for compatibility with callers that expect ExternalTask fields
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

        let is_all = filter.status.as_deref() == Some("all");

        if let Some(status_str) = &filter.status {
            if !is_all {
                let task_status = TaskStatus::from_str(status_str)
                    .ok_or_else(|| anyhow::anyhow!(
                        "invalid status: '{status_str}'. allowed: new, routed, in_progress, needs_review, in_review, approved, blocked, waiting, done, all"
                    ))?;
                let backend_status = match status_str.as_str() {
                    "new" => Status::New,
                    "routed" => Status::Routed,
                    "in_progress" => Status::InProgress,
                    "done" => Status::Done,
                    "blocked" => Status::Blocked,
                    "in_review" => Status::InReview,
                    "approved" => Status::Approved,
                    "needs_review" => Status::NeedsReview,
                    "waiting" => Status::Waiting,
                    _ => unreachable!(),
                };

                // Get internal tasks from store
                if let Some(ref store) = self.store {
                    let internal_tasks = store
                        .list_internal_by_status(&self.repo, task_status)
                        .await?;
                    for t in internal_tasks {
                        if let Some(ref source) = filter.source
                            && t.source != *source
                        {
                            continue;
                        }
                        tasks.push(Task::Internal(t));
                    }
                }

                // Get external tasks with this status (store-first, same pattern as list_external_by_status)
                let external_tasks = self.list_external_by_status(backend_status).await?;
                for t in external_tasks {
                    tasks.push(Task::External(t));
                }
            }
        } else if let Some(source) = &filter.source {
            // Only source filter — query internal tasks by source directly in SQL
            if let Some(ref store) = self.store {
                let by_source = store.list_internal_by_source(&self.repo, source).await?;
                for t in by_source {
                    tasks.push(Task::Internal(t));
                }
            }
        }

        // No status filter or --status all: return all tasks for the repo
        if filter.status.is_none() || is_all {
            let mut has_external_in_store = false;
            let store_tasks = if let Some(ref store) = self.store {
                if is_all {
                    let tasks = store.list_all(&self.repo).await?;
                    has_external_in_store = match store.has_external_tasks(&self.repo).await {
                        Ok(has_external) => has_external,
                        Err(e) => {
                            tracing::warn!(
                                repo = %self.repo,
                                error = %e,
                                "failed to check external task sentinel; using store-only list path"
                            );
                            true
                        }
                    };
                    tasks
                } else {
                    let tasks = store.list_active(&self.repo).await?;
                    has_external_in_store = match store.has_external_tasks(&self.repo).await {
                        Ok(has_external) => has_external,
                        Err(e) => {
                            tracing::warn!(
                                repo = %self.repo,
                                error = %e,
                                "failed to check external task sentinel; using store-only list path"
                            );
                            true
                        }
                    };
                    tasks
                }
            } else {
                vec![]
            };

            for t in store_tasks {
                if t.origin == "internal" {
                    tasks.push(Task::Internal(t));
                } else {
                    tasks.push(Task::External(store_task_to_external(&t)));
                }
            }

            if !has_external_in_store {
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
            let has_external_in_store = if external.is_empty() {
                match store.has_external_tasks(&self.repo).await {
                    Ok(has_external) => has_external,
                    Err(e) => {
                        tracing::warn!(
                            repo = %self.repo,
                            status = ?status,
                            error = %e,
                            "failed to check external task sentinel; skipping backend fallback"
                        );
                        true
                    }
                }
            } else {
                true
            };
            if has_external_in_store {
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
            let has_external_in_store = if external.is_empty() {
                match store.has_external_tasks(&self.repo).await {
                    Ok(has_external) => has_external,
                    Err(e) => {
                        tracing::warn!(
                            repo = %self.repo,
                            status = ?status,
                            error = %e,
                            "failed to check external task sentinel; skipping backend fallback"
                        );
                        true
                    }
                }
            } else {
                true
            };
            if has_external_in_store {
                tasks.extend(external.into_iter().map(|t| store_task_to_external(&t)));
            } else {
                // Store not synced yet — fall back to backend for external tasks
                tasks.extend(self.backend.list_by_status(status).await?);
            }
        } else {
            // No store - get from backend
            tasks.extend(self.backend.list_by_status(status).await?);
        }

        // Always include internal tasks from store when available
        if self.store.is_some()
            && let Ok(internal) = self
                .list_internal_by_status(status_to_task_status(status))
                .await
        {
            tasks.extend(internal);
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
            let has_external_in_store = if has_external_new {
                true
            } else {
                match store.has_external_tasks(&self.repo).await {
                    Ok(has_external) => has_external,
                    Err(e) => {
                        tracing::warn!(
                            repo = %self.repo,
                            error = %e,
                            "failed to check external task sentinel; skipping backend fallback"
                        );
                        true
                    }
                }
            };
            if has_external_in_store {
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
        self.update_task_status_with_duration(id, status, None)
            .await
    }

    pub async fn update_task_status_and_result(
        &self,
        id: &ExternalId,
        status: Status,
        updates: &[(&str, serde_json::Value)],
    ) -> anyhow::Result<()> {
        let task_status = status_to_task_status(status);
        self.release_dependents_if_done(task_status, &self.repo, &id.0)
            .await;

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
            self.publish_event(id, status, &pre_snapshot, &self.repo, None);
            return Ok(());
        }

        // External tasks: store-first, then mirror to backend.
        if let Some(ref store) = self.store
            && let Some(store_id) = snapshot_store_id
        {
            store
                .update_status_and_fields(store_id, task_status, updates)
                .await?;
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
        self.publish_event(id, status, &pre_snapshot, &self.repo, None);

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
        self.release_dependents_if_done(task_status, &self.repo, &id.0)
            .await;
        let expected_task_status = status_to_task_status(expected_status);

        let (pre_snapshot, snapshot_store_id) = self.read_task_snapshot(&id.0).await;

        if is_internal_id(&id.0) {
            let store = self
                .store
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("store required for internal task status update"))?;
            let store_id = snapshot_store_id
                .ok_or_else(|| anyhow::anyhow!("internal task {} not found in store", id.0))?;
            let updated = store
                .update_status_if(store_id, task_status, expected_task_status)
                .await?;
            if updated {
                self.publish_event(id, status, &pre_snapshot, &self.repo, None);
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

        self.publish_event(id, status, &pre_snapshot, &self.repo, None);
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
        self.release_dependents_if_done(task_status, &self.repo, &id.0)
            .await;
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
            self.publish_event(id, status, &pre_snapshot, &self.repo, duration_seconds);
            return Ok(());
        }

        if let Some(ref store) = self.store
            && let Some(store_id) = snapshot_store_id
        {
            if task_status != TaskStatus::Blocked {
                store.set_block_reason(store_id, None).await?;
            }
            store.update_status(store_id, task_status).await?;
        }

        if let Err(e) = self.backend.update_status(id, status).await {
            tracing::warn!(
                task_id = id.0,
                ?status,
                err = %e,
                "failed to mirror status to backend — store is authoritative"
            );
        }

        self.publish_event(id, status, &pre_snapshot, &self.repo, duration_seconds);

        Ok(())
    }

    /// Read the current task snapshot from the store for event enrichment.
    /// Returns `(snapshot, store_id)` — store_id is `Some` when the task was found,
    /// so callers can reuse it for subsequent store operations without a second lookup.
    async fn read_task_snapshot(&self, task_id: &str) -> (TaskSnapshot, Option<i64>) {
        let Some(ref store) = self.store else {
            return (TaskSnapshot::default(), None);
        };
        let Ok(Some(store_id)) = store.resolve_task_id(&self.repo, task_id).await else {
            return (TaskSnapshot::default(), None);
        };
        let snapshot = Self::snapshot_for_store_id(store, store_id, task_id).await;
        (snapshot, Some(store_id))
    }

    /// Build a `TaskSnapshot` for an already-resolved numeric store id, without
    /// going through `resolve_task_id`. `metric_task_id` is the external id string
    /// used as the key in `task_metrics` for duration lookup.
    async fn snapshot_for_store_id(
        store: &TaskStore,
        store_id: i64,
        metric_task_id: &str,
    ) -> TaskSnapshot {
        match store.get(store_id).await {
            Ok(task) => {
                let duration_seconds = store.latest_task_metric_duration(metric_task_id).await;
                TaskSnapshot {
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
                }
            }
            Err(e) => {
                tracing::warn!(
                    task_id = store_id,
                    err = %e,
                    "failed to read task snapshot for event enrichment — event context will be empty"
                );
                TaskSnapshot::default()
            }
        }
    }

    /**
     * Drop the blocker read cache for a task that is finishing, so the next snapshot
     * refresh reads it and releases its dependents instead of waiting out the read TTL.
     * Called before the write, because a cleared cache only costs one extra read if the
     * write then fails or no-ops
     **/
    async fn release_dependents_if_done(&self, status: TaskStatus, repo: &str, ext_id: &str) {
        if status != TaskStatus::Done {
            return;
        }
        if let Some(ref store) = self.store {
            dependencies::clear_blocker_read_cache(store, repo, ext_id).await;
        }
    }

    /// Publish a TaskEvent to the event bus (if wired).
    fn publish_event(
        &self,
        id: &ExternalId,
        status: Status,
        snapshot: &TaskSnapshot,
        repo: &str,
        duration_seconds: Option<f64>,
    ) {
        if let Some(ref tx) = self.event_tx {
            let event = TaskEvent {
                task_id: id.0.clone(),
                repo: repo.to_string(),
                old_status: snapshot.old_status.clone().unwrap_or_default(),
                new_status: status.as_str().to_string(),
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

    /// Mark an already-resolved task Done directly by its numeric store id and owning
    /// repo, bypassing the repo-scoped `resolve_task_id` lookup that `self.repo` would
    /// otherwise apply. For callers that already hold the correct store row from a
    /// cross-repo query — e.g. the global CI-failure unblock sweep — and would otherwise
    /// fail to resolve a task belonging to a different repo than this `TaskManager`.
    pub async fn mark_task_done_by_store_id(
        &self,
        store_id: i64,
        repo: &str,
        ext_id: &ExternalId,
    ) -> anyhow::Result<()> {
        self.update_task_status_by_store_id(store_id, repo, ext_id, Status::Done)
            .await
    }

    /// Update a task's status directly by its already-resolved numeric store id and
    /// owning repo, bypassing the repo-scoped `resolve_task_id` lookup that `self.repo`
    /// would otherwise apply. For callers that already hold the correct store row from a
    /// cross-repo query — e.g. a global sweep over tasks from inactive/removed repos —
    /// and would otherwise fail to resolve a task belonging to a different repo than this
    /// `TaskManager`. Does not mirror to the backend (GitHub), since the caller's backend
    /// instance may not correspond to the task's owning repo.
    pub async fn update_task_status_by_store_id(
        &self,
        store_id: i64,
        repo: &str,
        ext_id: &ExternalId,
        status: Status,
    ) -> anyhow::Result<()> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("store required for status update"))?;
        let task_status = status_to_task_status(status);
        self.release_dependents_if_done(task_status, repo, &ext_id.0)
            .await;

        let snapshot = Self::snapshot_for_store_id(store, store_id, &ext_id.0).await;
        if task_status != TaskStatus::Blocked {
            store.set_block_reason(store_id, None).await?;
        }
        store.update_status(store_id, task_status).await?;
        self.publish_event(ext_id, status, &snapshot, repo, None);
        Ok(())
    }

    /// Like [`Self::update_task_status_by_store_id`] but also sets additional result
    /// fields atomically with the status transition (e.g. `block_reason`, `last_error`).
    pub async fn update_task_status_and_result_by_store_id(
        &self,
        store_id: i64,
        repo: &str,
        ext_id: &ExternalId,
        status: Status,
        updates: &[(&str, serde_json::Value)],
    ) -> anyhow::Result<()> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("store required for status update"))?;
        let task_status = status_to_task_status(status);
        self.release_dependents_if_done(task_status, repo, &ext_id.0)
            .await;

        let snapshot = Self::snapshot_for_store_id(store, store_id, &ext_id.0).await;
        store
            .update_status_and_fields(store_id, task_status, updates)
            .await?;
        self.publish_event(ext_id, status, &snapshot, repo, None);
        Ok(())
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
mod tests;
