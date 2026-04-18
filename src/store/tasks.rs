use super::*;
use serde::{Deserialize, Serialize};
use sqlx::Row;

/// Explicit column list for `SELECT` queries on the `tasks` table.
///
/// Using `SELECT *` with `sqlx-sqlite`'s persistent prepared statements can
/// cause OOB panics when the schema changes (new migration columns) because
/// `sqlite3_column_count()` returns the updated count but the cached column
/// metadata vector was built at prepare time. Explicit columns prevent the
/// mismatch.
pub(crate) const TASK_COLS: &str = "id, external_id, repo, origin, title, body, status, \
    source, source_id, author, url, labels, agent, model, complexity, estimate, \
    route_reason, agent_profile, selected_skills, route_attempts, attempts, \
    branch, worktree, worktree_cleaned, summary, last_error, parent_id, \
    block_reason, pr_number, pr_review_context, last_review_ts, review_ts_map, \
    last_comment_review_ts, merge_conflict_retries, ci_merge_failures, \
    pr_create_failures, review_agent_failures, review_cycles, input_tokens, \
    output_tokens, input_cost_usd, output_cost_usd, total_cost_usd, \
    model_reroute_chain, limit_reroute_chain, budget_warning, budget_exceeded, \
    memory, delegations, created_at, updated_at, review_session_expected, \
     review_invocations, needs_review_refires, push_failures, auto_unblock_count, auto_unblock_last_at, \
     ci_recovery_count, auto_unblock_last_reason, no_code_reroutes, network_retries, no_code_last_agent";

/// Number of columns in TASK_COLS (used for diagnostic verification).
pub(crate) const TASK_COLS_COUNT: usize = 62;

/// Explicit column list for `SELECT` queries on the `task_runs` table.
const TASK_RUN_COLS: &str =
    "id, task_id, attempt, run_type, agent, model, command, prompt, env_vars, \
    exit_code, stdout, stderr, parsed_response, outcome, error, input_tokens, output_tokens, \
    total_cost_usd, duration_secs, started_at, completed_at";

/// Explicit column list for `SELECT` queries on the `task_activity` table.
const TASK_ACTIVITY_COLS: &str =
    "id, task_id, timestamp, event_type, from_status, to_status, agent, model, details";

/// Allowlist of columns that can be updated via `set_fields` and `batch_set_fields`.
const ALLOWED_FIELDS: &[&str] = &[
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
    "review_ts_map",
    "last_comment_review_ts",
    "merge_conflict_retries",
    "ci_merge_failures",
    "pr_create_failures",
    "push_failures",
    "review_agent_failures",
    "review_cycles",
    "review_invocations",
    "needs_review_refires",
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
    "network_retries",
    "no_code_last_agent",
];

