//! SQLite database — internal task store.
//!
//! Internal tasks (cron jobs, mention handlers, maintenance) live here.
//! External tasks live in GitHub Issues (via the `backends` module).
//! No bidirectional sync — each storage is authoritative for its domain.

use anyhow::Context;
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

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
    pub async fn conn(&self) -> tokio::sync::MutexGuard<'_, Connection> {
        self.conn.lock().await
    }
}

/// Schema v1 — initial tables for internal tasks and jobs.
const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS internal_tasks (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    title       TEXT NOT NULL,
    body        TEXT DEFAULT '',
    status      TEXT DEFAULT 'new',
    source      TEXT NOT NULL,  -- 'cron', 'mention', 'manual'
    source_id   TEXT DEFAULT '', -- job ID, mention thread, etc.
    created_at  TEXT DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    updated_at  TEXT DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
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

CREATE INDEX IF NOT EXISTS idx_internal_tasks_status ON internal_tasks(status);
CREATE INDEX IF NOT EXISTS idx_internal_tasks_source ON internal_tasks(source);
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
}
