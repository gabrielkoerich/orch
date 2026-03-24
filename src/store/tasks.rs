use super::*;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::Row;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    New,
    Routed,
    InProgress,
    Done,
    Blocked,
    InReview,
    NeedsReview,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Routed => "routed",
            Self::InProgress => "in_progress",
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::InReview => "in_review",
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
            "in_review" => Some(Self::InReview),
            "needs_review" => Some(Self::NeedsReview),
            _ => None,
        }
    }
}

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
    pub last_response: String,
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
    pub review_session_expected: bool,

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
#[derive(Debug, Clone, Default)]
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

/// Result of a sidecar migration.
#[derive(Debug, Default)]
pub struct MigrateResult {
    pub migrated: usize,
    pub skipped: usize,
    pub errors: usize,
}

impl TaskStore {
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

    /// Create an internal task, returning its auto-generated ID.
    ///
    /// Convenience wrapper around `create()` that sets origin = "internal"
    /// and generates `external_id = "internal:{id}"` after creation.
    pub async fn create_internal(
        &self,
        repo: &str,
        title: &str,
        body: &str,
        source: &str,
        source_id: &str,
    ) -> anyhow::Result<i64> {
        let id = self
            .create(&NewTask {
                external_id: None,
                repo: repo.to_string(),
                origin: "internal".to_string(),
                title: title.to_string(),
                body: body.to_string(),
                source: source.to_string(),
                source_id: source_id.to_string(),
                author: String::new(),
                url: String::new(),
                labels: vec![],
            })
            .await?;
        // Set external_id to "internal:{id}" so resolve_task_id can find it
        sqlx::query("UPDATE tasks SET external_id = ? WHERE id = ?")
            .bind(format!("internal:{id}"))
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(id)
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
            url = excluded.url
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
        let sql = if status == TaskStatus::Blocked {
            "UPDATE tasks SET status = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ?"
        } else {
            "UPDATE tasks SET status = ?, block_reason = NULL, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ?"
        };
        sqlx::query(sql)
            .bind(status.as_str())
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Update the block reason for a task.
    pub async fn set_block_reason(&self, id: i64, reason: Option<&str>) -> anyhow::Result<()> {
        let value = reason
            .map(|r| serde_json::Value::String(r.to_string()))
            .unwrap_or(serde_json::Value::Null);
        self.set_fields(id, &[("block_reason", value)]).await
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

    /// List external tasks by status within a repo (origin != 'internal').
    pub async fn list_external_by_status(
        &self,
        repo: &str,
        status: TaskStatus,
    ) -> anyhow::Result<Vec<Task>> {
        let rows = sqlx::query(
            "SELECT * FROM tasks WHERE repo = ? AND origin != 'internal' AND status = ? ORDER BY created_at DESC",
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

    /// List internal tasks by status within a repo (origin = 'internal').
    pub async fn list_internal_by_status(
        &self,
        repo: &str,
        status: TaskStatus,
    ) -> anyhow::Result<Vec<Task>> {
        let rows = sqlx::query(
        "SELECT * FROM tasks WHERE repo = ? AND origin = 'internal' AND status = ? ORDER BY created_at DESC",
    )
    .bind(repo)
    .bind(status.as_str())
    .fetch_all(&self.pool)
    .await?;

        rows.iter().map(Self::row_to_task).collect()
    }

    /// List all internal tasks for a repo (origin = 'internal').
    pub async fn list_all_internal(&self, repo: &str) -> anyhow::Result<Vec<Task>> {
        let rows = sqlx::query(
            "SELECT * FROM tasks WHERE repo = ? AND origin = 'internal' ORDER BY created_at DESC",
        )
        .bind(repo)
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(Self::row_to_task).collect()
    }

    /// List all external tasks for a repo (origin != 'internal').
    pub async fn list_all_external(&self, repo: &str) -> anyhow::Result<Vec<Task>> {
        let rows = sqlx::query(
            "SELECT * FROM tasks WHERE repo = ? AND origin != 'internal' ORDER BY created_at DESC",
        )
        .bind(repo)
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(Self::row_to_task).collect()
    }

    /// Check whether the store has any external tasks for a repo.
    pub async fn has_external_tasks(&self, repo: &str) -> bool {
        sqlx::query_scalar::<_, i32>(
            "SELECT 1 FROM tasks WHERE repo = ? AND origin != 'internal' LIMIT 1",
        )
        .bind(repo)
        .fetch_optional(&self.pool)
        .await
        .ok()
        .flatten()
        .is_some()
    }

    /// List all tasks for a repo, ordered by creation time descending.
    pub async fn list_all(&self, repo: &str) -> anyhow::Result<Vec<Task>> {
        let rows = sqlx::query("SELECT * FROM tasks WHERE repo = ? ORDER BY created_at DESC")
            .bind(repo)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(Self::row_to_task).collect()
    }

    /// List all active (non-done) tasks across all repos.
    ///
    /// Used by the CLI when no project context is available (e.g. running from
    /// a worktree without a `.orch.yml`). Does not require a repo argument.
    pub async fn list_all_active_global(&self) -> anyhow::Result<Vec<Task>> {
        let rows =
            sqlx::query("SELECT * FROM tasks WHERE status != 'done' ORDER BY created_at DESC")
                .fetch_all(&self.pool)
                .await?;
        rows.iter().map(Self::row_to_task).collect()
    }

    /// List all active tasks matching an optional status filter, across all repos.
    ///
    /// Used by the CLI fallback path when no project context is available.
    pub async fn list_all_by_status_global(&self, status: TaskStatus) -> anyhow::Result<Vec<Task>> {
        let rows = sqlx::query("SELECT * FROM tasks WHERE status = ? ORDER BY created_at DESC")
            .bind(status.as_str())
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(Self::row_to_task).collect()
    }

    /// Aggregate cost and token data for a repo.
    /// Returns (total_input_tokens, total_output_tokens, total_cost_usd).
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
            "last_response",
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
            "review_session_expected",
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
        let mut values: Vec<Option<String>> = Vec::new();

        for (col, val) in updates {
            set_parts.push(format!("{col} = ?"));
            match val {
                serde_json::Value::String(s) => values.push(Some(s.clone())),
                serde_json::Value::Number(n) => values.push(Some(n.to_string())),
                serde_json::Value::Bool(b) => {
                    values.push(Some(if *b { "1" } else { "0" }.to_string()));
                }
                serde_json::Value::Null => values.push(None),
                other => values.push(Some(serde_json::to_string(other)?)),
            }
        }

        set_parts.push("updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')".to_string());
        let sql = format!("UPDATE tasks SET {} WHERE id = ?", set_parts.join(", "));

        let mut query = sqlx::query(&sql);
        for v in &values {
            query = query.bind(v.as_deref());
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
        "UPDATE tasks SET {field} = {field} + 1, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ? RETURNING {field} AS val"
    );

        let row = sqlx::query(&sql).bind(id).fetch_one(&self.pool).await?;

        Ok(row.get("val"))
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
            review_session_expected = 0,
            updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
         WHERE id = ?",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Reset transient failure/retry counters to zero, preserving `review_cycles` and `ci_merge_failures`.
    ///
    /// Use this after `RequestChanges` to clear per-attempt noise without
    /// undoing the review-cycle count that `handle_review_changes` just set.
    /// `ci_merge_failures` is preserved here because it must accumulate across attempts
    /// to enforce `MAX_CI_MERGE_FAILURES`, just like `review_cycles`.
    pub async fn reset_failure_counters(&self, id: i64) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE tasks SET
            attempts = 0,
            route_attempts = 0,
            review_agent_failures = 0,
            merge_conflict_retries = 0,
            pr_create_failures = 0,
            review_session_expected = 0,
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
    ///
    /// Uses a transaction to prevent lost-update races when concurrent
    /// callers append simultaneously.
    pub async fn append_memory(&self, id: i64, entry: &MemoryEntry) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;

        let row = sqlx::query("SELECT memory FROM tasks WHERE id = ?")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;

        let memory_str: String = row.get("memory");
        let mut memory: Vec<MemoryEntry> = serde_json::from_str(&memory_str)
            .inspect_err(
                |e| tracing::warn!(task_id = id, error = %e, "corrupt memory JSON, resetting"),
            )
            .unwrap_or_default();
        memory.push(entry.clone());
        let new_json = serde_json::to_string(&memory)?;

        sqlx::query(
        "UPDATE tasks SET memory = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ?",
    )
    .bind(&new_json)
    .bind(id)
    .execute(&mut *tx)
    .await?;

        tx.commit().await?;
        Ok(())
    }

    /// Get the N most recent memory entries for a task.
    pub async fn recent_memory(&self, id: i64, max: usize) -> anyhow::Result<Vec<MemoryEntry>> {
        let row = sqlx::query("SELECT memory FROM tasks WHERE id = ?")
            .bind(id)
            .fetch_one(&self.pool)
            .await?;

        let memory_str: String = row.get("memory");
        let mut memory: Vec<MemoryEntry> = serde_json::from_str(&memory_str)
            .inspect_err(|e| tracing::warn!(task_id = id, error = %e, "corrupt memory JSON"))
            .unwrap_or_default();

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
        let pricing = pricing_for_model(model);
        let usage = TokenUsage {
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
    #[allow(dead_code)]
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
    #[allow(dead_code)]
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
    #[allow(dead_code)]
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
            status: TaskStatus::from_str(&status_str).unwrap_or_else(|| {
                tracing::warn!(
                    status = status_str,
                    "unknown task status, defaulting to New"
                );
                TaskStatus::New
            }),
            source: row.get("source"),
            source_id: row.get("source_id"),
            author: row.get("author"),
            url: row.get("url"),
            labels: serde_json::from_str(&labels_str)
                .inspect_err(
                    |e| tracing::warn!(error = %e, "corrupt labels JSON, defaulting to empty"),
                )
                .unwrap_or_default(),
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
            last_response: row.get("last_response"),
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
            review_session_expected: row.get::<i32, _>("review_session_expected") != 0,
            input_tokens: row.get("input_tokens"),
            output_tokens: row.get("output_tokens"),
            input_cost_usd: row.get("input_cost_usd"),
            output_cost_usd: row.get("output_cost_usd"),
            total_cost_usd: row.get("total_cost_usd"),
            model_reroute_chain: row.get("model_reroute_chain"),
            limit_reroute_chain: row.get("limit_reroute_chain"),
            budget_warning: row.get("budget_warning"),
            budget_exceeded: row.get::<i32, _>("budget_exceeded") != 0,
            memory: serde_json::from_str(&memory_str)
                .inspect_err(
                    |e| tracing::warn!(error = %e, "corrupt memory JSON, defaulting to empty"),
                )
                .unwrap_or_default(),
            delegations: serde_json::from_str(&delegations_str)
                .inspect_err(
                    |e| tracing::warn!(error = %e, "corrupt delegations JSON, defaulting to empty"),
                )
                .unwrap_or_default(),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        })
    }
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

    /// Sync fields from a sidecar JSON object to the store.
    ///
    /// Used by migration: reads key fields from the parsed JSON and writes them
    /// to the tasks table. Silently ignores errors (best-effort).
    async fn sync_sidecar_json_to_store(&self, store_id: i64, json: &serde_json::Value) {
        let mut updates: Vec<(&str, serde_json::Value)> = Vec::new();

        // Helper: extract string field
        let get_str = |field: &str| -> Option<String> {
            json.get(field).and_then(|v| match v {
                serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
                serde_json::Value::Number(n) => Some(n.to_string()),
                serde_json::Value::Bool(b) => Some(b.to_string()),
                _ => None,
            })
        };
        let get_u64 = |field: &str| -> u64 {
            get_str(field)
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0)
        };
        let get_f64 = |field: &str| -> f64 {
            get_str(field)
                .and_then(|s| s.trim().parse::<f64>().ok())
                .unwrap_or(0.0)
        };

        // Routing fields
        for field in ["agent", "model", "complexity", "route_reason"] {
            if let Some(v) = get_str(field) {
                updates.push((field, serde_json::json!(v)));
            }
        }

        // Execution fields
        for field in ["branch", "worktree", "summary", "last_error"] {
            if let Some(v) = get_str(field) {
                updates.push((field, serde_json::json!(v)));
            }
        }

        // Counters
        for field in ["attempts", "route_attempts"] {
            let val = get_u64(field);
            if val > 0 {
                updates.push((field, serde_json::json!(val)));
            }
        }

        // PR fields
        let pr_number = get_u64("pr_number");
        if pr_number > 0 {
            updates.push(("pr_number", serde_json::json!(pr_number)));
        }

        // Tokens & cost
        let input_tokens = get_u64("input_tokens");
        let output_tokens = get_u64("output_tokens");
        if input_tokens > 0 || output_tokens > 0 {
            updates.push(("input_tokens", serde_json::json!(input_tokens)));
            updates.push(("output_tokens", serde_json::json!(output_tokens)));
            updates.push((
                "input_cost_usd",
                serde_json::json!(get_f64("input_cost_usd")),
            ));
            updates.push((
                "output_cost_usd",
                serde_json::json!(get_f64("output_cost_usd")),
            ));
            updates.push((
                "total_cost_usd",
                serde_json::json!(get_f64("total_cost_usd")),
            ));
        }

        // Review counters
        for field in [
            "review_cycles",
            "review_agent_failures",
            "merge_conflict_retries",
        ] {
            let val = get_u64(field);
            if val > 0 {
                updates.push((field, serde_json::json!(val)));
            }
        }

        if !updates.is_empty() {
            if let Err(e) = self.set_fields(store_id, &updates).await {
                tracing::warn!(store_id, error = %e, "migration: failed to sync sidecar → store");
            }
        }
    }

    /// Resolve a task_id (e.g. "42" or "internal:3") to a store internal ID.
    /// Returns None if the task is not in the store yet.
    pub async fn resolve_task_id(&self, repo: &str, task_id: &str) -> anyhow::Result<Option<i64>> {
        if let Some(suffix) = task_id.strip_prefix("internal:") {
            // Internal tasks: look up by external_id="internal:{n}" with origin='internal'
            if let Some(task) = self.get_by_external_id(repo, task_id).await? {
                return Ok(Some(task.id));
            }
            // Fallback: try the numeric suffix as a direct store ID.
            // Must include the repo filter to avoid resolving a task that
            // belongs to a different repository in a multi-repo setup.
            if let Ok(id) = suffix.parse::<i64>() {
                let exists = sqlx::query("SELECT id FROM tasks WHERE id = ? AND repo = ?")
                    .bind(id)
                    .bind(repo)
                    .fetch_optional(&self.pool)
                    .await?;
                if exists.is_some() {
                    return Ok(Some(id));
                }
            }
            return Ok(None);
        }
        // External tasks: look up by external_id
        match self.get_by_external_id(repo, task_id).await? {
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

    /// Migrate old data into the unified tasks table.
    ///
    /// Reads from the legacy rusqlite tables (internal_tasks, kv, task_metrics,
    /// rate_limits) which share the same DB file, plus walks sidecar JSON files.
    /// All reads use sqlx raw queries — no rusqlite dependency needed.
    pub async fn migrate_sidecars(&self, default_repo: &str) -> anyhow::Result<MigrateResult> {
        let mut result = MigrateResult::default();

        // Idempotency guard: skip if migration was already completed.
        // Prevents duplicate task_metrics and rate_limits rows on repeated runs.
        if let Ok(Some(_)) = self.kv_get("migration_completed").await {
            tracing::info!("migration already completed, skipping");
            return Ok(result);
        }

        // 1. Migrate internal tasks from old SQLite table (if it exists)
        let old_tasks: Vec<_> = sqlx::query(
            "SELECT id, title, body, status, source, source_id FROM internal_tasks ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();

        for row in &old_tasks {
            if default_repo.is_empty() {
                result.skipped += 1;
                continue;
            }
            let old_id: i64 = row.get("id");
            let title: String = row.get("title");
            let body: String = row.get("body");
            let status_str: String = row.get("status");
            let source: String = row.get("source");
            let source_id: String = row.get("source_id");
            let status = TaskStatus::from_str(&status_str).unwrap_or(TaskStatus::New);

            match self
                .create(&NewTask {
                    external_id: None,
                    repo: default_repo.to_string(),
                    origin: "internal".to_string(),
                    title: title.clone(),
                    body: body.clone(),
                    source: source.clone(),
                    source_id: source_id.clone(),
                    author: "".to_string(),
                    url: "".to_string(),
                    labels: vec![],
                })
                .await
            {
                Ok(id) => {
                    let _ = self.update_status(id, status).await;
                    let sidecar_key = format!("internal:{}", old_id);
                    if let Ok(task_dir) = crate::home::task_dir(default_repo, &sidecar_key) {
                        let sidecar_file = task_dir.join("sidecar.json");
                        if let Ok(content) = std::fs::read_to_string(&sidecar_file) {
                            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                                self.sync_sidecar_json_to_store(id, &json).await;
                            }
                        }
                    }
                    result.migrated += 1;
                }
                Err(e) => {
                    tracing::warn!(task_id = old_id, err = %e, "failed to migrate internal task");
                    result.errors += 1;
                }
            }
        }

        // 2. Migrate KV store entries from old kv table
        // The old table uses (key, value) columns — same schema as our sqlx kv table,
        // but we read from the old table and write via our kv_set() which upserts.
        let old_kv: Vec<_> = sqlx::query("SELECT key, value FROM kv")
            .fetch_all(&self.pool)
            .await
            .unwrap_or_default();

        for row in &old_kv {
            let key: String = row.get("key");
            let value: String = row.get("value");
            if let Err(e) = self.kv_set(&key, &value).await {
                tracing::warn!(key, err = %e, "failed to migrate kv entry");
                result.errors += 1;
            } else {
                result.migrated += 1;
            }
        }

        // 3. Migrate task_metrics from old table
        let old_metrics: Vec<_> = sqlx::query(
            "SELECT task_id, agent, model, complexity, outcome, duration_seconds,
                started_at, completed_at, attempts, files_changed, error_type,
                input_tokens, output_tokens, input_cost_usd, output_cost_usd, total_cost_usd
         FROM task_metrics",
        )
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();

        for row in &old_metrics {
            let started_str: String = row.get("started_at");
            let completed_str: String = row.get("completed_at");
            let task_id: String = row.get("task_id");
            let agent: String = row.get("agent");
            let model: Option<String> = row.get("model");
            let complexity: Option<String> = row.get("complexity");
            let outcome: String = row.get("outcome");
            let error_type: Option<String> = row.get("error_type");

            let started_at = chrono::DateTime::parse_from_rfc3339(&started_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            let completed_at = chrono::DateTime::parse_from_rfc3339(&completed_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());

            let metric = InsertTaskMetric {
                repo: "",
                task_id: &task_id,
                agent: &agent,
                model: model.as_deref(),
                complexity: complexity.as_deref(),
                outcome: &outcome,
                duration_seconds: row.get("duration_seconds"),
                started_at: &started_at,
                completed_at: &completed_at,
                attempts: row.get("attempts"),
                files_changed: row.get("files_changed"),
                error_type: error_type.as_deref(),
                input_tokens: row.get("input_tokens"),
                output_tokens: row.get("output_tokens"),
                input_cost_usd: row.get("input_cost_usd"),
                output_cost_usd: row.get("output_cost_usd"),
                total_cost_usd: row.get("total_cost_usd"),
            };
            if let Err(e) = self.insert_task_metric(&metric).await {
                tracing::warn!(task_id, err = %e, "failed to migrate task metric");
                result.errors += 1;
            } else {
                result.migrated += 1;
            }
        }

        // 4. Migrate rate_limits from old table
        let old_rates: Vec<_> =
            sqlx::query("SELECT agent, limit_type, occurred_at, task_id FROM rate_limits")
                .fetch_all(&self.pool)
                .await
                .unwrap_or_default();

        for row in &old_rates {
            let agent: String = row.get("agent");
            let limit_type: String = row.get("limit_type");
            let occurred_at: String = row.get("occurred_at");
            let task_id: Option<String> = row.get("task_id");

            let res = sqlx::query(
            "INSERT INTO rate_limits (agent, limit_type, occurred_at, task_id) VALUES (?, ?, ?, ?)",
        )
        .bind(&agent)
        .bind(&limit_type)
        .bind(&occurred_at)
        .bind(&task_id)
        .execute(&self.pool)
        .await;

            match res {
                Ok(_) => result.migrated += 1,
                Err(e) => {
                    tracing::warn!(agent, err = %e, "failed to migrate rate limit");
                    result.errors += 1;
                }
            }
        }

        // 5. Walk sidecar files for external tasks
        let state_dir = match crate::home::orch_home() {
            Ok(h) => h.join("state"),
            Err(_) => return Ok(result),
        };

        if !state_dir.exists() {
            return Ok(result);
        }

        // Walk: state/{owner}/{repo}/tasks/{id}/sidecar.json
        for owner_entry in std::fs::read_dir(&state_dir).into_iter().flatten() {
            let owner_entry = match owner_entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if !owner_entry.path().is_dir() {
                continue;
            }
            let owner = owner_entry.file_name().to_string_lossy().to_string();
            if owner.starts_with('.') || owner == "orch.log" {
                continue;
            }

            for repo_entry in std::fs::read_dir(owner_entry.path()).into_iter().flatten() {
                let repo_entry = match repo_entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                if !repo_entry.path().is_dir() {
                    continue;
                }
                let repo_name = repo_entry.file_name().to_string_lossy().to_string();
                let repo_slug = format!("{}/{}", owner, repo_name);

                let tasks_dir = repo_entry.path().join("tasks");
                if !tasks_dir.exists() {
                    continue;
                }

                for task_entry in std::fs::read_dir(&tasks_dir).into_iter().flatten() {
                    let task_entry = match task_entry {
                        Ok(e) => e,
                        Err(_) => continue,
                    };
                    let task_id = task_entry.file_name().to_string_lossy().to_string();
                    let sidecar_file = task_entry.path().join("sidecar.json");
                    if !sidecar_file.exists() {
                        continue;
                    }

                    // Read sidecar JSON
                    let content = match std::fs::read_to_string(&sidecar_file) {
                        Ok(c) => c,
                        Err(_) => {
                            result.errors += 1;
                            continue;
                        }
                    };
                    let json: serde_json::Value = match serde_json::from_str(&content) {
                        Ok(v) => v,
                        Err(_) => {
                            result.errors += 1;
                            continue;
                        }
                    };

                    // Upsert into store
                    let title = json
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&format!("Task #{}", task_id))
                        .to_string();
                    let body = json
                        .get("body")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    match self
                        .upsert_external(&UpsertExternal {
                            repo: &repo_slug,
                            ext_id: &task_id,
                            title: &title,
                            body: &body,
                            author: "",
                            url: "",
                            labels: &[],
                            origin: "github",
                        })
                        .await
                    {
                        Ok(id) => {
                            self.sync_sidecar_json_to_store(id, &json).await;
                            result.migrated += 1;
                        }
                        Err(e) => {
                            tracing::warn!(task_id, repo = repo_slug, err = %e, "failed to migrate sidecar");
                            result.errors += 1;
                        }
                    }
                }
            }
        }

        // Mark migration as completed to prevent duplicate runs
        let _ = self.kv_set("migration_completed", "1").await;

        Ok(result)
    }
}
