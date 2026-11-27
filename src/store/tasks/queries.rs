use super::{
    BlockedEvent, MergedEvent, RunEvent, StatsEvents, StatusChange, TASK_COLS, Task, TaskStatus,
};
use crate::store::TaskStore;
use sqlx::Row;
use std::collections::HashMap;
use std::collections::HashSet;

impl TaskStore {
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

    /// Every open task in a repo and its `done_limit` most recently finished ones, newest first
    pub async fn list_for_board(&self, repo: &str, done_limit: i64) -> anyhow::Result<Vec<Task>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {TASK_COLS} FROM tasks WHERE repo = ? AND status != 'done'
             UNION ALL
             SELECT {TASK_COLS} FROM (SELECT {TASK_COLS} FROM tasks WHERE repo = ? AND status = 'done'
                            ORDER BY updated_at DESC LIMIT ?)
             ORDER BY updated_at DESC"
        )))
        .bind(repo)
        .bind(repo)
        .bind(done_limit)
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(Self::row_to_task).collect()
    }

    /// Tasks with an agent on them right now, in every repo, longest running first
    pub async fn list_running(&self) -> anyhow::Result<Vec<Task>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {TASK_COLS} FROM tasks WHERE status IN ('in_progress', 'in_review') ORDER BY updated_at ASC"
        )))
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(Self::row_to_task).collect()
    }

    /// The newest status changes, in one repo or all of them, oldest first. A re-fire that keeps
    /// the status is not a change
    pub async fn recent_status_changes(
        &self,
        repo: Option<&str>,
        limit: i64,
    ) -> anyhow::Result<Vec<StatusChange>> {
        let rows: Vec<StatusChange> = sqlx::query_as(
            "SELECT timestamp, task_id, repo, external_id, title, from_status, to_status, agent FROM (
                SELECT a.timestamp, a.task_id, t.repo, t.external_id, t.title,
                       a.from_status, a.to_status, a.agent
                FROM task_activity a JOIN tasks t ON t.id = a.task_id
                WHERE a.to_status IS NOT NULL AND a.to_status IS NOT a.from_status
                  AND (?1 IS NULL OR t.repo = ?1)
                ORDER BY a.timestamp DESC, a.id DESC LIMIT ?2
             ) ORDER BY timestamp ASC",
        )
        .bind(repo)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Every task that is not done, in one repo or all of them
    pub async fn list_open(&self, repo: Option<&str>) -> anyhow::Result<Vec<Task>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {TASK_COLS} FROM tasks WHERE status != 'done' AND (?1 IS NULL OR repo = ?1)
             ORDER BY updated_at DESC"
        )))
        .bind(repo)
        .fetch_all(&self.pool)
        .await?;

        rows.iter().map(Self::row_to_task).collect()
    }

    /// Tasks that entered `status` between two RFC 3339 instants, with when, newest first
    pub async fn entered_between(
        &self,
        status: TaskStatus,
        from: &str,
        to: &str,
        repo: Option<&str>,
    ) -> anyhow::Result<Vec<(String, Task)>> {
        let hits: Vec<(i64, String)> = sqlx::query_as(
            "SELECT a.task_id, MAX(a.timestamp) FROM task_activity a JOIN tasks t ON t.id = a.task_id
             WHERE a.to_status = ?1 AND a.from_status IS NOT ?1
               AND a.timestamp >= ?2 AND a.timestamp < ?3 AND (?4 IS NULL OR t.repo = ?4)
             GROUP BY a.task_id ORDER BY MAX(a.timestamp) DESC",
        )
        .bind(status.as_str())
        .bind(from)
        .bind(to)
        .bind(repo)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(hits.len());
        for (id, at) in hits {
            if let Ok(task) = self.get(id).await {
                out.push((at, task));
            }
        }
        Ok(out)
    }

    /// Merges, new blocks and agent runs since `since`, for the statistics page to count
    pub async fn stats_since(&self, since: &str) -> anyhow::Result<StatsEvents> {
        let merged: Vec<MergedEvent> = sqlx::query_as(
            "SELECT MAX(a.timestamp) AS at, t.id AS task_id, t.repo, t.agent, t.created_at,
                    t.attempts, t.review_cycles
             FROM task_activity a JOIN tasks t ON t.id = a.task_id
             WHERE a.to_status = 'done' AND a.timestamp >= ?1
             GROUP BY t.id ORDER BY at",
        )
        .bind(since)
        .fetch_all(&self.pool)
        .await?;
        let blocked: Vec<BlockedEvent> = sqlx::query_as(
            "SELECT a.timestamp AS at, t.id AS task_id, t.repo
             FROM task_activity a JOIN tasks t ON t.id = a.task_id
             WHERE a.to_status = 'blocked' AND a.from_status IS NOT 'blocked' AND a.timestamp >= ?1
             ORDER BY a.timestamp",
        )
        .bind(since)
        .fetch_all(&self.pool)
        .await?;
        let runs: Vec<RunEvent> = sqlx::query_as(
            "SELECT r.started_at AS at, t.repo, r.agent, r.model, r.run_type, r.outcome,
                    COALESCE(r.duration_secs, 0.0) AS duration_secs
             FROM task_runs r JOIN tasks t ON t.id = r.task_id
             WHERE r.started_at >= ?1 ORDER BY r.started_at",
        )
        .bind(since)
        .fetch_all(&self.pool)
        .await?;
        Ok(StatsEvents {
            merged,
            blocked,
            runs,
        })
    }

    /// When each task entered the status it has now, from its activity rows
    pub async fn entered_status(&self, ids: &[i64]) -> anyhow::Result<HashMap<i64, String>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }

        let marks = vec!["?"; ids.len()].join(",");
        let sql = format!(
            "SELECT a.task_id, MAX(a.timestamp) FROM task_activity a JOIN tasks t ON t.id = a.task_id
             WHERE a.task_id IN ({marks}) AND a.to_status = t.status
               AND a.from_status IS NOT a.to_status
             GROUP BY a.task_id"
        );
        let mut query = sqlx::query_as::<_, (i64, String)>(sqlx::AssertSqlSafe(sql));
        for id in ids {
            query = query.bind(id);
        }
        Ok(query.fetch_all(&self.pool).await?.into_iter().collect())
    }

    /// Task count per status in a repo
    pub async fn count_by_status(&self, repo: &str) -> anyhow::Result<Vec<(String, i64)>> {
        let rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT status, COUNT(*) FROM tasks WHERE repo = ? GROUP BY status")
                .bind(repo)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows)
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
    pub async fn existing_external_ids(&self, repo: &str) -> anyhow::Result<HashSet<String>> {
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

    /// Get multiple tasks by ID in a single query, returning a map from task ID to Task.
    /// Returns an empty map if `ids` is empty.
    pub async fn get_batch(&self, ids: &[i64]) -> anyhow::Result<HashMap<i64, Task>> {
        // SQLite has a hard limit on host parameters (commonly 999). Chunk the
        // IN-list into smaller batches to avoid exceeding the limit.
        const CHUNK_SIZE: usize = 500;
        if ids.is_empty() {
            return Ok(HashMap::new());
        }

        let mut result = HashMap::new();

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
    ) -> anyhow::Result<HashMap<String, TaskStatus>> {
        const CHUNK_SIZE: usize = 500;
        let mut result = HashMap::new();
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
}
