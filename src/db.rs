//! SQLite database — internal task store.
//!
//! Internal tasks (cron jobs, mention handlers, maintenance) live here.
//! External tasks live in GitHub Issues (via the `backends` module).
//! No bidirectional sync — each storage is authoritative for its domain.

use anyhow::Context;
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    New,
    Routed,
    InProgress,
    Done,
    Blocked,
    NeedsReview,
}

impl TaskStatus {
    #[allow(dead_code)]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Routed => "routed",
            Self::InProgress => "in_progress",
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::NeedsReview => "needs_review",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "new" => Some(Self::New),
            "routed" => Some(Self::Routed),
            "in_progress" => Some(Self::InProgress),
            "done" => Some(Self::Done),
            "blocked" => Some(Self::Blocked),
            "needs_review" => Some(Self::NeedsReview),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InternalTask {
    pub id: i64,
    pub title: String,
    pub body: String,
    pub status: TaskStatus,
    pub source: String,
    pub source_id: String,
    pub agent: Option<String>,
    pub block_reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Default database path: `~/.orchestrator/orchestrator.db`
pub fn default_path() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir().context("cannot determine home directory")?;
    let dir = home.join(".orchestrator");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("orchestrator.db"))
}

/// Database handle with async-safe locking.
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

#[allow(dead_code)]
impl Db {
    /// Open (or create) the database at the given path.
    pub fn open(path: &PathBuf) -> anyhow::Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening database: {}", path.display()))?;

        // WAL mode for concurrent reads
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Open an in-memory database (for testing).
    #[allow(dead_code)]
    pub fn open_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        // WAL is a no-op for :memory: — only set busy_timeout
        conn.execute_batch("PRAGMA busy_timeout=5000;")?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Run schema migrations.
    ///
    /// Uses `PRAGMA user_version` to track schema version and skip
    /// already-applied migrations on existing databases.
    pub async fn migrate(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock().await;
        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

        if version < 1 {
            conn.execute_batch(SCHEMA_V1)?;
            conn.pragma_update(None, "user_version", 1)?;
        }

        Ok(())
    }

