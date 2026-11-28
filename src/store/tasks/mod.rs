/// The SQL behind each replicated task write, run with the proposer's time
pub(crate) mod apply;
mod cli_ref;
mod fields;
mod queries;
mod runs;

use super::{Context, MemoryEntry, SqlitePool, TaskStore};
use crate::backends::ExternalTask;
use crate::security;
use crate::store::cluster;
use crate::store::cluster::Command;
pub use cli_ref::{CliTaskRef, CliTaskSpec, parse_cli_task_spec};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

/// Explicit column list for `SELECT` queries on the `tasks` table.
///
/// Using `SELECT *` with `sqlx-sqlite`'s persistent prepared statements can
/// cause OOB panics when the schema changes (new migration columns) because
/// `sqlite3_column_count()` returns the updated count but the cached column
/// metadata vector was built at prepare time. Explicit columns prevent the
/// mismatch.
pub(crate) const TASK_COLS: &str = "id, external_id, repo, origin, title, body, status, \
    source, source_id, author, url, labels, agent, model, complexity, estimate, \
    route_reason, agent_profile, route_attempts, attempts, \
    branch, worktree, worktree_cleaned, summary, last_error, parent_id, \
    block_reason, pr_number, pr_review_context, last_review_ts, review_ts_map, \
    last_comment_review_ts, merge_conflict_retries, ci_merge_failures, \
    pr_create_failures, review_agent_failures, review_resets, review_cycles, input_tokens, \
    output_tokens, input_cost_usd, output_cost_usd, total_cost_usd, \
    model_reroute_chain, limit_reroute_chain, \
    memory, delegations, created_at, updated_at, \
     review_invocations, needs_review_refires, push_failures, auto_unblock_count, auto_unblock_last_at, \
     ci_recovery_count, auto_unblock_last_reason, no_code_reroutes, network_retries, no_code_last_agent, \
     owner_node, timeout_retries";

/// Number of columns in TASK_COLS (used for diagnostic verification).
pub(crate) const TASK_COLS_COUNT: usize = 61;

/// Explicit column list for `SELECT` queries on the `task_runs` table.
const TASK_RUN_COLS: &str = "id, task_id, attempt, run_type, agent, model, command, prompt, env_vars, \
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
    "route_attempts",
    "attempts",
    "branch",
    "worktree",
    "worktree_cleaned",
    "owner_node",
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
    "review_resets",
    "review_cycles",
    "review_invocations",
    "needs_review_refires",
    "input_tokens",
    "output_tokens",
    "input_cost_usd",
    "output_cost_usd",
    "total_cost_usd",
    "model_reroute_chain",
    "limit_reroute_chain",
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
    "timeout_retries",
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
    "review_resets",
    "review_cycles",
    "review_invocations",
    "needs_review_refires",
    "auto_unblock_count",
    "ci_recovery_count",
    "no_code_reroutes",
    "network_retries",
    "timeout_retries",
];

