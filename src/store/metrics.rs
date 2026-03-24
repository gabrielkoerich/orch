use super::*;
use chrono::{Datelike, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;

/// Parameters for inserting a new task metric record.
#[derive(Debug, Clone)]
pub struct InsertTaskMetric<'a> {
    pub repo: &'a str,
    pub task_id: &'a str,
    pub agent: &'a str,
    pub model: Option<&'a str>,
    pub complexity: Option<&'a str>,
    pub outcome: &'a str,
    pub duration_seconds: f64,
    pub started_at: &'a chrono::DateTime<Utc>,
    pub completed_at: &'a chrono::DateTime<Utc>,
    pub attempts: i32,
    pub files_changed: i32,
    pub error_type: Option<&'a str>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub input_cost_usd: Option<f64>,
    pub output_cost_usd: Option<f64>,
    pub total_cost_usd: Option<f64>,
}

/// Metrics summary for the CLI output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSummary {
    pub tasks_completed_24h: i64,
    pub tasks_failed_24h: i64,
    pub avg_duration_simple: Option<f64>,
    pub avg_duration_medium: Option<f64>,
    pub avg_duration_complex: Option<f64>,
    pub agent_stats: Vec<AgentStat>,
    pub rate_limits_24h: i64,
}

/// Agent statistics from metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStat {
    pub agent: String,
    pub total_runs: i64,
    pub success_count: i64,
    pub success_rate: f64,
}

/// Slow task info for pattern detection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlowTaskInfo {
    pub task_id: String,
    pub agent: String,
    pub complexity: Option<String>,
    pub duration_seconds: f64,
}

/// Error type distribution for pattern detection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorStat {
    pub error_type: Option<String>,
    pub count: i64,
}

/// Task with high review cycle count (persistent review loop).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HighReviewCycleTask {
    pub external_id: Option<String>,
    pub agent: Option<String>,
    pub review_cycles: i64,
    pub title: String,
}

/// Cost summary across multiple time periods.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostSummary {
    pub periods: Vec<CostPeriod>,
}

/// Cost data for a single time period.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostPeriod {
    pub label: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub input_cost_usd: f64,
    pub output_cost_usd: f64,
    pub total_cost_usd: f64,
    pub task_count: i64,
}

/// Cost breakdown by a grouping dimension (agent, model, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostByGroup {
    pub name: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_cost_usd: f64,
    pub task_count: i64,
}

/// Build the week-scoped KV key for the self-improvement counter.
fn self_improvement_key() -> String {
    let now = Utc::now();
    format!(
        "self_improvement_issues_{}_w{}",
        now.iso_week().year(),
        now.iso_week().week()
    )
}

impl TaskStore {
    pub async fn cost_summary(&self, repo: &str) -> anyhow::Result<(i64, i64, f64)> {
        let row = sqlx::query(
            "SELECT
            COALESCE(SUM(input_tokens), 0) as total_input,
            COALESCE(SUM(output_tokens), 0) as total_output,
            COALESCE(SUM(total_cost_usd), 0.0) as total_cost
         FROM tasks WHERE repo = ?",
        )
        .bind(repo)
        .fetch_one(&self.pool)
        .await?;

        use sqlx::Row;
        Ok((
            row.get::<i64, _>("total_input"),
            row.get::<i64, _>("total_output"),
            row.get::<f64, _>("total_cost"),
        ))
    }

    /// Count tasks by status for a repo.
    /// Returns a map of status string → count.
    #[allow(dead_code)]
    pub async fn status_counts(
        &self,
        repo: &str,
    ) -> anyhow::Result<std::collections::HashMap<String, i64>> {
        let rows =
            sqlx::query("SELECT status, COUNT(*) as cnt FROM tasks WHERE repo = ? GROUP BY status")
                .bind(repo)
                .fetch_all(&self.pool)
                .await?;

        use sqlx::Row;
        let mut map = std::collections::HashMap::new();
        for row in &rows {
            let status: String = row.get("status");
            let count: i64 = row.get("cnt");
            map.insert(status, count);
        }
        Ok(map)
    }

