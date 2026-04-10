use super::*;
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;

/// Explicit column list for `SELECT` queries on the `control_messages` table.
///
/// Using `SELECT *` with `sqlx-sqlite`'s prepared statement cache can cause OOB
/// panics when the schema changes (new migration columns) because the cached
/// column metadata may not match the current table structure. Explicit columns
/// prevent this mismatch.
const CONTROL_MESSAGE_COLS: &str = "id, session_id, role, channel, channel_thread, \
    content, summary, model, agent, input_tokens, output_tokens, tokens_used, cost_usd, created_at";

/// A message in the control session conversation history.
#[derive(Debug, Clone)]
#[allow(dead_code)] // fields used by channel integration (Phase 2)
pub struct ControlMessage {
    pub id: i64,
    pub session_id: String,
    pub role: String,
    pub channel: String,
    pub channel_thread: Option<String>,
    pub content: String,
    pub summary: Option<String>,
    pub model: Option<String>,
    pub agent: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub tokens_used: Option<i64>,
    pub cost_usd: Option<f64>,
    pub created_at: String,
}

/// A single memory entry from a task attempt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryEntry {
    /// Attempt number (1-indexed)
    pub attempt: u32,
    /// Agent that made this attempt
    pub agent: String,
    /// Model used for this attempt
    pub model: Option<String>,
    /// Key learnings from this attempt
    pub learnings: Vec<String>,
    /// Error message if the attempt failed
    pub error: Option<String>,
    /// Files modified in this attempt
    pub files_modified: Vec<String>,
    /// Approach taken (from summary)
    pub approach: String,
    /// Timestamp of the attempt
    pub timestamp: String,
}

impl TaskStore {
    pub const DEFAULT_SESSION: &'static str = "default";