fn sanitize_activity_details(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) => serde_json::Value::String(security::redact(text)),
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .map(sanitize_activity_details)
                .collect::<Vec<_>>(),
        ),
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), sanitize_activity_details(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn sanitize_task_field_value(col: &str, value: &serde_json::Value) -> serde_json::Value {
    match (col, value) {
        ("last_error" | "block_reason", serde_json::Value::String(text)) => {
            serde_json::Value::String(security::redact(text))
        }
        _ => value.clone(),
    }
}

fn sql_value_for_task_field(
    col: &str,
    value: &serde_json::Value,
) -> anyhow::Result<Option<String>> {
    let sanitized = sanitize_task_field_value(col, value);
    match sanitized {
        serde_json::Value::String(text) => Ok(Some(text)),
        serde_json::Value::Number(number) => Ok(Some(number.to_string())),
        serde_json::Value::Bool(boolean) => Ok(Some(if boolean { "1" } else { "0" }.to_string())),
        serde_json::Value::Null => Ok(None),
        other => Ok(Some(serde_json::to_string(&other)?)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    New,
    Routed,
    InProgress,
    Done,
    Blocked,
    InReview,
    Approved,
    NeedsReview,
    Waiting,
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
            Self::Approved => "approved",
            Self::NeedsReview => "needs_review",
            Self::Waiting => "waiting",
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
            "approved" => Some(Self::Approved),
            "needs_review" => Some(Self::NeedsReview),
            "waiting" => Some(Self::Waiting),
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
    pub route_attempts: i32,

    // Execution
    pub attempts: i32,
    pub branch: String,
    pub worktree: String,
    pub worktree_cleaned: bool,
    /// The node that runs the task, None until one starts it
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_node: Option<String>,
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
    /// Runs that ended in a timeout, reset by retry.
    pub timeout_retries: i32,
    pub review_agent_failures: i32,
    // Review retries that ended without a decision and without counting as a failure
    pub review_resets: i32,
    pub review_cycles: i32,
    pub review_invocations: i32,
    // NeedsReview re-fire counter (incremented by sync catch-up and reset on successful review cycles)
    pub needs_review_refires: i32,

    // Tokens & Cost
    pub input_tokens: i64,
    pub output_tokens: i64,
    // Sum of the costs the task's runs reported, 0 when none did
    pub total_cost_usd: f64,

    // Recovery
    pub model_reroute_chain: String,
    pub limit_reroute_chain: String,

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

impl Task {
    /// The cost the task's runs reported, `None` when no run reported one.
    pub fn reported_cost_usd(&self) -> Option<f64> {
        (self.total_cost_usd > 0.0).then_some(self.total_cost_usd)
    }
}

/// Parameters for creating a new task.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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

/// A task reaching done, with what cycle time and first-try counts need
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct MergedEvent {
    pub at: String,
    pub task_id: i64,
    pub repo: String,
    pub agent: Option<String>,
    pub created_at: String,
    pub attempts: i32,
    pub review_cycles: i32,
}

/// A task entering blocked
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct BlockedEvent {
    pub at: String,
    pub task_id: i64,
    pub repo: String,
}

/// One agent run, for time spent per agent and project
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct RunEvent {
    pub at: Option<String>,
    pub repo: String,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub run_type: Option<String>,
    pub outcome: Option<String>,
    pub duration_secs: f64,
}

/// What the statistics page counts over a window
#[derive(Debug, Clone, Serialize)]
pub struct StatsEvents {
    pub merged: Vec<MergedEvent>,
    pub blocked: Vec<BlockedEvent>,
    pub runs: Vec<RunEvent>,
}

/// A task moving between statuses, with what a feed needs to name it
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct StatusChange {
    pub timestamp: String,
    pub task_id: i64,
    pub repo: String,
    pub external_id: Option<String>,
    pub title: String,
    pub from_status: Option<String>,
    pub to_status: Option<String>,
    pub agent: Option<String>,
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

fn parent_external_id_from_labels(labels: &[String]) -> Option<&str> {
    labels
        .iter()
        .find_map(|label| label.strip_prefix("parent:"))
        .filter(|parent| !parent.is_empty())
}

fn owned_updates(
    updates: &[(&str, serde_json::Value)],
) -> anyhow::Result<Vec<(String, serde_json::Value)>> {
    updates
        .iter()
        .map(|(col, val)| {
            anyhow::ensure!(
                ALLOWED_FIELDS.contains(col),
                "column {col} is not in the update allowlist"
            );
            Ok((col.to_string(), val.clone()))
        })
        .collect()
}

impl TaskStore {
    /// Append a lifecycle activity event for a task.
    #[expect(
        clippy::too_many_arguments,
        reason = "one argument per column of the activity row"
    )]
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
            Some(v) => serde_json::to_string(&sanitize_activity_details(v))?,
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

    /// When the task last entered `status`
    pub async fn status_entered_at(
        &self,
        task_id: i64,
        status: TaskStatus,
    ) -> anyhow::Result<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT timestamp FROM task_activity
             WHERE task_id = ? AND event_type = 'status_change' AND to_status = ?
             ORDER BY id DESC LIMIT 1",
        )
        .bind(task_id)
        .bind(status.as_str())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(ts,)| ts))
    }

    /// When the task last entered in_review, and whether a review decision was recorded since
    pub async fn in_review_entry(&self, task_id: i64) -> anyhow::Result<Option<(String, bool)>> {
        let row: Option<(String, bool)> = sqlx::query_as(
            "SELECT a.timestamp, EXISTS (
                 SELECT 1 FROM task_activity d
                 WHERE d.task_id = a.task_id AND d.event_type = 'review_decision' AND d.id > a.id
             )
             FROM task_activity a
             WHERE a.task_id = ? AND a.event_type = 'status_change' AND a.to_status = 'in_review'
             ORDER BY a.id DESC LIMIT 1",
        )
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Get task activity timeline in chronological order.
    pub async fn get_activity(
        &self,
        task_id: i64,
        limit: Option<usize>,
    ) -> anyhow::Result<Vec<TaskActivity>> {
        let rows = if let Some(limit) = limit {
            let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "SELECT {TASK_ACTIVITY_COLS} FROM task_activity WHERE task_id = ? ORDER BY timestamp ASC, id ASC LIMIT ?"
            )))
            .bind(task_id)
            .bind(limit_i64)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "SELECT {TASK_ACTIVITY_COLS} FROM task_activity WHERE task_id = ? ORDER BY timestamp ASC, id ASC"
            )))
            .bind(task_id)
            .fetch_all(&self.pool)
            .await?
        };

        rows.iter().map(Self::row_to_activity).collect()
    }

    pub async fn create(&self, new: &NewTask) -> anyhow::Result<i64> {
        self.write_int(Command::TaskCreate {
            new: new.clone(),
            internal: false,
        })
        .await
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
        let new = NewTask {
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
        };
        self.write_int(Command::TaskCreate {
            new,
            internal: true,
        })
        .await
    }

    /// An internal task born with its labels, so a hold label is there before any tick reads it
    pub async fn create_internal_labeled(
        &self,
        repo: &str,
        title: &str,
        body: &str,
        source: &str,
        labels: Vec<String>,
    ) -> anyhow::Result<i64> {
        let new = NewTask {
            external_id: None,
            repo: repo.to_string(),
            origin: "internal".to_string(),
            title: title.to_string(),
            body: body.to_string(),
            source: source.to_string(),
            source_id: source.to_string(),
            author: String::new(),
            url: String::new(),
            labels,
            parent_id: None,
        };
        self.write_int(Command::TaskCreate {
            new,
            internal: true,
        })
        .await
    }

    /// Get a task by its internal ID.
    pub async fn get(&self, id: i64) -> anyhow::Result<Task> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {TASK_COLS} FROM tasks WHERE id = ?"
        )))
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
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {TASK_COLS} FROM tasks WHERE repo = ? AND external_id = ?"
        )))
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

    /// The task whose worktree on this node is `path`
    pub async fn resolve_id_by_worktree(&self, path: &str) -> anyhow::Result<Option<i64>> {
        if path.is_empty() {
            return Ok(None);
        }

        let row = sqlx::query("SELECT id FROM tasks WHERE worktree = ? LIMIT 1")
            .bind(path)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.try_get::<i64, _>("id")).transpose()?)
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
        let parent_id = match parent_external_id_from_labels(ext.labels) {
            Some(parent_external_id) => self.resolve_task_id(ext.repo, parent_external_id).await?,
            None => None,
        };
        self.write_int(Command::TaskUpsertExternal(UpsertExternalCmd {
            repo: ext.repo.to_string(),
            ext_id: ext.ext_id.to_string(),
            title: ext.title.to_string(),
            body: ext.body.to_string(),
            author: ext.author.to_string(),
            url: ext.url.to_string(),
            labels: ext.labels.to_vec(),
            origin: ext.origin.to_string(),
            parent_id,
        }))
        .await
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
        let mut updates = owned_updates(updates)?;
        // Local columns stay on this node, as set_fields keeps them
        if self.replicates() {
            let local: Vec<_> = updates
                .iter()
                .filter(|(col, _)| is_local_column(col))
                .cloned()
                .collect();
            updates.retain(|(col, _)| !is_local_column(col));

            if !local.is_empty() {
                let mut tx = self.begin_write().await?;
                apply::set_fields(&mut tx, &cluster::now_stamp(), id, &local, true).await?;
                tx.commit().await?;
            }
        }

        self.write(Command::TaskUpdateStatusAndFields {
            id,
            status,
            updates,
        })
        .await
        .map(|_| ())
    }

    /// Update the status of a task.
    pub async fn update_status(&self, id: i64, status: TaskStatus) -> anyhow::Result<()> {
        self.write_bool(Command::TaskUpdateStatus {
            id,
            status,
            expected: None,
        })
        .await
        .map(|_| ())
    }

    /// Persist the external_id (e.g. a newly-created GitHub issue number) onto an existing task row.
    pub async fn update_external_id(&self, id: i64, external_id: &str) -> anyhow::Result<()> {
        self.write(Command::TaskUpdateExternalId {
            id,
            external_id: external_id.to_string(),
        })
        .await
        .map(|_| ())
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
        self.write_bool(Command::TaskUpdateStatus {
            id,
            status,
            expected: Some(expected),
        })
        .await
    }

    /// Touch `updated_at` to now without changing any other field.
    ///
    /// Called by the runner immediately after a tmux session exits so that the
    /// stuck-task recovery timer is reset before response_handler post-processing
    /// (git push, PR creation, status update) begins.  Without this, a session
    /// that ran longer than `no_session_stuck_timeout` (default 10 min) would be
    /// re-dispatched by the tick while the runner is still writing results.
    pub async fn touch_updated_at(&self, id: i64) -> anyhow::Result<()> {
        self.write(Command::TaskTouch { id }).await.map(|_| ())
    }

    /// Reset a task back to `new`, clearing `branch` and `worktree`
    pub async fn reset_to_new(&self, id: i64) -> anyhow::Result<()> {
        self.write(Command::TaskResetToNew {
            id,
            keep_branch: false,
        })
        .await
        .map(|_| ())
    }

    /// Reset a task back to `new` and keep `branch` when cleanup left the local ref
    pub async fn reset_to_new_keep_branch(&self, id: i64) -> anyhow::Result<()> {
        self.write(Command::TaskResetToNew {
            id,
            keep_branch: true,
        })
        .await
        .map(|_| ())
    }

    /// Update the block reason for a task.
    pub async fn set_block_reason(&self, id: i64, reason: Option<&str>) -> anyhow::Result<()> {
        let value = reason.map_or(serde_json::Value::Null, |r| {
            serde_json::Value::String(r.to_string())
        });
        self.set_fields(id, &[("block_reason", value)]).await
    }

    // ---------------------------------------------------------------
    // Routing
    // ---------------------------------------------------------------

    // ---------------------------------------------------------------
    // Memory
    // ---------------------------------------------------------------

    // ---------------------------------------------------------------
    // Tokens
    // ---------------------------------------------------------------

    // ---------------------------------------------------------------
    // Cleanup
    // ---------------------------------------------------------------

    // ---------------------------------------------------------------
    // Task Runs (audit trail)
    // ---------------------------------------------------------------

    // ---------------------------------------------------------------
    // Row mapping helpers
    // ---------------------------------------------------------------

    fn row_to_task(row: &SqliteRow) -> anyhow::Result<Task> {
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
            route_attempts: row.try_get("route_attempts").unwrap_or(0),
            attempts: row.try_get("attempts").unwrap_or(0),
            branch: row.try_get("branch").unwrap_or_default(),
            worktree: row.try_get("worktree").unwrap_or_default(),
            worktree_cleaned: row.try_get::<i32, _>("worktree_cleaned").unwrap_or(0) != 0,
            owner_node: row.try_get("owner_node").unwrap_or(None),
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
            timeout_retries: row.try_get::<i32, _>("timeout_retries").unwrap_or(0),
            review_agent_failures: row.try_get("review_agent_failures").unwrap_or(0),
            review_resets: row.try_get::<i32, _>("review_resets").unwrap_or(0),
            review_cycles: row.try_get("review_cycles").unwrap_or(0),
            review_invocations: row.try_get::<i32, _>("review_invocations").unwrap_or(0),
            needs_review_refires: row.try_get::<i32, _>("needs_review_refires").unwrap_or(0),
            input_tokens: row.try_get("input_tokens").unwrap_or(0),
            output_tokens: row.try_get("output_tokens").unwrap_or(0),
            total_cost_usd: row.try_get("total_cost_usd").unwrap_or(0.0),
            model_reroute_chain: row.try_get("model_reroute_chain").unwrap_or_default(),
            limit_reroute_chain: row.try_get("limit_reroute_chain").unwrap_or_default(),
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
            created_at: row
                .try_get::<String, _>("created_at")
                .map_err(|e| anyhow::anyhow!("task row missing created_at: {e}"))?,
            updated_at: row
                .try_get::<String, _>("updated_at")
                .map_err(|e| anyhow::anyhow!("task row missing updated_at: {e}"))?,
        })
    }
    pub async fn ensure_external_task(
        &self,
        repo: &str,
        ext: &ExternalTask,
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

    fn row_to_activity(row: &SqliteRow) -> anyhow::Result<TaskActivity> {
        let details_str: String = row
            .try_get("details")
            .map_err(|e| anyhow::anyhow!("activity row decode error for details: {e}"))?;
        let details = serde_json::from_str::<serde_json::Value>(&details_str).inspect_err(
            |e| tracing::warn!(error = %e, "corrupt activity details JSON, skipping field"),
        )?;
        Ok(TaskActivity {
            id: row
                .try_get("id")
                .map_err(|e| anyhow::anyhow!("activity row decode error for id: {e}"))?,
            task_id: row
                .try_get("task_id")
                .map_err(|e| anyhow::anyhow!("activity row decode error for task_id: {e}"))?,
            timestamp: row
                .try_get("timestamp")
                .map_err(|e| anyhow::anyhow!("activity row decode error for timestamp: {e}"))?,
            event_type: row
                .try_get("event_type")
                .map_err(|e| anyhow::anyhow!("activity row decode error for event_type: {e}"))?,
            from_status: row
                .try_get("from_status")
                .map_err(|e| anyhow::anyhow!("activity row decode error for from_status: {e}"))?,
            to_status: row
                .try_get("to_status")
                .map_err(|e| anyhow::anyhow!("activity row decode error for to_status: {e}"))?,
            agent: row
                .try_get("agent")
                .map_err(|e| anyhow::anyhow!("activity row decode error for agent: {e}"))?,
            model: row
                .try_get("model")
                .map_err(|e| anyhow::anyhow!("activity row decode error for model: {e}"))?,
            details,
        })
    }
}

