use super::{JobState, SqliteConnection};

pub(crate) async fn upsert_job_state(
    conn: &mut SqliteConnection,
    state: &JobState,
) -> anyhow::Result<()> {
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
    .execute(conn)
    .await?;
    Ok(())
}

pub(crate) async fn subscribe(
    conn: &mut SqliteConnection,
    channel: &str,
    thread_id: &str,
    repo: &str,
    topic_id: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO channel_subscriptions (channel, thread_id, repo, topic_id) VALUES (?, ?, ?, ?)",
    )
    .bind(channel)
    .bind(thread_id)
    .bind(repo)
    .bind(topic_id)
    .execute(conn)
    .await?;
    Ok(())
}

pub(crate) async fn unsubscribe(
    conn: &mut SqliteConnection,
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
    .execute(conn)
    .await?;
    Ok(())
}
