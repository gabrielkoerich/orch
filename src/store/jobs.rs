use super::*;
use serde::{Deserialize, Serialize};

/// Explicit column list for `SELECT` queries on the `job_state` table.
///
/// Using `SELECT *` with `sqlx-sqlite`'s prepared statement cache can cause OOB
/// panics when the schema changes (new migration columns) because the cached
/// column metadata may not match the current table structure. Explicit columns
/// prevent this mismatch.
const JOB_STATE_COLS: &str = "repo, job_id, last_run, last_task_status, active_task_id";

/// Runtime state for a scheduled job, stored in SQLite (not in .orch.yml).
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct JobState {
    pub repo: String,
    pub job_id: String,
    pub last_run: Option<String>,
    pub last_task_status: Option<String>,
    pub active_task_id: Option<String>,
}

impl TaskStore {
    pub async fn get_job_state(
        &self,
        repo: &str,
        job_id: &str,
    ) -> anyhow::Result<Option<JobState>> {
        let sql = format!(
            "SELECT {} FROM job_state WHERE repo = ? AND job_id = ?",
            JOB_STATE_COLS
        );
        let row: Option<JobState> = sqlx::query_as(&sql)
            .bind(repo)
            .bind(job_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    /// Upsert runtime state for a job.
    pub async fn upsert_job_state(&self, state: &JobState) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO job_state (repo, job_id, last_run, last_task_status, active_task_id)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(repo, job_id) DO UPDATE SET
           last_run = excluded.last_run,
           last_task_status = excluded.last_task_status,
           active_task_id = excluded.active_task_id",
        )
        .bind(&state.repo)
        .bind(&state.job_id)
        .bind(&state.last_run)
        .bind(&state.last_task_status)
        .bind(&state.active_task_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// List all job states for a repo.
    pub async fn list_job_states(&self, repo: &str) -> anyhow::Result<Vec<JobState>> {
        let sql = format!("SELECT {} FROM job_state WHERE repo = ?", JOB_STATE_COLS);
        let rows: Vec<JobState> = sqlx::query_as(&sql)
            .bind(repo)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    /// Delete job state (for cleanup when a job is removed from config).
    #[allow(dead_code)]
    pub async fn delete_job_state(&self, repo: &str, job_id: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM job_state WHERE repo = ? AND job_id = ?")
            .bind(repo)
            .bind(job_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---------------------------------------------------------------
    // Channel Subscriptions
    // ---------------------------------------------------------------

    /// Subscribe a channel/thread to notifications for a project.
    #[allow(dead_code)]
    pub async fn subscribe_channel(
        &self,
        channel: &str,
        thread_id: &str,
        repo: &str,
        topic_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let topic = topic_id.unwrap_or("");
        sqlx::query(
        "INSERT OR IGNORE INTO channel_subscriptions (channel, thread_id, repo, topic_id) VALUES (?, ?, ?, ?)",
    )
        .bind(channel)
        .bind(thread_id)
        .bind(repo)
        .bind(topic)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Unsubscribe a channel/thread from a project's notifications.
    #[allow(dead_code)]
    pub async fn unsubscribe_channel(
        &self,
        channel: &str,
        thread_id: &str,
        repo: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "DELETE FROM channel_subscriptions WHERE channel = ? AND thread_id = ? AND repo = ?",
        )
        .bind(channel)
        .bind(thread_id)
        .bind(repo)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// List repos that a channel/thread is subscribed to.
    #[allow(dead_code)]
    pub async fn list_channel_subscriptions(
        &self,
        channel: &str,
        thread_id: &str,
    ) -> anyhow::Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT repo FROM channel_subscriptions WHERE channel = ? AND thread_id = ?",
        )
        .bind(channel)
        .bind(thread_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    /// List all subscriptions (channel, thread_id, topic_id) for a repo.
    #[allow(dead_code)]
    pub async fn list_subscribers_for_repo(
        &self,
        repo: &str,
    ) -> anyhow::Result<Vec<(String, String, String)>> {
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT channel, thread_id, topic_id FROM channel_subscriptions WHERE repo = ?",
        )
        .bind(repo)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}
