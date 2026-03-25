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
            sqlx::query(
                "SELECT * FROM (
                SELECT * FROM control_messages
                WHERE session_id = ? AND created_at >= ?
                ORDER BY created_at DESC, id DESC LIMIT ?
            ) ORDER BY created_at ASC, id ASC",
            )
            .bind(session_id)
            .bind(ts)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                "SELECT * FROM (
                SELECT * FROM control_messages
                WHERE session_id = ?
                ORDER BY created_at DESC, id DESC LIMIT ?
            ) ORDER BY created_at ASC, id ASC",
            )
            .bind(session_id)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        };
        Ok(rows.iter().map(Self::row_to_control_message).collect())
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
            sqlx::query(
                "SELECT * FROM control_messages \
                 WHERE session_id = ? AND content LIKE ? ESCAPE '\\' AND created_at >= ? \
                 ORDER BY created_at DESC LIMIT ?",
            )
            .bind(session_id)
            .bind(&pattern)
            .bind(ts)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                "SELECT * FROM control_messages \
                 WHERE session_id = ? AND content LIKE ? ESCAPE '\\' \
                 ORDER BY created_at DESC LIMIT ?",
            )
            .bind(session_id)
            .bind(&pattern)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        };
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

/// Parse a human-readable duration string into an RFC3339 timestamp representing
/// `now - duration`.
///
/// Supported suffixes:
/// - `d` — days (e.g. `"7d"`)
/// - `h` — hours (e.g. `"24h"`)
/// - `m` — minutes (e.g. `"30m"`)
///
/// Returns an ISO8601/RFC3339 string suitable for comparison against
/// `control_messages.created_at` (stored as `YYYY-MM-DD HH:MM:SS` UTC).
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

    let secs: u64 = match unit {
        'd' => n * 86_400,
        'h' => n * 3_600,
        'm' => n * 60,
        _ => unreachable!(),
    };

    // Use std::time to get the current UTC time and subtract the duration.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| anyhow::anyhow!("system clock error: {e}"))?
        .as_secs();
    let cutoff = now.saturating_sub(secs);

    // Format as SQLite-compatible ISO8601: "YYYY-MM-DD HH:MM:SS"
    let ts = format_unix_as_sqlite(cutoff);
    Ok(ts)
}

/// Format a Unix timestamp (seconds) as `"YYYY-MM-DD HH:MM:SS"` (UTC),
/// matching the format SQLite uses for `datetime('now')`.
fn format_unix_as_sqlite(secs: u64) -> String {
    // Manual conversion — avoids pulling in chrono just for this.
    let s = secs as i64;
    let (mut days, rem) = (s / 86_400, s % 86_400);
    let (hour, rem) = (rem / 3_600, rem % 3_600);
    let (minute, second) = (rem / 60, rem % 60);

    // Days since Unix epoch (1970-01-01)
    let mut year = 1970i32;
    loop {
        let days_in_year = if is_leap_year(year) { 366 } else { 365 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
    }
    let months = [
        31,
        if is_leap_year(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1u32;
    for &m in &months {
        if days < m {
            break;
        }
        days -= m;
        month += 1;
    }
    let day = days + 1;

    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}",)
}

fn is_leap_year(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}
