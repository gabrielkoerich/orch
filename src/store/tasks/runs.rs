use super::{CompleteRun, RunTokenUsage, StartRun, TASK_RUN_COLS, TaskRun};
use crate::cluster::Command;
use crate::security;
use crate::store::{MemoryEntry, TaskStore};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use std::collections::HashMap;

impl TaskStore {
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

    /// Add a run's token usage to a task, and its cost when the agent reported one.
    pub async fn store_tokens(
        &self,
        id: i64,
        input: u64,
        output: u64,
        cost_usd: Option<f64>,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE tasks SET
            input_tokens = COALESCE(input_tokens, 0) + ?,
            output_tokens = COALESCE(output_tokens, 0) + ?,
            total_cost_usd = COALESCE(total_cost_usd, 0.0) + COALESCE(?, 0.0),
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
        .bind(cost_usd)
        .bind(id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

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
        let updated = sqlx::query(
            "UPDATE task_runs SET
            exit_code = ?, stdout = ?, stderr = ?, parsed_response = ?,
            outcome = ?, error = NULLIF(?, ''),
            input_tokens = ?, output_tokens = ?, total_cost_usd = ?,
            duration_secs = CASE WHEN ? > 0 THEN ?
                ELSE MAX(0, (julianday('now') - julianday(started_at)) * 86400.0) END,
            completed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
         WHERE id = ? AND outcome IS NULL",
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
        if updated.rows_affected() == 0 && run_ctx.is_some() {
            // The first writer knows the cause, so a closed run keeps its outcome and error
            tracing::debug!(
                run_id = run.run_id,
                "run already closed, keeping its outcome"
            );
            return Ok(());
        }
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
    ) -> anyhow::Result<HashMap<i64, Vec<TaskRun>>> {
        // SQLite has a hard limit on host parameters (commonly 999). Chunk the
        // IN-list into smaller batches to avoid exceeding the limit.
        const CHUNK_SIZE: usize = 500;
        if task_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let mut result: HashMap<i64, Vec<TaskRun>> = HashMap::new();

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

    /// Clear the text columns of runs for tasks that entered `done` over `days` days ago
    /// Metrics stay, and blocked tasks keep every run
    pub async fn prune_old_runs(&self, days: i32) -> anyhow::Result<u64> {
        let result = sqlx::query(
            "UPDATE task_runs
             SET stdout = '', stderr = '', prompt = '', parsed_response = ''
             WHERE (stdout != '' OR stderr != '' OR prompt != '' OR parsed_response != '')
               AND task_id IN (
                 SELECT t.id FROM tasks t
                 WHERE t.status = 'done'
                   AND (SELECT MAX(a.timestamp) FROM task_activity a
                        WHERE a.task_id = t.id
                          AND a.event_type = 'status_change'
                          AND a.to_status = 'done')
                       < strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ? || ' days')
               )",
        )
        .bind(format!("-{days}"))
        .execute(&self.pool)
        .await?;

        if result.rows_affected() > 0 {
            sqlx::query("PRAGMA incremental_vacuum")
                .execute(&self.pool)
                .await?;
        }

        Ok(result.rows_affected())
    }

    pub(super) fn row_to_run(row: &SqliteRow) -> anyhow::Result<TaskRun> {
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
}
