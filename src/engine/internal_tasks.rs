//! Internal task CRUD operations — SQLite-backed tasks.
//!
//! Internal tasks are created by cron jobs, mention handlers, and maintenance
//! routines. They don't create GitHub Issues and store results in SQLite.
//!
//! Operations:
//! - create_internal_task
//! - list_internal_tasks_by_status
//! - update_internal_task_status
//! - delete_internal_task

use crate::backends::Status;
use crate::db::Db;
use anyhow::Context;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

/// An internal task stored in SQLite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InternalTask {
    pub id: i64,
    pub title: String,
    pub body: String,
    pub status: String,
    pub source: String,
    pub source_id: String,
    pub created_at: String,
    pub updated_at: String,
}

/// Result storage for completed internal tasks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InternalTaskResult {
    pub id: i64,
    pub task_id: i64,
    pub summary: String,
    pub status: String,
    pub reason: String,
    pub files_changed: String, // JSON array as string
    pub created_at: String,
}

/// Create a new internal task.
///
/// Returns the auto-generated row ID.
pub async fn create_internal_task(
    db: &Db,
    title: &str,
    body: &str,
    source: &str,
) -> anyhow::Result<i64> {
    let conn = db.conn().await;
    conn.execute(
        "INSERT INTO internal_tasks (title, body, source, status) VALUES (?1, ?2, ?3, ?4)",
        [title, body, source, "new"],
    )
    .context("inserting internal task")?;

    let id = conn.last_insert_rowid();
    tracing::info!(task_id = id, source, title, "created internal task");
    Ok(id)
}

/// Create a new internal task with a source_id (e.g., job ID).
pub async fn create_internal_task_with_source(
    db: &Db,
    title: &str,
    body: &str,
    source: &str,
    source_id: &str,
) -> anyhow::Result<i64> {
    let conn = db.conn().await;
    conn.execute(
        "INSERT INTO internal_tasks (title, body, source, source_id, status) VALUES (?1, ?2, ?3, ?4, ?5)",
        [title, body, source, source_id, "new"],
    )
    .context("inserting internal task with source_id")?;

    let id = conn.last_insert_rowid();
    tracing::info!(
        task_id = id,
        source,
        source_id,
        title,
        "created internal task"
    );
    Ok(id)
}

/// List internal tasks by status.
///
/// Status values: "new", "in_progress", "done", "blocked", "needs_review"
pub async fn list_internal_tasks_by_status(
    db: &Db,
    status: &str,
) -> anyhow::Result<Vec<InternalTask>> {
    let conn = db.conn().await;
    let mut stmt = conn
        .prepare(
            "SELECT id, title, body, status, source, source_id, created_at, updated_at 
             FROM internal_tasks 
             WHERE status = ?1 
             ORDER BY created_at ASC",
        )
        .context("preparing list query")?;

    let rows = stmt
        .query_map([status], |row| {
            Ok(InternalTask {
                id: row.get(0)?,
                title: row.get(1)?,
                body: row.get(2)?,
                status: row.get(3)?,
                source: row.get(4)?,
                source_id: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
            })
        })
        .context("querying internal tasks")?;

    let mut tasks = Vec::new();
    for row in rows {
        tasks.push(row?);
    }

    Ok(tasks)
}

/// Get a single internal task by ID.
pub async fn get_internal_task(db: &Db, id: i64) -> anyhow::Result<Option<InternalTask>> {
    let conn = db.conn().await;
    let mut stmt = conn
        .prepare(
            "SELECT id, title, body, status, source, source_id, created_at, updated_at 
             FROM internal_tasks 
             WHERE id = ?1",
        )
        .context("preparing get query")?;

    let result = stmt
        .query_row([id], |row| {
            Ok(InternalTask {
                id: row.get(0)?,
                title: row.get(1)?,
                body: row.get(2)?,
                status: row.get(3)?,
                source: row.get(4)?,
                source_id: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
            })
        })
        .optional()
        .context("querying internal task")?;

    Ok(result)
}

