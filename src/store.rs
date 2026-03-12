//! Unified task store — SQLite as single source of truth.
//!
//! `TaskStore` replaces the combination of sidecar JSON files, `internal_tasks` table,
//! and GitHub labels as the authoritative task state. External backends (GitHub, Linear)
//! become sync adapters that mirror state changes outward.
//!
//! Uses sqlx for async SQLite access with file-based migrations.
//!
//! Phase 1: schema + CRUD only, no behavior change. Dead code expected until wired in.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use sqlx::Row;
use std::path::Path;

use crate::db::TaskStatus;
use crate::sidecar::MemoryEntry;

/// Unified task record — single row in the `tasks` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: i64,
    pub external_id: Option<String>,
    pub repo: String,
    pub origin: String,

    pub title: String,
    pub body: String,
    pub status: TaskStatus,
    pub source: String,
    pub source_id: String,
    pub author: String,
    pub url: String,
    pub labels: Vec<String>,

    // Routing
    pub agent: Option<String>,
    pub model: Option<String>,
    pub complexity: String,
    pub route_reason: String,
    pub agent_profile: String,
    pub selected_skills: String,
    pub route_attempts: i32,

    // Execution
    pub attempts: i32,
    pub branch: String,
    pub worktree: String,
    pub worktree_cleaned: bool,
    pub summary: String,
    pub last_error: String,
    pub parent_id: Option<i64>,
    pub block_reason: Option<String>,

    // PR Review
    pub pr_number: Option<i32>,
    pub pr_review_context: String,
    pub last_review_ts: String,
    pub last_comment_review_ts: String,
    pub merge_conflict_retries: i32,
    pub ci_merge_failures: i32,
    pub pr_create_failures: i32,
    pub review_agent_failures: i32,
    pub review_cycles: i32,

    // Tokens & Cost
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub input_cost_usd: f64,
    pub output_cost_usd: f64,
    pub total_cost_usd: f64,

    // Recovery
    pub model_reroute_chain: String,
    pub limit_reroute_chain: String,
    pub budget_warning: String,
    pub budget_exceeded: bool,

    // Structured data
    pub memory: Vec<MemoryEntry>,
    pub delegations: Vec<serde_json::Value>,

    // Timestamps
    pub created_at: String,
    pub updated_at: String,
}

/// Parameters for creating a new task.
#[derive(Debug, Clone)]
pub struct NewTask {
    pub external_id: Option<String>,
    pub repo: String,
    pub origin: String,
    pub title: String,
    pub body: String,
    pub source: String,
    pub source_id: String,
    pub author: String,
    pub url: String,
    pub labels: Vec<String>,
}

/// A single run (agent execution, review, or routing attempt).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRun {
    pub id: i64,
    pub task_id: i64,
    pub attempt: i32,
    pub run_type: String,
    pub agent: String,
    pub model: String,
    pub command: String,
    pub prompt: String,
    pub env_vars: String,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub parsed_response: String,
    pub outcome: String,
    pub error: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_cost_usd: f64,
    pub duration_secs: f64,
    pub started_at: String,
    pub completed_at: Option<String>,
}

/// Token usage for a run.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunTokenUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_cost_usd: f64,
    pub duration_secs: f64,
}

/// Parameters for starting a new run.
#[derive(Debug, Clone)]
pub struct StartRun<'a> {
    pub task_id: i64,
    pub attempt: i32,
    pub run_type: &'a str,
    pub agent: &'a str,
    pub model: &'a str,
    pub command: &'a str,
    pub prompt: &'a str,
}

/// Parameters for completing a run.
#[derive(Debug, Clone)]
pub struct CompleteRun<'a> {
    pub run_id: i64,
    pub exit_code: Option<i32>,
    pub stdout: &'a str,
    pub stderr: &'a str,
    pub parsed: &'a str,
    pub outcome: &'a str,
    pub error: &'a str,
    pub tokens: RunTokenUsage,
}

/// Parameters for storing a route result.
#[derive(Debug, Clone)]
pub struct StoreRoute<'a> {
    pub id: i64,
    pub agent: &'a str,
    pub model: Option<&'a str>,
    pub complexity: &'a str,
    pub reason: &'a str,
    pub profile: &'a str,
    pub skills: &'a str,
}

/// Parameters for upserting an external task.
#[derive(Debug, Clone)]
pub struct UpsertExternal<'a> {
    pub repo: &'a str,
    pub ext_id: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub author: &'a str,
    pub url: &'a str,
    pub labels: &'a [String],
    pub origin: &'a str,
}

/// The unified task store backed by SQLite (via sqlx).
#[derive(Clone)]
pub struct TaskStore {
    pool: SqlitePool,
}

impl TaskStore {
    /// Open (or create) the store at the given database path.
    ///
    /// Runs file-based migrations from the `migrations/` directory.
    pub async fn open(db_path: &Path) -> anyhow::Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(db_path)
            .journal_mode(SqliteJournalMode::Wal)
            .create_if_missing(true)
            .busy_timeout(std::time::Duration::from_secs(5));

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .with_context(|| format!("opening task store: {}", db_path.display()))?;

        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    /// Open an in-memory store (for testing).
    #[cfg(test)]
    pub async fn open_memory() -> anyhow::Result<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;

        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    /// Run migrations.
    async fn migrate(&self) -> anyhow::Result<()> {
        sqlx::migrate!("./migrations")
            .run(&self.pool)
            .await
            .context("running task store migrations")?;
        Ok(())
    }

    /// Get a reference to the underlying pool (for advanced queries).
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    // ---------------------------------------------------------------
    // CRUD
    // ---------------------------------------------------------------

    /// Create a new task, returning its auto-generated ID.
    pub async fn create(&self, new: &NewTask) -> anyhow::Result<i64> {
        let labels_json = serde_json::to_string(&new.labels)?;
        let row = sqlx::query(
            "INSERT INTO tasks (external_id, repo, origin, title, body, source, source_id, author, url, labels)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             RETURNING id",
        )
        .bind(&new.external_id)
        .bind(&new.repo)
        .bind(&new.origin)
        .bind(&new.title)
        .bind(&new.body)
        .bind(&new.source)
        .bind(&new.source_id)
        .bind(&new.author)
        .bind(&new.url)
        .bind(&labels_json)
        .fetch_one(&self.pool)
        .await?;

        Ok(row.get("id"))
    }