    /// Insert a control session message.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_control_message(
        &self,
        session_id: &str,
        role: &str,
        channel: &str,
        channel_thread: Option<&str>,
        content: &str,
        summary: Option<&str>,
        model: Option<&str>,
        agent: Option<&str>,
        input_tokens: Option<i64>,
        output_tokens: Option<i64>,
        tokens_used: Option<i64>,
        cost_usd: Option<f64>,
    ) -> anyhow::Result<i64> {
        let row = sqlx::query(
        "INSERT INTO control_messages (session_id, role, channel, channel_thread, content, summary, model, agent, input_tokens, output_tokens, tokens_used, cost_usd)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(session_id)
    .bind(role)
    .bind(channel)
    .bind(channel_thread)
    .bind(content)
    .bind(summary)
    .bind(model)
    .bind(agent)
    .bind(input_tokens)
    .bind(output_tokens)
    .bind(tokens_used)
    .bind(cost_usd)
    .fetch_one(&self.pool)
    .await?;
        Ok(row.try_get("id")?)
    }

    /// List the most recent control messages for a session (chronological order).
    ///
    /// If `since` is provided, only messages created at or after that RFC3339/ISO8601
    /// timestamp are returned. Use [`parse_since_duration`] to convert human-readable
    /// durations like `"7d"` or `"24h"` into a timestamp string.
    pub async fn list_control_messages(
        &self,
        session_id: &str,
        since: Option<&str>,
        limit: i64,
    ) -> anyhow::Result<Vec<ControlMessage>> {
        // Subquery: get the N most recent, then re-sort chronologically
        let rows = if let Some(ts) = since {
            sqlx::query(&format!(
                "SELECT {CONTROL_MESSAGE_COLS} FROM (
                SELECT {CONTROL_MESSAGE_COLS} FROM control_messages
                WHERE session_id = ? AND created_at >= ?
                ORDER BY created_at DESC, id DESC LIMIT ?
            ) ORDER BY created_at ASC, id ASC"
            ))
            .bind(session_id)
            .bind(ts)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(&format!(
                "SELECT {CONTROL_MESSAGE_COLS} FROM (
                SELECT {CONTROL_MESSAGE_COLS} FROM control_messages
                WHERE session_id = ?
                ORDER BY created_at DESC, id DESC LIMIT ?
            ) ORDER BY created_at ASC, id ASC"
            ))
            .bind(session_id)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        };
        rows.iter()
            .map(Self::row_to_control_message)
            .collect::<Result<Vec<_>, _>>()
            .map_err(anyhow::Error::from)
    }

    /// Search control messages by content (LIKE match, most recent first).
    ///
    /// If `since` is provided, only messages created at or after that RFC3339/ISO8601
    /// timestamp are returned. Use [`parse_since_duration`] to convert human-readable
    /// durations like `"7d"` or `"24h"` into a timestamp string.
    pub async fn search_control_messages(
        &self,
        session_id: &str,
        query: &str,
        since: Option<&str>,
        limit: i64,
    ) -> anyhow::Result<Vec<ControlMessage>> {
        // Escape LIKE wildcards in user input
        let escaped = query
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("%{escaped}%");
        let rows = if let Some(ts) = since {
            sqlx::query(&format!(
                "SELECT {CONTROL_MESSAGE_COLS} FROM control_messages \
                 WHERE session_id = ? AND content LIKE ? ESCAPE '\\' AND created_at >= ? \
                 ORDER BY created_at DESC LIMIT ?"
            ))
            .bind(session_id)
            .bind(&pattern)
            .bind(ts)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(&format!(
                "SELECT {CONTROL_MESSAGE_COLS} FROM control_messages \
                 WHERE session_id = ? AND content LIKE ? ESCAPE '\\' \
                 ORDER BY created_at DESC LIMIT ?"
            ))
            .bind(session_id)
            .bind(&pattern)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        };
        rows.iter()
            .map(Self::row_to_control_message)
            .collect::<Result<Vec<_>, _>>()
            .map_err(anyhow::Error::from)
    }

    /// Get the N most recent assistant message summaries (chronological order).
    pub async fn control_recent_summaries(
        &self,
        session_id: &str,
        limit: i64,
    ) -> anyhow::Result<Vec<String>> {
        // Subquery: get the N most recent, then re-sort chronologically
        let rows = sqlx::query(
            "SELECT summary FROM (
            SELECT summary, created_at, id FROM control_messages
            WHERE session_id = ? AND summary IS NOT NULL
            ORDER BY created_at DESC, id DESC LIMIT ?
        ) ORDER BY created_at ASC, id ASC",
        )
        .bind(session_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| r.try_get::<String, _>("summary").unwrap_or_default())
            .collect())
    }

    fn row_to_control_message(
        row: &sqlx::sqlite::SqliteRow,
    ) -> Result<ControlMessage, sqlx::Error> {
        use sqlx::Row;
        Ok(ControlMessage {
            id: row.try_get("id")?,
            session_id: row.try_get("session_id").unwrap_or_default(),
            role: row.try_get("role").unwrap_or_default(),
            channel: row.try_get("channel").unwrap_or_default(),
            channel_thread: row.try_get("channel_thread").unwrap_or(None),
            content: row.try_get("content").unwrap_or_default(),
            summary: row.try_get("summary").unwrap_or(None),
            model: row.try_get("model").unwrap_or(None),
            agent: row.try_get("agent").unwrap_or(None),
            input_tokens: row.try_get("input_tokens").unwrap_or(None),
            output_tokens: row.try_get("output_tokens").unwrap_or(None),
            tokens_used: row.try_get("tokens_used").unwrap_or(None),
            cost_usd: row.try_get("cost_usd").unwrap_or(None),
            created_at: row.try_get("created_at").unwrap_or_default(),
        })
    }
}

/// Cost summary for a chat session.
#[derive(Debug, Clone, Default)]
pub struct ChatCostSummary {
    /// Total number of messages (user + assistant)
    pub total_messages: i64,
    /// Total number of assistant messages (responses from LLM)
    pub assistant_messages: i64,
    /// Total tokens used across all messages
    pub total_tokens: i64,
    /// Input tokens from assistant messages.
    pub total_input_tokens: i64,
    /// Output tokens from assistant messages.
    pub total_output_tokens: i64,
    /// Total estimated cost in USD
    pub total_cost_usd: f64,
    /// Breakdown by model (model name -> (message_count, tokens, cost))
    pub by_model: Vec<(String, i64, i64, f64)>,
    /// The most frequently used model
    pub primary_model: Option<String>,
    /// The agent used (typically consistent per session)
    pub primary_agent: Option<String>,
}