    pub async fn insert_task_metric(&self, metric: &InsertTaskMetric<'_>) -> anyhow::Result<i64> {
        sqlx::query("DELETE FROM task_metrics WHERE completed_at < datetime('now', '-90 days')")
            .execute(&self.pool)
            .await?;

        let row = sqlx::query(
        "INSERT INTO task_metrics (repo, task_id, agent, model, complexity, outcome, duration_seconds,
         started_at, completed_at, attempts, files_changed, error_type,
         input_tokens, output_tokens, input_cost_usd, output_cost_usd, total_cost_usd)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(metric.repo)
    .bind(metric.task_id)
    .bind(metric.agent)
    .bind(metric.model)
    .bind(metric.complexity)
    .bind(metric.outcome)
    .bind(metric.duration_seconds)
    .bind(metric.started_at.to_rfc3339())
    .bind(metric.completed_at.to_rfc3339())
    .bind(metric.attempts)
    .bind(metric.files_changed)
    .bind(metric.error_type)
    .bind(metric.input_tokens)
    .bind(metric.output_tokens)
    .bind(metric.input_cost_usd)
    .bind(metric.output_cost_usd)
    .bind(metric.total_cost_usd)
    .fetch_one(&self.pool)
    .await?;
        Ok(row.get("id"))
    }

    /// Get aggregated metrics for the last 24 hours.
    pub async fn get_metrics_summary_24h(&self) -> anyhow::Result<MetricsSummary> {
        let completed: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM task_metrics WHERE completed_at >= datetime('now', '-24 hours') AND outcome = 'success'",
    )
    .fetch_one(&self.pool)
    .await?;