    /// Get a reference to the connection (for running queries).
    #[allow(dead_code)]
    pub async fn conn(&self) -> tokio::sync::MutexGuard<'_, Connection> {
        self.conn.lock().await
    }

    pub async fn create_internal_task(
        &self,
        title: &str,
        body: &str,
        source: &str,
        source_id: &str,
    ) -> anyhow::Result<i64> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO internal_tasks (title, body, source, source_id) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![title, body, source, source_id],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub async fn get_internal_task(&self, id: i64) -> anyhow::Result<InternalTask> {
        let conn = self.conn.lock().await;
        let task = conn.query_row(
            "SELECT id, title, body, status, source, source_id, agent, block_reason, created_at, updated_at
             FROM internal_tasks WHERE id = ?1",
            [id],
            |row| {
                let status_str: String = row.get(3)?;
                let created_str: String = row.get(8)?;
                let updated_str: String = row.get(9)?;
                Ok(InternalTask {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    body: row.get(2)?,
                    status: TaskStatus::from_str(&status_str).unwrap_or(TaskStatus::New),
                    source: row.get(4)?,
                    source_id: row.get(5)?,
                    agent: row.get(6)?,
                    block_reason: row.get(7)?,
                    created_at: DateTime::parse_from_rfc3339(&created_str)
                        .map(|dt| dt.with_timezone(&Utc))
                        .unwrap_or_else(|_| Utc::now()),
                    updated_at: DateTime::parse_from_rfc3339(&updated_str)
                        .map(|dt| dt.with_timezone(&Utc))
                        .unwrap_or_else(|_| Utc::now()),
                })
            },
        )?;
        Ok(task)
    }

    #[allow(dead_code)]
    pub async fn list_internal_tasks_by_status(
        &self,
        status: TaskStatus,
    ) -> anyhow::Result<Vec<InternalTask>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, title, body, status, source, source_id, agent, block_reason, created_at, updated_at
             FROM internal_tasks WHERE status = ?1 ORDER BY created_at DESC",
        )?;
        let tasks = stmt.query_map([status.as_str()], |row| {
            let status_str: String = row.get(3)?;
            let created_str: String = row.get(8)?;
            let updated_str: String = row.get(9)?;
            Ok(InternalTask {
                id: row.get(0)?,
                title: row.get(1)?,
                body: row.get(2)?,
                status: TaskStatus::from_str(&status_str).unwrap_or(TaskStatus::New),
                source: row.get(4)?,
                source_id: row.get(5)?,
                agent: row.get(6)?,
                block_reason: row.get(7)?,
                created_at: DateTime::parse_from_rfc3339(&created_str)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
                updated_at: DateTime::parse_from_rfc3339(&updated_str)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
            })
        })?;
        let result: Vec<InternalTask> = tasks.filter_map(|t| t.ok()).collect();
        Ok(result)
    }

    #[allow(dead_code)]
    pub async fn update_internal_task_status(
        &self,
        id: i64,
        status: TaskStatus,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE internal_tasks SET status = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ?2",
            rusqlite::params![status.as_str(), id],
        )?;
        Ok(())
    }

    #[allow(dead_code)]
    pub async fn delete_internal_task(&self, id: i64) -> anyhow::Result<()> {
        let conn = self.conn.lock().await;
        conn.execute("DELETE FROM internal_tasks WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Get a value from the kv store.
    pub async fn kv_get(&self, key: &str) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock().await;
        let result = conn.query_row("SELECT value FROM kv WHERE key = ?1", [key], |row| {
            row.get(0)
        });
        match result {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Set a value in the kv store (upsert).
    pub async fn kv_set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO kv (key, value, updated_at) VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
             ON CONFLICT(key) DO UPDATE SET value = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    pub async fn set_internal_task_agent(
        &self,
        id: i64,
        agent: Option<&str>,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE internal_tasks SET agent = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ?2",
            rusqlite::params![agent, id],
        )?;
        Ok(())
    }

    pub async fn set_internal_task_block_reason(
        &self,
        id: i64,
        reason: Option<&str>,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE internal_tasks SET block_reason = ?1, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ?2",
            rusqlite::params![reason, id],
        )?;
        Ok(())
    }

    /// List all internal tasks regardless of status.
    pub async fn list_all_internal_tasks(&self) -> anyhow::Result<Vec<InternalTask>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, title, body, status, source, source_id, agent, block_reason, created_at, updated_at
             FROM internal_tasks ORDER BY created_at DESC",
        )?;
        let tasks = stmt.query_map([], |row| {
            let status_str: String = row.get(3)?;
            let created_str: String = row.get(8)?;
            let updated_str: String = row.get(9)?;
            Ok(InternalTask {
                id: row.get(0)?,
                title: row.get(1)?,
                body: row.get(2)?,
                status: TaskStatus::from_str(&status_str).unwrap_or(TaskStatus::New),
                source: row.get(4)?,
                source_id: row.get(5)?,
                agent: row.get(6)?,
                block_reason: row.get(7)?,
                created_at: DateTime::parse_from_rfc3339(&created_str)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
                updated_at: DateTime::parse_from_rfc3339(&updated_str)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
            })
        })?;
        let result: Vec<InternalTask> = tasks.filter_map(|t| t.ok()).collect();
        Ok(result)
    }
}