/// Columns that describe this node's own copy of a task, never sent through the log
const LOCAL_TASK_COLUMNS: &[&str] = &[
    "worktree",
    "input_tokens",
    "output_tokens",
    "input_cost_usd",
    "output_cost_usd",
    "total_cost_usd",
];

pub(crate) fn is_local_column(col: &str) -> bool {
    LOCAL_TASK_COLUMNS.contains(&col)
}

/// Owned copy of [`UpsertExternal`], so it can travel in a Raft entry
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpsertExternalCmd {
    pub repo: String,
    pub ext_id: String,
    pub title: String,
    pub body: String,
    pub author: String,
    pub url: String,
    pub labels: Vec<String>,
    pub origin: String,
    pub parent_id: Option<i64>,
}

/// Owned copy of [`StoreRoute`], so it can travel in a Raft entry
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoreRouteCmd {
    pub id: i64,
    pub agent: String,
    pub model: Option<String>,
    pub complexity: String,
    pub estimate: u8,
    pub reason: String,
    pub profile: String,
}

/// The columns a done task's tombstone keeps. Nodes that had the task keep the rest locally
const TOMBSTONE_COLUMNS: &[&str] = &[
    "id",
    "external_id",
    "repo",
    "origin",
    "title",
    "status",
    "pr_number",
    "parent_id",
    "created_at",
    "updated_at",
];