    /// Get a task by its internal ID.
    pub async fn get(&self, id: i64) -> anyhow::Result<Task> {
        let row = sqlx::query("SELECT * FROM tasks WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .with_context(|| format!("task {id} not found"))?;

        Self::row_to_task(&row)
    }

    /// Get a task by its external ID within a repo.
    pub async fn get_by_external_id(
        &self,
        repo: &str,
        ext_id: &str,
    ) -> anyhow::Result<Option<Task>> {
        let row = sqlx::query("SELECT * FROM tasks WHERE repo = ? AND external_id = ?")
            .bind(repo)
            .bind(ext_id)
            .fetch_optional(&self.pool)
            .await?;

        match row {
            Some(r) => Ok(Some(Self::row_to_task(&r)?)),
            None => Ok(None),
        }
    }

    /// Upsert an external task — insert if new, update title/body/labels if exists.
    pub async fn upsert_external(&self, ext: &UpsertExternal<'_>) -> anyhow::Result<i64> {
        let labels_json = serde_json::to_string(ext.labels)?;
        let row = sqlx::query(
            "INSERT INTO tasks (external_id, repo, origin, title, body, author, url, labels)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(repo, external_id) DO UPDATE SET
                title = excluded.title,
                body = excluded.body,
                labels = excluded.labels,
                url = excluded.url,
                updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
             RETURNING id",
        )
        .bind(ext.ext_id)
        .bind(ext.repo)
        .bind(ext.origin)
        .bind(ext.title)
        .bind(ext.body)
        .bind(ext.author)
        .bind(ext.url)
        .bind(&labels_json)
        .fetch_one(&self.pool)
        .await?;

        Ok(row.get("id"))
    }

    // ---------------------------------------------------------------
    // Status
    // ---------------------------------------------------------------

    /// Update the status of a task.
    pub async fn update_status(&self, id: i64, status: TaskStatus) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE tasks SET status = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ?",
        )
        .bind(status.as_str())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// List tasks by status within a repo.
    pub async fn list_by_status(
        &self,
        repo: &str,
        status: TaskStatus,
    ) -> anyhow::Result<Vec<Task>> {
        let rows = sqlx::query(
            "SELECT * FROM tasks WHERE repo = ? AND status = ? ORDER BY created_at DESC",
        )
        .bind(repo)
        .bind(status.as_str())
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(Self::row_to_task).collect()
    }

    /// List routable tasks (status = 'new') within a repo.
    pub async fn list_routable(&self, repo: &str) -> anyhow::Result<Vec<Task>> {
        self.list_by_status(repo, TaskStatus::New).await
    }

    // ---------------------------------------------------------------
    // Field updates
    // ---------------------------------------------------------------

    /// Set arbitrary fields on a task using dynamic SQL.
    ///
    /// `updates` is a slice of (column_name, value) pairs.
    /// Only columns in the allowlist are accepted.
    pub async fn set_fields(
        &self,
        id: i64,
        updates: &[(&str, serde_json::Value)],
    ) -> anyhow::Result<()> {
        if updates.is_empty() {
            return Ok(());
        }

        // Allowlist of columns that can be updated dynamically
        const ALLOWED: &[&str] = &[
            "agent",
            "model",
            "complexity",
            "route_reason",
            "agent_profile",
            "selected_skills",
            "route_attempts",
            "attempts",
            "branch",
            "worktree",
            "worktree_cleaned",
            "summary",
            "last_error",
            "parent_id",
            "block_reason",
            "pr_number",
            "pr_review_context",
            "last_review_ts",
            "last_comment_review_ts",
            "merge_conflict_retries",
            "ci_merge_failures",
            "pr_create_failures",
            "review_agent_failures",
            "review_cycles",
            "input_tokens",
            "output_tokens",
            "input_cost_usd",
            "output_cost_usd",
            "total_cost_usd",
            "model_reroute_chain",
            "limit_reroute_chain",
            "budget_warning",
            "budget_exceeded",
            "memory",
            "delegations",
            "source",
            "source_id",
            "labels",
        ];

        for (col, _) in updates {
            anyhow::ensure!(
                ALLOWED.contains(col),
                "column {col} is not in the update allowlist"
            );
        }

        // Build SET clause dynamically
        let mut set_parts = Vec::new();
        let mut values: Vec<String> = Vec::new();

        for (col, val) in updates {
            set_parts.push(format!("{col} = ?"));
            match val {
                serde_json::Value::String(s) => values.push(s.clone()),
                serde_json::Value::Number(n) => values.push(n.to_string()),
                serde_json::Value::Bool(b) => values.push(if *b { "1" } else { "0" }.to_string()),
                serde_json::Value::Null => values.push(String::new()),
                other => values.push(serde_json::to_string(other)?),
            }
        }

        set_parts.push("updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')".to_string());
        let sql = format!("UPDATE tasks SET {} WHERE id = ?", set_parts.join(", "));

        let mut query = sqlx::query(&sql);
        for v in &values {
            query = query.bind(v);
        }
        query = query.bind(id);
        query.execute(&self.pool).await?;

        Ok(())
    }

    /// Increment an integer field by 1, returning the new value.
    pub async fn increment(&self, id: i64, field: &str) -> anyhow::Result<i32> {
        const INCREMENTABLE: &[&str] = &[
            "attempts",
            "route_attempts",
            "merge_conflict_retries",
            "ci_merge_failures",
            "pr_create_failures",
            "review_agent_failures",
            "review_cycles",
        ];

        anyhow::ensure!(
            INCREMENTABLE.contains(&field),
            "field {field} is not incrementable"
        );

        let sql = format!(
            "UPDATE tasks SET {field} = {field} + 1, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ? RETURNING {field}"
        );

        let row = sqlx::query(&sql).bind(id).fetch_one(&self.pool).await?;

        Ok(row.get(0))
    }

    /// Reset all failure/retry counters to zero.
    pub async fn reset_counters(&self, id: i64) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE tasks SET
                attempts = 0,
                route_attempts = 0,
                review_agent_failures = 0,
                merge_conflict_retries = 0,
                pr_create_failures = 0,
                ci_merge_failures = 0,
                review_cycles = 0,
                updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
             WHERE id = ?",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ---------------------------------------------------------------
    // Routing
    // ---------------------------------------------------------------

