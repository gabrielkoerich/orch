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

pub mod cluster;
pub(crate) mod control;
pub(crate) mod dependencies;
mod helpers;
pub(crate) mod jobs;
pub(crate) mod kv;
mod metrics;
pub(crate) mod replica;
pub(crate) mod tasks;

#[cfg(test)]
mod tests;
mod usage;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use sqlx::SqliteConnection;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tasks::{StoreRouteCmd, UpsertExternalCmd, apply};

pub use control::{ChatCostSummary, ControlMessage, MemoryEntry, parse_since_duration};
pub use dependencies::{
    BlockerSnapshot, DepSource, NewDependency, ObservedPullRequest, TaskDependency,
    WaitingOnDependency, blocker_failure_kind, blocker_is_satisfied, blocker_snapshot_key,
    dep_label, worktree_refresh_key,
};
pub use helpers::{
    StoreResult, get_recent_memory, get_recent_memory_result, get_reported_cost,
    get_reported_cost_result, get_task_field, get_task_field_direct, get_token_usage,
    get_token_usage_result, opt_store_get_task, opt_store_get_task_by_id, resolve_store_id,
    store_increment, store_increment_by_id, store_log_activity, store_reset_counters,
    store_reset_failure_counters, store_set, store_set_by_id, store_set_result,
    store_set_result_by_id, store_touch_updated_at,
};
pub use jobs::JobState;
pub use metrics::{
    AgentStat, CostByGroup, CostPeriod, CostSummary, ErrorStat, HighReviewCycleTask,
    InsertTaskMetric, MetricsSummary, SlowTaskInfo,
};
use sqlx::error::DatabaseError;
pub use tasks::{
    CliTaskRef, CliTaskSpec, CompleteRun, NewTask, RunTokenUsage, StartRun, StatsEvents,
    StatusChange, StoreRoute, Task, TaskActivity, TaskRun, TaskStatus, UpsertExternal,
    parse_cli_task_spec,
};
use tokio::time;
pub use usage::TokenUsage;

use crate::home;
use cluster::Node;
use cluster::local;
use cluster::local::Endpoint;

/// Default database path: `~/.orch/orch.db`
pub async fn default_db_path() -> anyhow::Result<PathBuf> {
    home::db_path().await
}

/// The unified task store backed by SQLite (via sqlx).
#[derive(Clone)]
pub struct TaskStore {
    pool: SqlitePool,
    /// Set once on the daemon. Without it, replicated writes apply directly, as in tests and the CLI
    raft: Arc<OnceLock<Node>>,
    /// Set on the CLI when a daemon runs, so its writes go through the daemon's log
    daemon: Arc<OnceLock<Endpoint>>,
}

