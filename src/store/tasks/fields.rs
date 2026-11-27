//! Task field, counter and route writes

#[cfg(test)]
use super::{ALLOWED_FIELDS, TASK_COLS, Task, sql_value_for_task_field};
use super::{
    INCREMENTABLE_FIELDS, StoreRoute, StoreRouteCmd, apply, is_local_column, owned_updates,
};
use crate::cluster;
use crate::cluster::Command;
use crate::store::TaskStore;
use sqlx::Row;
use std::collections::HashSet;

impl TaskStore {
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
            apply::set_fields(&mut tx, &cluster::now_stamp(), id, &all, silent).await?;
            tx.commit().await?;
            return Ok(());
        }

        let (local, replicated): (Vec<_>, Vec<_>) =
            all.into_iter().partition(|(col, _)| is_local_column(col));
        if !local.is_empty() {
            let mut tx = self.begin_write().await?;
            apply::set_fields(&mut tx, &cluster::now_stamp(), id, &local, silent).await?;
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
            timeout_retries = 0,
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
    pub async fn cleaned_external_ids(&self, repo: &str) -> anyhow::Result<HashSet<String>> {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT external_id FROM tasks WHERE repo = ? AND worktree_cleaned = 1 AND external_id IS NOT NULL")
                .bind(repo)
                .fetch_all(&self.pool)
                .await?;

        Ok(rows.into_iter().map(|(id,)| id).collect())
    }
}