async fn replicated_columns(conn: &mut sqlx::SqliteConnection) -> anyhow::Result<Vec<String>> {
    let cols: Vec<(String,)> = sqlx::query_as("SELECT name FROM pragma_table_info('tasks')")
        .fetch_all(conn)
        .await?;
    Ok(cols
        .into_iter()
        .map(|(c,)| c)
        .filter(|c| !is_local_column(c))
        .collect())
}

/// Every task row as JSON with its replicated columns, done tasks as tombstones
pub(crate) async fn replicated_rows(pool: &SqlitePool) -> anyhow::Result<Vec<serde_json::Value>> {
    let mut conn = pool.acquire().await?;
    let cols = replicated_columns(&mut conn).await?;
    let full: Vec<&String> = cols.iter().collect();
    let tomb: Vec<&String> = cols
        .iter()
        .filter(|c| TOMBSTONE_COLUMNS.contains(&c.as_str()))
        .collect();
    let sql = format!(
        "SELECT CASE WHEN status = 'done' THEN {} ELSE {} END FROM tasks ORDER BY id",
        super::replica::json_row_sql(&tomb),
        super::replica::json_row_sql(&full)
    );
    let rows: Vec<(String,)> = sqlx::query_as(sqlx::AssertSqlSafe(sql.as_str()))
        .fetch_all(&mut *conn)
        .await?;
    rows.into_iter()
        .map(|(r,)| Ok(serde_json::from_str(&r)?))
        .collect()
}