impl Debug for TaskStore {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskStore").finish_non_exhaustive()
    }
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
            .busy_timeout(Duration::from_secs(5))
            .pragma("foreign_keys", "ON");

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .with_context(|| format!("opening task store: {}", db_path.display()))?;

        let store = Self {
            pool,
            raft: Arc::default(),
            daemon: Arc::default(),
        };
        store.migrate().await?;
        Ok(store)
    }

    /// Open a store with a single connection.
    ///
    /// Uses `max_connections(1)` to avoid WAL visibility issues where a write
    /// on one pooled connection isn't visible to a read on another. Used by
    /// integration tests that need read-your-own-writes consistency.
    pub async fn open_single(db_path: &Path) -> anyhow::Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(db_path)
            .journal_mode(SqliteJournalMode::Wal)
            .create_if_missing(true)
            .busy_timeout(Duration::from_secs(5))
            .pragma("foreign_keys", "ON");

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .with_context(|| format!("opening task store (single): {}", db_path.display()))?;

        let store = Self {
            pool,
            raft: Arc::default(),
            daemon: Arc::default(),
        };
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

        let store = Self {
            pool,
            raft: Arc::default(),
            daemon: Arc::default(),
        };
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

    pub fn attach_node(&self, node: Node) {
        if self.raft.set(node).is_err() {
            tracing::warn!("raft already attached to the task store");
        }
    }

    /// True on the node that decides: the Raft leader, or a store with no Raft, as in the CLI and tests.
    /// A single node always leads, so one machine runs every phase as before
    pub fn is_leader(&self) -> bool {
        self.raft.get().is_none_or(cluster::Node::is_leader)
    }

    /// Writes go through Raft or a daemon that runs it, so they reach the other machines
    pub(crate) fn replicates(&self) -> bool {
        self.raft.get().is_some() || self.daemon.get().is_some()
    }

    pub fn node(&self) -> Option<&Node> {
        self.raft.get()
    }

    pub fn attach_daemon(&self, endpoint: Endpoint) {
        let _ = self.daemon.set(endpoint);
    }

    /// Propose a replicated write through Raft, or apply it directly when no Raft is attached
    pub(crate) async fn write(&self, cmd: Command) -> anyhow::Result<Reply> {
        let Some(node) = self.raft.get() else {
            if let Some(endpoint) = self.daemon.get() {
                match local::send(endpoint, &cmd).await {
                    Ok(reply) => return reply_result(reply),
                    // Nothing was sent, so applying here cannot apply the command twice
                    Err(e) if e.is_connect() => {
                        tracing::debug!(err = %e, "daemon not listening, writing directly");
                    }
                    Err(e) => return Err(e.into()),
                }
            }

            return self.apply_direct(cmd).await;
        };
        let req = Request {
            now: now_stamp(),
            cmd,
        };
        reply_result(node.write(req).await?)
    }

    pub(crate) async fn write_int(&self, cmd: Command) -> anyhow::Result<i64> {
        match self.write(cmd).await? {
            Reply::Int(n) => Ok(n),
            other => anyhow::bail!("expected an integer reply, got {other:?}"),
        }
    }

    pub(crate) async fn write_bool(&self, cmd: Command) -> anyhow::Result<bool> {
        match self.write(cmd).await? {
            Reply::Bool(b) => Ok(b),
            other => anyhow::bail!("expected a boolean reply, got {other:?}"),
        }
    }

    /// The same SQL apply runs, in one transaction on this node only
    pub(crate) async fn apply_direct(&self, cmd: Command) -> anyhow::Result<Reply> {
        let mut attempt = 0;
        loop {
            match self.apply_once(cmd.clone()).await {
                // A dropped transaction rolls back, so running the command again is safe
                Err(e) if attempt < WRITE_LOCK_RETRIES && is_write_lock_error(&e) => {
                    attempt += 1;
                    tracing::warn!(attempt, err = %e, "database is locked, retrying the write");
                    time::sleep(Duration::from_millis(100 << (attempt - 1))).await;
                }
                result => return result,
            }
        }
    }

    async fn apply_once(&self, cmd: Command) -> anyhow::Result<Reply> {
        let mut tx = self.begin_write().await?;
        let reply = cmd.apply(&mut tx, &now_stamp(), false).await?;
        tx.commit().await?;
        Ok(reply)
    }

    /// Start a transaction that holds the write lock from its first statement.
    ///
    /// A deferred `BEGIN` that reads and then writes fails at once with `database is locked` when
    /// another connection commits in between, and `busy_timeout` cannot wait that out. `BEGIN
    /// IMMEDIATE` waits for the lock up front, where `busy_timeout` applies
    pub(crate) async fn begin_write(
        &self,
    ) -> sqlx::Result<sqlx::Transaction<'static, sqlx::Sqlite>> {
        self.pool.begin_with("BEGIN IMMEDIATE").await
    }

    /// Get a reference to the underlying pool (for advanced queries).
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

/// Retries for a write that still finds the database locked after `busy_timeout`
const WRITE_LOCK_RETRIES: u32 = 4;

/// `SQLITE_BUSY` (5) or `SQLITE_BUSY_SNAPSHOT` (517) anywhere in the error chain
fn is_write_lock_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<sqlx::Error>()
            .and_then(|e| e.as_database_error())
            .and_then(DatabaseError::code)
            .is_some_and(|code| code == "5" || code == "517")
    })
}

fn reply_result(reply: Reply) -> anyhow::Result<Reply> {
    match reply {
        Reply::Err(e) => Err(anyhow::anyhow!(e)),
        other => Ok(other),
    }
}

impl TaskStore {
    /// The replicated tables, task rows and kv rows a raft snapshot carries
    pub(crate) async fn snapshot_rows(
        &self,
    ) -> anyhow::Result<(
        BTreeMap<String, Vec<serde_json::Value>>,
        Vec<serde_json::Value>,
        Vec<kv::Row>,
    )> {
        let mut tables = BTreeMap::new();
        for table in replica::TABLES {
            tables.insert(
                table.to_string(),
                replica::table_rows(&self.pool, table).await?,
            );
        }
        let tasks = tasks::replicated_rows(&self.pool).await?;
        let kv = kv::replicated_rows(&self.pool).await?;
        Ok((tables, tasks, kv))
    }
}

