use super::{pricing_for_model, Context, MemoryEntry, SqlitePool, TaskStore, TokenUsage};
use crate::cluster::Command;
use crate::security;
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
    pr_create_failures, review_agent_failures, review_resets, review_cycles, input_tokens, \
    output_tokens, input_cost_usd, output_cost_usd, total_cost_usd, \
    model_reroute_chain, limit_reroute_chain, \
    memory, delegations, created_at, updated_at, \
     review_invocations, needs_review_refires, push_failures, auto_unblock_count, auto_unblock_last_at, \
     ci_recovery_count, auto_unblock_last_reason, no_code_reroutes, network_retries, no_code_last_agent, \
     owner_node";

/// Number of columns in TASK_COLS (used for diagnostic verification).
pub(crate) const TASK_COLS_COUNT: usize = 61;

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
    pub selected_skills: String,
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
    pub input_cost_usd: f64,
    pub output_cost_usd: f64,
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

/// How an operator names a task on the CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliTaskSpec {
    /// `internal:58`
    Internal(i64),
    /// `owner/repo#58`
    Issue { repo: String, number: i64 },
    /// `58` or `#58`
    Bare(i64),
}

/// A task picked by [`TaskStore::resolve_cli_id`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliTaskRef {
    pub store_id: Option<i64>,
    pub repo: String,
    pub origin: String,
    pub title: String,
    pub external_id: String,
    pub status: String,
}

impl CliTaskRef {
    pub fn is_internal(&self) -> bool {
        self.origin == "internal"
    }

    pub fn unsynced_issue(repo: String, number: i64) -> Self {
        Self {
            store_id: None,
            repo,
            origin: "github".to_string(),
            title: String::new(),
            external_id: number.to_string(),
            status: String::new(),
        }
    }

    pub fn display_spec(&self) -> String {
        if self.is_internal() {
            match self.store_id {
                Some(id) => format!("internal:{id}"),
                None => self.external_id.clone(),
            }
        } else {
            format!("{}#{}", self.repo, self.external_id)
        }
    }

    fn from_row(row: &sqlx::sqlite::SqliteRow) -> anyhow::Result<Self> {
        let id: i64 = row.try_get("id")?;
        let origin: String = row.try_get("origin")?;
        let external_id: Option<String> = row.try_get("external_id")?;
        let external_id = external_id.unwrap_or_else(|| {
            if origin == "internal" {
                format!("internal:{id}")
            } else {
                String::new()
            }
        });
        Ok(Self {
            store_id: Some(id),
            repo: row.try_get("repo")?,
            origin,
            title: row.try_get("title")?,
            external_id,
            status: row.try_get("status")?,
        })
    }

    fn format_match(&self) -> String {
        format!(
            "  {}  [{}]  {}",
            self.display_spec(),
            if self.status.is_empty() {
                "unknown"
            } else {
                &self.status
            },
            if self.title.is_empty() {
                "-"
            } else {
                &self.title
            }
        )
    }
}

/// Parse `internal:N`, `owner/repo#N`, `#N`, or a bare number.
pub fn parse_cli_task_spec(raw: &str) -> anyhow::Result<CliTaskSpec> {
    let spec = raw.trim();
    if spec.is_empty() {
        anyhow::bail!("empty task id");
    }
    if let Some(suffix) = spec.strip_prefix("internal:") {
        let number: i64 = suffix
            .parse()
            .map_err(|_| anyhow::anyhow!("internal task id '{suffix}' is not numeric"))?;
        if number <= 0 {
            anyhow::bail!("internal task id must be a positive number");
        }
        return Ok(CliTaskSpec::Internal(number));
    }
    if let Some((repo, number)) = parse_repo_issue(spec) {
        return Ok(CliTaskSpec::Issue { repo, number });
    }
    let bare = spec.strip_prefix('#').unwrap_or(spec);
    if let Ok(number) = bare.parse::<i64>() {
        if number > 0 {
            return Ok(CliTaskSpec::Bare(number));
        }
    }
    anyhow::bail!("invalid task id '{spec}': use a number, owner/repo#N, or internal:N")
}

fn parse_repo_issue(spec: &str) -> Option<(String, i64)> {
    let (repo, number) = spec.rsplit_once('#')?;
    if repo.is_empty() {
        return None;
    }
    let number: i64 = number.parse().ok()?;
    if number <= 0 {
        return None;
    }
    let mut parts = repo.split('/');
    let owner = parts.next()?;
    let name = parts.next()?;
    if parts.next().is_some() || owner.is_empty() || name.is_empty() {
        return None;
    }
    let ok = |part: &str| {
        part.chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'))
    };
    if !ok(owner) || !ok(name) {
        return None;
    }
    Some((repo.to_string(), number))
}

fn ambiguous_cli_id_error(spec: &str, matches: &[CliTaskRef]) -> anyhow::Error {
    let mut lines = vec![format!("task ID {spec} matches more than one task:")];
    for task in matches {
        lines.push(task.format_match());
    }
    lines.push(ambiguous_cli_id_hint(matches));
    anyhow::anyhow!(lines.join("\n"))
}

fn ambiguous_cli_id_hint(matches: &[CliTaskRef]) -> String {
    let github = matches
        .iter()
        .find(|task| !task.is_internal())
        .map(CliTaskRef::display_spec);
    let internal = matches
        .iter()
        .find(|task| task.is_internal())
        .map(CliTaskRef::display_spec);
    match (github, internal) {
        (Some(github), Some(internal)) => {
            format!("Use {github} for the GitHub task or {internal} for the internal one")
        }
        _ => "Use owner/repo#N for an issue, or internal:N for an internal task.".to_string(),
    }
}