/// Allowlist of fields that can be incremented via `increment` and `batch_increment`.
const INCREMENTABLE_FIELDS: &[&str] = &[
    "attempts",
    "route_attempts",
    "merge_conflict_retries",
    "ci_merge_failures",
    "pr_create_failures",
    "push_failures",
    "review_agent_failures",
    "review_cycles",
    "review_invocations",
    "needs_review_refires",
    "auto_unblock_count",
    "ci_recovery_count",
    "no_code_reroutes",
    "network_retries",
];

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
    /// Fibonacci effort estimate (1, 2, 3, 5, 8, 13, or 21). 0 means not provided.
    pub estimate: u8,
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
    pub review_ts_map: String,
    pub last_comment_review_ts: String,
    pub merge_conflict_retries: i32,
    pub ci_merge_failures: i32,
    pub pr_create_failures: i32,
    pub push_failures: i32,
    pub network_retries: i32,
    pub review_agent_failures: i32,
    pub review_cycles: i32,
    pub review_invocations: i32,
    pub review_session_expected: bool,
    // NeedsReview re-fire counter (incremented by sync catch-up and reset on successful review cycles)
    pub needs_review_refires: i32,

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
    /// Which agent produced the last no-code failure (used to detect same-agent loops).
    pub no_code_last_agent: String,

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
    /// Optional link to a parent task (e.g. the issue/PR a mention was posted on).
    pub parent_id: Option<i64>,
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
    pub input_tokens: u64,
    pub output_tokens: u64,
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
    /// Fibonacci effort estimate (1, 2, 3, 5, 8, 13, or 21). 0 means not provided.
    pub estimate: u8,
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
            sqlx::query(&format!(
                "SELECT {TASK_ACTIVITY_COLS} FROM task_activity WHERE task_id = ? ORDER BY timestamp ASC, id ASC LIMIT ?"
            ))
            .bind(task_id)
            .bind(limit_i64)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(&format!(
                "SELECT {TASK_ACTIVITY_COLS} FROM task_activity WHERE task_id = ? ORDER BY timestamp ASC, id ASC"
            ))
            .bind(task_id)
            .fetch_all(&self.pool)
            .await?
        };

        rows.iter().map(Self::row_to_activity).collect()
    }

    pub async fn create(&self, new: &NewTask) -> anyhow::Result<i64> {
        let labels_json = serde_json::to_string(&new.labels)?;
        let row = sqlx::query(
        "INSERT INTO tasks (external_id, repo, origin, title, body, source, source_id, author, url, labels, parent_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
    .bind(new.parent_id)
    .fetch_one(&self.pool)
    .await?;

        row.try_get("id")
            .map_err(|e| anyhow::anyhow!("INSERT RETURNING id returned NULL or invalid: {e}"))
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
        parent_id: Option<i64>,
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
                parent_id,
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
        let row = sqlx::query(&format!("SELECT {TASK_COLS} FROM tasks WHERE id = ?"))
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
        let row = sqlx::query(&format!(
            "SELECT {TASK_COLS} FROM tasks WHERE repo = ? AND external_id = ?"
        ))
        .bind(repo)
        .bind(ext_id)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some(r) => Ok(Some(Self::row_to_task(&r)?)),
            None => Ok(None),
        }
    }

    /// Lightweight lookup: return only the numeric `id` for a given repo + external_id.
    ///
    /// Many call sites only need the internal store ID and were previously calling
    /// `get_by_external_id()` which deserializes the full 60-column `Task` row. This
    /// method avoids that work by selecting only `id`.
    pub async fn resolve_id_by_external(
        &self,
        repo: &str,
        ext_id: &str,
    ) -> anyhow::Result<Option<i64>> {
        let row = sqlx::query("SELECT id FROM tasks WHERE repo = ? AND external_id = ?")
            .bind(repo)
            .bind(ext_id)
            .fetch_optional(&self.pool)
            .await?;
        let id = row.map(|r| r.try_get::<i64, _>("id")).transpose()?;
        Ok(id)
    }

    /// Resolve the parent task ID by PR number.
    ///
    /// When a mention arrives on a PR, we need to find the issue task that owns
    /// this PR. The task store tracks `pr_number` on tasks, so we look up the
    /// task whose `pr_number` matches the given PR number.
    pub async fn resolve_id_by_pr_number(
        &self,
        repo: &str,
        pr_number: i32,
    ) -> anyhow::Result<Option<i64>> {
        let row = sqlx::query("SELECT id FROM tasks WHERE repo = ? AND pr_number = ? LIMIT 1")
            .bind(repo)
            .bind(pr_number)
            .fetch_optional(&self.pool)
            .await?;
        let id = row.map(|r| r.try_get::<i64, _>("id")).transpose()?;
        Ok(id)
    }

    /// Lightweight lookup: return only the `agent` column for a given repo + external_id.
    /// Returns None when no row matches or the agent is NULL/empty.
    pub async fn get_agent_by_external_id(
        &self,
        repo: &str,
        ext_id: &str,
    ) -> anyhow::Result<Option<String>> {
        let row = sqlx::query("SELECT agent FROM tasks WHERE repo = ? AND external_id = ?")
            .bind(repo)
            .bind(ext_id)
            .fetch_optional(&self.pool)
            .await?;
        let agent = row
            .map(|r| r.try_get::<Option<String>, _>("agent"))
            .transpose()?
            .flatten();
        Ok(agent)
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

        row.try_get("id")
            .map_err(|e| anyhow::anyhow!("INSERT RETURNING id returned NULL or invalid: {e}"))
    }

    // ---------------------------------------------------------------
    // Status
    // ---------------------------------------------------------------

    pub async fn update_status_and_fields(
        &self,
        id: i64,
        status: TaskStatus,
        updates: &[(&str, serde_json::Value)],
    ) -> anyhow::Result<()> {
        for (col, _) in updates {
            anyhow::ensure!(
                ALLOWED_FIELDS.contains(col),
                "column {col} is not in the update allowlist"
            );
        }

        let previous = sqlx::query("SELECT status, agent, model FROM tasks WHERE id = ?")
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
        let from_status: String = previous.try_get("status").unwrap_or_default();
        let agent: Option<String> = previous.try_get("agent").unwrap_or(None);
        let model: Option<String> = previous.try_get("model").unwrap_or(None);

        let mut set_parts = Vec::new();
        let mut values: Vec<Option<String>> = Vec::new();

        set_parts.push("status = ?".to_string());
        values.push(Some(status.as_str().to_string()));

        let mut block_reason_in_updates = false;
        for (col, val) in updates {
            if *col == "block_reason" {
                block_reason_in_updates = true;
            }
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

        if !block_reason_in_updates && status != TaskStatus::Blocked {
            set_parts.push("block_reason = NULL".to_string());
        }

        set_parts.push("updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')".to_string());
        let sql = format!("UPDATE tasks SET {} WHERE id = ?", set_parts.join(", "));

        let mut query = sqlx::query(&sql);
        for v in &values {
            query = query.bind(v.as_deref());
        }
        query = query.bind(id);
        query.execute(&self.pool).await?;

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

    /// Update the status of a task.
    pub async fn update_status(&self, id: i64, status: TaskStatus) -> anyhow::Result<()> {
        let previous = sqlx::query("SELECT status, agent, model FROM tasks WHERE id = ?")
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
        let from_status: String = previous.try_get("status").unwrap_or_default();
        let agent: Option<String> = previous.try_get("agent").unwrap_or(None);
        let model: Option<String> = previous.try_get("model").unwrap_or(None);
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

    /// Persist the external_id (e.g. a newly-created GitHub issue number) onto an existing task row.
    pub async fn update_external_id(&self, id: i64, external_id: &str) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE tasks SET external_id = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ?",
        )
        .bind(external_id)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Conditionally update the status of a task only if it currently has `expected` status.
    ///
    /// Returns `true` if the row was updated, `false` if the current status did not match
    /// `expected` (i.e. a concurrent transition already moved the task elsewhere).
    pub async fn update_status_if(
        &self,
        id: i64,
        status: TaskStatus,
        expected: TaskStatus,
    ) -> anyhow::Result<bool> {
        let previous = sqlx::query("SELECT status, agent, model FROM tasks WHERE id = ?")
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
        let from_status: String = previous.try_get("status").unwrap_or_default();
        let agent: Option<String> = previous.try_get("agent").unwrap_or(None);
        let model: Option<String> = previous.try_get("model").unwrap_or(None);
        let sql = if status == TaskStatus::Blocked {
            "UPDATE tasks SET status = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ? AND status = ?"
        } else {
            "UPDATE tasks SET status = ?, block_reason = NULL, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ? AND status = ?"
        };
        let result = sqlx::query(sql)
            .bind(status.as_str())
            .bind(id)
            .bind(expected.as_str())
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Ok(false);
        }
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
        Ok(true)
    }

    /// Touch `updated_at` to now without changing any other field.
    ///
    /// Called by the runner immediately after a tmux session exits so that the
    /// stuck-task recovery timer is reset before response_handler post-processing
    /// (git push, PR creation, status update) begins.  Without this, a session
    /// that ran longer than `no_session_stuck_timeout` (default 10 min) would be
    /// re-dispatched by the tick while the runner is still writing results.
    pub async fn touch_updated_at(&self, id: i64) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE tasks SET updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ?",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Reset a task back to `new`.
    pub async fn reset_to_new(&self, id: i64) -> anyhow::Result<()> {
        let previous = sqlx::query("SELECT status, agent, model FROM tasks WHERE id = ?")
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
        let from_status: String = previous.try_get("status").unwrap_or_default();
        let agent: Option<String> = previous.try_get("agent").unwrap_or(None);
        let model: Option<String> = previous.try_get("model").unwrap_or(None);
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
        let rows = sqlx::query(&format!(
            "SELECT {TASK_COLS} FROM tasks WHERE repo = ? AND status = ? ORDER BY created_at DESC"
        ))
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
            &format!("SELECT {TASK_COLS} FROM tasks WHERE repo = ? AND origin != 'internal' AND status = ? ORDER BY created_at DESC"),
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
        &format!("SELECT {TASK_COLS} FROM tasks WHERE repo = ? AND origin = 'internal' AND status = ? ORDER BY created_at DESC"),
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
            &format!("SELECT {TASK_COLS} FROM tasks WHERE repo = ? AND origin = 'internal' ORDER BY created_at DESC"),
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
            &format!("SELECT {TASK_COLS} FROM tasks WHERE repo = ? AND origin = 'internal' AND source = ? ORDER BY created_at DESC"),
        )
        .bind(repo)
        .bind(source)
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(Self::row_to_task).collect()
    }

    /// Return only the `source_id` values for internal tasks with a specific source.
    /// Use this instead of `list_internal_by_source` when only deduplication is needed —
    /// avoids fetching all 57 task columns.
    pub async fn list_source_ids_by_source(
        &self,
        repo: &str,
        source: &str,
    ) -> anyhow::Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT source_id FROM tasks WHERE repo = ? AND origin = 'internal' AND source = ?",
        )
        .bind(repo)
        .bind(source)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| r.get::<String, _>(0)).collect())
    }

    /// List all active (non-done) tasks for a repo.
    pub async fn list_active(&self, repo: &str) -> anyhow::Result<Vec<Task>> {
        let rows = sqlx::query(
            &format!("SELECT {TASK_COLS} FROM tasks WHERE repo = ? AND status != 'done' ORDER BY created_at DESC"),
        )
        .bind(repo)
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(Self::row_to_task).collect()
    }

    /// List all external tasks for a repo (origin != 'internal').
    pub async fn list_all_external(&self, repo: &str) -> anyhow::Result<Vec<Task>> {
        let rows = sqlx::query(
            &format!("SELECT {TASK_COLS} FROM tasks WHERE repo = ? AND origin != 'internal' ORDER BY created_at DESC"),
        )
        .bind(repo)
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(Self::row_to_task).collect()
    }

    /// Check whether the store has any external tasks for a repo.
    pub async fn has_external_tasks(&self, repo: &str) -> anyhow::Result<bool> {
        let has_external = sqlx::query_scalar::<_, i32>(
            "SELECT 1 FROM tasks WHERE repo = ? AND origin != 'internal' LIMIT 1",
        )
        .bind(repo)
        .fetch_optional(&self.pool)
        .await?
        .is_some();
        Ok(has_external)
    }

    /// Return a set of external IDs for all external tasks in a repo.
    ///
    /// Used to eliminate N+1 queries when ingesting lists of external tasks.
    pub async fn existing_external_ids(
        &self,
        repo: &str,
    ) -> anyhow::Result<std::collections::HashSet<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT external_id FROM tasks WHERE repo = ? AND origin != 'internal' AND external_id IS NOT NULL",
        )
        .bind(repo)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .filter_map(|(id,)| if id.is_empty() { None } else { Some(id) })
            .collect())
    }

    /// List tasks for `orch doctor`: active tasks + done tasks updated since `cutoff_str`.
    /// Much cheaper than `list_all` on repos with many historical done tasks.
    pub async fn list_for_doctor(&self, repo: &str, cutoff_str: &str) -> anyhow::Result<Vec<Task>> {
        let rows = sqlx::query(
            &format!("SELECT {TASK_COLS} FROM tasks WHERE repo = ? AND (status != 'done' OR updated_at >= ?) ORDER BY created_at DESC"),
        )
        .bind(repo)
        .bind(cutoff_str)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::row_to_task).collect()
    }

    /// List all tasks for a repo, ordered by creation time descending.
    pub async fn list_all(&self, repo: &str) -> anyhow::Result<Vec<Task>> {
        let rows = sqlx::query(&format!(
            "SELECT {TASK_COLS} FROM tasks WHERE repo = ? ORDER BY created_at DESC"
        ))
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
        let rows = sqlx::query(&format!(
            "SELECT {TASK_COLS} FROM tasks WHERE status != 'done' ORDER BY created_at DESC"
        ))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::row_to_task).collect()
    }

    /// List all tasks across all repos regardless of status.
    ///
    /// Used by the CLI when `--status all` is passed.
    pub async fn list_all_global(&self) -> anyhow::Result<Vec<Task>> {
        let rows = sqlx::query(&format!(
            "SELECT {TASK_COLS} FROM tasks ORDER BY created_at DESC"
        ))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::row_to_task).collect()
    }

    /// List all active tasks matching an optional status filter, across all repos.
    ///
    /// Used by the CLI fallback path when no project context is available.
    pub async fn list_all_by_status_global(&self, status: TaskStatus) -> anyhow::Result<Vec<Task>> {
        let rows = sqlx::query(&format!(
            "SELECT {TASK_COLS} FROM tasks WHERE status = ? ORDER BY created_at DESC"
        ))
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

        for (col, _) in updates {
            anyhow::ensure!(
                ALLOWED_FIELDS.contains(col),
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
        anyhow::ensure!(
            INCREMENTABLE_FIELDS.contains(&field),
            "field {field} is not incrementable"
        );

        let sql = format!(
        "UPDATE tasks SET {field} = {field} + 1, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ? RETURNING {field} AS val"
    );

        let row = sqlx::query(&sql).bind(id).fetch_one(&self.pool).await?;

        Ok(row.try_get("val")?)
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
            network_retries = 0,
            ci_merge_failures = 0,
            review_cycles = 0,
            review_invocations = 0,
            review_session_expected = 0,
            auto_unblock_count = 0,
            auto_unblock_last_at = '',
            auto_unblock_last_reason = '',
            ci_recovery_count = 0,
            no_code_reroutes = 0,
            no_code_last_agent = '',
            needs_review_refires = 0,
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
    ///
    /// NOTE: `attempts` is intentionally NOT reset here. It must increase monotonically
    /// across the task's lifetime so that `(task_id, attempt, run_type)` keys in
    /// `task_runs` remain unique. Resetting it caused subsequent retries to overwrite
    /// earlier audit trail records via the ON CONFLICT UPSERT in `start_run()`.
    pub async fn reset_failure_counters(&self, id: i64) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE tasks SET
            route_attempts = 0,
            review_agent_failures = 0,
            review_invocations = 0,
            merge_conflict_retries = 0,
            pr_create_failures = 0,
            push_failures = 0,
            network_retries = 0,
            review_session_expected = 0,
            auto_unblock_count = 0,
            auto_unblock_last_at = '',
            auto_unblock_last_reason = '',
            ci_recovery_count = 0,
            no_code_reroutes = 0,
            no_code_last_agent = '',
            needs_review_refires = 0,
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
            agent = ?, model = ?, complexity = ?, estimate = ?, route_reason = ?,
            agent_profile = ?, selected_skills = ?,
            updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
         WHERE id = ?",
        )
        .bind(route.agent)
        .bind(route.model)
        .bind(route.complexity)
        .bind(route.estimate as i64)
        .bind(route.reason)
        .bind(route.profile)
        .bind(route.skills)
        .bind(route.id)
        .execute(&self.pool)
        .await?;
        let details = serde_json::json!({
            "complexity": route.complexity,
            "estimate": route.estimate,
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

    /// Return `(external_id, estimate)` for all non-internal tasks that have a
    /// positive Fibonacci estimate stored in the database.
    ///
    /// Used by startup estimate reconciliation to push orch-stored estimates to
    /// the GitHub Projects board for tasks that were routed before the estimate
    /// sync feature was available or before `project_estimate_field_id` was
    /// configured.
    pub async fn list_external_tasks_with_estimates(&self) -> anyhow::Result<Vec<(String, u8)>> {
        let rows = sqlx::query(
            "SELECT external_id, estimate FROM tasks \
             WHERE estimate > 0 AND external_id NOT LIKE 'internal:%' \
             AND status NOT IN ('done', 'blocked')",
        )
        .fetch_all(&self.pool)
        .await?;

        let mut result = Vec::new();
        for row in &rows {
            let external_id: String = row.try_get("external_id")?;
            let estimate_raw: i64 = row.try_get("estimate")?;
            if (1..=255).contains(&estimate_raw) {
                result.push((external_id, estimate_raw as u8));
            }
        }
        Ok(result)
    }

    /// Set the Fibonacci estimate for a task (only if currently 0).
    ///
    /// Used during ingestion to populate `tasks.estimate` from GitHub Projects
    /// when the orch estimate is still 0. This avoids overwriting estimates
    /// already set by the router.
    pub async fn set_estimate_if_zero(&self, id: i64, estimate: u8) -> anyhow::Result<bool> {
        let result = sqlx::query("UPDATE tasks SET estimate = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ? AND estimate = 0")
            .bind(estimate as i64)
            .bind(id)
            .execute(&self.pool)
            .await?;

        Ok(result.rows_affected() > 0)
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

        let memory_str: String = row.try_get("memory").unwrap_or_default();
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

        let memory_str: String = row.try_get("memory").unwrap_or_default();
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
        input: u64,
        output: u64,
        model: &str,
    ) -> anyhow::Result<()> {
        let pricing = pricing_for_model(model);
        let usage = TokenUsage {
            input_tokens: input,
            output_tokens: output,
        };
        let cost = pricing.estimate_cost_usd(usage);

        sqlx::query(
            "UPDATE tasks SET
            input_tokens = COALESCE(input_tokens, 0) + ?,
            output_tokens = COALESCE(output_tokens, 0) + ?,
            input_cost_usd = COALESCE(input_cost_usd, 0.0) + ?,
            output_cost_usd = COALESCE(output_cost_usd, 0.0) + ?,
            total_cost_usd = COALESCE(total_cost_usd, 0.0) + ?,
            updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
         WHERE id = ?",
        )
        .bind(i64::try_from(input).unwrap_or(i64::MAX))
        .bind(i64::try_from(output).unwrap_or(i64::MAX))
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
    /// Test-only helper / bulk operations used by admin paths and tests.
    #[cfg(test)]
    pub async fn batch_set_fields(
        &self,
        updates: &[(i64, &[(&str, serde_json::Value)])],
    ) -> anyhow::Result<()> {
        if updates.is_empty() {
            return Ok(());
        }

        for (_, entry_updates) in updates {
            for (col, _) in *entry_updates {
                anyhow::ensure!(
                    ALLOWED_FIELDS.contains(col),
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
    /// Test-only helper for bulk increments used by tests.
    #[cfg(test)]
    pub async fn batch_increment(&self, entries: &[(i64, &str)]) -> anyhow::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }

        for (_, field) in entries {
            anyhow::ensure!(
                INCREMENTABLE_FIELDS.contains(field),
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
    /// Test-only helper for bulk counter resets used by tests.
    #[cfg(test)]
    pub async fn batch_reset_failure_counters(&self, ids: &[i64]) -> anyhow::Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        // SQLite has a hard limit on host parameters (commonly 999). Chunk the
        // IN-list into smaller batches to avoid exceeding the limit.
        const CHUNK_SIZE: usize = 500;

        let mut tx = self.pool.begin().await?;

        for chunk in ids.chunks(CHUNK_SIZE) {
            // Build WHERE id IN (?, ?, ...) placeholders for this chunk
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
            let sql = format!(
                "UPDATE tasks SET
            route_attempts = 0,
            review_agent_failures = 0,
            review_invocations = 0,
            merge_conflict_retries = 0,
            pr_create_failures = 0,
            push_failures = 0,
            network_retries = 0,
            review_session_expected = 0,
            auto_unblock_count = 0,
            auto_unblock_last_at = '',
            auto_unblock_last_reason = '',
            ci_recovery_count = 0,
            no_code_reroutes = 0,
            needs_review_refires = 0,
            updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
         WHERE id IN ({placeholders})",
            );
            let mut query = sqlx::query(&sql);
            for id in chunk {
                query = query.bind(*id);
            }
            query.execute(&mut *tx).await?;
        }

        tx.commit().await?;
        Ok(())
    }

    /// Mark multiple tasks' worktrees as cleaned in a single transaction.
    ///
    /// Appends a `branch_delete` activity event for each task.
    /// Test-only helper for bulk cleaning used by tests.
    #[cfg(test)]
    pub async fn batch_mark_cleaned(&self, ids: &[i64]) -> anyhow::Result<()> {
        if ids.is_empty() {
            return Ok(());
        }

        // Chunk updates to avoid exceeding SQLite's parameter limit.
        const CHUNK_SIZE: usize = 500;

        let mut tx = self.pool.begin().await?;

        for chunk in ids.chunks(CHUNK_SIZE) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
            let sql = format!(
                "UPDATE tasks SET worktree_cleaned = 1, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id IN ({placeholders})",
            );
            let mut query = sqlx::query(&sql);
            for id in chunk {
                query = query.bind(*id);
            }
            query.execute(&mut *tx).await?;
        }

        // Append activity for each task (must be done individually — activity is per-task JSON)
        // Insert using the same transaction to avoid connection pool exhaustion
        // / deadlocks in tests.
        let details = serde_json::json!({ "worktree_cleaned": true });
        let details_json = serde_json::to_string(&details)?;
        for id in ids {
            sqlx::query(
                "INSERT INTO task_activity (task_id, event_type, from_status, to_status, agent, model, details) VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(*id)
            .bind("branch_delete")
            .bind(None::<&str>)
            .bind(None::<&str>)
            .bind(None::<&str>)
            .bind(None::<&str>)
            .bind(details_json.as_str())
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    /// List tasks that are done/blocked with worktrees that haven't been cleaned.
    /// Test-only helper and used by cleanup tests.
    #[cfg(test)]
    pub async fn list_cleanable(&self, repo: &str) -> anyhow::Result<Vec<Task>> {
        let rows = sqlx::query(&format!(
            "SELECT {TASK_COLS} FROM tasks
         WHERE repo = ?
           AND worktree != ''
           AND worktree_cleaned = 0
           AND status IN ('done', 'blocked')
         ORDER BY updated_at ASC"
        ))
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
            sqlx::query_as("SELECT external_id FROM tasks WHERE repo = ? AND worktree_cleaned = 1 AND external_id IS NOT NULL")
                .bind(repo)
                .fetch_all(&self.pool)
                .await?;

        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    // ---------------------------------------------------------------
    // Task Runs (audit trail)
    // ---------------------------------------------------------------

    /// Start a new run, returning its ID.
    pub async fn start_run(&self, run: &StartRun<'_>) -> anyhow::Result<i64> {
        let row = sqlx::query(
            "INSERT INTO task_runs (task_id, attempt, run_type, agent, model, command, prompt, outcome)
         VALUES (?, ?, ?, ?, ?, ?, ?, NULL)
         ON CONFLICT(task_id, attempt, run_type) DO UPDATE SET
            agent = excluded.agent, model = excluded.model,
            command = excluded.command, prompt = excluded.prompt,
            outcome = NULL,
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

        let run_id: i64 = row
            .try_get("id")
            .map_err(|e| anyhow::anyhow!("INSERT RETURNING id returned NULL or invalid: {e}"))?;
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
            outcome = ?, error = NULLIF(?, ''),
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
        .bind(i64::try_from(run.tokens.input_tokens).unwrap_or(i64::MAX))
        .bind(i64::try_from(run.tokens.output_tokens).unwrap_or(i64::MAX))
        .bind(run.tokens.total_cost_usd)
        .bind(run.tokens.duration_secs)
        .bind(run.run_id)
        .execute(&self.pool)
        .await?;
        if let Some(run_ctx) = run_ctx {
            let task_id: i64 = run_ctx.try_get("task_id").map_err(|e| {
                anyhow::anyhow!("failed to decode task_id from run_id {}: {e}", run.run_id)
            })?;
            let run_type: String = run_ctx.try_get("run_type").map_err(|e| {
                anyhow::anyhow!("failed to decode run_type from run_id {}: {e}", run.run_id)
            })?;
            let agent: String = run_ctx.try_get("agent").map_err(|e| {
                anyhow::anyhow!("failed to decode agent from run_id {}: {e}", run.run_id)
            })?;
            let model: String = run_ctx.try_get("model").map_err(|e| {
                anyhow::anyhow!("failed to decode model from run_id {}: {e}", run.run_id)
            })?;
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
        } else if run.outcome != "success" {
            tracing::warn!(
                run_id = run.run_id,
                outcome = run.outcome,
                "run_id not found in task_runs; skipping timeout/error activity log"
            );
        }
        Ok(())
    }

    /// Get all runs for a task, ordered by attempt.
    pub async fn get_runs(&self, task_id: i64) -> anyhow::Result<Vec<TaskRun>> {
        let rows = sqlx::query(&format!(
            "SELECT {TASK_RUN_COLS} FROM task_runs WHERE task_id = ? ORDER BY attempt ASC, run_type ASC"
        ))
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
        // SQLite has a hard limit on host parameters (commonly 999). Chunk the
        // IN-list into smaller batches to avoid exceeding the limit.
        const CHUNK_SIZE: usize = 500;

        let mut result: std::collections::HashMap<i64, Vec<TaskRun>> =
            std::collections::HashMap::new();

        for chunk in task_ids.chunks(CHUNK_SIZE) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
            let sql = format!(
                "SELECT {TASK_RUN_COLS} FROM task_runs WHERE task_id IN ({placeholders}) ORDER BY task_id ASC, attempt ASC, run_type ASC"
            );
            let mut query = sqlx::query(&sql);
            for id in chunk {
                query = query.bind(*id);
            }
            let rows = query.fetch_all(&self.pool).await?;
            for row in &rows {
                let run = Self::row_to_run(row)?;
                result.entry(run.task_id).or_default().push(run);
            }
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
        // SQLite has a hard limit on host parameters (commonly 999). Chunk the
        // IN-list into smaller batches to avoid exceeding the limit.
        const CHUNK_SIZE: usize = 500;

        let mut result = std::collections::HashMap::new();

        for chunk in ids.chunks(CHUNK_SIZE) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
            let sql = format!("SELECT {TASK_COLS} FROM tasks WHERE id IN ({placeholders})");
            let mut query = sqlx::query(&sql);
            for id in chunk {
                query = query.bind(*id);
            }
            let rows = query.fetch_all(&self.pool).await?;
            for row in &rows {
                let task = Self::row_to_task(row)?;
                result.insert(task.id, task);
            }
        }
        Ok(result)
    }

    /// Get statuses for a batch of external IDs in a single query.
    /// Returns a map from external_id -> TaskStatus for rows found in the DB.
    /// Missing external_ids are simply not present in the returned map.
    /// Uses chunking to avoid SQLite parameter limits.
    pub async fn get_statuses_by_external_ids(
        &self,
        repo: &str,
        ext_ids: &[&str],
    ) -> anyhow::Result<std::collections::HashMap<String, TaskStatus>> {
        let mut result = std::collections::HashMap::new();
        if ext_ids.is_empty() {
            return Ok(result);
        }

        const CHUNK_SIZE: usize = 500;
        for chunk in ext_ids.chunks(CHUNK_SIZE) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
            let sql = format!(
                "SELECT external_id, status FROM tasks WHERE repo = ? AND external_id IN ({placeholders})"
            );
            let mut query = sqlx::query(&sql);
            query = query.bind(repo);
            for id in chunk {
                query = query.bind(id);
            }
            let rows = query.fetch_all(&self.pool).await?;
            for row in &rows {
                let external_id: String = row.try_get("external_id").unwrap_or_default();
                let status_str: String = row.try_get("status").unwrap_or_default();
                if let Some(status) = TaskStatus::from_str(&status_str) {
                    result.insert(external_id, status);
                } else {
                    tracing::warn!(external_id = %external_id, status = %status_str, "unknown status value in tasks table");
                }
            }
        }

        Ok(result)
    }

    /// Get the last run of a specific type for a task.
    /// Used by tests to assert run ordering.
    #[cfg(test)]
    pub async fn get_last_run(
        &self,
        task_id: i64,
        run_type: &str,
    ) -> anyhow::Result<Option<TaskRun>> {
        let row = sqlx::query(&format!(
        "SELECT {TASK_RUN_COLS} FROM task_runs WHERE task_id = ? AND run_type = ? ORDER BY attempt DESC LIMIT 1"
    ))
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
    /// Test-only maintenance helper.
    #[cfg(test)]
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
        // Diagnostic: verify column count matches expected to prevent OOB panics
        // when schema changes without updating TASK_COLS
        let col_count = row.len();
        if col_count != TASK_COLS_COUNT {
            tracing::warn!(
                expected = TASK_COLS_COUNT,
                actual = col_count,
                "TASK_COLS column count mismatch - schema may have changed"
            );
        }

        let status_str: String = row
            .try_get("status")
            .map_err(|e| anyhow::anyhow!("task row missing status column: {e}"))?;
        let labels_str: String = row.try_get("labels").unwrap_or_default();
        let memory_str: String = row.try_get("memory").unwrap_or_default();
        let delegations_str: String = row.try_get("delegations").unwrap_or_default();

        Ok(Task {
            id: row
                .try_get("id")
                .map_err(|e| anyhow::anyhow!("task row missing id: {e}"))?,
            external_id: row.try_get("external_id").unwrap_or(None),
            repo: row
                .try_get("repo")
                .map_err(|e| anyhow::anyhow!("task row missing or invalid repo: {e}"))?,
            origin: row
                .try_get("origin")
                .map_err(|e| anyhow::anyhow!("task row missing or invalid origin: {e}"))?,
            title: row
                .try_get("title")
                .map_err(|e| anyhow::anyhow!("task row missing or invalid title: {e}"))?,
            body: row
                .try_get("body")
                .map_err(|e| anyhow::anyhow!("task row missing or invalid body: {e}"))?,
            status: TaskStatus::from_str(&status_str)
                .ok_or_else(|| anyhow::anyhow!("unknown task status: '{status_str}'"))?,
            source: row
                .try_get("source")
                .map_err(|e| anyhow::anyhow!("task row missing or invalid source: {e}"))?,
            source_id: row
                .try_get("source_id")
                .map_err(|e| anyhow::anyhow!("task row missing or invalid source_id: {e}"))?,
            author: row
                .try_get("author")
                .map_err(|e| anyhow::anyhow!("task row missing or invalid author: {e}"))?,
            url: row
                .try_get("url")
                .map_err(|e| anyhow::anyhow!("task row missing or invalid url: {e}"))?,
            labels: serde_json::from_str(&labels_str)
                .inspect_err(
                    |e| tracing::warn!(error = %e, "corrupt labels JSON, defaulting to empty"),
                )
                .unwrap_or_default(),
            agent: row.try_get("agent").unwrap_or(None),
            model: row.try_get("model").unwrap_or(None),
            complexity: row.try_get("complexity").unwrap_or_default(),
            estimate: {
                let raw: i32 = row.try_get("estimate").unwrap_or(0);
                u8::try_from(raw).unwrap_or_else(|_| {
                    tracing::warn!(
                        raw_estimate = raw,
                        "estimate value out of u8 range; defaulting to 0"
                    );
                    0
                })
            },
            route_reason: row.try_get("route_reason").unwrap_or_default(),
            agent_profile: row.try_get("agent_profile").unwrap_or_default(),
            selected_skills: row.try_get("selected_skills").unwrap_or_default(),
            route_attempts: row.try_get("route_attempts").unwrap_or(0),
            attempts: row.try_get("attempts").unwrap_or(0),
            branch: row.try_get("branch").unwrap_or_default(),
            worktree: row.try_get("worktree").unwrap_or_default(),
            worktree_cleaned: row.try_get::<i32, _>("worktree_cleaned").unwrap_or(0) != 0,
            summary: row.try_get("summary").unwrap_or_default(),
            last_error: row.try_get("last_error").unwrap_or_default(),
            parent_id: row.try_get("parent_id").unwrap_or(None),
            block_reason: row.try_get("block_reason").unwrap_or(None),
            pr_number: row.try_get("pr_number").unwrap_or(None),
            pr_review_context: row.try_get("pr_review_context").unwrap_or_default(),
            last_review_ts: row.try_get("last_review_ts").unwrap_or_default(),
            review_ts_map: row.try_get("review_ts_map").unwrap_or_default(),
            last_comment_review_ts: row.try_get("last_comment_review_ts").unwrap_or_default(),
            merge_conflict_retries: row.try_get("merge_conflict_retries").unwrap_or(0),
            ci_merge_failures: row.try_get("ci_merge_failures").unwrap_or(0),
            pr_create_failures: row.try_get("pr_create_failures").unwrap_or(0),
            push_failures: row.try_get::<i32, _>("push_failures").unwrap_or(0),
            network_retries: row.try_get::<i32, _>("network_retries").unwrap_or(0),
            review_agent_failures: row.try_get("review_agent_failures").unwrap_or(0),
            review_cycles: row.try_get("review_cycles").unwrap_or(0),
            review_invocations: row.try_get::<i32, _>("review_invocations").unwrap_or(0),
            review_session_expected: row
                .try_get::<i32, _>("review_session_expected")
                .unwrap_or(0)
                != 0,
            needs_review_refires: row.try_get::<i32, _>("needs_review_refires").unwrap_or(0),
            input_tokens: row.try_get("input_tokens").unwrap_or(0),
            output_tokens: row.try_get("output_tokens").unwrap_or(0),
            input_cost_usd: row.try_get("input_cost_usd").unwrap_or(0.0),
            output_cost_usd: row.try_get("output_cost_usd").unwrap_or(0.0),
            total_cost_usd: row.try_get("total_cost_usd").unwrap_or(0.0),
            model_reroute_chain: row.try_get("model_reroute_chain").unwrap_or_default(),
            limit_reroute_chain: row.try_get("limit_reroute_chain").unwrap_or_default(),
            budget_warning: row.try_get("budget_warning").unwrap_or_default(),
            budget_exceeded: row.try_get::<i32, _>("budget_exceeded").unwrap_or(0) != 0,
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
            no_code_last_agent: row
                .try_get::<String, _>("no_code_last_agent")
                .unwrap_or_default(),
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
            if let Some(id) = self.resolve_id_by_external(repo, task_id).await? {
                return Ok(Some(id));
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
        // External tasks: look up by external_id using the lightweight id-only
        // lookup to avoid deserializing the full Task row.
        self.resolve_id_by_external(repo, task_id).await
    }

    fn row_to_run(row: &sqlx::sqlite::SqliteRow) -> anyhow::Result<TaskRun> {
        Ok(TaskRun {
            id: row
                .try_get("id")
                .map_err(|e| anyhow::anyhow!("run row missing id: {e}"))?,
            task_id: row
                .try_get("task_id")
                .map_err(|e| anyhow::anyhow!("run row missing task_id: {e}"))?,
            attempt: row.try_get("attempt").unwrap_or(0),
            run_type: row.try_get("run_type").unwrap_or_default(),
            agent: row.try_get("agent").unwrap_or_default(),
            model: row.try_get("model").unwrap_or_default(),
            command: row.try_get("command").unwrap_or_default(),
            prompt: row.try_get("prompt").unwrap_or_default(),
            env_vars: row.try_get("env_vars").unwrap_or_default(),
            exit_code: row.try_get("exit_code").unwrap_or(None),
            stdout: row.try_get("stdout").unwrap_or_default(),
            stderr: row.try_get("stderr").unwrap_or_default(),
            parsed_response: row.try_get("parsed_response").unwrap_or_default(),
            outcome: row.try_get("outcome").unwrap_or_default(),
            error: row.try_get("error").unwrap_or_default(),
            input_tokens: row.try_get("input_tokens").unwrap_or(0),
            output_tokens: row.try_get("output_tokens").unwrap_or(0),
            total_cost_usd: row.try_get("total_cost_usd").unwrap_or(0.0),
            duration_secs: row.try_get("duration_secs").unwrap_or(0.0),
            started_at: row.try_get("started_at").unwrap_or_default(),
            completed_at: row.try_get("completed_at").unwrap_or(None),
        })
    }

    fn row_to_activity(row: &sqlx::sqlite::SqliteRow) -> anyhow::Result<TaskActivity> {
        let details_str: String = row.try_get("details").unwrap_or_else(|_| "{}".to_string());
        Ok(TaskActivity {
            id: row
                .try_get("id")
                .map_err(|e| anyhow::anyhow!("activity row missing id: {e}"))?,
            task_id: row
                .try_get("task_id")
                .map_err(|e| anyhow::anyhow!("activity row missing task_id: {e}"))?,
            timestamp: row.try_get("timestamp").unwrap_or_default(),
            event_type: row.try_get("event_type").unwrap_or_default(),
            from_status: row.try_get("from_status").unwrap_or(None),
            to_status: row.try_get("to_status").unwrap_or(None),
            agent: row.try_get("agent").unwrap_or(None),
            model: row.try_get("model").unwrap_or(None),
            details: serde_json::from_str(&details_str)
                .inspect_err(
                    |e| tracing::warn!(error = %e, "corrupt activity details JSON, defaulting to empty"),
                )
                .unwrap_or_else(|_| serde_json::json!({})),
        })
    }
}

#[cfg(test)]
mod row_to_task_tests {
    use super::*;

    async fn store_with_task() -> (crate::store::TaskStore, i64) {
        let store = crate::store::TaskStore::open_memory().await.unwrap();
        let id = store
            .create(&NewTask {
                external_id: None,
                repo: "owner/repo".to_string(),
                origin: "internal".to_string(),
                title: "Test task".to_string(),
                body: "Test body".to_string(),
                source: "cron".to_string(),
                source_id: "test-1".to_string(),
                author: "tester".to_string(),
                url: "https://example.com/1".to_string(),
                labels: vec![],
                parent_id: None,
            })
            .await
            .unwrap();
        (store, id)
    }

    // Each test selects all previously-required columns so that exactly the
    // column under test is absent and triggers the Err path.
    macro_rules! missing_column_test {
        ($name:ident, $col:literal, $select:literal) => {
            #[tokio::test]
            async fn $name() {
                let (store, id) = store_with_task().await;
                let sql = format!("{} WHERE id = {id}", $select);
                let row = sqlx::query(&sql).fetch_one(store.pool()).await.unwrap();
                let err = TaskStore::row_to_task(&row).unwrap_err();
                assert!(
                    err.to_string().contains($col),
                    "expected '{}' in error message, got: {err}",
                    $col
                );
            }
        };
    }

    missing_column_test!(
        row_to_task_fails_on_missing_repo,
        "repo",
        "SELECT id, status FROM tasks"
    );
    missing_column_test!(
        row_to_task_fails_on_missing_origin,
        "origin",
        "SELECT id, repo, status FROM tasks"
    );
    missing_column_test!(
        row_to_task_fails_on_missing_title,
        "title",
        "SELECT id, repo, origin, status FROM tasks"
    );
    missing_column_test!(
        row_to_task_fails_on_missing_body,
        "body",
        "SELECT id, repo, origin, title, status FROM tasks"
    );
    missing_column_test!(
        row_to_task_fails_on_missing_source,
        "source",
        "SELECT id, repo, origin, title, body, status FROM tasks"
    );
    missing_column_test!(
        row_to_task_fails_on_missing_source_id,
        "source_id",
        "SELECT id, repo, origin, title, body, source, status FROM tasks"
    );
    missing_column_test!(
        row_to_task_fails_on_missing_author,
        "author",
        "SELECT id, repo, origin, title, body, source, source_id, status FROM tasks"
    );
    missing_column_test!(
        row_to_task_fails_on_missing_url,
        "url",
        "SELECT id, repo, origin, title, body, source, source_id, author, status FROM tasks"
    );

    // Decode-error test: inject a BLOB with invalid UTF-8 bytes for the `repo`
    // column. SQLite's type coercion converts NULL and integers to strings, so
    // they don't produce a decode error. A raw BLOB that is not valid UTF-8
    // (X'DEADBEEF') does cause `try_get::<String>` to fail with a ColumnDecode
    // error, exercising the decode-error-propagation path that is distinct from
    // the missing-column (ColumnNotFound) path tested above.
    #[tokio::test]
    async fn row_to_task_fails_on_decode_error_invalid_utf8_repo() {
        let (store, id) = store_with_task().await;
        // X'DEADBEEF' is a 4-byte BLOB; bytes 0xDE 0xAD 0xBE 0xEF are not
        // valid UTF-8, so try_get::<String>("repo") will fail with a decode error.
        let sql = format!(
            "SELECT id, X'DEADBEEF' as repo, origin, title, body, status, \
             source, source_id, author, url FROM tasks WHERE id = {id}"
        );
        let row = sqlx::query(&sql).fetch_one(store.pool()).await.unwrap();
        let err = TaskStore::row_to_task(&row).unwrap_err();
        assert!(
            err.to_string().contains("repo"),
            "expected 'repo' in error message, got: {err}"
        );
    }
}
