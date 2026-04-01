use super::*;
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
    pub push_failures: i32,
    pub review_agent_failures: i32,
    pub review_cycles: i32,
    pub review_invocations: i32,
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

    // Auto-unblock tracking
    pub auto_unblock_count: i32,
    pub auto_unblock_last_at: String,
    pub auto_unblock_last_reason: String,

    // CI-based auto-recovery tracking (separate from auto_unblock_count)
    pub ci_recovery_count: i32,

    // Done-without-PR circuit breaker
    pub no_code_reroutes: i32,

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

/// A single task lifecycle activity event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskActivity {
    pub id: i64,
    pub task_id: i64,
    pub timestamp: String,
    pub event_type: String,
    pub from_status: Option<String>,
    pub to_status: Option<String>,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub details: serde_json::Value,
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

impl TaskStore {
    /// Append a lifecycle activity event for a task.
    #[allow(clippy::too_many_arguments)]
    pub async fn append_activity(
        &self,
        task_id: i64,
        event_type: &str,
        from_status: Option<&str>,
        to_status: Option<&str>,
        agent: Option<&str>,
        model: Option<&str>,
        details: Option<&serde_json::Value>,
    ) -> anyhow::Result<()> {
        let details_json = match details {
            Some(v) => serde_json::to_string(v)?,
            None => "{}".to_string(),
        };
        sqlx::query(
            "INSERT INTO task_activity
             (task_id, event_type, from_status, to_status, agent, model, details)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(task_id)
        .bind(event_type)
        .bind(from_status)
        .bind(to_status)
        .bind(agent)
        .bind(model)
        .bind(details_json)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Get task activity timeline in chronological order.
    pub async fn get_activity(
        &self,
        task_id: i64,
        limit: Option<usize>,
    ) -> anyhow::Result<Vec<TaskActivity>> {
        let rows = if let Some(limit) = limit {
            let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
            sqlx::query(
                "SELECT * FROM task_activity WHERE task_id = ?
                 ORDER BY timestamp ASC, id ASC LIMIT ?",
            )
            .bind(task_id)
            .bind(limit_i64)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                "SELECT * FROM task_activity WHERE task_id = ?
                 ORDER BY timestamp ASC, id ASC",
            )
            .bind(task_id)
            .fetch_all(&self.pool)
            .await?
        };

        rows.iter().map(Self::row_to_activity).collect()
    }

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
        let previous = sqlx::query("SELECT status, agent, model FROM tasks WHERE id = ?")
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
        let from_status: String = previous.get("status");
        let agent: Option<String> = previous.get("agent");
        let model: Option<String> = previous.get("model");
        let sql = if status == TaskStatus::Blocked {
            "UPDATE tasks SET status = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ?"
        } else if status == TaskStatus::NeedsReview {
            "UPDATE tasks SET status = ?, block_reason = NULL, review_cycles = 0, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ?"
        } else {
            "UPDATE tasks SET status = ?, block_reason = NULL, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ?"
        };
        sqlx::query(sql)
            .bind(status.as_str())
            .bind(id)
            .execute(&self.pool)
            .await?;

        self.append_activity(
            id,
            "status_change",
            Some(from_status.as_str()),
            Some(status.as_str()),
            agent.as_deref(),
            model.as_deref(),
            None,
        )
        .await?;
        Ok(())
    }