impl TaskStore {
    /// Get cost summary for a chat session.
    ///
    /// Returns aggregated statistics including total messages, tokens, cost,
    /// and breakdown by model.
    pub async fn get_session_cost_summary(
        &self,
        session_id: &str,
    ) -> anyhow::Result<ChatCostSummary> {
        // Get total counts and cost
        let row = sqlx::query(
            "SELECT
                COUNT(*) as total_messages,
                COUNT(CASE WHEN role = 'assistant' THEN 1 END) as assistant_messages,
                COALESCE(SUM(tokens_used), 0) as total_tokens,
                COALESCE(SUM(input_tokens), 0) as total_input_tokens,
                COALESCE(SUM(output_tokens), 0) as total_output_tokens,
                COALESCE(SUM(cost_usd), 0.0) as total_cost_usd
             FROM control_messages
             WHERE session_id = ?",
        )
        .bind(session_id)
        .fetch_one(&self.pool)
        .await?;

        let total_messages: i64 = row.try_get("total_messages")?;
        let assistant_messages: i64 = row.try_get("assistant_messages")?;
        let total_tokens: i64 = row.try_get("total_tokens")?;
        let total_input_tokens: i64 = row.try_get("total_input_tokens")?;
        let total_output_tokens: i64 = row.try_get("total_output_tokens")?;
        let total_cost_usd: f64 = row.try_get("total_cost_usd")?;

        // Get breakdown by model
        let model_rows = sqlx::query(
            "SELECT
                COALESCE(model, 'unknown') as model_name,
                COUNT(*) as message_count,
                COALESCE(SUM(tokens_used), 0) as tokens,
                COALESCE(SUM(cost_usd), 0.0) as cost
             FROM control_messages
             WHERE session_id = ? AND role = 'assistant'
             GROUP BY model
             ORDER BY message_count DESC",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;

        let mut by_model = Vec::new();
        for row in model_rows {
            let model: String = row.try_get("model_name")?;
            let count: i64 = row.try_get("message_count")?;
            let tokens: i64 = row.try_get("tokens")?;
            let cost: f64 = row.try_get("cost")?;
            by_model.push((model, count, tokens, cost));
        }

        // Determine primary model (most used)
        let primary_model = by_model.first().map(|(m, _, _, _)| m.clone());

        // Get primary agent (most used)
        let agent_row = sqlx::query(
            "SELECT COALESCE(agent, 'unknown') as agent_name
             FROM control_messages
             WHERE session_id = ? AND role = 'assistant' AND agent IS NOT NULL
             GROUP BY agent
             ORDER BY COUNT(*) DESC
             LIMIT 1",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;

        let primary_agent = agent_row.and_then(|r| r.try_get::<String, _>("agent_name").ok());

        Ok(ChatCostSummary {
            total_messages,
            assistant_messages,
            total_tokens,
            total_input_tokens,
            total_output_tokens,
            total_cost_usd,
            by_model,
            primary_model,
            primary_agent,
        })
    }
}

/// Parse a human-readable duration string into an RFC3339 timestamp representing
/// `now - duration`.
///
/// Supported suffixes:
/// - `d` — days (e.g. `"7d"`)
/// - `h` — hours (e.g. `"24h"`)
/// - `m` — minutes (e.g. `"30m"`)
///
/// Returns an ISO8601/RFC3339 string suitable for comparison against
/// `control_messages.created_at` (stored as UTC RFC3339).
///
/// Returns `Err` if the format is unrecognised or the number overflows.
pub fn parse_since_duration(s: &str) -> anyhow::Result<String> {
    let s = s.trim();
    let (num_str, unit) = if let Some(n) = s.strip_suffix('d') {
        (n, 'd')
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 'h')
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 'm')
    } else {
        anyhow::bail!(
            "unrecognised --since format {:?}; expected a number followed by d, h, or m (e.g. 7d, 24h, 30m)",
            s
        );
    };

    let n: u64 = num_str.trim().parse().map_err(|_| {
        anyhow::anyhow!(
            "invalid number in --since {:?}; expected a positive integer followed by d, h, or m",
            s
        )
    })?;
    if n == 0 {
        anyhow::bail!("--since value must be greater than zero");
    }

    let secs: i64 = match unit {
        'd' => n.checked_mul(86_400),
        'h' => n.checked_mul(3_600),
        'm' => n.checked_mul(60),
        _ => unreachable!(),
    }
    .ok_or_else(|| anyhow::anyhow!("--since value is too large"))?
    .try_into()
    .map_err(|_| anyhow::anyhow!("--since value is too large"))?;

    Ok((Utc::now() - Duration::seconds(secs)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}