/// Make the task rows match `rows`. Local columns, and a tombstone's missing columns, keep the local
/// value. Tasks missing from `rows` go, with this node's runs and activity for them
pub(crate) async fn replace_replicated(
    conn: &mut sqlx::SqliteConnection,
    rows: &[serde_json::Value],
) -> anyhow::Result<()> {
    let allowed = replicated_columns(conn).await?;
    sqlx::query("PRAGMA defer_foreign_keys = ON")
        .execute(&mut *conn)
        .await?;
    sqlx::query("CREATE TEMP TABLE IF NOT EXISTS snapshot_ids (id INTEGER PRIMARY KEY)")
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM snapshot_ids")
        .execute(&mut *conn)
        .await?;
    for row in rows {
        let id = row["id"]
            .as_i64()
            .context("snapshot task row without an id")?;
        sqlx::query("INSERT INTO snapshot_ids (id) VALUES (?)")
            .bind(id)
            .execute(&mut *conn)
            .await?;
    }

    for table in ["task_runs", "task_activity"] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE task_id NOT IN (SELECT id FROM snapshot_ids)"
        )))
        .execute(&mut *conn)
        .await?;
    }

    sqlx::query("DELETE FROM tasks WHERE id NOT IN (SELECT id FROM snapshot_ids)")
        .execute(&mut *conn)
        .await?;

    for row in rows {
        let obj = row
            .as_object()
            .context("snapshot task row is not an object")?;
        let cols: Vec<&String> = obj.keys().collect();
        for c in &cols {
            anyhow::ensure!(
                allowed.contains(c),
                "snapshot names unknown task column {c}"
            );
        }

        let values: Vec<String> = cols
            .iter()
            .map(|c| format!("json_extract(?1, '$.\"{c}\"')"))
            .collect();
        let updates: Vec<String> = cols.iter().map(|c| format!("{c} = excluded.{c}")).collect();
        let names: Vec<&str> = cols.iter().map(|c| c.as_str()).collect();
        let sql = format!(
            "INSERT INTO tasks ({}) VALUES ({}) ON CONFLICT(id) DO UPDATE SET {}",
            names.join(", "),
            values.join(", "),
            updates.join(", ")
        );
        sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
            .bind(serde_json::to_string(row)?)
            .execute(&mut *conn)
            .await?;
    }

    sqlx::query("DROP TABLE snapshot_ids")
        .execute(&mut *conn)
        .await?;
    Ok(())
}

#[cfg(test)]
mod row_to_task_tests;