/// Schema v1 — initial tables for internal tasks and jobs.
const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS internal_tasks (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    title         TEXT NOT NULL,
    body          TEXT DEFAULT '',
    status        TEXT DEFAULT 'new',
    source        TEXT NOT NULL,  -- 'cron', 'mention', 'manual'
    source_id     TEXT DEFAULT '', -- job ID, mention thread, etc.
    agent         TEXT DEFAULT NULL,
    block_reason  TEXT DEFAULT NULL,
    created_at    TEXT DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    updated_at    TEXT DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE TABLE IF NOT EXISTS internal_task_results (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id     INTEGER NOT NULL,
    summary     TEXT DEFAULT '',
    status      TEXT DEFAULT '',
    reason      TEXT DEFAULT '',
    files_changed TEXT DEFAULT '[]',
    created_at  TEXT DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    FOREIGN KEY (task_id) REFERENCES internal_tasks(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS jobs (
    id          TEXT PRIMARY KEY,
    schedule    TEXT NOT NULL,
    job_type    TEXT NOT NULL DEFAULT 'task',
    command     TEXT DEFAULT '',
    task_title  TEXT DEFAULT '',
    task_body   TEXT DEFAULT '',
    enabled     INTEGER DEFAULT 1,
    last_run    TEXT DEFAULT '',
    last_status TEXT DEFAULT '',
    created_at  TEXT DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE TABLE IF NOT EXISTS kv (
    key         TEXT PRIMARY KEY,
    value       TEXT NOT NULL,
    updated_at  TEXT DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_internal_tasks_status ON internal_tasks(status);
CREATE INDEX IF NOT EXISTS idx_internal_tasks_source ON internal_tasks(source);
CREATE INDEX IF NOT EXISTS idx_internal_task_results_task_id ON internal_task_results(task_id);
CREATE INDEX IF NOT EXISTS idx_jobs_enabled ON jobs(enabled);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_memory_db() {
        let db = Db::open_memory().unwrap();
        db.migrate().await.unwrap();
    }

    #[tokio::test]
    async fn migrate_creates_tables() {
        let db = Db::open_memory().unwrap();
        db.migrate().await.unwrap();

        let conn = db.conn().await;
        // internal_tasks table should exist
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM internal_tasks", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);

        // jobs table should exist
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM jobs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn insert_and_query_internal_task() {
        let db = Db::open_memory().unwrap();
        db.migrate().await.unwrap();

        let conn = db.conn().await;
        conn.execute(
            "INSERT INTO internal_tasks (title, body, source) VALUES (?1, ?2, ?3)",
            ["Test task", "Test body", "manual"],
        )
        .unwrap();

        let title: String = conn
            .query_row("SELECT title FROM internal_tasks WHERE id = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(title, "Test task");
    }

    #[tokio::test]
    async fn insert_and_query_job() {
        let db = Db::open_memory().unwrap();
        db.migrate().await.unwrap();

        let conn = db.conn().await;
        conn.execute(
            "INSERT INTO jobs (id, schedule, job_type) VALUES (?1, ?2, ?3)",
            ["morning-review", "0 8 * * *", "task"],
        )
        .unwrap();

        let schedule: String = conn
            .query_row(
                "SELECT schedule FROM jobs WHERE id = 'morning-review'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(schedule, "0 8 * * *");
    }

    #[tokio::test]
    async fn migrate_is_idempotent() {
        let db = Db::open_memory().unwrap();
        db.migrate().await.unwrap();
        db.migrate().await.unwrap(); // should not error
    }

    #[tokio::test]
    async fn create_internal_task_crud() {
        let db = Db::open_memory().unwrap();
        db.migrate().await.unwrap();

        let id = db
            .create_internal_task("Test task", "Test body", "cron", "daily-sync")
            .await
            .unwrap();
        assert_eq!(id, 1);

        let task = db.get_internal_task(id).await.unwrap();
        assert_eq!(task.title, "Test task");
        assert_eq!(task.body, "Test body");
        assert_eq!(task.source, "cron");
        assert_eq!(task.source_id, "daily-sync");
        assert_eq!(task.status, TaskStatus::New);

        db.update_internal_task_status(id, TaskStatus::Done)
            .await
            .unwrap();
        let task = db.get_internal_task(id).await.unwrap();
        assert_eq!(task.status, TaskStatus::Done);

        db.delete_internal_task(id).await.unwrap();
        let result = db.get_internal_task(id).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn list_internal_tasks_by_status() {
        let db = Db::open_memory().unwrap();
        db.migrate().await.unwrap();

        db.create_internal_task("Task 1", "", "cron", "job1")
            .await
            .unwrap();
        db.create_internal_task("Task 2", "", "cron", "job2")
            .await
            .unwrap();
        db.create_internal_task("Task 3", "", "manual", "manual1")
            .await
            .unwrap();

        let new_tasks = db
            .list_internal_tasks_by_status(TaskStatus::New)
            .await
            .unwrap();
        assert_eq!(new_tasks.len(), 3);

        db.update_internal_task_status(1, TaskStatus::Done)
            .await
            .unwrap();
        let done_tasks = db
            .list_internal_tasks_by_status(TaskStatus::Done)
            .await
            .unwrap();
        assert_eq!(done_tasks.len(), 1);
        assert_eq!(done_tasks[0].title, "Task 1");
    }

    #[tokio::test]
    async fn set_internal_task_agent() {
        let db = Db::open_memory().unwrap();
        db.migrate().await.unwrap();

        let id = db
            .create_internal_task("Test", "", "manual", "")
            .await
            .unwrap();

        db.set_internal_task_agent(id, Some("claude"))
            .await
            .unwrap();
        let task = db.get_internal_task(id).await.unwrap();
        assert_eq!(task.agent, Some("claude".to_string()));

        db.set_internal_task_agent(id, None).await.unwrap();
        let task = db.get_internal_task(id).await.unwrap();
        assert_eq!(task.agent, None);
    }

    #[tokio::test]
    async fn kv_get_set() {
        let db = Db::open_memory().unwrap();
        db.migrate().await.unwrap();

        // Missing key returns None
        assert_eq!(db.kv_get("foo").await.unwrap(), None);

        // Set and get
        db.kv_set("foo", "bar").await.unwrap();
        assert_eq!(db.kv_get("foo").await.unwrap(), Some("bar".to_string()));

        // Upsert overwrites
        db.kv_set("foo", "baz").await.unwrap();
        assert_eq!(db.kv_get("foo").await.unwrap(), Some("baz".to_string()));
    }

    #[tokio::test]
    async fn kv_multiple_keys() {
        let db = Db::open_memory().unwrap();
        db.migrate().await.unwrap();

        db.kv_set("a", "1").await.unwrap();
        db.kv_set("b", "2").await.unwrap();

        assert_eq!(db.kv_get("a").await.unwrap(), Some("1".to_string()));
        assert_eq!(db.kv_get("b").await.unwrap(), Some("2".to_string()));
    }

    #[test]
    fn task_status_as_str() {
        assert_eq!(TaskStatus::New.as_str(), "new");
        assert_eq!(TaskStatus::Routed.as_str(), "routed");
        assert_eq!(TaskStatus::InProgress.as_str(), "in_progress");
        assert_eq!(TaskStatus::Done.as_str(), "done");
        assert_eq!(TaskStatus::Blocked.as_str(), "blocked");
        assert_eq!(TaskStatus::NeedsReview.as_str(), "needs_review");
    }

    #[test]
    fn task_status_from_str() {
        assert_eq!(TaskStatus::from_str("new"), Some(TaskStatus::New));
        assert_eq!(TaskStatus::from_str("routed"), Some(TaskStatus::Routed));
        assert_eq!(
            TaskStatus::from_str("in_progress"),
            Some(TaskStatus::InProgress)
        );
        assert_eq!(TaskStatus::from_str("done"), Some(TaskStatus::Done));
        assert_eq!(TaskStatus::from_str("blocked"), Some(TaskStatus::Blocked));
        assert_eq!(
            TaskStatus::from_str("needs_review"),
            Some(TaskStatus::NeedsReview)
        );
        assert_eq!(TaskStatus::from_str("invalid"), None);
    }
}