    /// Reset a task back to `new`.
    pub async fn reset_to_new(&self, id: i64) -> anyhow::Result<()> {
        let previous = sqlx::query("SELECT status, agent, model FROM tasks WHERE id = ?")
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
        let from_status: String = previous.get("status");
        let agent: Option<String> = previous.get("agent");
        let model: Option<String> = previous.get("model");
        sqlx::query(
            "UPDATE tasks SET status = 'new', branch = '', worktree = '', worktree_cleaned = 0, block_reason = NULL, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ?",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        let details = serde_json::json!({ "op": "reset_to_new" });
        self.append_activity(
            id,
            "status_change",
            Some(from_status.as_str()),
            Some(TaskStatus::New.as_str()),
            agent.as_deref(),
            model.as_deref(),
            Some(&details),
        )
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

    /// List internal tasks with a specific source value (origin = 'internal' AND source = ?).
    /// More efficient than `list_all_internal` + in-memory filter when only a subset is needed.
    pub async fn list_internal_by_source(
        &self,
        repo: &str,
        source: &str,
    ) -> anyhow::Result<Vec<Task>> {
        let rows = sqlx::query(
            "SELECT * FROM tasks WHERE repo = ? AND origin = 'internal' AND source = ? ORDER BY created_at DESC",
        )
        .bind(repo)
        .bind(source)
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

    /// Return the distinct repo slugs present in the tasks table.
    pub async fn distinct_repos(&self) -> anyhow::Result<Vec<String>> {
        let rows =
            sqlx::query("SELECT DISTINCT repo FROM tasks WHERE repo IS NOT NULL AND repo != ''")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows
            .iter()
            .filter_map(|r| r.try_get::<String, _>("repo").ok())
            .collect())
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
            "parent_id",
            "block_reason",
            "pr_number",
            "pr_review_context",
            "last_review_ts",
            "last_comment_review_ts",
            "merge_conflict_retries",
            "ci_merge_failures",
            "pr_create_failures",
            "push_failures",
            "review_agent_failures",
            "review_cycles",
            "review_invocations",
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
            "auto_unblock_count",
            "auto_unblock_last_at",
            "auto_unblock_last_reason",
            "ci_recovery_count",
            "no_code_reroutes",
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
            "push_failures",
            "review_agent_failures",
            "review_cycles",
            "review_invocations",
            "auto_unblock_count",
            "ci_recovery_count",
            "no_code_reroutes",
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
            push_failures = 0,
            ci_merge_failures = 0,
            review_cycles = 0,
            review_invocations = 0,
            review_session_expected = 0,
            auto_unblock_count = 0,
            auto_unblock_last_at = '',
            auto_unblock_last_reason = '',
            ci_recovery_count = 0,
            no_code_reroutes = 0,
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
            review_invocations = 0,
            merge_conflict_retries = 0,
            pr_create_failures = 0,
            push_failures = 0,
            review_session_expected = 0,
            auto_unblock_count = 0,
            auto_unblock_last_at = '',
            auto_unblock_last_reason = '',
            ci_recovery_count = 0,
            no_code_reroutes = 0,
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
        let details = serde_json::json!({
            "complexity": route.complexity,
            "reason": route.reason,
            "skills": route.skills,
        });
        self.append_activity(
            route.id,
            "dispatch",
            None,
            None,
            Some(route.agent),
            route.model,
            Some(&details),
        )
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
        self.append_activity(
            id,
            "branch_delete",
            None,
            None,
            None,
            None,
            Some(&serde_json::json!({ "worktree_cleaned": true })),
        )
        .await?;
        Ok(())
    }

    /// Batch-update fields for multiple tasks in a single transaction.
    ///
    /// Each entry is `(id, updates)` where `updates` is a slice of `(column, value)` pairs.
    /// Skips entries with empty update slices.
    #[allow(dead_code)] // provided for callers that update many tasks at once
    pub async fn batch_set_fields(
        &self,
        updates: &[(i64, &[(&str, serde_json::Value)])],
    ) -> anyhow::Result<()> {
        if updates.is_empty() {
            return Ok(());
        }

        // Allowlist matches set_fields
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
            "push_failures",
            "review_agent_failures",
            "review_cycles",
            "review_invocations",
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
            "auto_unblock_count",
            "auto_unblock_last_at",
            "auto_unblock_last_reason",
            "ci_recovery_count",
            "no_code_reroutes",
        ];

        for (_, entry_updates) in updates {
            for (col, _) in *entry_updates {
                anyhow::ensure!(
                    ALLOWED.contains(col),
                    "column {col} is not in the update allowlist"
                );
            }
        }

        let mut tx = self.pool.begin().await?;
        for (id, entry_updates) in updates {
            if entry_updates.is_empty() {
                continue;
            }
            let mut set_parts = Vec::new();
            let mut values: Vec<Option<String>> = Vec::new();
            for (col, val) in *entry_updates {
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
            query = query.bind(*id);
            query.execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Batch-increment a field for multiple tasks in a single transaction.
    ///
    /// Each entry is `(id, field)`. All fields must be in the incrementable allowlist.
    #[allow(dead_code)] // provided for callers that increment counters for many tasks at once
    pub async fn batch_increment(&self, entries: &[(i64, &str)]) -> anyhow::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }

        const INCREMENTABLE: &[&str] = &[
            "attempts",
            "route_attempts",
            "merge_conflict_retries",
            "ci_merge_failures",
            "pr_create_failures",
            "push_failures",
            "review_agent_failures",
            "review_cycles",
            "review_invocations",
            "auto_unblock_count",
            "ci_recovery_count",
            "no_code_reroutes",
        ];

        for (_, field) in entries {
            anyhow::ensure!(
                INCREMENTABLE.contains(field),
                "field {field} is not incrementable"
            );
        }

        let mut tx = self.pool.begin().await?;
        for (id, field) in entries {
            let sql = format!(
                "UPDATE tasks SET {field} = {field} + 1, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ?"
            );
            sqlx::query(&sql).bind(*id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Reset transient failure/retry counters for multiple tasks in a single transaction.
    ///
    /// Preserves `review_cycles` and `ci_merge_failures` (same semantics as
    /// [`reset_failure_counters`]).
    #[allow(dead_code)] // provided for callers that reset counters for many tasks at once
    pub async fn batch_reset_failure_counters(&self, ids: &[i64]) -> anyhow::Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        // Build WHERE id IN (?, ?, ...) placeholders
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let sql = format!(
            "UPDATE tasks SET
            attempts = 0,
            route_attempts = 0,
            review_agent_failures = 0,
            review_invocations = 0,
            merge_conflict_retries = 0,
            pr_create_failures = 0,
            push_failures = 0,
            review_session_expected = 0,
            auto_unblock_count = 0,
            auto_unblock_last_at = '',
            auto_unblock_last_reason = '',
            ci_recovery_count = 0,
            no_code_reroutes = 0,
            updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
         WHERE id IN ({placeholders})"
        );
        let mut query = sqlx::query(&sql);
        for id in ids {
            query = query.bind(*id);
        }
        query.execute(&self.pool).await?;
        Ok(())
    }

    /// Mark multiple tasks' worktrees as cleaned in a single transaction.
    ///
    /// Appends a `branch_delete` activity event for each task.
    #[allow(dead_code)] // provided for callers that clean many worktrees at once
    pub async fn batch_mark_cleaned(&self, ids: &[i64]) -> anyhow::Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let sql = format!(
            "UPDATE tasks SET worktree_cleaned = 1, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id IN ({placeholders})"
        );
        let mut query = sqlx::query(&sql);
        for id in ids {
            query = query.bind(*id);
        }
        query.execute(&self.pool).await?;
        // Append activity for each task (must be done individually — activity is per-task JSON)
        let details = serde_json::json!({ "worktree_cleaned": true });
        for id in ids {
            self.append_activity(*id, "branch_delete", None, None, None, None, Some(&details))
                .await?;
        }
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

    /// Return external IDs of tasks whose worktrees have already been cleaned.
    ///
    /// Used by `cleanup_done_worktrees` to skip already-cleaned tasks without
    /// issuing one `resolve_task_id + get` per task (N+1 elimination).
    pub async fn cleaned_external_ids(
        &self,
        repo: &str,
    ) -> anyhow::Result<std::collections::HashSet<String>> {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT external_id FROM tasks WHERE repo = ? AND worktree_cleaned = 1")
                .bind(repo)
                .fetch_all(&self.pool)
                .await?;

        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    /// Batch-fetch runs for multiple task IDs in a single query.
    ///
    /// Returns a map of task_id → Vec<TaskRun>. Used by `auto_unblock_blocked_tasks`
    /// to avoid N+1 `get_runs()` calls per blocked task.
    pub async fn get_runs_for_tasks(
        &self,
        task_ids: &[i64],
    ) -> anyhow::Result<std::collections::HashMap<i64, Vec<TaskRun>>> {
        if task_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        // Build a comma-separated list of placeholders for the IN clause.
        let placeholders: String = task_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let query = format!(
            "SELECT * FROM task_runs WHERE task_id IN ({}) ORDER BY attempt ASC, run_type ASC",
            placeholders
        );

        let mut q = sqlx::query(&query);
        for id in task_ids {
            q = q.bind(id);
        }

        let rows = q.fetch_all(&self.pool).await?;

        let mut map: std::collections::HashMap<i64, Vec<TaskRun>> =
            std::collections::HashMap::new();
        for row in &rows {
            let run = Self::row_to_run(row)?;
            map.entry(run.task_id).or_default().push(run);
        }

        Ok(map)
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

        let run_id = row.get("id");
        let event_type = match run.run_type {
            "review" => "review_start",
            _ => "dispatch",
        };
        let details = serde_json::json!({
            "run_type": run.run_type,
            "attempt": run.attempt,
            "command": run.command,
        });
        self.append_activity(
            run.task_id,
            event_type,
            None,
            None,
            Some(run.agent),
            Some(run.model),
            Some(&details),
        )
        .await?;

        Ok(run_id)
    }

    /// Complete a run with results.
    pub async fn complete_run(&self, run: &CompleteRun<'_>) -> anyhow::Result<()> {
        let run_ctx =
            sqlx::query("SELECT task_id, run_type, agent, model FROM task_runs WHERE id = ?")
                .bind(run.run_id)
                .fetch_optional(&self.pool)
                .await?;
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
        if let Some(run_ctx) = run_ctx {
            let task_id: i64 = run_ctx.get("task_id");
            let run_type: String = run_ctx.get("run_type");
            let agent: String = run_ctx.get("agent");
            let model: String = run_ctx.get("model");
            if run.outcome == "timeout" {
                self.append_activity(
                    task_id,
                    "timeout",
                    None,
                    None,
                    Some(&agent),
                    Some(&model),
                    Some(&serde_json::json!({
                        "run_type": run_type,
                        "error": run.error,
                        "stderr": run.stderr,
                    })),
                )
                .await?;
            } else if run.outcome != "success" {
                self.append_activity(
                    task_id,
                    "error",
                    None,
                    None,
                    Some(&agent),
                    Some(&model),
                    Some(&serde_json::json!({
                        "run_type": run_type,
                        "outcome": run.outcome,
                        "error": run.error,
                    })),
                )
                .await?;
            }
        }
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

    /// Get all runs for a batch of task IDs in a single query, grouped by task ID.
    /// Returns an empty map if `task_ids` is empty.
    pub async fn get_runs_batch(
        &self,
        task_ids: &[i64],
    ) -> anyhow::Result<std::collections::HashMap<i64, Vec<TaskRun>>> {
        if task_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let placeholders = task_ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let sql = format!(
            "SELECT * FROM task_runs WHERE task_id IN ({placeholders}) ORDER BY task_id ASC, attempt ASC, run_type ASC"
        );
        let mut query = sqlx::query(&sql);
        for id in task_ids {
            query = query.bind(*id);
        }
        let rows = query.fetch_all(&self.pool).await?;
        let mut result: std::collections::HashMap<i64, Vec<TaskRun>> =
            std::collections::HashMap::new();
        for row in &rows {
            let run = Self::row_to_run(row)?;
            result.entry(run.task_id).or_default().push(run);
        }
        Ok(result)
    }

    /// Get multiple tasks by ID in a single query, returning a map from task ID to Task.
    /// Returns an empty map if `ids` is empty.
    pub async fn get_batch(
        &self,
        ids: &[i64],
    ) -> anyhow::Result<std::collections::HashMap<i64, Task>> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let sql = format!("SELECT * FROM tasks WHERE id IN ({placeholders})");
        let mut query = sqlx::query(&sql);
        for id in ids {
            query = query.bind(*id);
        }
        let rows = query.fetch_all(&self.pool).await?;
        let mut result = std::collections::HashMap::new();
        for row in &rows {
            let task = Self::row_to_task(row)?;
            result.insert(task.id, task);
        }
        Ok(result)
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

    /// List distinct agents that have previously reviewed this task.
    /// Used to exclude them when picking the next review agent, so a different
    /// agent reviews each cycle (avoids the same agent hitting the same issues).
    pub async fn previous_review_agents(&self, task_id: i64) -> anyhow::Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT agent FROM task_runs \
             WHERE task_id = ? AND run_type = 'review' \
             ORDER BY agent",
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(a,)| a).collect())
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
            parent_id: row.get("parent_id"),
            block_reason: row.get("block_reason"),
            pr_number: row.get("pr_number"),
            pr_review_context: row.get("pr_review_context"),
            last_review_ts: row.get("last_review_ts"),
            last_comment_review_ts: row.get("last_comment_review_ts"),
            merge_conflict_retries: row.get("merge_conflict_retries"),
            ci_merge_failures: row.get("ci_merge_failures"),
            pr_create_failures: row.get("pr_create_failures"),
            push_failures: row.get("push_failures"),
            review_agent_failures: row.get("review_agent_failures"),
            review_cycles: row.get("review_cycles"),
            review_invocations: row.get("review_invocations"),
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
            // Some deployments may be running databases that haven't had
            // the most recent migrations applied. Use `try_get` with
            // sensible defaults for recently-added columns so a missing
            // column doesn't cause a panic in the sqlx row accessor.
            auto_unblock_count: row.try_get::<i32, _>("auto_unblock_count").unwrap_or(0),
            auto_unblock_last_at: row
                .try_get::<String, _>("auto_unblock_last_at")
                .unwrap_or_default(),
            auto_unblock_last_reason: row
                .try_get::<String, _>("auto_unblock_last_reason")
                .unwrap_or_default(),
            ci_recovery_count: row.try_get::<i32, _>("ci_recovery_count").unwrap_or(0),
            no_code_reroutes: row.try_get::<i32, _>("no_code_reroutes").unwrap_or(0),
            created_at: row.try_get::<String, _>("created_at").unwrap_or_default(),
            updated_at: row.try_get::<String, _>("updated_at").unwrap_or_default(),
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

    fn row_to_activity(row: &sqlx::sqlite::SqliteRow) -> anyhow::Result<TaskActivity> {
        let details_str: String = row.get("details");
        Ok(TaskActivity {
            id: row.get("id"),
            task_id: row.get("task_id"),
            timestamp: row.get("timestamp"),
            event_type: row.get("event_type"),
            from_status: row.get("from_status"),
            to_status: row.get("to_status"),
            agent: row.get("agent"),
            model: row.get("model"),
            details: serde_json::from_str(&details_str).unwrap_or_else(|_| serde_json::json!({})),
        })
    }
}
