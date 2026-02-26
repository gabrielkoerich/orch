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

/// Task metrics record — stores execution metrics for each task run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskMetric {
    pub id: i64,
    pub task_id: String,
    pub agent: String,
    pub model: Option<String>,
    pub complexity: Option<String>,
    pub outcome: String, // "success", "failed", "timeout", "rate_limit", "auth_error"
    pub duration_seconds: f64, // task execution duration in seconds
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub attempts: i32,
    pub files_changed: i32,
    pub error_type: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Parameters for inserting a new task metric record.
#[derive(Debug, Clone)]
pub struct InsertTaskMetric<'a> {
    pub task_id: &'a str,
    pub agent: &'a str,
    pub model: Option<&'a str>,
    pub complexity: Option<&'a str>,
    pub outcome: &'a str,
    pub duration_seconds: f64,
    pub started_at: &'a DateTime<Utc>,
    pub completed_at: &'a DateTime<Utc>,
    pub attempts: i32,
    pub files_changed: i32,
    pub error_type: Option<&'a str>,
}

/// Rate limit event record — tracks rate limit occurrences.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct RateLimitEvent {
    pub id: i64,
    pub agent: String,
    pub limit_type: String, // "rate", "tokens", "budget"
    pub occurred_at: DateTime<Utc>,
    pub task_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

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

