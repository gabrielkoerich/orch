use serde::{Deserialize, Serialize};
use sqlx::SqliteConnection;

use crate::store::control::MemoryEntry;
use crate::store::dependencies::{self, NewDependency};
use crate::store::jobs::{self, JobState};
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
        if let Some((col, _)) = updates
            .iter()
            .find(|(col, _)| crate::store::tasks::is_local_column(col))
        {
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