        let failed: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM task_metrics WHERE completed_at >= datetime('now', '-24 hours') AND outcome != 'success'",
    )
    .fetch_one(&self.pool)
    .await?;

        let avg_simple: Option<(f64,)> = sqlx::query_as(
        "SELECT AVG(duration_seconds) FROM task_metrics WHERE completed_at >= datetime('now', '-24 hours') AND complexity = 'simple'",
    )
    .fetch_optional(&self.pool)
    .await?;

        let avg_medium: Option<(f64,)> = sqlx::query_as(
        "SELECT AVG(duration_seconds) FROM task_metrics WHERE completed_at >= datetime('now', '-24 hours') AND complexity = 'medium'",
    )
    .fetch_optional(&self.pool)
    .await?;

        let avg_complex: Option<(f64,)> = sqlx::query_as(
        "SELECT AVG(duration_seconds) FROM task_metrics WHERE completed_at >= datetime('now', '-24 hours') AND complexity = 'complex'",
    )
    .fetch_optional(&self.pool)
    .await?;

        let agent_rows = sqlx::query(
            "SELECT agent, COUNT(*) as total,
                SUM(CASE WHEN outcome = 'success' THEN 1 ELSE 0 END) as success_count
         FROM task_metrics
         WHERE completed_at >= datetime('now', '-24 hours')
         GROUP BY agent",
        )
        .fetch_all(&self.pool)
        .await?;

        let agent_stats: Vec<AgentStat> = agent_rows
            .iter()
            .map(|row| {
                let total: i64 = row.get("total");
                let success: i64 = row.get("success_count");
                AgentStat {
                    agent: row.get("agent"),
                    total_runs: total,
                    success_count: success,
                    success_rate: if total > 0 {
                        (success as f64 / total as f64) * 100.0
                    } else {
                        0.0
                    },
                }
            })
            .collect();

        let rate_limit_count: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM rate_limits WHERE occurred_at >= datetime('now', '-24 hours')",
        )
        .fetch_one(&self.pool)
        .await?;

        Ok(MetricsSummary {
            tasks_completed_24h: completed.0,
            tasks_failed_24h: failed.0,
            avg_duration_simple: avg_simple.map(|r| r.0),
            avg_duration_medium: avg_medium.map(|r| r.0),
            avg_duration_complex: avg_complex.map(|r| r.0),
            agent_stats,
            rate_limits_24h: rate_limit_count.0,
        })
    }

    /// Get metrics summary for the last 24 hours, filtered by repo.
    /// Joins task_metrics with tasks table to get repo context.
    #[allow(dead_code)]
    pub async fn get_metrics_summary_24h_by_repo(
        &self,
        repo: &str,
    ) -> anyhow::Result<MetricsSummary> {
        // Use COALESCE: prefer task_metrics.repo if populated, otherwise join tasks table
        let completed: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM task_metrics m
         LEFT JOIN tasks t ON m.task_id = CAST(t.id AS TEXT) OR m.task_id = t.external_id
         WHERE m.completed_at >= datetime('now', '-24 hours')
         AND m.outcome = 'success'
         AND COALESCE(NULLIF(m.repo, ''), t.repo) = ?",
        )
        .bind(repo)
        .fetch_one(&self.pool)
        .await?;

        let failed: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM task_metrics m
         LEFT JOIN tasks t ON m.task_id = CAST(t.id AS TEXT) OR m.task_id = t.external_id
         WHERE m.completed_at >= datetime('now', '-24 hours')
         AND m.outcome != 'success'
         AND COALESCE(NULLIF(m.repo, ''), t.repo) = ?",
        )
        .bind(repo)
        .fetch_one(&self.pool)
        .await?;

        let avg_simple: Option<(f64,)> = sqlx::query_as(
            "SELECT AVG(m.duration_seconds) FROM task_metrics m
         LEFT JOIN tasks t ON m.task_id = CAST(t.id AS TEXT) OR m.task_id = t.external_id
         WHERE m.completed_at >= datetime('now', '-24 hours') AND m.complexity = 'simple'
         AND COALESCE(NULLIF(m.repo, ''), t.repo) = ?",
        )
        .bind(repo)
        .fetch_optional(&self.pool)
        .await?;

        let avg_medium: Option<(f64,)> = sqlx::query_as(
            "SELECT AVG(m.duration_seconds) FROM task_metrics m
         LEFT JOIN tasks t ON m.task_id = CAST(t.id AS TEXT) OR m.task_id = t.external_id
         WHERE m.completed_at >= datetime('now', '-24 hours') AND m.complexity = 'medium'
         AND COALESCE(NULLIF(m.repo, ''), t.repo) = ?",
        )
        .bind(repo)
        .fetch_optional(&self.pool)
        .await?;

        let avg_complex: Option<(f64,)> = sqlx::query_as(
            "SELECT AVG(m.duration_seconds) FROM task_metrics m
         LEFT JOIN tasks t ON m.task_id = CAST(t.id AS TEXT) OR m.task_id = t.external_id
         WHERE m.completed_at >= datetime('now', '-24 hours') AND m.complexity = 'complex'
         AND COALESCE(NULLIF(m.repo, ''), t.repo) = ?",
        )
        .bind(repo)
        .fetch_optional(&self.pool)
        .await?;

        let agent_rows = sqlx::query(
            "SELECT m.agent, COUNT(*) as total,
                SUM(CASE WHEN m.outcome = 'success' THEN 1 ELSE 0 END) as success_count
         FROM task_metrics m
         LEFT JOIN tasks t ON m.task_id = CAST(t.id AS TEXT) OR m.task_id = t.external_id
         WHERE m.completed_at >= datetime('now', '-24 hours')
         AND COALESCE(NULLIF(m.repo, ''), t.repo) = ?
         GROUP BY m.agent",
        )
        .bind(repo)
        .fetch_all(&self.pool)
        .await?;

        let agent_stats: Vec<AgentStat> = agent_rows
            .iter()
            .map(|row| {
                let total: i64 = row.get("total");
                let success: i64 = row.get("success_count");
                AgentStat {
                    agent: row.get("agent"),
                    total_runs: total,
                    success_count: success,
                    success_rate: if total > 0 {
                        (success as f64 / total as f64) * 100.0
                    } else {
                        0.0
                    },
                }
            })
            .collect();

        let rate_limits: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM rate_limits WHERE occurred_at >= datetime('now', '-24 hours')",
        )
        .fetch_one(&self.pool)
        .await?;

        Ok(MetricsSummary {
            tasks_completed_24h: completed.0,
            tasks_failed_24h: failed.0,
            avg_duration_simple: avg_simple.map(|r| r.0),
            avg_duration_medium: avg_medium.map(|r| r.0),
            avg_duration_complex: avg_complex.map(|r| r.0),
            agent_stats,
            rate_limits_24h: rate_limits.0,
        })
    }

    /// Get 24-hour cost summary filtered to a specific repository.
    ///
    /// Uses the same `COALESCE(NULLIF(m.repo, ''), t.repo)` join pattern as
    /// `get_metrics_summary_24h_by_repo` so older metrics rows that lack a `repo`
    /// column value are still matched via the tasks table.
    pub async fn get_cost_summary_24h_by_repo(&self, repo: &str) -> anyhow::Result<CostSummary> {
        let row = sqlx::query(
            "SELECT COALESCE(SUM(m.input_tokens), 0) as input_tokens,
                COALESCE(SUM(m.output_tokens), 0) as output_tokens,
                COALESCE(SUM(m.input_cost_usd), 0.0) as input_cost_usd,
                COALESCE(SUM(m.output_cost_usd), 0.0) as output_cost_usd,
                COALESCE(SUM(m.total_cost_usd), 0.0) as total_cost_usd,
                COUNT(*) as task_count
         FROM task_metrics m
         LEFT JOIN tasks t ON m.task_id = CAST(t.id AS TEXT) OR m.task_id = t.external_id
         WHERE m.completed_at >= datetime('now', '-24 hours')
         AND COALESCE(NULLIF(m.repo, ''), t.repo) = ?",
        )
        .bind(repo)
        .fetch_one(&self.pool)
        .await?;

        Ok(CostSummary {
            periods: vec![CostPeriod {
                label: "24h".to_string(),
                input_tokens: row.get("input_tokens"),
                output_tokens: row.get("output_tokens"),
                input_cost_usd: row.get("input_cost_usd"),
                output_cost_usd: row.get("output_cost_usd"),
                total_cost_usd: row.get("total_cost_usd"),
                task_count: row.get("task_count"),
            }],
        })
    }

    /// Record a rate limit event. Prunes records older than 30 days.
    pub async fn record_rate_limit(
        &self,
        agent: &str,
        limit_type: &str,
        task_id: Option<&str>,
    ) -> anyhow::Result<i64> {
        sqlx::query("DELETE FROM rate_limits WHERE occurred_at < datetime('now', '-30 days')")
            .execute(&self.pool)
            .await?;

        let row = sqlx::query(
            "INSERT INTO rate_limits (agent, limit_type, occurred_at, task_id)
         VALUES (?, ?, datetime('now'), ?)
         RETURNING id",
        )
        .bind(agent)
        .bind(limit_type)
        .bind(task_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get("id"))
    }

    /// Get slow tasks (top 10 longest running) from the last 7 days.
    pub async fn get_slow_tasks_7d(&self) -> anyhow::Result<Vec<SlowTaskInfo>> {
        let rows = sqlx::query(
            "SELECT task_id, agent, complexity, duration_seconds
         FROM task_metrics
         WHERE completed_at >= datetime('now', '-7 days') AND outcome = 'success'
         ORDER BY duration_seconds DESC
         LIMIT 10",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(|row| SlowTaskInfo {
                task_id: row.get("task_id"),
                agent: row.get("agent"),
                complexity: row.get("complexity"),
                duration_seconds: row.get("duration_seconds"),
            })
            .collect())
    }

    /// Get error type distribution from the last 7 days.
    pub async fn get_error_distribution_7d(&self) -> anyhow::Result<Vec<ErrorStat>> {
        let rows = sqlx::query(
            "SELECT error_type, COUNT(*) as count
         FROM task_metrics
         WHERE completed_at >= datetime('now', '-7 days')
           AND outcome != 'success'
           AND error_type IS NOT NULL
         GROUP BY error_type
         ORDER BY count DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(|row| ErrorStat {
                error_type: row.get("error_type"),
                count: row.get("count"),
            })
            .collect())
    }

    /// Get cost summary over multiple time windows (24h, 7d, 30d).
    ///
    /// Uses separate static queries per window because SQLite's `datetime()`
    /// modifier argument cannot be parameterised via `?` bind.
    pub async fn get_cost_summary(&self) -> anyhow::Result<CostSummary> {
        // Each entry: (static SQL with the interval baked in, label)
        let windows: &[(&str, &str)] = &[
            (
                "SELECT COALESCE(SUM(input_tokens), 0) as input_tokens,
                    COALESCE(SUM(output_tokens), 0) as output_tokens,
                    COALESCE(SUM(input_cost_usd), 0.0) as input_cost_usd,
                    COALESCE(SUM(output_cost_usd), 0.0) as output_cost_usd,
                    COALESCE(SUM(total_cost_usd), 0.0) as total_cost_usd,
                    COUNT(*) as task_count
             FROM task_metrics
             WHERE completed_at >= datetime('now', '-24 hours')",
                "24h",
            ),
            (
                "SELECT COALESCE(SUM(input_tokens), 0) as input_tokens,
                    COALESCE(SUM(output_tokens), 0) as output_tokens,
                    COALESCE(SUM(input_cost_usd), 0.0) as input_cost_usd,
                    COALESCE(SUM(output_cost_usd), 0.0) as output_cost_usd,
                    COALESCE(SUM(total_cost_usd), 0.0) as total_cost_usd,
                    COUNT(*) as task_count
             FROM task_metrics
             WHERE completed_at >= datetime('now', '-7 days')",
                "7d",
            ),
            (
                "SELECT COALESCE(SUM(input_tokens), 0) as input_tokens,
                    COALESCE(SUM(output_tokens), 0) as output_tokens,
                    COALESCE(SUM(input_cost_usd), 0.0) as input_cost_usd,
                    COALESCE(SUM(output_cost_usd), 0.0) as output_cost_usd,
                    COALESCE(SUM(total_cost_usd), 0.0) as total_cost_usd,
                    COUNT(*) as task_count
             FROM task_metrics
             WHERE completed_at >= datetime('now', '-30 days')",
                "30d",
            ),
        ];

        let mut periods = Vec::new();
        for (query, label) in windows {
            let row = sqlx::query(query).fetch_one(&self.pool).await?;
            periods.push(CostPeriod {
                label: label.to_string(),
                input_tokens: row.get("input_tokens"),
                output_tokens: row.get("output_tokens"),
                input_cost_usd: row.get("input_cost_usd"),
                output_cost_usd: row.get("output_cost_usd"),
                total_cost_usd: row.get("total_cost_usd"),
                task_count: row.get("task_count"),
            });
        }

        Ok(CostSummary { periods })
    }

    /// Get cost breakdown by agent.
    pub async fn get_cost_by_agent(&self) -> anyhow::Result<Vec<CostByGroup>> {
        let rows = sqlx::query(
            "SELECT agent as name,
                COALESCE(SUM(input_tokens), 0) as input_tokens,
                COALESCE(SUM(output_tokens), 0) as output_tokens,
                COALESCE(SUM(total_cost_usd), 0.0) as total_cost_usd,
                COUNT(*) as task_count
         FROM task_metrics
         GROUP BY agent
         ORDER BY SUM(total_cost_usd) DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(|row| CostByGroup {
                name: row.get("name"),
                input_tokens: row.get("input_tokens"),
                output_tokens: row.get("output_tokens"),
                total_cost_usd: row.get("total_cost_usd"),
                task_count: row.get("task_count"),
            })
            .collect())
    }

    /// Get cost breakdown by model.
    pub async fn get_cost_by_model(&self) -> anyhow::Result<Vec<CostByGroup>> {
        let rows = sqlx::query(
            "SELECT COALESCE(model, 'unknown') as name,
                COALESCE(SUM(input_tokens), 0) as input_tokens,
                COALESCE(SUM(output_tokens), 0) as output_tokens,
                COALESCE(SUM(total_cost_usd), 0.0) as total_cost_usd,
                COUNT(*) as task_count
         FROM task_metrics
         GROUP BY model
         ORDER BY SUM(total_cost_usd) DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(|row| CostByGroup {
                name: row.get("name"),
                input_tokens: row.get("input_tokens"),
                output_tokens: row.get("output_tokens"),
                total_cost_usd: row.get("total_cost_usd"),
                task_count: row.get("task_count"),
            })
            .collect())
    }

    /// Get the duration_seconds from the most recent metric row for a task.
    /// Returns `None` when no metric has been recorded yet.
    pub async fn latest_task_metric_duration(&self, task_id: &str) -> Option<f64> {
        sqlx::query_scalar(
            "SELECT duration_seconds FROM task_metrics WHERE task_id = ? ORDER BY completed_at DESC LIMIT 1",
        )
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await
        .ok()
        .flatten()
    }

    /// Get count of self-improvement issues created this week.
    pub async fn count_self_improvement_issues_7d(&self) -> anyhow::Result<i64> {
        let key = self_improvement_key();
        let count = self.kv_get(&key).await?;
        Ok(count.and_then(|c| c.parse().ok()).unwrap_or(0))
    }

    /// Get tasks with high review cycle counts (persistent review loops) from the last 7 days.
    pub async fn get_high_review_cycle_tasks_7d(&self) -> anyhow::Result<Vec<HighReviewCycleTask>> {
        let rows = sqlx::query(
            "SELECT external_id, agent, review_cycles, title
         FROM tasks
         WHERE review_cycles >= 2
           AND updated_at >= datetime('now', '-7 days')
         ORDER BY review_cycles DESC
         LIMIT 10",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(|row| HighReviewCycleTask {
                external_id: row.get("external_id"),
                agent: row.get("agent"),
                review_cycles: row.get("review_cycles"),
                title: row.get("title"),
            })
            .collect())
    }

    /// Increment the self-improvement issue counter for the current week.
    /// Uses a single atomic SQL statement to avoid TOCTOU race conditions.
    pub async fn increment_self_improvement_counter(&self) -> anyhow::Result<()> {
        let key = self_improvement_key();
        sqlx::query(
        "INSERT INTO kv (key, value, updated_at) VALUES (?, '1', strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
         ON CONFLICT(key) DO UPDATE SET value = CAST(value AS INTEGER) + 1, updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
    )
    .bind(&key)
    .execute(&self.pool)
    .await?;
        Ok(())
    }
}