/// Update internal task status.
///
/// Also updates the `updated_at` timestamp automatically.
pub async fn update_internal_task_status(db: &Db, id: i64, status: &str) -> anyhow::Result<()> {
    let conn = db.conn().await;
    let affected = conn
        .execute(
            "UPDATE internal_tasks 
             SET status = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') 
             WHERE id = ?2",
            [status, &id.to_string()],
        )
        .context("updating internal task status")?;

    if affected == 0 {
        anyhow::bail!("internal task {} not found", id);
    }

    tracing::info!(task_id = id, status, "updated internal task status");
    Ok(())
}

/// Delete an internal task.
pub async fn delete_internal_task(db: &Db, id: i64) -> anyhow::Result<()> {
    let conn = db.conn().await;
    let affected = conn
        .execute("DELETE FROM internal_tasks WHERE id = ?1", [id])
        .context("deleting internal task")?;

    if affected == 0 {
        anyhow::bail!("internal task {} not found", id);
    }

    tracing::info!(task_id = id, "deleted internal task");
    Ok(())
}

/// List all internal tasks (for dashboard/inspection).
pub async fn list_all_internal_tasks(db: &Db, limit: usize) -> anyhow::Result<Vec<InternalTask>> {
    let conn = db.conn().await;
    let mut stmt = conn
        .prepare(
            "SELECT id, title, body, status, source, source_id, created_at, updated_at 
             FROM internal_tasks 
             ORDER BY updated_at DESC 
             LIMIT ?1",
        )
        .context("preparing list all query")?;

    let rows = stmt
        .query_map([limit as i64], |row| {
            Ok(InternalTask {
                id: row.get(0)?,
                title: row.get(1)?,
                body: row.get(2)?,
                status: row.get(3)?,
                source: row.get(4)?,
                source_id: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
            })
        })
        .context("querying all internal tasks")?;

    let mut tasks = Vec::new();
    for row in rows {
        tasks.push(row?);
    }

    Ok(tasks)
}

/// Store result for a completed internal task.
pub async fn store_internal_task_result(
    db: &Db,
    task_id: i64,
    summary: &str,
    status: &str,
    reason: &str,
    files_changed: &[String],
) -> anyhow::Result<()> {
    let conn = db.conn().await;
    let files_json = serde_json::to_string(files_changed).unwrap_or_default();

    conn.execute(
        "INSERT INTO internal_task_results (task_id, summary, status, reason, files_changed) 
         VALUES (?1, ?2, ?3, ?4, ?5)",
        [
            task_id.to_string(),
            summary.to_string(),
            status.to_string(),
            reason.to_string(),
            files_json,
        ],
    )
    .context("inserting internal task result")?;

    tracing::info!(task_id, status, "stored internal task result");
    Ok(())
}

/// Get results for an internal task.
pub async fn get_internal_task_results(
    db: &Db,
    task_id: i64,
) -> anyhow::Result<Vec<InternalTaskResult>> {
    let conn = db.conn().await;
    let mut stmt = conn
        .prepare(
            "SELECT id, task_id, summary, status, reason, files_changed, created_at 
             FROM internal_task_results 
             WHERE task_id = ?1 
             ORDER BY created_at DESC",
        )
        .context("preparing get results query")?;

    let rows = stmt
        .query_map([task_id], |row| {
            Ok(InternalTaskResult {
                id: row.get(0)?,
                task_id: row.get(1)?,
                summary: row.get(2)?,
                status: row.get(3)?,
                reason: row.get(4)?,
                files_changed: row.get(5)?,
                created_at: row.get(6)?,
            })
        })
        .context("querying internal task results")?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }

    Ok(results)
}

/// Convert Status enum to internal task status string.
pub fn status_to_string(status: Status) -> &'static str {
    match status {
        Status::New => "new",
        Status::Routed => "routed",
        Status::InProgress => "in_progress",
        Status::Done => "done",
        Status::Blocked => "blocked",
        Status::InReview => "in_review",
        Status::NeedsReview => "needs_review",
    }
}