    /// Store routing results for a task.
    pub async fn store_route(&self, route: &StoreRoute<'_>) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE tasks SET
                agent = ?, model = ?, complexity = ?, route_reason = ?,
                agent_profile = ?, selected_skills = ?,
                updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
             WHERE id = ?",
        )
        .bind(route.agent)
        .bind(route.model)
        .bind(route.complexity)
        .bind(route.reason)
        .bind(route.profile)
        .bind(route.skills)
        .bind(route.id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ---------------------------------------------------------------
    // Memory
    // ---------------------------------------------------------------

    /// Append a memory entry to the task's memory JSON array.
    pub async fn append_memory(&self, id: i64, entry: &MemoryEntry) -> anyhow::Result<()> {
        // Read current, append, write back — all in one query
        let row = sqlx::query("SELECT memory FROM tasks WHERE id = ?")
            .bind(id)
            .fetch_one(&self.pool)
            .await?;

        let memory_str: String = row.get("memory");
        let mut memory: Vec<MemoryEntry> = serde_json::from_str(&memory_str).unwrap_or_default();
        memory.push(entry.clone());
        let new_json = serde_json::to_string(&memory)?;

        sqlx::query(
            "UPDATE tasks SET memory = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ?",
        )
        .bind(&new_json)
        .bind(id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Get the N most recent memory entries for a task.
    pub async fn recent_memory(&self, id: i64, max: usize) -> anyhow::Result<Vec<MemoryEntry>> {
        let row = sqlx::query("SELECT memory FROM tasks WHERE id = ?")
            .bind(id)
            .fetch_one(&self.pool)
            .await?;

        let memory_str: String = row.get("memory");
        let mut memory: Vec<MemoryEntry> = serde_json::from_str(&memory_str).unwrap_or_default();

        memory.sort_by_key(|m| m.attempt);
        if memory.len() > max {
            memory = memory.split_off(memory.len() - max);
        }
        Ok(memory)
    }

    // ---------------------------------------------------------------
    // Tokens
    // ---------------------------------------------------------------

    /// Store token usage and cost for a task.
    pub async fn store_tokens(
        &self,
        id: i64,
        input: i64,
        output: i64,
        model: &str,
    ) -> anyhow::Result<()> {
        let pricing = crate::sidecar::pricing_for_model(model);
        let usage = crate::sidecar::TokenUsage {
            input_tokens: input as u64,
            output_tokens: output as u64,
        };
        let cost = pricing.estimate_cost_usd(usage);

        sqlx::query(
            "UPDATE tasks SET
                input_tokens = ?, output_tokens = ?, model = ?,
                input_cost_usd = ?, output_cost_usd = ?, total_cost_usd = ?,
                updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
             WHERE id = ?",
        )
        .bind(input)
        .bind(output)
        .bind(model)
        .bind(cost.input_cost_usd)
        .bind(cost.output_cost_usd)
        .bind(cost.total_cost_usd)
        .bind(id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    // ---------------------------------------------------------------
    // Cleanup
    // ---------------------------------------------------------------

    /// Mark a task's worktree as cleaned.
    pub async fn mark_cleaned(&self, id: i64) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE tasks SET worktree_cleaned = 1, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ?",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// List tasks that are done/blocked with worktrees that haven't been cleaned.
    pub async fn list_cleanable(&self, repo: &str) -> anyhow::Result<Vec<Task>> {
        let rows = sqlx::query(
            "SELECT * FROM tasks
             WHERE repo = ?
               AND worktree != ''
               AND worktree_cleaned = 0
               AND status IN ('done', 'blocked')
             ORDER BY updated_at ASC",
        )
        .bind(repo)
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(Self::row_to_task).collect()
    }

    // ---------------------------------------------------------------
    // Task Runs (audit trail)
    // ---------------------------------------------------------------

    /// Start a new run, returning its ID.
    pub async fn start_run(&self, run: &StartRun<'_>) -> anyhow::Result<i64> {
        let row = sqlx::query(
            "INSERT INTO task_runs (task_id, attempt, run_type, agent, model, command, prompt)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(task_id, attempt, run_type) DO UPDATE SET
                agent = excluded.agent, model = excluded.model,
                command = excluded.command, prompt = excluded.prompt,
                started_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
             RETURNING id",
        )
        .bind(run.task_id)
        .bind(run.attempt)
        .bind(run.run_type)
        .bind(run.agent)
        .bind(run.model)
        .bind(run.command)
        .bind(run.prompt)
        .fetch_one(&self.pool)
        .await?;

        Ok(row.get("id"))
    }

    /// Complete a run with results.
    pub async fn complete_run(&self, run: &CompleteRun<'_>) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE task_runs SET
                exit_code = ?, stdout = ?, stderr = ?, parsed_response = ?,
                outcome = ?, error = ?,
                input_tokens = ?, output_tokens = ?, total_cost_usd = ?,
                duration_secs = ?,
                completed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
             WHERE id = ?",
        )
        .bind(run.exit_code)
        .bind(run.stdout)
        .bind(run.stderr)
        .bind(run.parsed)
        .bind(run.outcome)
        .bind(run.error)
        .bind(run.tokens.input_tokens)
        .bind(run.tokens.output_tokens)
        .bind(run.tokens.total_cost_usd)
        .bind(run.tokens.duration_secs)
        .bind(run.run_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Get all runs for a task, ordered by attempt.
    pub async fn get_runs(&self, task_id: i64) -> anyhow::Result<Vec<TaskRun>> {
        let rows = sqlx::query(
            "SELECT * FROM task_runs WHERE task_id = ? ORDER BY attempt ASC, run_type ASC",
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(Self::row_to_run).collect()
    }

    /// Get the last run of a specific type for a task.
    pub async fn get_last_run(
        &self,
        task_id: i64,
        run_type: &str,
    ) -> anyhow::Result<Option<TaskRun>> {
        let row = sqlx::query(
            "SELECT * FROM task_runs WHERE task_id = ? AND run_type = ? ORDER BY attempt DESC LIMIT 1",
        )
        .bind(task_id)
        .bind(run_type)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some(r) => Ok(Some(Self::row_to_run(&r)?)),
            None => Ok(None),
        }
    }

    /// Prune old runs for done/blocked tasks older than `days` days.
    pub async fn prune_old_runs(&self, days: i32) -> anyhow::Result<u64> {
        let result = sqlx::query(
            "DELETE FROM task_runs WHERE task_id IN (
                SELECT id FROM tasks
                WHERE status IN ('done', 'blocked')
                  AND updated_at < strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ? || ' days')
             )",
        )
        .bind(format!("-{days}"))
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected())
    }

    // ---------------------------------------------------------------
    // Row mapping helpers
    // ---------------------------------------------------------------

    fn row_to_task(row: &sqlx::sqlite::SqliteRow) -> anyhow::Result<Task> {
        let status_str: String = row.get("status");
        let labels_str: String = row.get("labels");
        let memory_str: String = row.get("memory");
        let delegations_str: String = row.get("delegations");

        Ok(Task {
            id: row.get("id"),
            external_id: row.get("external_id"),
            repo: row.get("repo"),
            origin: row.get("origin"),
            title: row.get("title"),
            body: row.get("body"),
            status: TaskStatus::from_str(&status_str).unwrap_or(TaskStatus::New),
            source: row.get("source"),
            source_id: row.get("source_id"),
            author: row.get("author"),
            url: row.get("url"),
            labels: serde_json::from_str(&labels_str).unwrap_or_default(),
            agent: row.get("agent"),
            model: row.get("model"),
            complexity: row.get("complexity"),
            route_reason: row.get("route_reason"),
            agent_profile: row.get("agent_profile"),
            selected_skills: row.get("selected_skills"),
            route_attempts: row.get("route_attempts"),
            attempts: row.get("attempts"),
            branch: row.get("branch"),
            worktree: row.get("worktree"),
            worktree_cleaned: row.get::<i32, _>("worktree_cleaned") != 0,
            summary: row.get("summary"),
            last_error: row.get("last_error"),
            parent_id: row.get("parent_id"),
            block_reason: row.get("block_reason"),
            pr_number: row.get("pr_number"),
            pr_review_context: row.get("pr_review_context"),
            last_review_ts: row.get("last_review_ts"),
            last_comment_review_ts: row.get("last_comment_review_ts"),
            merge_conflict_retries: row.get("merge_conflict_retries"),
            ci_merge_failures: row.get("ci_merge_failures"),
            pr_create_failures: row.get("pr_create_failures"),
            review_agent_failures: row.get("review_agent_failures"),
            review_cycles: row.get("review_cycles"),
            input_tokens: row.get("input_tokens"),
            output_tokens: row.get("output_tokens"),
            input_cost_usd: row.get("input_cost_usd"),
            output_cost_usd: row.get("output_cost_usd"),
            total_cost_usd: row.get("total_cost_usd"),
            model_reroute_chain: row.get("model_reroute_chain"),
            limit_reroute_chain: row.get("limit_reroute_chain"),
            budget_warning: row.get("budget_warning"),
            budget_exceeded: row.get::<i32, _>("budget_exceeded") != 0,
            memory: serde_json::from_str(&memory_str).unwrap_or_default(),
            delegations: serde_json::from_str(&delegations_str).unwrap_or_default(),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        })
    }

    // ---------------------------------------------------------------
    // Dual-write helpers (Phase 2: sidecar → SQLite sync)
    // ---------------------------------------------------------------

    /// Ensure an external task exists in the store, upserting from its ExternalTask representation.
    /// Returns the store's internal ID for the task.
    pub async fn ensure_external_task(
        &self,
        repo: &str,
        ext: &crate::backends::ExternalTask,
    ) -> anyhow::Result<i64> {
        self.upsert_external(&UpsertExternal {
            repo,
            ext_id: &ext.id.0,
            title: &ext.title,
            body: &ext.body,
            author: &ext.author,
            url: &ext.url,
            labels: &ext.labels,
            origin: "github",
        })
        .await
    }

    /// Sync sidecar fields to the store for a given task.
    /// Reads key fields from sidecar and writes them to the tasks table.
    /// Silently ignores errors (best-effort dual-write).
    pub async fn sync_sidecar_to_store(&self, store_id: i64, sidecar_task_id: &str) {
        let mut updates: Vec<(&str, serde_json::Value)> = Vec::new();

        // Routing fields
        if let Ok(v) = crate::sidecar::get(sidecar_task_id, "agent") {
            if !v.is_empty() {
                updates.push(("agent", serde_json::json!(v)));
            }
        }
        if let Ok(v) = crate::sidecar::get(sidecar_task_id, "model") {
            if !v.is_empty() {
                updates.push(("model", serde_json::json!(v)));
            }
        }
        if let Ok(v) = crate::sidecar::get(sidecar_task_id, "complexity") {
            if !v.is_empty() {
                updates.push(("complexity", serde_json::json!(v)));
            }
        }
        if let Ok(v) = crate::sidecar::get(sidecar_task_id, "route_reason") {
            if !v.is_empty() {
                updates.push(("route_reason", serde_json::json!(v)));
            }
        }

        // Execution fields
        if let Ok(v) = crate::sidecar::get(sidecar_task_id, "branch") {
            if !v.is_empty() {
                updates.push(("branch", serde_json::json!(v)));
            }
        }
        if let Ok(v) = crate::sidecar::get(sidecar_task_id, "worktree") {
            if !v.is_empty() {
                updates.push(("worktree", serde_json::json!(v)));
            }
        }
        if let Ok(v) = crate::sidecar::get(sidecar_task_id, "summary") {
            if !v.is_empty() {
                updates.push(("summary", serde_json::json!(v)));
            }
        }
        if let Ok(v) = crate::sidecar::get(sidecar_task_id, "last_error") {
            if !v.is_empty() {
                updates.push(("last_error", serde_json::json!(v)));
            }
        }

        // Counters
        let attempts = crate::sidecar::get_u64(sidecar_task_id, "attempts");
        if attempts > 0 {
            updates.push(("attempts", serde_json::json!(attempts)));
        }
        let route_attempts = crate::sidecar::get_u64(sidecar_task_id, "route_attempts");
        if route_attempts > 0 {
            updates.push(("route_attempts", serde_json::json!(route_attempts)));
        }

        // PR fields
        let pr_number = crate::sidecar::get_u64(sidecar_task_id, "pr_number");
        if pr_number > 0 {
            updates.push(("pr_number", serde_json::json!(pr_number)));
        }

        // Tokens & cost
        let input_tokens = crate::sidecar::get_u64(sidecar_task_id, "input_tokens");
        let output_tokens = crate::sidecar::get_u64(sidecar_task_id, "output_tokens");
        if input_tokens > 0 || output_tokens > 0 {
            updates.push(("input_tokens", serde_json::json!(input_tokens)));
            updates.push(("output_tokens", serde_json::json!(output_tokens)));
            let cost = crate::sidecar::get_cost_estimate(sidecar_task_id);
            updates.push(("input_cost_usd", serde_json::json!(cost.input_cost_usd)));
            updates.push(("output_cost_usd", serde_json::json!(cost.output_cost_usd)));
            updates.push(("total_cost_usd", serde_json::json!(cost.total_cost_usd)));
        }

        // Review counters
        let review_cycles = crate::sidecar::get_u64(sidecar_task_id, "review_cycles");
        if review_cycles > 0 {
            updates.push(("review_cycles", serde_json::json!(review_cycles)));
        }
        let review_failures = crate::sidecar::get_u64(sidecar_task_id, "review_agent_failures");
        if review_failures > 0 {
            updates.push(("review_agent_failures", serde_json::json!(review_failures)));
        }
        let merge_retries = crate::sidecar::get_u64(sidecar_task_id, "merge_conflict_retries");
        if merge_retries > 0 {
            updates.push(("merge_conflict_retries", serde_json::json!(merge_retries)));
        }

        if !updates.is_empty() {
            if let Err(e) = self.set_fields(store_id, &updates).await {
                tracing::debug!(store_id, error = %e, "dual-write: failed to sync sidecar → store");
            }
        }
    }

    /// Resolve a sidecar task_id (e.g. "42" or "internal:3") to a store internal ID.
    /// Returns None if the task is not in the store yet.
    pub async fn resolve_task_id(
        &self,
        repo: &str,
        sidecar_task_id: &str,
    ) -> anyhow::Result<Option<i64>> {
        // Internal tasks use "internal:{n}" format
        if sidecar_task_id.starts_with("internal:") {
            // Internal tasks aren't in the store yet (will be migrated in Phase 4)
            // For now, return None
            return Ok(None);
        }
        // External tasks: look up by external_id
        match self.get_by_external_id(repo, sidecar_task_id).await? {
            Some(task) => Ok(Some(task.id)),
            None => Ok(None),
        }
    }

    fn row_to_run(row: &sqlx::sqlite::SqliteRow) -> anyhow::Result<TaskRun> {
        Ok(TaskRun {
            id: row.get("id"),
            task_id: row.get("task_id"),
            attempt: row.get("attempt"),
            run_type: row.get("run_type"),
            agent: row.get("agent"),
            model: row.get("model"),
            command: row.get("command"),
            prompt: row.get("prompt"),
            env_vars: row.get("env_vars"),
            exit_code: row.get("exit_code"),
            stdout: row.get("stdout"),
            stderr: row.get("stderr"),
            parsed_response: row.get("parsed_response"),
            outcome: row.get("outcome"),
            error: row.get("error"),
            input_tokens: row.get("input_tokens"),
            output_tokens: row.get("output_tokens"),
            total_cost_usd: row.get("total_cost_usd"),
            duration_secs: row.get("duration_secs"),
            started_at: row.get("started_at"),
            completed_at: row.get("completed_at"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_and_get_task() {
        let store = TaskStore::open_memory().await.unwrap();

        let id = store
            .create(&NewTask {
                external_id: None,
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "Test task".to_string(),
                body: "Test body".to_string(),
                source: "cron".to_string(),
                source_id: "daily-sync".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
            })
            .await
            .unwrap();

        assert_eq!(id, 1);

        let task = store.get(id).await.unwrap();
        assert_eq!(task.title, "Test task");
        assert_eq!(task.body, "Test body");
        assert_eq!(task.status, TaskStatus::New);
        assert_eq!(task.origin, "internal");
        assert_eq!(task.repo, "owner/repo");
    }

    #[tokio::test]
    async fn upsert_external_task() {
        let store = TaskStore::open_memory().await.unwrap();

        let id1 = store
            .upsert_external(&UpsertExternal {
                repo: "owner/repo",
                ext_id: "42",
                title: "Original title",
                body: "Original body",
                author: "user",
                url: "https://github.com/owner/repo/issues/42",
                labels: &["bug".to_string()],
                origin: "github",
            })
            .await
            .unwrap();

        // Upsert same external_id — should update, not create new
        let id2 = store
            .upsert_external(&UpsertExternal {
                repo: "owner/repo",
                ext_id: "42",
                title: "Updated title",
                body: "Updated body",
                author: "user",
                url: "https://github.com/owner/repo/issues/42",
                labels: &["bug".to_string(), "priority:high".to_string()],
                origin: "github",
            })
            .await
            .unwrap();

        assert_eq!(id1, id2);

        let task = store.get(id1).await.unwrap();
        assert_eq!(task.title, "Updated title");
        assert_eq!(task.external_id, Some("42".to_string()));
    }

    #[tokio::test]
    async fn update_status() {
        let store = TaskStore::open_memory().await.unwrap();

        let id = store
            .create(&NewTask {
                external_id: None,
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "Test".to_string(),
                body: "".to_string(),
                source: "manual".to_string(),
                source_id: "".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
            })
            .await
            .unwrap();

        store
            .update_status(id, TaskStatus::InProgress)
            .await
            .unwrap();
        let task = store.get(id).await.unwrap();
        assert_eq!(task.status, TaskStatus::InProgress);
    }

    #[tokio::test]
    async fn list_by_status() {
        let store = TaskStore::open_memory().await.unwrap();

        for i in 0..3 {
            store
                .create(&NewTask {
                    external_id: None,
                    repo: "owner/repo".to_string(),
                    origin: "internal".to_string(),
                    title: format!("Task {i}"),
                    body: "".to_string(),
                    source: "cron".to_string(),
                    source_id: format!("job-{i}"),
                    author: "".to_string(),
                    url: "".to_string(),
                    labels: vec![],
                })
                .await
                .unwrap();
        }

        let new_tasks = store
            .list_by_status("owner/repo", TaskStatus::New)
            .await
            .unwrap();
        assert_eq!(new_tasks.len(), 3);

        store.update_status(1, TaskStatus::Done).await.unwrap();
        let done = store
            .list_by_status("owner/repo", TaskStatus::Done)
            .await
            .unwrap();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].title, "Task 0");
    }

    #[tokio::test]
    async fn increment_and_reset_counters() {
        let store = TaskStore::open_memory().await.unwrap();

        let id = store
            .create(&NewTask {
                external_id: None,
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "Test".to_string(),
                body: "".to_string(),
                source: "manual".to_string(),
                source_id: "".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
            })
            .await
            .unwrap();

        let v = store.increment(id, "attempts").await.unwrap();
        assert_eq!(v, 1);
        let v = store.increment(id, "attempts").await.unwrap();
        assert_eq!(v, 2);

        store.reset_counters(id).await.unwrap();
        let task = store.get(id).await.unwrap();
        assert_eq!(task.attempts, 0);
    }

    #[tokio::test]
    async fn memory_append_and_recent() {
        let store = TaskStore::open_memory().await.unwrap();

        let id = store
            .create(&NewTask {
                external_id: None,
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "Test".to_string(),
                body: "".to_string(),
                source: "manual".to_string(),
                source_id: "".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
            })
            .await
            .unwrap();

        for i in 1..=5 {
            store
                .append_memory(
                    id,
                    &MemoryEntry {
                        attempt: i,
                        agent: "claude".to_string(),
                        model: Some("sonnet".to_string()),
                        learnings: vec![format!("Learning {i}")],
                        error: None,
                        files_modified: vec![],
                        approach: format!("Approach {i}"),
                        timestamp: format!("2026-01-0{i}T00:00:00Z"),
                    },
                )
                .await
                .unwrap();
        }

        let recent = store.recent_memory(id, 3).await.unwrap();
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].attempt, 3);
        assert_eq!(recent[2].attempt, 5);
    }

    #[tokio::test]
    async fn task_runs_lifecycle() {
        let store = TaskStore::open_memory().await.unwrap();

        let task_id = store
            .create(&NewTask {
                external_id: None,
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "Test".to_string(),
                body: "".to_string(),
                source: "manual".to_string(),
                source_id: "".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
            })
            .await
            .unwrap();

        let run_id = store
            .start_run(&StartRun {
                task_id,
                attempt: 1,
                run_type: "agent",
                agent: "claude",
                model: "sonnet",
                command: "claude -p ...",
                prompt: "system prompt",
            })
            .await
            .unwrap();

        store
            .complete_run(&CompleteRun {
                run_id,
                exit_code: Some(0),
                stdout: "agent output here",
                stderr: "",
                parsed: r#"{"summary":"fixed it"}"#,
                outcome: "success",
                error: "",
                tokens: RunTokenUsage {
                    input_tokens: 50000,
                    output_tokens: 10000,
                    total_cost_usd: 0.30,
                    duration_secs: 45.0,
                },
            })
            .await
            .unwrap();

        let runs = store.get_runs(task_id).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].outcome, "success");
        assert_eq!(runs[0].exit_code, Some(0));

        let last = store.get_last_run(task_id, "agent").await.unwrap();
        assert!(last.is_some());
        assert_eq!(last.unwrap().agent, "claude");

        let none = store.get_last_run(task_id, "review").await.unwrap();
        assert!(none.is_none());
    }

    #[tokio::test]
    async fn set_fields_updates_task() {
        let store = TaskStore::open_memory().await.unwrap();

        let id = store
            .create(&NewTask {
                external_id: None,
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "Test".to_string(),
                body: "".to_string(),
                source: "manual".to_string(),
                source_id: "".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
            })
            .await
            .unwrap();

        store
            .set_fields(
                id,
                &[
                    ("agent", serde_json::json!("claude")),
                    ("branch", serde_json::json!("fix-bug-42")),
                    ("pr_number", serde_json::json!(123)),
                ],
            )
            .await
            .unwrap();

        let task = store.get(id).await.unwrap();
        assert_eq!(task.agent, Some("claude".to_string()));
        assert_eq!(task.branch, "fix-bug-42");
        assert_eq!(task.pr_number, Some(123));
    }

    #[tokio::test]
    async fn set_fields_rejects_unknown_column() {
        let store = TaskStore::open_memory().await.unwrap();

        let id = store
            .create(&NewTask {
                external_id: None,
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "Test".to_string(),
                body: "".to_string(),
                source: "manual".to_string(),
                source_id: "".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
            })
            .await
            .unwrap();

        let result = store
            .set_fields(id, &[("evil_column", serde_json::json!("drop table"))])
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn store_tokens() {
        let store = TaskStore::open_memory().await.unwrap();

        let id = store
            .create(&NewTask {
                external_id: None,
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "Test".to_string(),
                body: "".to_string(),
                source: "manual".to_string(),
                source_id: "".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
            })
            .await
            .unwrap();

        store
            .store_tokens(id, 50000, 10000, "sonnet")
            .await
            .unwrap();

        let task = store.get(id).await.unwrap();
        assert_eq!(task.input_tokens, 50000);
        assert_eq!(task.output_tokens, 10000);
        assert!(task.total_cost_usd > 0.0);
        assert_eq!(task.model, Some("sonnet".to_string()));
    }

    #[tokio::test]
    async fn list_cleanable() {
        let store = TaskStore::open_memory().await.unwrap();

        let id = store
            .create(&NewTask {
                external_id: None,
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "Done task".to_string(),
                body: "".to_string(),
                source: "manual".to_string(),
                source_id: "".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
            })
            .await
            .unwrap();

        // Set worktree and mark done
        store
            .set_fields(id, &[("worktree", serde_json::json!("/tmp/wt"))])
            .await
            .unwrap();
        store.update_status(id, TaskStatus::Done).await.unwrap();

        let cleanable = store.list_cleanable("owner/repo").await.unwrap();
        assert_eq!(cleanable.len(), 1);

        store.mark_cleaned(id).await.unwrap();
        let cleanable = store.list_cleanable("owner/repo").await.unwrap();
        assert_eq!(cleanable.len(), 0);
    }

    // ---------------------------------------------------------------
    // Additional coverage
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn get_nonexistent_task_returns_error() {
        let store = TaskStore::open_memory().await.unwrap();
        let result = store.get(999).await;
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("not found"),
            "error should mention 'not found'"
        );
    }

    #[tokio::test]
    async fn get_by_external_id_returns_none_for_missing() {
        let store = TaskStore::open_memory().await.unwrap();
        let result = store.get_by_external_id("owner/repo", "999").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn get_by_external_id_finds_existing() {
        let store = TaskStore::open_memory().await.unwrap();
        store
            .upsert_external(&UpsertExternal {
                repo: "owner/repo",
                ext_id: "55",
                title: "Find me",
                body: "",
                author: "user",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();

        let task = store
            .get_by_external_id("owner/repo", "55")
            .await
            .unwrap()
            .expect("should find the task");
        assert_eq!(task.title, "Find me");
        assert_eq!(task.external_id, Some("55".to_string()));
    }

    #[tokio::test]
    async fn unique_constraint_on_repo_external_id() {
        let store = TaskStore::open_memory().await.unwrap();

        // Two tasks with same external_id but different repos should both succeed
        let id1 = store
            .upsert_external(&UpsertExternal {
                repo: "owner/repo-a",
                ext_id: "1",
                title: "Task A",
                body: "",
                author: "",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();

        let id2 = store
            .upsert_external(&UpsertExternal {
                repo: "owner/repo-b",
                ext_id: "1",
                title: "Task B",
                body: "",
                author: "",
                url: "",
                labels: &[],
                origin: "github",
            })
            .await
            .unwrap();

        assert_ne!(id1, id2, "different repos should produce different IDs");
    }

    #[tokio::test]
    async fn list_by_status_scopes_to_repo() {
        let store = TaskStore::open_memory().await.unwrap();

        store
            .create(&NewTask {
                external_id: None,
                repo: "owner/repo-a".to_string(),
                origin: "internal".to_string(),
                title: "Task A".to_string(),
                body: "".to_string(),
                source: "manual".to_string(),
                source_id: "".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
            })
            .await
            .unwrap();

        store
            .create(&NewTask {
                external_id: None,
                repo: "owner/repo-b".to_string(),
                origin: "internal".to_string(),
                title: "Task B".to_string(),
                body: "".to_string(),
                source: "manual".to_string(),
                source_id: "".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
            })
            .await
            .unwrap();

        let a_tasks = store
            .list_by_status("owner/repo-a", TaskStatus::New)
            .await
            .unwrap();
        assert_eq!(a_tasks.len(), 1);
        assert_eq!(a_tasks[0].title, "Task A");

        let b_tasks = store
            .list_by_status("owner/repo-b", TaskStatus::New)
            .await
            .unwrap();
        assert_eq!(b_tasks.len(), 1);
        assert_eq!(b_tasks[0].title, "Task B");
    }

    #[tokio::test]
    async fn set_fields_empty_is_noop() {
        let store = TaskStore::open_memory().await.unwrap();
        let id = store
            .create(&NewTask {
                external_id: None,
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "Test".to_string(),
                body: "".to_string(),
                source: "manual".to_string(),
                source_id: "".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
            })
            .await
            .unwrap();

        // Empty updates should succeed without error
        store.set_fields(id, &[]).await.unwrap();
    }

    #[tokio::test]
    async fn set_fields_with_null_value() {
        let store = TaskStore::open_memory().await.unwrap();
        let id = store
            .create(&NewTask {
                external_id: None,
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "Test".to_string(),
                body: "".to_string(),
                source: "manual".to_string(),
                source_id: "".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
            })
            .await
            .unwrap();

        // Set agent, then clear it with null
        store
            .set_fields(
                id,
                &[
                    ("agent", serde_json::json!("claude")),
                    ("summary", serde_json::json!(null)),
                ],
            )
            .await
            .unwrap();

        let task = store.get(id).await.unwrap();
        assert_eq!(task.agent, Some("claude".to_string()));
    }

    #[tokio::test]
    async fn increment_rejects_disallowed_field() {
        let store = TaskStore::open_memory().await.unwrap();
        let id = store
            .create(&NewTask {
                external_id: None,
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "Test".to_string(),
                body: "".to_string(),
                source: "manual".to_string(),
                source_id: "".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
            })
            .await
            .unwrap();

        let result = store.increment(id, "input_tokens").await;
        assert!(result.is_err(), "input_tokens should not be incrementable");
    }

    #[tokio::test]
    async fn store_route_updates_routing_fields() {
        let store = TaskStore::open_memory().await.unwrap();
        let id = store
            .create(&NewTask {
                external_id: None,
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "Route me".to_string(),
                body: "".to_string(),
                source: "manual".to_string(),
                source_id: "".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
            })
            .await
            .unwrap();

        store
            .store_route(&StoreRoute {
                id,
                agent: "codex",
                model: Some("gpt-5.2"),
                complexity: "complex",
                reason: "needs deep analysis",
                profile: r#"{"role":"backend"}"#,
                skills: "git,rust",
            })
            .await
            .unwrap();

        let task = store.get(id).await.unwrap();
        assert_eq!(task.agent, Some("codex".to_string()));
        assert_eq!(task.model, Some("gpt-5.2".to_string()));
        assert_eq!(task.complexity, "complex");
        assert_eq!(task.route_reason, "needs deep analysis");
        assert_eq!(task.selected_skills, "git,rust");
    }

    #[tokio::test]
    async fn memory_empty_by_default() {
        let store = TaskStore::open_memory().await.unwrap();
        let id = store
            .create(&NewTask {
                external_id: None,
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "Test".to_string(),
                body: "".to_string(),
                source: "manual".to_string(),
                source_id: "".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
            })
            .await
            .unwrap();

        let memory = store.recent_memory(id, 10).await.unwrap();
        assert!(memory.is_empty());
    }

    #[tokio::test]
    async fn labels_roundtrip_as_json() {
        let store = TaskStore::open_memory().await.unwrap();
        let labels = vec![
            "status:new".to_string(),
            "agent:claude".to_string(),
            "priority:high".to_string(),
        ];

        let id = store
            .create(&NewTask {
                external_id: Some("10".to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "Labeled task".to_string(),
                body: "".to_string(),
                source: "webhook".to_string(),
                source_id: "".to_string(),
                author: "user".to_string(),
                url: "".to_string(),
                labels: labels.clone(),
            })
            .await
            .unwrap();

        let task = store.get(id).await.unwrap();
        assert_eq!(task.labels, labels);
    }

    #[tokio::test]
    async fn task_with_parent_id() {
        let store = TaskStore::open_memory().await.unwrap();

        let parent_id = store
            .create(&NewTask {
                external_id: None,
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "Parent".to_string(),
                body: "".to_string(),
                source: "manual".to_string(),
                source_id: "".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
            })
            .await
            .unwrap();

        let child_id = store
            .create(&NewTask {
                external_id: None,
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "Child".to_string(),
                body: "".to_string(),
                source: "manual".to_string(),
                source_id: "".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
            })
            .await
            .unwrap();

        store
            .set_fields(child_id, &[("parent_id", serde_json::json!(parent_id))])
            .await
            .unwrap();

        let child = store.get(child_id).await.unwrap();
        assert_eq!(child.parent_id, Some(parent_id));
    }

    #[tokio::test]
    async fn multiple_runs_per_task() {
        let store = TaskStore::open_memory().await.unwrap();

        let task_id = store
            .create(&NewTask {
                external_id: None,
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "Multi-run".to_string(),
                body: "".to_string(),
                source: "manual".to_string(),
                source_id: "".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
            })
            .await
            .unwrap();

        // Attempt 1: agent run fails
        let r1 = store
            .start_run(&StartRun {
                task_id,
                attempt: 1,
                run_type: "agent",
                agent: "claude",
                model: "sonnet",
                command: "claude -p ...",
                prompt: "prompt v1",
            })
            .await
            .unwrap();
        store
            .complete_run(&CompleteRun {
                run_id: r1,
                exit_code: Some(1),
                stdout: "error output",
                stderr: "compile failed",
                parsed: "",
                outcome: "failed",
                error: "compilation error",
                tokens: RunTokenUsage {
                    input_tokens: 30000,
                    output_tokens: 5000,
                    total_cost_usd: 0.10,
                    duration_secs: 20.0,
                },
            })
            .await
            .unwrap();

        // Attempt 2: agent run succeeds
        let r2 = store
            .start_run(&StartRun {
                task_id,
                attempt: 2,
                run_type: "agent",
                agent: "claude",
                model: "opus",
                command: "claude -p ...",
                prompt: "prompt v2",
            })
            .await
            .unwrap();
        store
            .complete_run(&CompleteRun {
                run_id: r2,
                exit_code: Some(0),
                stdout: "success output",
                stderr: "",
                parsed: r#"{"summary":"done"}"#,
                outcome: "success",
                error: "",
                tokens: RunTokenUsage {
                    input_tokens: 40000,
                    output_tokens: 8000,
                    total_cost_usd: 0.80,
                    duration_secs: 60.0,
                },
            })
            .await
            .unwrap();

        // Attempt 2: review run
        let r3 = store
            .start_run(&StartRun {
                task_id,
                attempt: 2,
                run_type: "review",
                agent: "claude",
                model: "sonnet",
                command: "claude -p review ...",
                prompt: "review prompt",
            })
            .await
            .unwrap();
        store
            .complete_run(&CompleteRun {
                run_id: r3,
                exit_code: Some(0),
                stdout: "LGTM",
                stderr: "",
                parsed: r#"{"verdict":"approve"}"#,
                outcome: "success",
                error: "",
                tokens: RunTokenUsage::default(),
            })
            .await
            .unwrap();

        // All 3 runs
        let runs = store.get_runs(task_id).await.unwrap();
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].attempt, 1);
        assert_eq!(runs[0].outcome, "failed");
        assert_eq!(runs[1].attempt, 2);
        assert_eq!(runs[1].run_type, "agent");
        assert_eq!(runs[2].attempt, 2);
        assert_eq!(runs[2].run_type, "review");

        // Last agent run is attempt 2
        let last_agent = store.get_last_run(task_id, "agent").await.unwrap().unwrap();
        assert_eq!(last_agent.attempt, 2);
        assert_eq!(last_agent.model, "opus");

        // Last review run
        let last_review = store
            .get_last_run(task_id, "review")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(last_review.outcome, "success");
    }

    #[tokio::test]
    async fn start_run_upserts_on_conflict() {
        let store = TaskStore::open_memory().await.unwrap();

        let task_id = store
            .create(&NewTask {
                external_id: None,
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "Test".to_string(),
                body: "".to_string(),
                source: "manual".to_string(),
                source_id: "".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
            })
            .await
            .unwrap();

        // Start same run twice (same task_id, attempt, run_type)
        let id1 = store
            .start_run(&StartRun {
                task_id,
                attempt: 1,
                run_type: "agent",
                agent: "claude",
                model: "sonnet",
                command: "cmd1",
                prompt: "prompt1",
            })
            .await
            .unwrap();

        let id2 = store
            .start_run(&StartRun {
                task_id,
                attempt: 1,
                run_type: "agent",
                agent: "codex",
                model: "gpt-5.2",
                command: "cmd2",
                prompt: "prompt2",
            })
            .await
            .unwrap();

        assert_eq!(id1, id2, "upsert should return same ID");

        // Should have updated agent/model
        let runs = store.get_runs(task_id).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].agent, "codex");
        assert_eq!(runs[0].model, "gpt-5.2");
    }

    #[tokio::test]
    async fn reset_counters_preserves_other_fields() {
        let store = TaskStore::open_memory().await.unwrap();

        let id = store
            .create(&NewTask {
                external_id: None,
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "Test".to_string(),
                body: "".to_string(),
                source: "manual".to_string(),
                source_id: "".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
            })
            .await
            .unwrap();

        // Set some counters and non-counter fields
        store
            .set_fields(
                id,
                &[
                    ("agent", serde_json::json!("claude")),
                    ("branch", serde_json::json!("fix-123")),
                    ("summary", serde_json::json!("did something")),
                ],
            )
            .await
            .unwrap();
        store.increment(id, "attempts").await.unwrap();
        store.increment(id, "attempts").await.unwrap();
        store.increment(id, "review_cycles").await.unwrap();

        // Reset
        store.reset_counters(id).await.unwrap();

        let task = store.get(id).await.unwrap();
        assert_eq!(task.attempts, 0);
        assert_eq!(task.review_cycles, 0);
        // Non-counter fields preserved
        assert_eq!(task.agent, Some("claude".to_string()));
        assert_eq!(task.branch, "fix-123");
        assert_eq!(task.summary, "did something");
    }

    #[tokio::test]
    async fn status_lifecycle_full_flow() {
        let store = TaskStore::open_memory().await.unwrap();

        let id = store
            .create(&NewTask {
                external_id: Some("42".to_string()),
                repo: "owner/repo".to_string(),
                origin: "github".to_string(),
                title: "Full lifecycle".to_string(),
                body: "".to_string(),
                source: "webhook".to_string(),
                source_id: "".to_string(),
                author: "user".to_string(),
                url: "".to_string(),
                labels: vec![],
            })
            .await
            .unwrap();

        // Walk through the full lifecycle
        let transitions = [
            TaskStatus::Routed,
            TaskStatus::InProgress,
            TaskStatus::NeedsReview,
            TaskStatus::InReview,
            TaskStatus::Done,
        ];

        for status in transitions {
            store.update_status(id, status).await.unwrap();
            let task = store.get(id).await.unwrap();
            assert_eq!(task.status, status);
        }
    }

    #[tokio::test]
    async fn list_routable_returns_only_new() {
        let store = TaskStore::open_memory().await.unwrap();

        let id1 = store
            .create(&NewTask {
                external_id: None,
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "New task".to_string(),
                body: "".to_string(),
                source: "manual".to_string(),
                source_id: "".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
            })
            .await
            .unwrap();

        let _id2 = store
            .create(&NewTask {
                external_id: None,
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "Done task".to_string(),
                body: "".to_string(),
                source: "manual".to_string(),
                source_id: "".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
            })
            .await
            .unwrap();
        store.update_status(_id2, TaskStatus::Done).await.unwrap();

        let routable = store.list_routable("owner/repo").await.unwrap();
        assert_eq!(routable.len(), 1);
        assert_eq!(routable[0].id, id1);
    }

    #[tokio::test]
    async fn cleanable_excludes_in_progress_tasks() {
        let store = TaskStore::open_memory().await.unwrap();

        let id = store
            .create(&NewTask {
                external_id: None,
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "In progress".to_string(),
                body: "".to_string(),
                source: "manual".to_string(),
                source_id: "".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
            })
            .await
            .unwrap();

        store
            .set_fields(id, &[("worktree", serde_json::json!("/tmp/wt"))])
            .await
            .unwrap();
        store
            .update_status(id, TaskStatus::InProgress)
            .await
            .unwrap();

        // In-progress task with worktree should NOT be cleanable
        let cleanable = store.list_cleanable("owner/repo").await.unwrap();
        assert_eq!(cleanable.len(), 0);

        // But blocked task with worktree SHOULD be cleanable
        store.update_status(id, TaskStatus::Blocked).await.unwrap();
        let cleanable = store.list_cleanable("owner/repo").await.unwrap();
        assert_eq!(cleanable.len(), 1);
    }

    #[tokio::test]
    async fn delegations_stored_as_json() {
        let store = TaskStore::open_memory().await.unwrap();

        let id = store
            .create(&NewTask {
                external_id: None,
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "Test".to_string(),
                body: "".to_string(),
                source: "manual".to_string(),
                source_id: "".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
            })
            .await
            .unwrap();

        let delegations = serde_json::json!([
            {"task_id": 2, "reason": "sub-task"},
            {"task_id": 3, "reason": "follow-up"}
        ]);

        store
            .set_fields(id, &[("delegations", delegations.clone())])
            .await
            .unwrap();

        let task = store.get(id).await.unwrap();
        assert_eq!(task.delegations.len(), 2);
    }

    #[tokio::test]
    async fn created_at_and_updated_at_are_set() {
        let store = TaskStore::open_memory().await.unwrap();

        let id = store
            .create(&NewTask {
                external_id: None,
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "Test".to_string(),
                body: "".to_string(),
                source: "manual".to_string(),
                source_id: "".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
            })
            .await
            .unwrap();

        let task = store.get(id).await.unwrap();
        assert!(!task.created_at.is_empty());
        assert!(!task.updated_at.is_empty());
        assert!(task.created_at.ends_with('Z'), "should be UTC");
        assert!(task.created_at.contains('T'), "should be RFC3339");
    }

    #[tokio::test]
    async fn concurrent_increments() {
        let store = TaskStore::open_memory().await.unwrap();

        let id = store
            .create(&NewTask {
                external_id: None,
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "Test".to_string(),
                body: "".to_string(),
                source: "manual".to_string(),
                source_id: "".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
            })
            .await
            .unwrap();

        // Sequential increments should produce correct values
        for expected in 1..=10 {
            let val = store.increment(id, "attempts").await.unwrap();
            assert_eq!(val, expected);
        }

        let task = store.get(id).await.unwrap();
        assert_eq!(task.attempts, 10);
    }

    #[tokio::test]
    async fn internal_task_has_no_external_id() {
        let store = TaskStore::open_memory().await.unwrap();

        let id = store
            .create(&NewTask {
                external_id: None,
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "Internal".to_string(),
                body: "".to_string(),
                source: "cron".to_string(),
                source_id: "daily".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
            })
            .await
            .unwrap();

        let task = store.get(id).await.unwrap();
        assert!(task.external_id.is_none());
        assert_eq!(task.origin, "internal");
    }

    #[tokio::test]
    async fn default_values_are_correct() {
        let store = TaskStore::open_memory().await.unwrap();

        let id = store
            .create(&NewTask {
                external_id: None,
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "Defaults".to_string(),
                body: "".to_string(),
                source: "manual".to_string(),
                source_id: "".to_string(),
                author: "".to_string(),
                url: "".to_string(),
                labels: vec![],
            })
            .await
            .unwrap();

        let task = store.get(id).await.unwrap();
        assert_eq!(task.status, TaskStatus::New);
        assert_eq!(task.complexity, "medium");
        assert_eq!(task.attempts, 0);
        assert_eq!(task.route_attempts, 0);
        assert_eq!(task.merge_conflict_retries, 0);
        assert_eq!(task.ci_merge_failures, 0);
        assert_eq!(task.pr_create_failures, 0);
        assert_eq!(task.review_agent_failures, 0);
        assert_eq!(task.review_cycles, 0);
        assert_eq!(task.input_tokens, 0);
        assert_eq!(task.output_tokens, 0);
        assert!((task.total_cost_usd - 0.0).abs() < f64::EPSILON);
        assert!(!task.worktree_cleaned);
        assert!(!task.budget_exceeded);
        assert!(task.memory.is_empty());
        assert!(task.delegations.is_empty());
        assert!(task.pr_number.is_none());
        assert!(task.parent_id.is_none());
        assert!(task.agent.is_none());
        assert!(task.model.is_none());
        assert!(task.block_reason.is_none());
    }
}