fn dedup_cli_refs(rows: Vec<sqlx::sqlite::SqliteRow>) -> anyhow::Result<Vec<CliTaskRef>> {
    let mut seen = Vec::new();
    let mut out = Vec::new();
    for row in rows {
        let task = CliTaskRef::from_row(&row)?;
        if let Some(id) = task.store_id {
            if seen.contains(&id) {
                continue;
            }
            seen.push(id);
        }
        out.push(task);
    }
    Ok(out)
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

    /// Append a lifecycle activity event for a task within a transaction.
    #[doc(hidden)]
    #[expect(
        clippy::too_many_arguments,
        reason = "one argument per column of the activity row"
    )]
    pub async fn append_activity_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
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
        .execute(&mut **tx)
        .await?;
        Ok(())
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
                apply::set_fields(&mut tx, &crate::cluster::now_stamp(), id, &local, true).await?;
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

    /// Reset a task back to `new`.
    pub async fn reset_to_new(&self, id: i64) -> anyhow::Result<()> {
        self.write(Command::TaskResetToNew { id }).await.map(|_| ())
    }

    /// Update the block reason for a task.
    pub async fn set_block_reason(&self, id: i64, reason: Option<&str>) -> anyhow::Result<()> {
        let value = reason.map_or(serde_json::Value::Null, |r| {
            serde_json::Value::String(r.to_string())
        });
        self.set_fields(id, &[("block_reason", value)]).await
    }

    /// List tasks by status within a repo.
    pub async fn list_by_status(
        &self,
        repo: &str,
        status: TaskStatus,
    ) -> anyhow::Result<Vec<Task>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {TASK_COLS} FROM tasks WHERE repo = ? AND status = ? ORDER BY created_at DESC"
        )))
        .bind(repo)
        .bind(status.as_str())
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(Self::row_to_task).collect()
    }

    /// List tasks that are linked to the given parent task.
    pub async fn list_children(&self, repo: &str, parent_id: i64) -> anyhow::Result<Vec<Task>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {TASK_COLS} FROM tasks WHERE repo = ? AND parent_id = ? ORDER BY created_at ASC"
        )))
        .bind(repo)
        .bind(parent_id)
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
            sqlx::AssertSqlSafe(format!("SELECT {TASK_COLS} FROM tasks WHERE repo = ? AND origin != 'internal' AND status = ? ORDER BY created_at DESC")),
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
        sqlx::AssertSqlSafe(format!("SELECT {TASK_COLS} FROM tasks WHERE repo = ? AND origin = 'internal' AND status = ? ORDER BY created_at DESC")),
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
            sqlx::AssertSqlSafe(format!("SELECT {TASK_COLS} FROM tasks WHERE repo = ? AND origin = 'internal' ORDER BY created_at DESC")),
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
            sqlx::AssertSqlSafe(format!("SELECT {TASK_COLS} FROM tasks WHERE repo = ? AND origin = 'internal' AND source = ? ORDER BY created_at DESC")),
        )
        .bind(repo)
        .bind(source)
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(Self::row_to_task).collect()
    }

    /// Return only the `source_id` values for internal tasks with a specific source,
    /// created within the last `since_days` days.
    /// Use this instead of `list_internal_by_source` when only deduplication is needed —
    /// avoids fetching all 57 task columns and bounds memory usage to a rolling window.
    pub async fn list_source_ids_by_source(
        &self,
        repo: &str,
        source: &str,
        since_days: i64,
    ) -> anyhow::Result<Vec<String>> {
        let cutoff = format!("-{since_days} days");
        let rows = sqlx::query(
            "SELECT source_id FROM tasks WHERE repo = ? AND origin = 'internal' AND source = ? AND created_at >= datetime('now', ?)",
        )
        .bind(repo)
        .bind(source)
        .bind(&cutoff)
        .fetch_all(&self.pool)
        .await?;

        let mut ids = Vec::with_capacity(rows.len());
        for row in &rows {
            ids.push(row.try_get::<String, _>("source_id").map_err(|e| {
                anyhow::anyhow!(
                    "list_source_ids_by_source: failed to decode source_id for repo={repo}, source={source}: {e}"
                )
            })?);
        }
        Ok(ids)
    }

    /// List all active (non-done) tasks for a repo.
    pub async fn list_active(&self, repo: &str) -> anyhow::Result<Vec<Task>> {
        let rows = sqlx::query(
            sqlx::AssertSqlSafe(format!("SELECT {TASK_COLS} FROM tasks WHERE repo = ? AND status != 'done' ORDER BY created_at DESC")),
        )
        .bind(repo)
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(Self::row_to_task).collect()
    }

    /// List all external tasks for a repo (origin != 'internal').
    pub async fn list_all_external(&self, repo: &str) -> anyhow::Result<Vec<Task>> {
        let rows = sqlx::query(
            sqlx::AssertSqlSafe(format!("SELECT {TASK_COLS} FROM tasks WHERE repo = ? AND origin != 'internal' ORDER BY created_at DESC")),
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

    /// List tasks for `orch service doctor`: active tasks + done tasks updated since `cutoff_str`.
    /// Much cheaper than `list_all` on repos with many historical done tasks.
    pub async fn list_for_doctor(&self, repo: &str, cutoff_str: &str) -> anyhow::Result<Vec<Task>> {
        let rows = sqlx::query(
            sqlx::AssertSqlSafe(format!("SELECT {TASK_COLS} FROM tasks WHERE repo = ? AND (status != 'done' OR updated_at >= ?) ORDER BY created_at DESC")),
        )
        .bind(repo)
        .bind(cutoff_str)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::row_to_task).collect()
    }

    /// List all tasks for a repo, ordered by creation time descending.
    pub async fn list_all(&self, repo: &str) -> anyhow::Result<Vec<Task>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {TASK_COLS} FROM tasks WHERE repo = ? ORDER BY created_at DESC"
        )))
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
        let mut repos = Vec::new();
        for row in &rows {
            repos.push(row.try_get::<String, _>("repo").map_err(|e| {
                anyhow::anyhow!("distinct_repos: failed to decode 'repo' column: {e}")
            })?);
        }
        Ok(repos)
    }

    /// List all active (non-done) tasks across all repos.
    ///
    /// Used by the CLI when no project context is available (e.g. running from
    /// a worktree without a `.orch.yml`). Does not require a repo argument.
    pub async fn list_all_active_global(&self) -> anyhow::Result<Vec<Task>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {TASK_COLS} FROM tasks WHERE status != 'done' ORDER BY created_at DESC"
        )))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::row_to_task).collect()
    }

    /// List all tasks across all repos regardless of status.
    ///
    /// Used by the CLI when `--status all` is passed.
    pub async fn list_all_global(&self) -> anyhow::Result<Vec<Task>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {TASK_COLS} FROM tasks ORDER BY created_at DESC"
        )))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Self::row_to_task).collect()
    }

    /// List all active tasks matching an optional status filter, across all repos.
    ///
    /// Used by the CLI fallback path when no project context is available.
    pub async fn list_all_by_status_global(&self, status: TaskStatus) -> anyhow::Result<Vec<Task>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {TASK_COLS} FROM tasks WHERE status = ? ORDER BY created_at DESC"
        )))
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
        self.set_fields_split(id, updates, false).await
    }

    /// Like `set_fields` but does NOT touch `updated_at`.
    ///
    /// Use this for internal bookkeeping writes (probe timestamps, counters) that
    /// must not make a stale task appear recently-modified to triage queries.
    pub async fn set_fields_silent(
        &self,
        id: i64,
        updates: &[(&str, serde_json::Value)],
    ) -> anyhow::Result<()> {
        self.set_fields_split(id, updates, true).await
    }

    /// Local columns are written on this node only, the rest go through the log
    async fn set_fields_split(
        &self,
        id: i64,
        updates: &[(&str, serde_json::Value)],
        silent: bool,
    ) -> anyhow::Result<()> {
        let all = owned_updates(updates)?;
        // On one machine nothing is replicated, so the whole update stays one statement
        if !self.replicates() {
            let mut tx = self.begin_write().await?;
            apply::set_fields(&mut tx, &crate::cluster::now_stamp(), id, &all, silent).await?;
            tx.commit().await?;
            return Ok(());
        }

        let (local, replicated): (Vec<_>, Vec<_>) =
            all.into_iter().partition(|(col, _)| is_local_column(col));
        if !local.is_empty() {
            let mut tx = self.begin_write().await?;
            apply::set_fields(&mut tx, &crate::cluster::now_stamp(), id, &local, silent).await?;
            tx.commit().await?;
        }

        if replicated.is_empty() {
            return Ok(());
        }

        self.write(Command::TaskSetFields {
            id,
            updates: replicated,
            silent,
        })
        .await
        .map(|_| ())
    }

    /// Increment an integer field by 1, returning the new value.
    pub async fn increment(&self, id: i64, field: &str) -> anyhow::Result<i32> {
        anyhow::ensure!(
            INCREMENTABLE_FIELDS.contains(&field),
            "field {field} is not incrementable"
        );
        let n = self
            .write_int(Command::TaskIncrement {
                id,
                field: field.to_string(),
            })
            .await?;
        Ok(i32::try_from(n)?)
    }

    /// Reset all failure/retry counters to zero.
    pub async fn reset_counters(&self, id: i64) -> anyhow::Result<()> {
        self.write(Command::TaskResetCounters {
            id,
            failure_only: false,
        })
        .await
        .map(|_| ())
    }

    /// Reset transient failure/retry counters to zero, preserving
    /// `review_cycles`, `ci_merge_failures`, and `merge_conflict_retries`.
    ///
    /// Use this after `RequestChanges` to clear per-attempt noise without
    /// undoing the review-cycle count that `handle_review_changes` just set.
    /// `ci_merge_failures` and `merge_conflict_retries` are preserved here because
    /// they must accumulate across attempts to enforce their circuit breakers,
    /// just like `review_cycles`.
    ///
    /// NOTE: `attempts` and `review_invocations` are intentionally NOT reset here.
    /// Both must increase monotonically across the task's lifetime so that
    /// `(task_id, attempt, run_type)` keys in `task_runs` remain unique. Resetting
    /// either causes subsequent retries to overwrite earlier audit trail records via
    /// the ON CONFLICT UPSERT in `start_run()`.
    pub async fn reset_failure_counters(&self, id: i64) -> anyhow::Result<()> {
        self.write(Command::TaskResetCounters {
            id,
            failure_only: true,
        })
        .await
        .map(|_| ())
    }

    // ---------------------------------------------------------------
    // Routing
    // ---------------------------------------------------------------

    /// Store routing results for a task.
    pub async fn store_route(&self, route: &StoreRoute<'_>) -> anyhow::Result<()> {
        self.write(Command::TaskStoreRoute(StoreRouteCmd {
            id: route.id,
            agent: route.agent.to_string(),
            model: route.model.map(str::to_string),
            complexity: route.complexity.to_string(),
            estimate: route.estimate,
            reason: route.reason.to_string(),
            profile: route.profile.to_string(),
            skills: route.skills.to_string(),
        }))
        .await
        .map(|_| ())
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
            if let Ok(estimate @ 1..) = u8::try_from(estimate_raw) {
                result.push((external_id, estimate));
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
        self.write_bool(Command::TaskSetEstimateIfZero { id, estimate })
            .await
    }

    // ---------------------------------------------------------------
    // Memory
    // ---------------------------------------------------------------

    /// Append a memory entry to the task's memory JSON array.
    ///
    /// Uses a transaction to prevent lost-update races when concurrent
    /// callers append simultaneously.
    pub async fn append_memory(&self, id: i64, entry: &MemoryEntry) -> anyhow::Result<()> {
        self.write(Command::TaskAppendMemory {
            id,
            entry: entry.clone(),
        })
        .await
        .map(|_| ())
    }

    /// Get the N most recent memory entries for a task.
    pub async fn recent_memory(&self, id: i64, max: usize) -> anyhow::Result<Vec<MemoryEntry>> {
        let row = sqlx::query("SELECT memory FROM tasks WHERE id = ?")
            .bind(id)
            .fetch_one(&self.pool)
            .await?;

        let memory_opt: Option<String> = row
            .try_get::<Option<String>, _>("memory")
            .map_err(|e| anyhow::anyhow!("failed to read memory column for task {id}: {e}"))?;
        let memory_str = memory_opt.unwrap_or_default();

        let mut memory: Vec<MemoryEntry> = if memory_str.trim().is_empty() {
            Vec::new()
        } else {
            serde_json::from_str(&memory_str)
                .map_err(|e| anyhow::anyhow!("failed to parse memory JSON for task {id}: {e}"))?
        };

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
        .bind(i64::try_from(input).unwrap_or_else(|_| {
            tracing::warn!(input, "input_tokens overflows i64, clamping to MAX");
            i64::MAX
        }))
        .bind(i64::try_from(output).unwrap_or_else(|_| {
            tracing::warn!(output, "output_tokens overflows i64, clamping to MAX");
            i64::MAX
        }))
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
        self.write(Command::TaskMarkCleaned { id })
            .await
            .map(|_| ())
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

        let mut tx = self.begin_write().await?;
        for (id, entry_updates) in updates {
            if entry_updates.is_empty() {
                continue;
            }
            let mut set_parts = Vec::new();
            let mut values: Vec<Option<String>> = Vec::new();
            for (col, val) in *entry_updates {
                set_parts.push(format!("{col} = ?"));
                values.push(sql_value_for_task_field(col, val)?);
            }
            set_parts.push("updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')".to_string());
            let sql = format!("UPDATE tasks SET {} WHERE id = ?", set_parts.join(", "));
            let mut query = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()));
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

        let mut tx = self.begin_write().await?;
        for (id, field) in entries {
            let sql = format!(
                "UPDATE tasks SET {field} = {field} + 1, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ?"
            );
            sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
                .bind(*id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Reset transient failure/retry counters for multiple tasks in a single transaction.
    ///
    /// Preserves `review_cycles`, `ci_merge_failures`, and
    /// `merge_conflict_retries` (same semantics as
    /// [`reset_failure_counters`]).
    /// Test-only helper for bulk counter resets used by tests.
    #[cfg(test)]
    pub async fn batch_reset_failure_counters(&self, ids: &[i64]) -> anyhow::Result<()> {
        // SQLite has a hard limit on host parameters (commonly 999). Chunk the
        // IN-list into smaller batches to avoid exceeding the limit.
        const CHUNK_SIZE: usize = 500;
        if ids.is_empty() {
            return Ok(());
        }

        let mut tx = self.begin_write().await?;

        for chunk in ids.chunks(CHUNK_SIZE) {
            // Build WHERE id IN (?, ?, ...) placeholders for this chunk
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
            let sql = format!(
                "UPDATE tasks SET
            route_attempts = 0,
            review_agent_failures = 0,
            review_resets = 0,
            review_invocations = 0,
            pr_create_failures = 0,
            push_failures = 0,
            network_retries = 0,
            auto_unblock_count = 0,
            auto_unblock_last_at = '',
            auto_unblock_last_reason = '',
            ci_recovery_count = 0,
            no_code_reroutes = 0,
            needs_review_refires = 0,
            updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
         WHERE id IN ({placeholders})",
            );
            let mut query = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()));
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
        // Chunk updates to avoid exceeding SQLite's parameter limit.
        const CHUNK_SIZE: usize = 500;
        if ids.is_empty() {
            return Ok(());
        }

        let mut tx = self.begin_write().await?;

        for chunk in ids.chunks(CHUNK_SIZE) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
            let sql = format!(
                "UPDATE tasks SET worktree_cleaned = 1, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id IN ({placeholders})",
            );
            let mut query = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()));
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
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {TASK_COLS} FROM tasks
         WHERE repo = ?
           AND worktree != ''
           AND worktree_cleaned = 0
           AND status IN ('done', 'blocked')
         ORDER BY updated_at ASC"
        )))
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
        // Self-heal: an earlier attempt of the same run_type may have been
        // abandoned by a crash or SIGKILL that never reached
        // complete_run/finalize_incomplete_runs. Close those out now so they
        // don't leak as permanently-open rows. Scoped to the same run_type so
        // a still-open run of a *different* run_type (e.g. an agent run left
        // open when a review run starts) is untouched here — that case is
        // handled by the explicit finalize_incomplete_runs calls in the
        // engine's recovery paths, not this opportunistic self-heal. The
        // current attempt is excluded so a legitimate retry of the same run
        // still goes through the ON CONFLICT upsert below instead of being
        // finalized out from under itself.
        let orphaned: Vec<(i64,)> = sqlx::query_as(
            "SELECT id FROM task_runs
             WHERE task_id = ? AND run_type = ? AND attempt != ?
               AND outcome IS NULL AND completed_at IS NULL",
        )
        .bind(run.task_id)
        .bind(run.run_type)
        .bind(run.attempt)
        .fetch_all(&self.pool)
        .await?;

        for (run_id,) in orphaned {
            self.complete_run(&CompleteRun {
                run_id,
                exit_code: Some(-1),
                stdout: "",
                stderr: "",
                parsed: "",
                outcome: "aborted",
                error: "superseded by a new run start before this one completed",
                tokens: RunTokenUsage::default(),
            })
            .await?;
        }

        let row = sqlx::query(
            "INSERT INTO task_runs (task_id, attempt, run_type, agent, model, command, prompt, outcome)
         VALUES (?, ?, ?, ?, ?, ?, ?, NULL)
         ON CONFLICT(task_id, attempt, run_type) DO UPDATE SET
            agent = excluded.agent, model = excluded.model,
            command = excluded.command, prompt = excluded.prompt,
            outcome = NULL,
            started_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
            completed_at = NULL, error = NULL, exit_code = NULL,
            stdout = NULL, stderr = NULL, parsed_response = NULL,
            duration_secs = NULL, input_tokens = NULL, output_tokens = NULL,
            total_cost_usd = NULL
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
            duration_secs = CASE WHEN ? > 0 THEN ?
                ELSE MAX(0, (julianday('now') - julianday(started_at)) * 86400.0) END,
            completed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
         WHERE id = ?",
        )
        .bind(run.exit_code)
        .bind(run.stdout)
        .bind(security::redact(run.stderr))
        .bind(run.parsed)
        .bind(run.outcome)
        .bind(security::redact(run.error))
        .bind(i64::try_from(run.tokens.input_tokens).unwrap_or_else(|_| {
            tracing::warn!(
                run.tokens.input_tokens,
                "input_tokens overflows i64, clamping to MAX"
            );
            i64::MAX
        }))
        .bind(i64::try_from(run.tokens.output_tokens).unwrap_or_else(|_| {
            tracing::warn!(
                run.tokens.output_tokens,
                "output_tokens overflows i64, clamping to MAX"
            );
            i64::MAX
        }))
        .bind(run.tokens.total_cost_usd)
        .bind(run.tokens.duration_secs)
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

    /// Finalize all incomplete runs for a task.
    ///
    /// This is used on runner early-return/error paths and engine shutdown
    /// paths to ensure every `start_run` has a terminal record.
    pub async fn finalize_incomplete_runs(
        &self,
        task_id: i64,
        outcome: &str,
        error: &str,
    ) -> anyhow::Result<u64> {
        let rows: Vec<(i64,)> = sqlx::query_as(
            "SELECT id FROM task_runs
             WHERE task_id = ?
               AND outcome IS NULL
               AND completed_at IS NULL
             ORDER BY id ASC",
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await?;

        for (run_id,) in &rows {
            self.complete_run(&CompleteRun {
                run_id: *run_id,
                exit_code: Some(-1),
                stdout: "",
                stderr: "",
                parsed: "",
                outcome,
                error,
                tokens: RunTokenUsage::default(),
            })
            .await?;
        }

        Ok(rows.len() as u64)
    }

    /// Get all runs for a task, ordered by attempt.
    pub async fn get_runs(&self, task_id: i64) -> anyhow::Result<Vec<TaskRun>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {TASK_RUN_COLS} FROM task_runs WHERE task_id = ? ORDER BY attempt ASC, run_type ASC"
        )))
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
        // SQLite has a hard limit on host parameters (commonly 999). Chunk the
        // IN-list into smaller batches to avoid exceeding the limit.
        const CHUNK_SIZE: usize = 500;
        if task_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        let mut result: std::collections::HashMap<i64, Vec<TaskRun>> =
            std::collections::HashMap::new();

        for chunk in task_ids.chunks(CHUNK_SIZE) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
            let sql = format!(
                "SELECT {TASK_RUN_COLS} FROM task_runs WHERE task_id IN ({placeholders}) ORDER BY task_id ASC, attempt ASC, run_type ASC"
            );
            let mut query = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()));
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
        // SQLite has a hard limit on host parameters (commonly 999). Chunk the
        // IN-list into smaller batches to avoid exceeding the limit.
        const CHUNK_SIZE: usize = 500;
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        let mut result = std::collections::HashMap::new();

        for chunk in ids.chunks(CHUNK_SIZE) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
            let sql = format!("SELECT {TASK_COLS} FROM tasks WHERE id IN ({placeholders})");
            let mut query = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()));
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
        const CHUNK_SIZE: usize = 500;
        let mut result = std::collections::HashMap::new();
        if ext_ids.is_empty() {
            return Ok(result);
        }

        for chunk in ext_ids.chunks(CHUNK_SIZE) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
            let sql = format!(
                "SELECT external_id, status FROM tasks WHERE repo = ? AND external_id IN ({placeholders})"
            );
            let mut query = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()));
            query = query.bind(repo);
            for id in chunk {
                query = query.bind(id);
            }
            let rows = query.fetch_all(&self.pool).await?;
            for row in &rows {
                let external_id: Result<String, _> = row.try_get("external_id");
                let status_str: Result<String, _> = row.try_get("status");
                match (external_id, status_str) {
                    (Ok(ext_id), Ok(sts)) => {
                        if ext_id.is_empty() {
                            tracing::warn!(status = %sts, "skipping task row with empty external_id");
                            continue;
                        }
                        if let Some(status) = TaskStatus::from_str(&sts) {
                            result.insert(ext_id, status);
                        } else {
                            tracing::warn!(external_id = %ext_id, status = %sts, "unknown status value in tasks table");
                        }
                    }
                    (Err(e), _) => {
                        tracing::warn!(error = %e, "skipping task row with decode error for external_id");
                    }
                    (_, Err(e)) => {
                        tracing::warn!(error = %e, "skipping task row with decode error for status");
                    }
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
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {TASK_RUN_COLS} FROM task_runs WHERE task_id = ? AND run_type = ? ORDER BY attempt DESC LIMIT 1"
    )))
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
            review_agent_failures: row.try_get("review_agent_failures").unwrap_or(0),
            review_resets: row.try_get::<i32, _>("review_resets").unwrap_or(0),
            review_cycles: row.try_get("review_cycles").unwrap_or(0),
            review_invocations: row.try_get::<i32, _>("review_invocations").unwrap_or(0),
            needs_review_refires: row.try_get::<i32, _>("needs_review_refires").unwrap_or(0),
            input_tokens: row.try_get("input_tokens").unwrap_or(0),
            output_tokens: row.try_get("output_tokens").unwrap_or(0),
            input_cost_usd: row.try_get("input_cost_usd").unwrap_or(0.0),
            output_cost_usd: row.try_get("output_cost_usd").unwrap_or(0.0),
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

    /// Resolve a task_id globally (across all repos) when a repo-scoped lookup fails.
    ///
    /// Returns `(internal_id, repo)` if found unambiguously, `None` if not found,
    /// or an error if the external_id matches tasks in multiple repos (GitHub issue
    /// numbers are not globally unique).
    pub async fn resolve_task_id_globally(
        &self,
        task_id: &str,
    ) -> anyhow::Result<Option<(i64, String)>> {
        if let Some(suffix) = task_id.strip_prefix("internal:") {
            // internal:N — external_id is globally unique; primary key is also unique.
            let row = sqlx::query("SELECT id, repo FROM tasks WHERE external_id = ?")
                .bind(task_id)
                .fetch_optional(&self.pool)
                .await?;
            if let Some(r) = row {
                return Ok(Some((r.try_get("id")?, r.try_get("repo")?)));
            }
            if let Ok(id) = suffix.parse::<i64>() {
                let row = sqlx::query("SELECT id, repo FROM tasks WHERE id = ?")
                    .bind(id)
                    .fetch_optional(&self.pool)
                    .await?;
                if let Some(r) = row {
                    return Ok(Some((r.try_get("id")?, r.try_get("repo")?)));
                }
            }
            return Ok(None);
        }
        // External IDs (e.g. GitHub issue numbers) may collide across repos.
        let rows = sqlx::query("SELECT id, repo FROM tasks WHERE external_id = ?")
            .bind(task_id)
            .fetch_all(&self.pool)
            .await?;
        match rows.len() {
            0 => Ok(None),
            1 => Ok(Some((rows[0].try_get("id")?, rows[0].try_get("repo")?))),
            _ => {
                let repos: Vec<String> = rows
                    .iter()
                    .filter_map(|r| r.try_get::<String, _>("repo").ok())
                    .collect();
                anyhow::bail!(
                    "task ID {task_id:?} is ambiguous — found in multiple repos: {}. \
                     Run from the repo worktree so the current-repo lookup resolves it.",
                    repos.join(", ")
                )
            }
        }
    }

    /// Resolve a CLI task id. A bare number in a project is that project's issue first
    pub async fn resolve_cli_id(
        &self,
        spec: &str,
        current_repo: Option<&str>,
    ) -> anyhow::Result<Option<CliTaskRef>> {
        let current_repo = current_repo.filter(|repo| !repo.is_empty());
        match parse_cli_task_spec(spec)? {
            CliTaskSpec::Internal(number) => self.lookup_internal_cli_id(number).await,
            CliTaskSpec::Issue { repo, number } => self.lookup_issue_cli_id(&repo, number).await,
            CliTaskSpec::Bare(number) => {
                if let Some(repo) = current_repo {
                    if let Some(task) = self.lookup_issue_cli_id(repo, number).await? {
                        return Ok(Some(task));
                    }
                    if let Some(task) = self.lookup_internal_in_repo(number, repo).await? {
                        return Ok(Some(task));
                    }
                    return Ok(None);
                }
                let mut matches = self.lookup_bare_cli_id_matches(number).await?;
                match matches.len() {
                    0 => Ok(None),
                    1 => Ok(matches.pop()),
                    _ => Err(ambiguous_cli_id_error(spec, &matches)),
                }
            }
        }
    }

    async fn lookup_internal_cli_id(&self, number: i64) -> anyhow::Result<Option<CliTaskRef>> {
        let ext = format!("internal:{number}");
        let rows = sqlx::query(
            "SELECT id, repo, origin, title, external_id, status FROM tasks \
             WHERE origin = 'internal' AND (id = ? OR external_id = ?) \
             ORDER BY id",
        )
        .bind(number)
        .bind(&ext)
        .fetch_all(&self.pool)
        .await?;
        let mut matches = dedup_cli_refs(rows)?;
        match matches.len() {
            0 => Ok(None),
            1 => Ok(matches.pop()),
            _ => Err(ambiguous_cli_id_error(&ext, &matches)),
        }
    }

    async fn lookup_internal_in_repo(
        &self,
        number: i64,
        repo: &str,
    ) -> anyhow::Result<Option<CliTaskRef>> {
        let row = sqlx::query(
            "SELECT id, repo, origin, title, external_id, status FROM tasks \
             WHERE origin = 'internal' AND id = ? AND repo = ?",
        )
        .bind(number)
        .bind(repo)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(row) => Ok(Some(CliTaskRef::from_row(&row)?)),
            None => Ok(None),
        }
    }

    async fn lookup_issue_cli_id(
        &self,
        repo: &str,
        number: i64,
    ) -> anyhow::Result<Option<CliTaskRef>> {
        let ext = number.to_string();
        let rows = sqlx::query(
            "SELECT id, repo, origin, title, external_id, status FROM tasks \
             WHERE repo = ? AND origin != 'internal' AND external_id = ? \
             ORDER BY id",
        )
        .bind(repo)
        .bind(&ext)
        .fetch_all(&self.pool)
        .await?;
        let mut matches = dedup_cli_refs(rows)?;
        match matches.len() {
            0 => Ok(None),
            1 => Ok(matches.pop()),
            _ => Err(ambiguous_cli_id_error(
                &format!("{repo}#{number}"),
                &matches,
            )),
        }
    }

    async fn lookup_bare_cli_id_matches(&self, number: i64) -> anyhow::Result<Vec<CliTaskRef>> {
        let ext = number.to_string();
        let rows = sqlx::query(
            "SELECT id, repo, origin, title, external_id, status FROM tasks \
             WHERE (origin = 'internal' AND id = ?) \
                OR (origin != 'internal' AND external_id = ?) \
             ORDER BY repo, id",
        )
        .bind(number)
        .bind(&ext)
        .fetch_all(&self.pool)
        .await?;
        dedup_cli_refs(rows)
    }

    fn row_to_run(row: &sqlx::sqlite::SqliteRow) -> anyhow::Result<TaskRun> {
        Ok(TaskRun {
            id: row
                .try_get("id")
                .map_err(|e| anyhow::anyhow!("run row decode error for id: {e}"))?,
            task_id: row
                .try_get("task_id")
                .map_err(|e| anyhow::anyhow!("run row decode error for task_id: {e}"))?,
            attempt: row
                .try_get("attempt")
                .map_err(|e| anyhow::anyhow!("run row decode error for attempt: {e}"))?,
            run_type: row
                .try_get("run_type")
                .map_err(|e| anyhow::anyhow!("run row decode error for run_type: {e}"))?,
            agent: row
                .try_get("agent")
                .map_err(|e| anyhow::anyhow!("run row decode error for agent: {e}"))?,
            model: row
                .try_get("model")
                .map_err(|e| anyhow::anyhow!("run row decode error for model: {e}"))?,
            command: row
                .try_get("command")
                .map_err(|e| anyhow::anyhow!("run row decode error for command: {e}"))?,
            prompt: row
                .try_get("prompt")
                .map_err(|e| anyhow::anyhow!("run row decode error for prompt: {e}"))?,
            env_vars: row
                .try_get("env_vars")
                .map_err(|e| anyhow::anyhow!("run row decode error for env_vars: {e}"))?,
            exit_code: row
                .try_get("exit_code")
                .map_err(|e| anyhow::anyhow!("run row decode error for exit_code: {e}"))?,
            stdout: row
                .try_get("stdout")
                .map_err(|e| anyhow::anyhow!("run row decode error for stdout: {e}"))?,
            stderr: row
                .try_get("stderr")
                .map_err(|e| anyhow::anyhow!("run row decode error for stderr: {e}"))?,
            parsed_response: row
                .try_get("parsed_response")
                .map_err(|e| anyhow::anyhow!("run row decode error for parsed_response: {e}"))?,
            outcome: row
                .try_get("outcome")
                .map_err(|e| anyhow::anyhow!("run row decode error for outcome: {e}"))?,
            error: row
                .try_get("error")
                .map_err(|e| anyhow::anyhow!("run row decode error for error: {e}"))?,
            input_tokens: row
                .try_get("input_tokens")
                .map_err(|e| anyhow::anyhow!("run row decode error for input_tokens: {e}"))?,
            output_tokens: row
                .try_get("output_tokens")
                .map_err(|e| anyhow::anyhow!("run row decode error for output_tokens: {e}"))?,
            total_cost_usd: row
                .try_get("total_cost_usd")
                .map_err(|e| anyhow::anyhow!("run row decode error for total_cost_usd: {e}"))?,
            duration_secs: row
                .try_get("duration_secs")
                .map_err(|e| anyhow::anyhow!("run row decode error for duration_secs: {e}"))?,
            started_at: row
                .try_get("started_at")
                .map_err(|e| anyhow::anyhow!("run row decode error for started_at: {e}"))?,
            completed_at: row.try_get("completed_at").unwrap_or(None),
        })
    }

    fn row_to_activity(row: &sqlx::sqlite::SqliteRow) -> anyhow::Result<TaskActivity> {
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
    pub skills: String,
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

/// The SQL behind each replicated task write, run with the proposer's time
pub(crate) mod apply {
    use super::{
        sanitize_activity_details, sql_value_for_task_field, MemoryEntry, NewTask, Row,
        StoreRouteCmd, TaskStatus, UpsertExternalCmd, ALLOWED_FIELDS, INCREMENTABLE_FIELDS,
    };
    use sqlx::SqliteConnection;

    async fn previous(
        conn: &mut SqliteConnection,
        id: i64,
    ) -> anyhow::Result<(String, Option<String>, Option<String>)> {
        let row = sqlx::query("SELECT status, agent, model FROM tasks WHERE id = ?")
            .bind(id)
            .fetch_one(&mut *conn)
            .await?;
        let status: String = row
            .try_get("status")
            .map_err(|e| anyhow::anyhow!("failed to read status column before update: {e}"))?;
        let agent: Option<String> = row
            .try_get("agent")
            .map_err(|e| anyhow::anyhow!("failed to read agent column before update: {e}"))?;
        let model: Option<String> = row
            .try_get("model")
            .map_err(|e| anyhow::anyhow!("failed to read model column before update: {e}"))?;
        Ok((status, agent, model))
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one argument per column of the activity row"
    )]
    pub(crate) async fn activity(
        conn: &mut SqliteConnection,
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
        .execute(conn)
        .await?;
        Ok(())
    }

    async fn status_change(
        conn: &mut SqliteConnection,
        id: i64,
        previous: &(String, Option<String>, Option<String>),
        to: TaskStatus,
        details: Option<&serde_json::Value>,
    ) -> anyhow::Result<()> {
        activity(
            conn,
            id,
            "status_change",
            Some(previous.0.as_str()),
            Some(to.as_str()),
            previous.1.as_deref(),
            previous.2.as_deref(),
            details,
        )
        .await
    }

    /// Through Raft, ids come from the replicated rows so every node assigns the same one.
    /// Otherwise NULL lets SQLite's AUTOINCREMENT pick it, as before
    pub(crate) async fn create(
        conn: &mut SqliteConnection,
        now: &str,
        new: &NewTask,
        internal: bool,
        replicated: bool,
    ) -> anyhow::Result<i64> {
        let labels_json = serde_json::to_string(&new.labels)?;
        let row = sqlx::query(
            "INSERT INTO tasks (id, external_id, repo, origin, title, body, source, source_id, author, url, labels, parent_id, created_at, updated_at)
             VALUES ((SELECT CASE WHEN ? THEN COALESCE(MAX(id), 0) + 1 END FROM tasks), ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             RETURNING id",
        )
        .bind(replicated)
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
        .bind(now)
        .bind(now)
        .fetch_one(&mut *conn)
        .await?;
        let id: i64 = row
            .try_get("id")
            .map_err(|e| anyhow::anyhow!("INSERT RETURNING id returned NULL or invalid: {e}"))?;
        if internal {
            // resolve_task_id finds internal tasks by this external id
            sqlx::query("UPDATE tasks SET external_id = ? WHERE id = ?")
                .bind(format!("internal:{id}"))
                .bind(id)
                .execute(conn)
                .await?;
        }

        Ok(id)
    }

    pub(crate) async fn upsert_external(
        conn: &mut SqliteConnection,
        now: &str,
        ext: &UpsertExternalCmd,
        replicated: bool,
    ) -> anyhow::Result<i64> {
        let labels_json = serde_json::to_string(&ext.labels)?;
        let row = sqlx::query(
            "INSERT INTO tasks (id, external_id, repo, origin, title, body, author, url, labels, parent_id, created_at, updated_at)
             VALUES ((SELECT CASE WHEN ? THEN COALESCE(MAX(id), 0) + 1 END FROM tasks), ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(repo, external_id) DO UPDATE SET
                title = excluded.title,
                body = excluded.body,
                labels = excluded.labels,
                url = excluded.url,
                parent_id = COALESCE(excluded.parent_id, tasks.parent_id)
             RETURNING id",
        )
        .bind(replicated)
        .bind(&ext.ext_id)
        .bind(&ext.repo)
        .bind(&ext.origin)
        .bind(&ext.title)
        .bind(&ext.body)
        .bind(&ext.author)
        .bind(&ext.url)
        .bind(&labels_json)
        .bind(ext.parent_id)
        .bind(now)
        .bind(now)
        .fetch_one(conn)
        .await?;
        row.try_get("id")
            .map_err(|e| anyhow::anyhow!("INSERT RETURNING id returned NULL or invalid: {e}"))
    }

    fn ensure_allowed(updates: &[(String, serde_json::Value)]) -> anyhow::Result<()> {
        for (col, _) in updates {
            anyhow::ensure!(
                ALLOWED_FIELDS.contains(&col.as_str()),
                "column {col} is not in the update allowlist"
            );
        }

        Ok(())
    }

    pub(crate) async fn update_status_and_fields(
        conn: &mut SqliteConnection,
        now: &str,
        id: i64,
        status: TaskStatus,
        updates: &[(String, serde_json::Value)],
    ) -> anyhow::Result<()> {
        ensure_allowed(updates)?;
        let prev = previous(conn, id).await?;

        let mut set_parts = vec!["status = ?".to_string()];
        let mut values: Vec<Option<String>> = vec![Some(status.as_str().to_string())];
        let mut block_reason_in_updates = false;
        for (col, val) in updates {
            if col == "block_reason" {
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

        set_parts.push("updated_at = ?".to_string());
        values.push(Some(now.to_string()));
        let sql = format!("UPDATE tasks SET {} WHERE id = ?", set_parts.join(", "));
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()));
        for v in &values {
            query = query.bind(v.as_deref());
        }
        query.bind(id).execute(&mut *conn).await?;
        status_change(conn, id, &prev, status, None).await
    }

    /// Moves the task to `status`, and when `expected` is set only if it still holds that status
    pub(crate) async fn update_status(
        conn: &mut SqliteConnection,
        now: &str,
        id: i64,
        status: TaskStatus,
        expected: Option<TaskStatus>,
    ) -> anyhow::Result<bool> {
        let prev = previous(conn, id).await?;
        let clear_block = if status == TaskStatus::Blocked {
            ""
        } else {
            ", block_reason = NULL"
        };
        let guard = if expected.is_some() {
            " AND status = ?"
        } else {
            ""
        };
        let sql =
            format!("UPDATE tasks SET status = ?{clear_block}, updated_at = ? WHERE id = ?{guard}");
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
            .bind(status.as_str())
            .bind(now)
            .bind(id);
        if let Some(expected) = expected {
            query = query.bind(expected.as_str());
        }

        if query.execute(&mut *conn).await?.rows_affected() == 0 {
            return Ok(false);
        }

        status_change(conn, id, &prev, status, None).await?;
        Ok(true)
    }

    pub(crate) async fn update_external_id(
        conn: &mut SqliteConnection,
        now: &str,
        id: i64,
        external_id: &str,
    ) -> anyhow::Result<()> {
        sqlx::query("UPDATE tasks SET external_id = ?, updated_at = ? WHERE id = ?")
            .bind(external_id)
            .bind(now)
            .bind(id)
            .execute(conn)
            .await?;
        Ok(())
    }

    pub(crate) async fn touch(
        conn: &mut SqliteConnection,
        now: &str,
        id: i64,
    ) -> anyhow::Result<()> {
        sqlx::query("UPDATE tasks SET updated_at = ? WHERE id = ?")
            .bind(now)
            .bind(id)
            .execute(conn)
            .await?;
        Ok(())
    }

    pub(crate) async fn reset_to_new(
        conn: &mut SqliteConnection,
        now: &str,
        id: i64,
    ) -> anyhow::Result<()> {
        let prev = previous(conn, id).await?;
        sqlx::query(
            "UPDATE tasks SET status = 'new', branch = '', worktree = '', worktree_cleaned = 0, block_reason = NULL, updated_at = ? WHERE id = ?",
        )
        .bind(now)
        .bind(id)
        .execute(&mut *conn)
        .await?;
        let details = serde_json::json!({ "op": "reset_to_new" });
        status_change(conn, id, &prev, TaskStatus::New, Some(&details)).await
    }

    /// `silent` leaves `updated_at` alone, for bookkeeping that must not look like activity
    pub(crate) async fn set_fields(
        conn: &mut SqliteConnection,
        now: &str,
        id: i64,
        updates: &[(String, serde_json::Value)],
        silent: bool,
    ) -> anyhow::Result<()> {
        if updates.is_empty() {
            return Ok(());
        }

        ensure_allowed(updates)?;
        let mut set_parts = Vec::new();
        let mut values: Vec<Option<String>> = Vec::new();
        for (col, val) in updates {
            set_parts.push(format!("{col} = ?"));
            values.push(sql_value_for_task_field(col, val)?);
        }

        if !silent {
            set_parts.push("updated_at = ?".to_string());
            values.push(Some(now.to_string()));
        }

        let sql = format!("UPDATE tasks SET {} WHERE id = ?", set_parts.join(", "));
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()));
        for v in &values {
            query = query.bind(v.as_deref());
        }
        query.bind(id).execute(conn).await?;
        Ok(())
    }

    pub(crate) async fn increment(
        conn: &mut SqliteConnection,
        now: &str,
        id: i64,
        field: &str,
    ) -> anyhow::Result<i64> {
        anyhow::ensure!(
            INCREMENTABLE_FIELDS.contains(&field),
            "field {field} is not incrementable"
        );
        let sql = format!(
            "UPDATE tasks SET {field} = {field} + 1, updated_at = ? WHERE id = ? RETURNING {field} AS val"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
            .bind(now)
            .bind(id)
            .fetch_one(conn)
            .await?;
        let val: i32 = row.try_get("val")?;
        Ok(val.into())
    }

    /// `failure_only` keeps `attempts`, `review_invocations`, `review_cycles`,
    /// `ci_merge_failures` and `merge_conflict_retries`, see `reset_failure_counters`
    pub(crate) async fn reset_counters(
        conn: &mut SqliteConnection,
        now: &str,
        id: i64,
        failure_only: bool,
    ) -> anyhow::Result<()> {
        let all = if failure_only {
            ""
        } else {
            "attempts = 0, merge_conflict_retries = 0, ci_merge_failures = 0, review_cycles = 0, review_invocations = 0,"
        };
        let sql = format!(
            "UPDATE tasks SET {all}
            route_attempts = 0,
            review_agent_failures = 0,
            review_resets = 0,
            pr_create_failures = 0,
            push_failures = 0,
            network_retries = 0,
            auto_unblock_count = 0,
            auto_unblock_last_at = '',
            auto_unblock_last_reason = '',
            ci_recovery_count = 0,
            no_code_reroutes = 0,
            no_code_last_agent = '',
            needs_review_refires = 0,
            updated_at = ?
         WHERE id = ?"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
            .bind(now)
            .bind(id)
            .execute(conn)
            .await?;
        Ok(())
    }

    pub(crate) async fn store_route(
        conn: &mut SqliteConnection,
        now: &str,
        route: &StoreRouteCmd,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE tasks SET
            agent = ?, model = ?, complexity = ?, estimate = ?, route_reason = ?,
            agent_profile = ?, selected_skills = ?,
            updated_at = ?
         WHERE id = ?",
        )
        .bind(&route.agent)
        .bind(&route.model)
        .bind(&route.complexity)
        .bind(i64::from(route.estimate))
        .bind(&route.reason)
        .bind(&route.profile)
        .bind(&route.skills)
        .bind(now)
        .bind(route.id)
        .execute(&mut *conn)
        .await?;
        let details = serde_json::json!({
            "complexity": route.complexity,
            "estimate": route.estimate,
            "reason": route.reason,
            "skills": route.skills,
        });
        activity(
            conn,
            route.id,
            "dispatch",
            None,
            None,
            Some(&route.agent),
            route.model.as_deref(),
            Some(&details),
        )
        .await
    }

    pub(crate) async fn set_estimate_if_zero(
        conn: &mut SqliteConnection,
        now: &str,
        id: i64,
        estimate: u8,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE tasks SET estimate = ?, updated_at = ? WHERE id = ? AND estimate = 0",
        )
        .bind(i64::from(estimate))
        .bind(now)
        .bind(id)
        .execute(conn)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub(crate) async fn append_memory(
        conn: &mut SqliteConnection,
        now: &str,
        id: i64,
        entry: &MemoryEntry,
    ) -> anyhow::Result<()> {
        let row = sqlx::query("SELECT memory FROM tasks WHERE id = ?")
            .bind(id)
            .fetch_one(&mut *conn)
            .await?;
        // A corrupt value must fail, an empty array in its place would lose the old entries
        let memory_opt: Option<String> = row
            .try_get::<Option<String>, _>("memory")
            .map_err(|e| anyhow::anyhow!("failed to read memory column for task {id}: {e}"))?;
        let memory_str = memory_opt.unwrap_or_default();
        let mut memory: Vec<MemoryEntry> = if memory_str.trim().is_empty() {
            Vec::new()
        } else {
            serde_json::from_str(&memory_str)
                .map_err(|e| anyhow::anyhow!("failed to parse memory JSON for task {id}: {e}"))?
        };
        memory.push(entry.clone());
        sqlx::query("UPDATE tasks SET memory = ?, updated_at = ? WHERE id = ?")
            .bind(serde_json::to_string(&memory)?)
            .bind(now)
            .bind(id)
            .execute(conn)
            .await?;
        Ok(())
    }

    pub(crate) async fn mark_cleaned(
        conn: &mut SqliteConnection,
        now: &str,
        id: i64,
    ) -> anyhow::Result<()> {
        sqlx::query("UPDATE tasks SET worktree_cleaned = 1, updated_at = ? WHERE id = ?")
            .bind(now)
            .bind(id)
            .execute(&mut *conn)
            .await?;
        activity(
            conn,
            id,
            "branch_delete",
            None,
            None,
            None,
            None,
            Some(&serde_json::json!({ "worktree_cleaned": true })),
        )
        .await
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
                let row = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
                    .fetch_one(store.pool())
                    .await
                    .unwrap();
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
        let row = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
            .fetch_one(store.pool())
            .await
            .unwrap();
        let err = TaskStore::row_to_task(&row).unwrap_err();
        assert!(
            err.to_string().contains("repo"),
            "expected 'repo' in error message, got: {err}"
        );
    }

    // ── distinct_repos decode-error tests ─────────────────────────────────

    #[tokio::test]
    async fn distinct_repos_fails_on_decode_error() {
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

        // Inject an invalid UTF-8 BLOB as repo — try_get::<String> will fail.
        let sql = format!("UPDATE tasks SET repo = X'DEADBEEF' WHERE id = {id}");
        sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
            .execute(store.pool())
            .await
            .unwrap();

        let err = store.distinct_repos().await.unwrap_err();
        assert!(
            err.to_string().contains("distinct_repos") && err.to_string().contains("repo"),
            "expected 'distinct_repos' and 'repo' in error message, got: {err}"
        );
    }

    // ── row_to_run decode-error tests ────────────────────────────────────────

    async fn store_with_run() -> (crate::store::TaskStore, i64, i64) {
        let store = crate::store::TaskStore::open_memory().await.unwrap();
        let task_id = store
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
        let run_id = store
            .start_run(&crate::store::StartRun {
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
        (store, task_id, run_id)
    }

    #[tokio::test]
    async fn start_run_upsert_clears_stale_completion_fields() {
        let (store, task_id, run_id) = store_with_run().await;
        store
            .complete_run(&CompleteRun {
                run_id,
                exit_code: Some(0),
                stdout: "",
                stderr: "",
                parsed: "",
                outcome: "success",
                error: "boom",
                tokens: RunTokenUsage::default(),
            })
            .await
            .unwrap();

        // Re-dispatch the same (task_id, attempt, run_type) key.
        let same_run_id = store
            .start_run(&crate::store::StartRun {
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
        assert_eq!(run_id, same_run_id, "upsert must reuse the same row");

        let row = sqlx::query(
            "SELECT outcome, completed_at, exit_code, error FROM task_runs WHERE id = ?",
        )
        .bind(run_id)
        .fetch_one(store.pool())
        .await
        .unwrap();
        let outcome: Option<String> = row.try_get("outcome").unwrap();
        let completed_at: Option<String> = row.try_get("completed_at").unwrap();
        let exit_code: Option<i64> = row.try_get("exit_code").unwrap();
        let error: Option<String> = row.try_get("error").unwrap();
        assert_eq!(outcome, None);
        assert_eq!(
            completed_at, None,
            "stale completion timestamp must be cleared"
        );
        assert_eq!(exit_code, None);
        assert_eq!(error, None);
    }

    // NOTE: Unlike the task table (where all required columns are TEXT and NULL
    // propagates a ColumnNotFound error), the task_runs table has INTEGER and REAL
    // columns (id, task_id, attempt, started_at). SQLite coerces NULL/INTEGER to
    // String when try_get::<String> is called, so we cannot use the missing-column
    // pattern here. Instead, we use BLOB injection to trigger actual decode errors
    // that propagate through map_err.
    macro_rules! run_blob_decode_test {
        ($name:ident, $col:literal, $select:literal) => {
            #[tokio::test]
            async fn $name() {
                let (store, _task_id, run_id) = store_with_run().await;
                let sql = format!($select, run_id = run_id);
                let row = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
                    .fetch_one(store.pool())
                    .await
                    .unwrap();
                let err = TaskStore::row_to_run(&row).unwrap_err();
                assert!(
                    err.to_string().contains($col),
                    "expected '{0}' in error message, got: {err}",
                    $col
                );
            }
        };
    }

    run_blob_decode_test!(
        row_to_run_fails_on_decode_error_invalid_utf8_id,
        "id",
        "SELECT X'DEADBEEF' as id, task_id, attempt, run_type, agent, model, command, prompt, env_vars, exit_code, stdout, stderr, parsed_response, outcome, error, input_tokens, output_tokens, total_cost_usd, duration_secs, started_at, completed_at FROM task_runs WHERE id = {run_id}"
    );
    run_blob_decode_test!(
        row_to_run_fails_on_decode_error_invalid_utf8_task_id,
        "task_id",
        "SELECT id, X'DEADBEEF' as task_id, attempt, run_type, agent, model, command, prompt, env_vars, exit_code, stdout, stderr, parsed_response, outcome, error, input_tokens, output_tokens, total_cost_usd, duration_secs, started_at, completed_at FROM task_runs WHERE id = {run_id}"
    );
    run_blob_decode_test!(
        row_to_run_fails_on_decode_error_invalid_utf8_started_at,
        "started_at",
        "SELECT id, task_id, attempt, run_type, agent, model, command, prompt, env_vars, exit_code, stdout, stderr, parsed_response, outcome, error, input_tokens, output_tokens, total_cost_usd, duration_secs, X'DEADBEEF' as started_at, completed_at FROM task_runs WHERE id = {run_id}"
    );

    // ── row_to_activity decode-error tests ───────────────────────────────────

    async fn store_with_activity() -> (crate::store::TaskStore, i64, i64) {
        let store = crate::store::TaskStore::open_memory().await.unwrap();
        let task_id = store
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
        store
            .append_activity(task_id, "created", None, Some("new"), None, None, None)
            .await
            .unwrap();
        (store, task_id, 1)
    }

    // Each test drops exactly one column from the full TASK_ACTIVITY_COLS so that
    // the first missing-column try_get (id) propagates the error.
    #[tokio::test]
    async fn row_to_activity_fails_on_decode_error_invalid_utf8_id() {
        let (store, _task_id, activity_id) = store_with_activity().await;
        let sql = format!(
            "SELECT X'DEADBEEF' as id, task_id, timestamp, event_type, from_status, to_status, agent, model, details FROM task_activity WHERE id = {activity_id}"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
            .fetch_one(store.pool())
            .await
            .unwrap();
        let err = TaskStore::row_to_activity(&row).unwrap_err();
        assert!(
            err.to_string().contains("id"),
            "expected 'id' in error message, got: {err}"
        );
    }

    #[tokio::test]
    async fn row_to_activity_fails_on_decode_error_invalid_utf8_task_id() {
        let (store, _task_id, activity_id) = store_with_activity().await;
        let sql = format!(
            "SELECT id, X'DEADBEEF' as task_id, timestamp, event_type, from_status, to_status, agent, model, details FROM task_activity WHERE id = {activity_id}"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
            .fetch_one(store.pool())
            .await
            .unwrap();
        let err = TaskStore::row_to_activity(&row).unwrap_err();
        assert!(
            err.to_string().contains("task_id"),
            "expected 'task_id' in error message, got: {err}"
        );
    }

    #[tokio::test]
    async fn row_to_activity_fails_on_decode_error_invalid_utf8_timestamp() {
        let (store, _task_id, activity_id) = store_with_activity().await;
        let sql = format!(
            "SELECT id, task_id, X'DEADBEEF' as timestamp, event_type, from_status, to_status, agent, model, details FROM task_activity WHERE id = {activity_id}"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
            .fetch_one(store.pool())
            .await
            .unwrap();
        let err = TaskStore::row_to_activity(&row).unwrap_err();
        assert!(
            err.to_string().contains("timestamp"),
            "expected 'timestamp' in error message, got: {err}"
        );
    }

    #[tokio::test]
    async fn row_to_activity_fails_on_decode_error_invalid_utf8_event_type() {
        let (store, _task_id, activity_id) = store_with_activity().await;
        let sql = format!(
            "SELECT id, task_id, timestamp, X'DEADBEEF' as event_type, from_status, to_status, agent, model, details FROM task_activity WHERE id = {activity_id}"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
            .fetch_one(store.pool())
            .await
            .unwrap();
        let err = TaskStore::row_to_activity(&row).unwrap_err();
        assert!(
            err.to_string().contains("event_type"),
            "expected 'event_type' in error message, got: {err}"
        );
    }

    #[tokio::test]
    async fn row_to_activity_fails_on_corrupt_details_json() {
        let (store, task_id, _activity_id) = store_with_activity().await;
        // Insert a row with invalid JSON in details
        sqlx::query("INSERT INTO task_activity (task_id, event_type, details) VALUES (?, ?, ?)")
            .bind(task_id)
            .bind("test")
            .bind("not valid json {{{")
            .execute(store.pool())
            .await
            .unwrap();
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {TASK_ACTIVITY_COLS} FROM task_activity WHERE task_id = {task_id} ORDER BY id DESC LIMIT 1"
        )))
        .fetch_one(store.pool())
        .await
        .unwrap();
        let err = TaskStore::row_to_activity(&row).unwrap_err();
        assert!(
            err.to_string().contains("details")
                || err.to_string().contains("json")
                || err.to_string().contains("expected"),
            "expected 'details', 'json', or 'expected' in error message, got: {err}"
        );
    }

    // ── row_to_run: remaining fields with map_err ──────────────────────────────

    run_blob_decode_test!(
        row_to_run_fails_on_decode_error_invalid_utf8_model,
        "model",
        "SELECT id, task_id, attempt, run_type, agent, X'DeadbEEF' as model, command, prompt, env_vars, exit_code, stdout, stderr, parsed_response, outcome, error, input_tokens, output_tokens, total_cost_usd, duration_secs, started_at, completed_at FROM task_runs WHERE id = {run_id}"
    );
    run_blob_decode_test!(
        row_to_run_fails_on_decode_error_invalid_utf8_command,
        "command",
        "SELECT id, task_id, attempt, run_type, agent, model, X'DeadbEEF' as command, prompt, env_vars, exit_code, stdout, stderr, parsed_response, outcome, error, input_tokens, output_tokens, total_cost_usd, duration_secs, started_at, completed_at FROM task_runs WHERE id = {run_id}"
    );
    run_blob_decode_test!(
        row_to_run_fails_on_decode_error_invalid_utf8_prompt,
        "prompt",
        "SELECT id, task_id, attempt, run_type, agent, model, command, X'DeadbEEF' as prompt, env_vars, exit_code, stdout, stderr, parsed_response, outcome, error, input_tokens, output_tokens, total_cost_usd, duration_secs, started_at, completed_at FROM task_runs WHERE id = {run_id}"
    );
    run_blob_decode_test!(
        row_to_run_fails_on_decode_error_invalid_utf8_env_vars,
        "env_vars",
        "SELECT id, task_id, attempt, run_type, agent, model, command, prompt, X'DeadbEEF' as env_vars, exit_code, stdout, stderr, parsed_response, outcome, error, input_tokens, output_tokens, total_cost_usd, duration_secs, started_at, completed_at FROM task_runs WHERE id = {run_id}"
    );
    run_blob_decode_test!(
        row_to_run_fails_on_decode_error_invalid_utf8_stdout,
        "stdout",
        "SELECT id, task_id, attempt, run_type, agent, model, command, prompt, env_vars, exit_code, X'DeadbEEF' as stdout, stderr, parsed_response, outcome, error, input_tokens, output_tokens, total_cost_usd, duration_secs, started_at, completed_at FROM task_runs WHERE id = {run_id}"
    );
    run_blob_decode_test!(
        row_to_run_fails_on_decode_error_invalid_utf8_stderr,
        "stderr",
        "SELECT id, task_id, attempt, run_type, agent, model, command, prompt, env_vars, exit_code, stdout, X'DeadbEEF' as stderr, parsed_response, outcome, error, input_tokens, output_tokens, total_cost_usd, duration_secs, started_at, completed_at FROM task_runs WHERE id = {run_id}"
    );
    run_blob_decode_test!(
        row_to_run_fails_on_decode_error_invalid_utf8_parsed_response,
        "parsed_response",
        "SELECT id, task_id, attempt, run_type, agent, model, command, prompt, env_vars, exit_code, stdout, stderr, X'DeadbEEF' as parsed_response, outcome, error, input_tokens, output_tokens, total_cost_usd, duration_secs, started_at, completed_at FROM task_runs WHERE id = {run_id}"
    );
    run_blob_decode_test!(
        row_to_run_fails_on_decode_error_invalid_utf8_outcome,
        "outcome",
        "SELECT id, task_id, attempt, run_type, agent, model, command, prompt, env_vars, exit_code, stdout, stderr, parsed_response, X'DeadbEEF' as outcome, error, input_tokens, output_tokens, total_cost_usd, duration_secs, started_at, completed_at FROM task_runs WHERE id = {run_id}"
    );
    run_blob_decode_test!(
        row_to_run_fails_on_decode_error_invalid_utf8_error,
        "error",
        "SELECT id, task_id, attempt, run_type, agent, model, command, prompt, env_vars, exit_code, stdout, stderr, parsed_response, outcome, X'DeadbEEF' as error, input_tokens, output_tokens, total_cost_usd, duration_secs, started_at, completed_at FROM task_runs WHERE id = {run_id}"
    );
    run_blob_decode_test!(
        row_to_run_fails_on_decode_error_invalid_utf8_input_tokens,
        "input_tokens",
        "SELECT id, task_id, attempt, run_type, agent, model, command, prompt, env_vars, exit_code, stdout, stderr, parsed_response, outcome, error, X'DeadbEEF' as input_tokens, output_tokens, total_cost_usd, duration_secs, started_at, completed_at FROM task_runs WHERE id = {run_id}"
    );
    run_blob_decode_test!(
        row_to_run_fails_on_decode_error_invalid_utf8_output_tokens,
        "output_tokens",
        "SELECT id, task_id, attempt, run_type, agent, model, command, prompt, env_vars, exit_code, stdout, stderr, parsed_response, outcome, error, input_tokens, X'DeadbEEF' as output_tokens, total_cost_usd, duration_secs, started_at, completed_at FROM task_runs WHERE id = {run_id}"
    );
    run_blob_decode_test!(
        row_to_run_fails_on_decode_error_invalid_utf8_total_cost_usd,
        "total_cost_usd",
        "SELECT id, task_id, attempt, run_type, agent, model, command, prompt, env_vars, exit_code, stdout, stderr, parsed_response, outcome, error, input_tokens, output_tokens, X'DeadbEEF' as total_cost_usd, duration_secs, started_at, completed_at FROM task_runs WHERE id = {run_id}"
    );
    run_blob_decode_test!(
        row_to_run_fails_on_decode_error_invalid_utf8_duration_secs,
        "duration_secs",
        "SELECT id, task_id, attempt, run_type, agent, model, command, prompt, env_vars, exit_code, stdout, stderr, parsed_response, outcome, error, input_tokens, output_tokens, total_cost_usd, X'DeadbEEF' as duration_secs, started_at, completed_at FROM task_runs WHERE id = {run_id}"
    );
    run_blob_decode_test!(
        row_to_run_fails_on_decode_error_invalid_utf8_exit_code,
        "exit_code",
        "SELECT id, task_id, attempt, run_type, agent, model, command, prompt, env_vars, exit_code, stdout, stderr, parsed_response, outcome, error, input_tokens, output_tokens, total_cost_usd, duration_secs, started_at, X'DeadbEEF' as exit_code, completed_at FROM task_runs WHERE id = {run_id}"
    );

    // ── row_to_activity: nullable columns with map_err ────────────────────────

    // from_status, to_status, agent, model are nullable (Option<String>) but their
    // decode errors should still propagate via map_err, not be masked as None.
    // Each test injects BLOB into exactly one column and NULL into the rest so
    // the error always references the intended column (the first failing map_err).
    #[tokio::test]
    async fn row_to_activity_fails_on_decode_error_invalid_utf8_from_status() {
        let (store, _task_id, activity_id) = store_with_activity().await;
        let sql = format!(
            "SELECT id, task_id, timestamp, event_type, \
             X'DeadbEEF' as from_status, NULL as to_status, \
             NULL as agent, NULL as model, details \
             FROM task_activity WHERE id = {activity_id}"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
            .fetch_one(store.pool())
            .await
            .unwrap();
        let err = TaskStore::row_to_activity(&row).unwrap_err();
        assert!(
            err.to_string().contains("from_status"),
            "expected 'from_status' in error message, got: {err}"
        );
    }

    #[tokio::test]
    async fn row_to_activity_fails_on_decode_error_invalid_utf8_to_status() {
        let (store, _task_id, activity_id) = store_with_activity().await;
        let sql = format!(
            "SELECT id, task_id, timestamp, event_type, \
             NULL as from_status, X'DeadbEEF' as to_status, \
             NULL as agent, NULL as model, details \
             FROM task_activity WHERE id = {activity_id}"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
            .fetch_one(store.pool())
            .await
            .unwrap();
        let err = TaskStore::row_to_activity(&row).unwrap_err();
        assert!(
            err.to_string().contains("to_status"),
            "expected 'to_status' in error message, got: {err}"
        );
    }

    #[tokio::test]
    async fn row_to_activity_fails_on_decode_error_invalid_utf8_agent() {
        let (store, _task_id, activity_id) = store_with_activity().await;
        let sql = format!(
            "SELECT id, task_id, timestamp, event_type, \
             NULL as from_status, NULL as to_status, \
             X'DeadbEEF' as agent, NULL as model, details \
             FROM task_activity WHERE id = {activity_id}"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
            .fetch_one(store.pool())
            .await
            .unwrap();
        let err = TaskStore::row_to_activity(&row).unwrap_err();
        assert!(
            err.to_string().contains("agent"),
            "expected 'agent' in error message, got: {err}"
        );
    }

    #[tokio::test]
    async fn row_to_activity_fails_on_decode_error_invalid_utf8_model() {
        let (store, _task_id, activity_id) = store_with_activity().await;
        let sql = format!(
            "SELECT id, task_id, timestamp, event_type, \
             NULL as from_status, NULL as to_status, \
             NULL as agent, X'DeadbEEF' as model, details \
             FROM task_activity WHERE id = {activity_id}"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
            .fetch_one(store.pool())
            .await
            .unwrap();
        let err = TaskStore::row_to_activity(&row).unwrap_err();
        assert!(
            err.to_string().contains("model"),
            "expected 'model' in error message, got: {err}"
        );
    }

    // ── get_statuses_by_external_ids: decode-failure rows are skipped ───────────

    #[tokio::test]
    async fn get_statuses_by_external_ids_skips_rows_with_decode_error() {
        // Insert a task row with a BLOB value in the external_id column (valid
        // SQLite but not valid UTF-8 TEXT), so row.try_get("external_id") fails
        // with a decode error. The method should skip this row (not panic) and
        // continue processing the remaining rows.
        let store = crate::store::TaskStore::open_memory().await.unwrap();
        sqlx::query("INSERT INTO tasks (repo, external_id, status, title, origin) VALUES ('owner/repo', X'DeadbEEF', 'new', '', 'internal')")
            .execute(store.pool())
            .await
            .unwrap();
        // A second row with a valid external_id should be returned.
        sqlx::query("INSERT INTO tasks (repo, external_id, status, title, origin) VALUES ('owner/repo', 'valid-id', 'done', '', 'internal')")
            .execute(store.pool())
            .await
            .unwrap();

        let result = store
            .get_statuses_by_external_ids("owner/repo", &["valid-id"])
            .await
            .unwrap();
        assert_eq!(result.len(), 1, "expected exactly one result for valid-id");
        assert_eq!(
            result.get("valid-id"),
            Some(&crate::store::TaskStatus::Done)
        );
    }
}