/// Convert internal task status string to Status enum.
pub fn string_to_status(status: &str) -> Option<Status> {
    match status {
        "new" => Some(Status::New),
        "routed" => Some(Status::Routed),
        "in_progress" => Some(Status::InProgress),
        "done" => Some(Status::Done),
        "blocked" => Some(Status::Blocked),
        "in_review" => Some(Status::InReview),
        "needs_review" => Some(Status::NeedsReview),
        _ => None,
    }
}

/// Count internal tasks by status (for dashboard).
pub async fn count_internal_tasks_by_status(db: &Db) -> anyhow::Result<Vec<(String, i64)>> {
    let conn = db.conn().await;
    let mut stmt = conn
        .prepare(
            "SELECT status, COUNT(*) 
             FROM internal_tasks 
             GROUP BY status 
             ORDER BY status",
        )
        .context("preparing count query")?;

    let rows = stmt
        .query_map([], |row| {
            let status: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            Ok((status, count))
        })
        .context("counting internal tasks")?;

    let mut counts = Vec::new();
    for row in rows {
        counts.push(row?);
    }

    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup_test_db() -> Db {
        let db = Db::open_memory().unwrap();
        db.migrate().await.unwrap();
        db
    }

    #[tokio::test]
    async fn test_create_and_get_internal_task() {
        let db = setup_test_db().await;

        let id = create_internal_task(&db, "Test Task", "Test body", "manual")
            .await
            .unwrap();

        assert_eq!(id, 1);

        let task = get_internal_task(&db, id).await.unwrap().unwrap();
        assert_eq!(task.title, "Test Task");
        assert_eq!(task.body, "Test body");
        assert_eq!(task.source, "manual");
        assert_eq!(task.status, "new");
    }

    #[tokio::test]
    async fn test_create_with_source_id() {
        let db = setup_test_db().await;

        let id =
            create_internal_task_with_source(&db, "Job Task", "Body", "cron", "morning-review")
                .await
                .unwrap();

        let task = get_internal_task(&db, id).await.unwrap().unwrap();
        assert_eq!(task.source_id, "morning-review");
    }

    #[tokio::test]
    async fn test_list_by_status() {
        let db = setup_test_db().await;

        create_internal_task(&db, "Task 1", "Body 1", "manual")
            .await
            .unwrap();
        create_internal_task(&db, "Task 2", "Body 2", "cron")
            .await
            .unwrap();

        let tasks = list_internal_tasks_by_status(&db, "new").await.unwrap();
        assert_eq!(tasks.len(), 2);

        // Update one to done
        update_internal_task_status(&db, 1, "done").await.unwrap();

        let new_tasks = list_internal_tasks_by_status(&db, "new").await.unwrap();
        assert_eq!(new_tasks.len(), 1);

        let done_tasks = list_internal_tasks_by_status(&db, "done").await.unwrap();
        assert_eq!(done_tasks.len(), 1);
    }

    #[tokio::test]
    async fn test_update_status() {
        let db = setup_test_db().await;

        let id = create_internal_task(&db, "Task", "Body", "manual")
            .await
            .unwrap();

        update_internal_task_status(&db, id, "in_progress")
            .await
            .unwrap();

        let task = get_internal_task(&db, id).await.unwrap().unwrap();
        assert_eq!(task.status, "in_progress");
    }

    #[tokio::test]
    async fn test_delete_task() {
        let db = setup_test_db().await;

        let id = create_internal_task(&db, "Task", "Body", "manual")
            .await
            .unwrap();

        delete_internal_task(&db, id).await.unwrap();

        let task = get_internal_task(&db, id).await.unwrap();
        assert!(task.is_none());
    }

    #[tokio::test]
    async fn test_status_conversions() {
        assert_eq!(status_to_string(Status::New), "new");
        assert_eq!(status_to_string(Status::InProgress), "in_progress");
        assert_eq!(status_to_string(Status::Done), "done");

        assert_eq!(string_to_status("new"), Some(Status::New));
        assert_eq!(string_to_status("in_progress"), Some(Status::InProgress));
        assert_eq!(string_to_status("invalid"), None);
    }
}