type Updates = Vec<(String, serde_json::Value)>;

/// One replicated write. The proposer stamps `now`, so every node applies the same value
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Request {
    pub now: String,
    pub cmd: Command,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Command {
    KvSet {
        key: String,
        value: String,
    },
    KvIncrement {
        key: String,
    },
    KvInsertIfAbsent {
        key: String,
        value: String,
    },
    KvDelete {
        key: String,
    },
    TaskCreate {
        new: NewTask,
        internal: bool,
    },
    TaskUpsertExternal(UpsertExternalCmd),
    TaskUpdateStatusAndFields {
        id: i64,
        status: TaskStatus,
        updates: Updates,
    },
    TaskUpdateStatus {
        id: i64,
        status: TaskStatus,
        expected: Option<TaskStatus>,
    },
    TaskUpdateExternalId {
        id: i64,
        external_id: String,
    },
    TaskTouch {
        id: i64,
    },
    TaskResetToNew {
        id: i64,
        #[serde(default)]
        keep_branch: bool,
    },
    TaskSetFields {
        id: i64,
        updates: Updates,
        silent: bool,
    },
    TaskIncrement {
        id: i64,
        field: String,
    },
    TaskResetCounters {
        id: i64,
        failure_only: bool,
    },
    TaskStoreRoute(StoreRouteCmd),
    TaskSetEstimateIfZero {
        id: i64,
        estimate: u8,
    },
    TaskAppendMemory {
        id: i64,
        entry: MemoryEntry,
    },
    TaskMarkCleaned {
        id: i64,
    },
    MoveRepo {
        old: String,
        new: String,
    },
    DepReplace {
        task_id: i64,
        rows: Vec<NewDependency>,
    },
    DepLinkKnown {
        task_id: i64,
        repo: String,
        number: i64,
    },
    DepBlockClaim {
        task_id: i64,
    },
    JobStateUpsert(JobState),
    ChannelSubscribe {
        channel: String,
        thread_id: String,
        repo: String,
        topic_id: String,
    },
    ChannelUnsubscribe {
        channel: String,
        thread_id: String,
        repo: String,
    },
}

/// A failed command is `Err` on every node, so one bad command cannot stop the state machine
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Reply {
    Done,
    Bool(bool),
    Int(i64),
    Count(u64),
    Value(String),
    Unmet(Option<Vec<String>>),
    Err(String),
}

impl Command {
    /// A write through the log may not touch this node's own state: a local kv key, such as its
    /// Raft position, or a local task column, such as the worktree path that cleanup removes
    pub(crate) fn check_replicable(&self) -> anyhow::Result<()> {
        let key = match self {
            Command::KvSet { key, .. }
            | Command::KvIncrement { key }
            | Command::KvInsertIfAbsent { key, .. }
            | Command::KvDelete { key } => Some(key),
            _ => None,
        };
        if let Some(key) = key {
            anyhow::ensure!(
                !kv::is_local(key),
                "{key} is local to each node, never replicated"
            );
        }

        let updates = match self {
            Command::TaskSetFields { updates, .. }
            | Command::TaskUpdateStatusAndFields { updates, .. } => &updates[..],
            _ => &[],
        };
        if let Some((col, _)) = updates.iter().find(|(col, _)| tasks::is_local_column(col)) {
            anyhow::bail!("{col} is local to each node, never replicated");
        }

        Ok(())
    }

    /// `replicated` is true when Raft applies the command, so new ids come from the replicated
    /// rows. A direct write keeps SQLite's AUTOINCREMENT, as orch on one machine always has
    pub(crate) async fn apply(
        self,
        conn: &mut SqliteConnection,
        now: &str,
        replicated: bool,
    ) -> anyhow::Result<Reply> {
        if replicated {
            self.check_replicable()?;
        }

        Ok(match self {
            Command::KvSet { key, value } => {
                kv::set(conn, now, &key, &value).await?;
                Reply::Done
            }
            Command::KvIncrement { key } => Reply::Count(kv::increment(conn, now, &key).await?),
            Command::KvInsertIfAbsent { key, value } => {
                Reply::Value(kv::insert_if_absent(conn, now, &key, &value).await?)
            }
            Command::KvDelete { key } => {
                kv::delete(conn, &key).await?;
                Reply::Done
            }
            Command::TaskCreate { new, internal } => {
                Reply::Int(apply::create(conn, now, &new, internal, replicated).await?)
            }
            Command::TaskUpsertExternal(ext) => {
                Reply::Int(apply::upsert_external(conn, now, &ext, replicated).await?)
            }
            Command::TaskUpdateStatusAndFields {
                id,
                status,
                updates,
            } => {
                apply::update_status_and_fields(conn, now, id, status, &updates).await?;
                Reply::Done
            }
            Command::TaskUpdateStatus {
                id,
                status,
                expected,
            } => Reply::Bool(apply::update_status(conn, now, id, status, expected).await?),
            Command::TaskUpdateExternalId { id, external_id } => {
                apply::update_external_id(conn, now, id, &external_id).await?;
                Reply::Done
            }
            Command::TaskTouch { id } => {
                apply::touch(conn, now, id).await?;
                Reply::Done
            }
            Command::TaskResetToNew { id, keep_branch } => {
                apply::reset_to_new(conn, now, id, keep_branch).await?;
                Reply::Done
            }
            Command::TaskSetFields {
                id,
                updates,
                silent,
            } => {
                apply::set_fields(conn, now, id, &updates, silent).await?;
                Reply::Done
            }
            Command::TaskIncrement { id, field } => {
                Reply::Int(apply::increment(conn, now, id, &field).await?)
            }
            Command::TaskResetCounters { id, failure_only } => {
                apply::reset_counters(conn, now, id, failure_only).await?;
                Reply::Done
            }
            Command::TaskStoreRoute(route) => {
                apply::store_route(conn, now, &route).await?;
                Reply::Done
            }
            Command::TaskSetEstimateIfZero { id, estimate } => {
                Reply::Bool(apply::set_estimate_if_zero(conn, now, id, estimate).await?)
            }
            Command::TaskAppendMemory { id, entry } => {
                apply::append_memory(conn, now, id, &entry).await?;
                Reply::Done
            }
            Command::TaskMarkCleaned { id } => {
                apply::mark_cleaned(conn, now, id).await?;
                Reply::Done
            }
            Command::MoveRepo { old, new } => {
                let moved = dependencies::apply::move_repo(conn, &old, &new).await?;
                Reply::Int(i64::try_from(moved)?)
            }
            Command::DepReplace { task_id, rows } => {
                dependencies::apply::replace_dependencies(conn, task_id, &rows).await?;
                Reply::Done
            }
            Command::DepLinkKnown {
                task_id,
                repo,
                number,
            } => {
                dependencies::apply::link_known_task(conn, task_id, &repo, number).await?;
                Reply::Done
            }
            Command::DepBlockClaim { task_id } => {
                Reply::Unmet(dependencies::apply::block_claim(conn, now, task_id).await?)
            }
            Command::JobStateUpsert(state) => {
                jobs::apply::upsert_job_state(conn, &state).await?;
                Reply::Done
            }
            Command::ChannelSubscribe {
                channel,
                thread_id,
                repo,
                topic_id,
            } => {
                jobs::apply::subscribe(conn, &channel, &thread_id, &repo, &topic_id).await?;
                Reply::Done
            }
            Command::ChannelUnsubscribe {
                channel,
                thread_id,
                repo,
            } => {
                jobs::apply::unsubscribe(conn, &channel, &thread_id, &repo).await?;
                Reply::Done
            }
        })
    }
}

/// The proposer's clock, in the format the store writes timestamps in
pub fn now_stamp() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// A kv row as a raft snapshot carries it
pub type KvRow = kv::Row;

/// Write a kv value inside a raft storage transaction
pub(crate) async fn put_kv(
    conn: &mut SqliteConnection,
    now: &str,
    key: &str,
    value: &str,
) -> anyhow::Result<()> {
    kv::set(conn, now, key, value).await
}

/// Replace the replicated rows with a raft snapshot's
pub(crate) async fn replace_snapshot_rows(
    conn: &mut SqliteConnection,
    tables: &BTreeMap<String, Vec<serde_json::Value>>,
    task_rows: &[serde_json::Value],
    kv_rows: &[KvRow],
) -> anyhow::Result<()> {
    tasks::replace_replicated(conn, task_rows).await?;
    for table in replica::TABLES {
        let rows = tables.get(table).map(Vec::as_slice).unwrap_or_default();
        replica::replace_table(conn, table, rows).await?;
    }
    kv::replace_replicated(conn, kv_rows).await
}
