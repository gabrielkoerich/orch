use super::*;
use serde::{Deserialize, Serialize};
use sqlx::Row;

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
        tokens_used: Option<i64>,
        cost_usd: Option<f64>,
    ) -> anyhow::Result<i64> {
        let row = sqlx::query(
        "INSERT INTO control_messages (session_id, role, channel, channel_thread, content, summary, model, agent, tokens_used, cost_usd)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(session_id)
    .bind(role)
    .bind(channel)
    .bind(channel_thread)
    .bind(content)
    .bind(summary)
    .bind(model)
    .bind(agent)
    .bind(tokens_used)
    .bind(cost_usd)
    .fetch_one(&self.pool)
    .await?;
        Ok(row.get("id"))
    }

    /// List the most recent control messages for a session (chronological order).
    pub async fn list_control_messages(
        &self,
        session_id: &str,
        limit: i64,
    ) -> anyhow::Result<Vec<ControlMessage>> {
        // Subquery: get the N most recent, then re-sort chronologically
        let rows = sqlx::query(
            "SELECT * FROM (
            SELECT * FROM control_messages
            WHERE session_id = ?
            ORDER BY created_at DESC, id DESC LIMIT ?
        ) ORDER BY created_at ASC, id ASC",
        )
        .bind(session_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(Self::row_to_control_message).collect())
    }

    /// Search control messages by content (LIKE match, most recent first).
    pub async fn search_control_messages(
        &self,
        session_id: &str,
        query: &str,
        limit: i64,
    ) -> anyhow::Result<Vec<ControlMessage>> {
        // Escape LIKE wildcards in user input
        let escaped = query
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("%{escaped}%");
        let rows = sqlx::query(
        "SELECT * FROM control_messages WHERE session_id = ? AND content LIKE ? ESCAPE '\\' ORDER BY created_at DESC LIMIT ?",
    )
    .bind(session_id)
    .bind(&pattern)
    .bind(limit)
    .fetch_all(&self.pool)
    .await?;
        Ok(rows.iter().map(Self::row_to_control_message).collect())
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
        Ok(rows.iter().map(|r| r.get::<String, _>("summary")).collect())
    }

    fn row_to_control_message(row: &sqlx::sqlite::SqliteRow) -> ControlMessage {
        use sqlx::Row;
        ControlMessage {
            id: row.get("id"),
            session_id: row.get("session_id"),
            role: row.get("role"),
            channel: row.get("channel"),
            channel_thread: row.get("channel_thread"),
            content: row.get("content"),
            summary: row.get("summary"),
            model: row.get("model"),
            agent: row.get("agent"),
            tokens_used: row.get("tokens_used"),
            cost_usd: row.get("cost_usd"),
            created_at: row.get("created_at"),
        }
    }
}