/// Default database path: `~/.orch/orchestrator.db`
pub fn default_path() -> anyhow::Result<PathBuf> {
    crate::home::db_path()
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

        if version < 2 {
            conn.execute_batch(SCHEMA_V2)?;
            conn.pragma_update(None, "user_version", 2)?;
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

    /// Insert a new task metric record.
    pub async fn insert_task_metric(&self, metric: InsertTaskMetric<'_>) -> anyhow::Result<i64> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO task_metrics (task_id, agent, model, complexity, outcome, duration_seconds, started_at, completed_at, attempts, files_changed, error_type)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![
                metric.task_id,
                metric.agent,
                metric.model,
                metric.complexity,
                metric.outcome,
                metric.duration_seconds,
                metric.started_at.to_rfc3339(),
                metric.completed_at.to_rfc3339(),
                metric.attempts,
                metric.files_changed,
                metric.error_type,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Get task metrics for a specific task.
    pub async fn get_task_metrics(&self, task_id: &str) -> anyhow::Result<Vec<TaskMetric>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, task_id, agent, model, complexity, outcome, duration_seconds, started_at, completed_at, attempts, files_changed, error_type, created_at
             FROM task_metrics WHERE task_id = ?1 ORDER BY created_at DESC",
        )?;
        let metrics = stmt.query_map([task_id], |row| {
            let started_str: String = row.get(7)?;
            let completed_str: String = row.get(8)?;
            let created_str: String = row.get(12)?;
            Ok(TaskMetric {
                id: row.get(0)?,
                task_id: row.get(1)?,
                agent: row.get(2)?,
                model: row.get(3)?,
                complexity: row.get(4)?,
                outcome: row.get(5)?,
                duration_seconds: row.get(6)?,
                started_at: DateTime::parse_from_rfc3339(&started_str)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
                completed_at: DateTime::parse_from_rfc3339(&completed_str)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
                attempts: row.get(9)?,
                files_changed: row.get(10)?,
                error_type: row.get(11)?,
                created_at: DateTime::parse_from_rfc3339(&created_str)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
            })
        })?;
        let result: Vec<TaskMetric> = metrics.filter_map(|m| m.ok()).collect();
        Ok(result)
    }

    /// Get aggregated metrics for the last 24 hours.
    pub async fn get_metrics_summary_24h(&self) -> anyhow::Result<MetricsSummary> {
        let conn = self.conn.lock().await;

        // Tasks completed/failed in last 24h
        let completed_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM task_metrics WHERE completed_at >= datetime('now', '-24 hours') AND outcome = 'success'",
            [],
            |row| row.get(0),
        )?;

        let failed_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM task_metrics WHERE completed_at >= datetime('now', '-24 hours') AND outcome != 'success'",
            [],
            |row| row.get(0),
        )?;

        // Average duration by complexity
        let avg_duration_simple: Option<f64> = conn.query_row(
            "SELECT AVG(duration_seconds) FROM task_metrics WHERE completed_at >= datetime('now', '-24 hours') AND complexity = 'simple'",
            [],
            |row| row.get(0),
        ).ok().flatten();

        let avg_duration_medium: Option<f64> = conn.query_row(
            "SELECT AVG(duration_seconds) FROM task_metrics WHERE completed_at >= datetime('now', '-24 hours') AND complexity = 'medium'",
            [],
            |row| row.get(0),
        ).ok().flatten();

        let avg_duration_complex: Option<f64> = conn.query_row(
            "SELECT AVG(duration_seconds) FROM task_metrics WHERE completed_at >= datetime('now', '-24 hours') AND complexity = 'complex'",
            [],
            |row| row.get(0),
        ).ok().flatten();

        // Agent success rates
        let mut stmt = conn.prepare(
            "SELECT agent,
                    COUNT(*) as total,
                    SUM(CASE WHEN outcome = 'success' THEN 1 ELSE 0 END) as success_count
             FROM task_metrics
             WHERE completed_at >= datetime('now', '-24 hours')
             GROUP BY agent",
        )?;
        let agent_stats: Vec<AgentStat> = stmt
            .query_map([], |row| {
                let total: i64 = row.get(1)?;
                let success: i64 = row.get(2)?;
                Ok(AgentStat {
                    agent: row.get(0)?,
                    total_runs: total,
                    success_count: success,
                    success_rate: if total > 0 {
                        (success as f64 / total as f64) * 100.0
                    } else {
                        0.0
                    },
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        // Rate limit events in last 24h
        let rate_limit_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM rate_limits WHERE occurred_at >= datetime('now', '-24 hours')",
            [],
            |row| row.get(0),
        )?;

        Ok(MetricsSummary {
            tasks_completed_24h: completed_count,
            tasks_failed_24h: failed_count,
            avg_duration_simple,
            avg_duration_medium,
            avg_duration_complex,
            agent_stats,
            rate_limits_24h: rate_limit_count,
        })
    }

    /// Record a rate limit event.
    pub async fn record_rate_limit(
        &self,
        agent: &str,
        limit_type: &str,
        task_id: Option<&str>,
    ) -> anyhow::Result<i64> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO rate_limits (agent, limit_type, occurred_at, task_id) VALUES (?1, ?2, datetime('now'), ?3)",
            rusqlite::params![agent, limit_type, task_id],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Get failed tasks with error details from the last 7 days.
    pub async fn get_failed_tasks_7d(&self) -> anyhow::Result<Vec<FailedTaskInfo>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT task_id, agent, error_type, COUNT(*) as failure_count
             FROM task_metrics
             WHERE completed_at >= datetime('now', '-7 days')
               AND outcome != 'success'
             GROUP BY task_id, agent, error_type
             ORDER BY failure_count DESC
             LIMIT 20",
        )?;
        let failures = stmt
            .query_map([], |row| {
                Ok(FailedTaskInfo {
                    task_id: row.get(0)?,
                    agent: row.get(1)?,
                    error_type: row.get(2)?,
                    failure_count: row.get(3)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(failures)
    }

    /// Get slow tasks (top 10 longest running) from the last 7 days.
    pub async fn get_slow_tasks_7d(&self) -> anyhow::Result<Vec<SlowTaskInfo>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT task_id, agent, complexity, duration_seconds
             FROM task_metrics
             WHERE completed_at >= datetime('now', '-7 days')
               AND outcome = 'success'
             ORDER BY duration_seconds DESC
             LIMIT 10",
        )?;
        let slow_tasks = stmt
            .query_map([], |row| {
                Ok(SlowTaskInfo {
                    task_id: row.get(0)?,
                    agent: row.get(1)?,
                    complexity: row.get(2)?,
                    duration_seconds: row.get(3)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(slow_tasks)
    }

    /// Get error type distribution from the last 7 days.
    pub async fn get_error_distribution_7d(&self) -> anyhow::Result<Vec<ErrorStat>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT error_type, COUNT(*) as count
             FROM task_metrics
             WHERE completed_at >= datetime('now', '-7 days')
               AND outcome != 'success'
               AND error_type IS NOT NULL
             GROUP BY error_type
             ORDER BY count DESC",
        )?;
        let errors = stmt
            .query_map([], |row| {
                Ok(ErrorStat {
                    error_type: row.get(0)?,
                    count: row.get(1)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(errors)
    }

    /// Get agent performance by complexity level from the last 7 days.
    pub async fn get_agent_complexity_performance_7d(
        &self,
    ) -> anyhow::Result<Vec<AgentComplexityStat>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT agent, complexity,
                    COUNT(*) as total,
                    SUM(CASE WHEN outcome = 'success' THEN 1 ELSE 0 END) as success_count,
                    AVG(duration_seconds) as avg_duration
             FROM task_metrics
             WHERE completed_at >= datetime('now', '-7 days')
               AND complexity IS NOT NULL
             GROUP BY agent, complexity
             ORDER BY agent, complexity",
        )?;
        let stats = stmt
            .query_map([], |row| {
                let total: i64 = row.get(2)?;
                let success: i64 = row.get(3)?;
                Ok(AgentComplexityStat {
                    agent: row.get(0)?,
                    complexity: row.get(1)?,
                    total_runs: total,
                    success_count: success,
                    success_rate: if total > 0 {
                        (success as f64 / total as f64) * 100.0
                    } else {
                        0.0
                    },
                    avg_duration_seconds: row.get(4)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(stats)
    }

    /// Get count of self-improvement issues created in the last 7 days.
    pub async fn count_self_improvement_issues_7d(&self) -> anyhow::Result<i64> {
        // This is tracked via kv store - we store a weekly counter
        let count = self.kv_get("self_improvement_issues_7d").await?;
        Ok(count.and_then(|c| c.parse().ok()).unwrap_or(0))
    }

    /// Increment the self-improvement issue counter for the current week.
    pub async fn increment_self_improvement_counter(&self) -> anyhow::Result<()> {
        let current = self.kv_get("self_improvement_issues_7d").await?;
        let new_count = current.and_then(|c| c.parse::<i64>().ok()).unwrap_or(0) + 1;
        self.kv_set("self_improvement_issues_7d", &new_count.to_string())
            .await?;
        Ok(())
    }

    /// Reset weekly self-improvement counter (call this at week start).
    pub async fn reset_self_improvement_counter(&self) -> anyhow::Result<()> {
        self.kv_set("self_improvement_issues_7d", "0").await
    }
}

/// Metrics summary for the CLI output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSummary {
    pub tasks_completed_24h: i64,
    pub tasks_failed_24h: i64,
    pub avg_duration_simple: Option<f64>,
    pub avg_duration_medium: Option<f64>,
    pub avg_duration_complex: Option<f64>,
    pub agent_stats: Vec<AgentStat>,
    pub rate_limits_24h: i64,
}

/// Agent statistics from metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStat {
    pub agent: String,
    pub total_runs: i64,
    pub success_count: i64,
    pub success_rate: f64,
}

/// Failed task info for pattern detection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailedTaskInfo {
    pub task_id: String,
    pub agent: String,
    pub error_type: Option<String>,
    pub failure_count: i64,
}

/// Slow task info for pattern detection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlowTaskInfo {
    pub task_id: String,
    pub agent: String,
    pub complexity: Option<String>,
    pub duration_seconds: f64,
}

/// Error type distribution for pattern detection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorStat {
    pub error_type: Option<String>,
    pub count: i64,
}

/// Agent complexity performance for pattern detection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentComplexityStat {
    pub agent: String,
    pub complexity: Option<String>,
    pub total_runs: i64,
    pub success_count: i64,
    pub success_rate: f64,
    pub avg_duration_seconds: Option<f64>,
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

/// Schema v2 — adds task_metrics and rate_limits tables for observability.
const SCHEMA_V2: &str = r#"
CREATE TABLE IF NOT EXISTS task_metrics (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id         TEXT NOT NULL,
    agent           TEXT NOT NULL,
    model           TEXT DEFAULT NULL,
    complexity      TEXT DEFAULT NULL,
    outcome         TEXT NOT NULL,         -- "success", "failed", "timeout", "rate_limit", "auth_error"
    duration_seconds REAL DEFAULT 0,
    started_at      TEXT NOT NULL,
    completed_at    TEXT NOT NULL,
    attempts        INTEGER DEFAULT 1,
    files_changed   INTEGER DEFAULT 0,
    error_type      TEXT DEFAULT NULL,
    created_at      TEXT DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE TABLE IF NOT EXISTS rate_limits (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    agent           TEXT NOT NULL,
    limit_type      TEXT NOT NULL,  -- "rate", "tokens", "budget"
    occurred_at     TEXT NOT NULL,
    task_id         TEXT DEFAULT NULL,
    created_at      TEXT DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_task_metrics_task_id ON task_metrics(task_id);
CREATE INDEX IF NOT EXISTS idx_task_metrics_agent ON task_metrics(agent);
CREATE INDEX IF NOT EXISTS idx_task_metrics_completed_at ON task_metrics(completed_at);
CREATE INDEX IF NOT EXISTS idx_task_metrics_outcome ON task_metrics(outcome);
CREATE INDEX IF NOT EXISTS idx_rate_limits_agent ON rate_limits(agent);
CREATE INDEX IF NOT EXISTS idx_rate_limits_occurred_at ON rate_limits(occurred_at);
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
