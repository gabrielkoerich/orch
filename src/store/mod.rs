//! Unified task store — SQLite as single source of truth.
//!
//! `TaskStore` replaces the legacy `internal_tasks` table and GitHub labels as the
//! authoritative task state. External backends (GitHub, Linear) become sync adapters
//! that mirror state changes outward.
//!
//! Uses sqlx for async SQLite access with file-based migrations.
//!
//! All task state, metrics, KV, and rate limits flow through this module.
//!
//! # Safety Warning: Never Use `SELECT *`
//!
//! Using `SELECT *` with `sqlx-sqlite`'s prepared statement cache can cause **OOB
//! (out-of-bounds) panics** when the schema changes (e.g., new migration adds a column).
//!
//! The issue occurs because:
//! 1. SQLite caches prepared statements with column metadata at prepare time
//! 2. When a migration adds a new column, `sqlite3_column_count()` returns the updated count
//! 3. The cached statement's column metadata vector was built with the old count
//! 4. Accessing column index N when only N columns exist (0..N-1) causes a panic
//!
//! **Solution:** Always use explicit column lists via constants like `TASK_COLS`,
//! `CONTROL_MESSAGE_COLS`, `JOB_STATE_COLS`, etc. See `tasks.rs` and `control.rs`
//! for examples. CI will fail if `SELECT *` is detected in store files.

use anyhow::Context;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use std::path::Path;

mod control;
mod helpers;
mod jobs;
mod kv;
mod metrics;
mod pricing;
mod tasks;

#[allow(unused_imports)]
pub use control::{parse_since_duration, ChatCostSummary, ControlMessage, MemoryEntry};
#[allow(unused_imports)]
pub use helpers::{
    get_cost_estimate, get_recent_memory, get_token_summary, get_token_usage, get_total_tokens,
    opt_store_get_task, opt_store_get_task_by_id, resolve_store_id, review_session_expected,
    set_review_session_expected, store_increment, store_increment_by_id, store_log_activity,
    store_reset_counters, store_reset_failure_counters, store_set, store_set_by_id,
    store_set_result, store_set_result_by_id, store_touch_updated_at, store_touch_updated_at_by_id,
};
#[allow(unused_imports)]
pub use jobs::JobState;
#[allow(unused_imports)]
pub use metrics::{
    AgentStat, CostByGroup, CostPeriod, CostSummary, ErrorStat, HighReviewCycleTask,
    InsertTaskMetric, MetricsSummary, SlowTaskInfo,
};
#[allow(unused_imports)]
pub use pricing::{pricing_for_model, CostEstimate, ModelPricing, TokenUsage};
#[allow(unused_imports)]
pub use tasks::{
    CompleteRun, NewTask, RunTokenUsage, StartRun, StoreRoute, Task, TaskActivity, TaskRun,
    TaskStatus, UpsertExternal,
};

/// Default database path: `~/.orch/orch.db`
pub async fn default_db_path() -> anyhow::Result<std::path::PathBuf> {
    crate::home::db_path().await
}

/// The unified task store backed by SQLite (via sqlx).
#[derive(Clone)]
pub struct TaskStore {
    pool: SqlitePool,
}

impl TaskStore {
    /// Open (or create) the store at the given database path.
    ///
    /// Runs file-based migrations from the `migrations/` directory.
    pub async fn open(db_path: &Path) -> anyhow::Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(db_path)
            .journal_mode(SqliteJournalMode::Wal)
            .create_if_missing(true)
            .busy_timeout(std::time::Duration::from_secs(5))
            .pragma("foreign_keys", "ON");

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .with_context(|| format!("opening task store: {}", db_path.display()))?;

        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    /// Open a store with a single connection.
    #[allow(dead_code)]
    ///
    /// Uses `max_connections(1)` to avoid WAL visibility issues where a write
    /// on one pooled connection isn't visible to a read on another. Used by
    /// integration tests that need read-your-own-writes consistency.
    pub async fn open_single(db_path: &Path) -> anyhow::Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(db_path)
            .journal_mode(SqliteJournalMode::Wal)
            .create_if_missing(true)
            .busy_timeout(std::time::Duration::from_secs(5))
            .pragma("foreign_keys", "ON");

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .with_context(|| format!("opening task store (single): {}", db_path.display()))?;

        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    /// Open an in-memory store (for testing).
    #[cfg(test)]
    pub async fn open_memory() -> anyhow::Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(":memory:")
            .pragma("foreign_keys", "ON");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;

        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    /// Run migrations.
    async fn migrate(&self) -> anyhow::Result<()> {
        sqlx::migrate!("./migrations")
            .run(&self.pool)
            .await
            .context("running task store migrations")?;
        Ok(())
    }

    /// Get a reference to the underlying pool (for advanced queries).
    #[allow(dead_code)]
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

#[cfg(test)]
mod tests;
