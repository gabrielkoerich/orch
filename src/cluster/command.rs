use serde::{Deserialize, Serialize};
use sqlx::SqliteConnection;

use crate::store::control::MemoryEntry;
use crate::store::kv;
use crate::store::tasks::{apply, NewTask, StoreRouteCmd, TaskStatus, UpsertExternalCmd};

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
}

/// A failed command is `Err` on every node, so one bad command cannot stop the state machine
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Reply {
    Done,
    Bool(bool),
    Int(i64),
    Count(u64),
    Value(String),
    Err(String),
}

impl Command {
    pub(crate) async fn apply(
        self,
        conn: &mut SqliteConnection,
        now: &str,
    ) -> anyhow::Result<Reply> {
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
                Reply::Int(apply::create(conn, now, &new, internal).await?)
            }
            Command::TaskUpsertExternal(ext) => {
                Reply::Int(apply::upsert_external(conn, now, &ext).await?)
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
            Command::TaskResetToNew { id } => {
                apply::reset_to_new(conn, now, id).await?;
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
        })
    }
}
