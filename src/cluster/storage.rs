// openraft's trait methods return its own StorageError, so its size is not ours to change
#![allow(clippy::result_large_err)]

use std::fmt::Debug;
use std::io::Cursor;
use std::ops::{Bound, RangeBounds};
use std::path::Path;

use openraft::storage::{LogState, RaftLogReader, RaftSnapshotBuilder, Snapshot};
use openraft::{
    BasicNode, Entry, EntryPayload, LogId, OptionalSend, RaftLogId, RaftStorage, SnapshotMeta,
    StorageError, StorageIOError, StoredMembership, Vote,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::{Acquire, SqliteConnection};

use super::{NodeId, Reply, TypeConfig};
use crate::store::{kv, replica, tasks, TaskStore};

type Err = StorageError<NodeId>;

const APPLIED_KEY: &str = "raft:applied";
const MEMBERSHIP_KEY: &str = "raft:membership";

fn io<E: std::error::Error + 'static>(e: E) -> Err {
    StorageIOError::read(&e).into()
}

fn io_any(e: anyhow::Error) -> Err {
    io(std::io::Error::other(format!("{e:#}")))
}

/// The replicated rows plus the Raft position they reflect
#[derive(Serialize, Deserialize)]
struct SnapshotData {
    last_applied: Option<LogId<NodeId>>,
    membership: StoredMembership<NodeId, BasicNode>,
    tasks: Vec<serde_json::Value>,
    tables: std::collections::BTreeMap<String, Vec<serde_json::Value>>,
    kv: Vec<kv::Row>,
}

#[derive(Serialize, Deserialize)]
struct StoredSnapshot {
    meta: SnapshotMeta<NodeId, BasicNode>,
    data: Vec<u8>,
}

/**
 * The log and vote live in `raft.db`, the state machine is orch's own database. Apply writes
 * each entry and `raft:applied` in one transaction on `orch.db`, so a crash never applies an
 * entry twice, and a log that runs ahead of the state is normal for Raft
 **/
#[derive(Clone)]
pub struct RaftStore {
    log: SqlitePool,
    store: TaskStore,
}

impl RaftStore {
    pub async fn open(raft_db: &Path, store: TaskStore) -> anyhow::Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(raft_db)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .create_if_missing(true);
        let log = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS log (idx INTEGER PRIMARY KEY, entry TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )
        .execute(&log)
        .await?;
        Ok(Self { log, store })
    }

    async fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, Err> {
        let v: Option<(String,)> = sqlx::query_as("SELECT value FROM meta WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.log)
            .await
            .map_err(io)?;
        v.map(|(s,)| serde_json::from_str(&s).map_err(io))
            .transpose()
    }

    async fn put<T: Serialize>(&self, key: &str, value: &T) -> Result<(), Err> {
        let s = serde_json::to_string(value).map_err(io)?;
        sqlx::query("INSERT OR REPLACE INTO meta (key, value) VALUES (?, ?)")
            .bind(key)
            .bind(s)
            .execute(&self.log)
            .await
            .map_err(io)?;
        Ok(())
    }

    async fn state_get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, Err> {
        let v = self.store.kv_get(key).await.map_err(io_any)?;
        v.map(|s| serde_json::from_str(&s).map_err(io)).transpose()
    }
}

async fn state_put<T: Serialize>(
    conn: &mut SqliteConnection,
    now: &str,
    key: &str,
    value: &T,
) -> Result<(), Err> {
    let s = serde_json::to_string(value).map_err(io)?;
    kv::set(conn, now, key, &s).await.map_err(io_any)
}

impl RaftLogReader<TypeConfig> for RaftStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + OptionalSend>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<TypeConfig>>, Err> {
        let start = match range.start_bound() {
            Bound::Included(&s) => s,
            Bound::Excluded(&s) => s + 1,
            Bound::Unbounded => 0,
        };
        let end = match range.end_bound() {
            Bound::Included(&e) => e.saturating_add(1),
            Bound::Excluded(&e) => e,
            Bound::Unbounded => u64::MAX,
        };
        // SQLite integers are i64, so the open end is clamped
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT entry FROM log WHERE idx >= ? AND idx < ? ORDER BY idx")
                .bind(start as i64)
                .bind(end.min(i64::MAX as u64) as i64)
                .fetch_all(&self.log)
                .await
                .map_err(io)?;
        rows.into_iter()
            .map(|(s,)| serde_json::from_str(&s).map_err(io))
            .collect()
    }
}

impl RaftSnapshotBuilder<TypeConfig> for RaftStore {
    async fn build_snapshot(&mut self) -> Result<Snapshot<TypeConfig>, Err> {
        let mut tables = std::collections::BTreeMap::new();
        for table in replica::TABLES {
            let rows = replica::table_rows(self.store.pool(), table)
                .await
                .map_err(io_any)?;
            tables.insert(table.to_string(), rows);
        }

        let snap = SnapshotData {
            tables,
            last_applied: self.state_get(APPLIED_KEY).await?,
            membership: self.state_get(MEMBERSHIP_KEY).await?.unwrap_or_default(),
            tasks: tasks::replicated_rows(self.store.pool())
                .await
                .map_err(io_any)?,
            kv: kv::replicated_rows(self.store.pool())
                .await
                .map_err(io_any)?,
        };
        let data = serde_json::to_vec(&snap).map_err(io)?;
        let last = snap.last_applied;
        let meta = SnapshotMeta {
            last_log_id: last,
            last_membership: snap.membership,
            snapshot_id: last
                .map(|l| format!("{}-{}", l.leader_id, l.index))
                .unwrap_or_default(),
        };
        self.put(
            "snapshot",
            &StoredSnapshot {
                meta: meta.clone(),
                data: data.clone(),
            },
        )
        .await?;
        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(data)),
        })
    }
}

impl RaftStorage<TypeConfig> for RaftStore {
    type LogReader = Self;
    type SnapshotBuilder = Self;

    async fn save_vote(&mut self, vote: &Vote<NodeId>) -> Result<(), Err> {
        self.put("vote", vote).await
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<NodeId>>, Err> {
        self.get("vote").await
    }

    async fn save_committed(&mut self, committed: Option<LogId<NodeId>>) -> Result<(), Err> {
        self.put("committed", &committed).await
    }

    async fn read_committed(&mut self) -> Result<Option<LogId<NodeId>>, Err> {
        Ok(self
            .get::<Option<LogId<NodeId>>>("committed")
            .await?
            .flatten())
    }

    async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, Err> {
        let purged = self.get::<Option<LogId<NodeId>>>("purged").await?.flatten();
        let last: Option<(String,)> =
            sqlx::query_as("SELECT entry FROM log ORDER BY idx DESC LIMIT 1")
                .fetch_optional(&self.log)
                .await
                .map_err(io)?;
        let last = match last {
            Some((s,)) => Some(
                *serde_json::from_str::<Entry<TypeConfig>>(&s)
                    .map_err(io)?
                    .get_log_id(),
            ),
            None => purged,
        };
        Ok(LogState {
            last_purged_log_id: purged,
            last_log_id: last,
        })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn append_to_log<I>(&mut self, entries: I) -> Result<(), Err>
    where
        I: IntoIterator<Item = Entry<TypeConfig>> + OptionalSend,
    {
        let rows = entries
            .into_iter()
            .map(|e| {
                Ok((
                    e.log_id.index as i64,
                    serde_json::to_string(&e).map_err(io)?,
                ))
            })
            .collect::<Result<Vec<_>, Err>>()?;
        let mut tx = self.log.begin().await.map_err(io)?;
        for (idx, s) in rows {
            sqlx::query("INSERT OR REPLACE INTO log (idx, entry) VALUES (?, ?)")
                .bind(idx)
                .bind(s)
                .execute(&mut *tx)
                .await
                .map_err(io)?;
        }

        tx.commit().await.map_err(io)
    }

    async fn delete_conflict_logs_since(&mut self, log_id: LogId<NodeId>) -> Result<(), Err> {
        sqlx::query("DELETE FROM log WHERE idx >= ?")
            .bind(log_id.index as i64)
            .execute(&self.log)
            .await
            .map_err(io)?;
        Ok(())
    }

    async fn purge_logs_upto(&mut self, log_id: LogId<NodeId>) -> Result<(), Err> {
        self.put("purged", &Some(log_id)).await?;
        sqlx::query("DELETE FROM log WHERE idx <= ?")
            .bind(log_id.index as i64)
            .execute(&self.log)
            .await
            .map_err(io)?;
        Ok(())
    }

    async fn last_applied_state(
        &mut self,
    ) -> Result<(Option<LogId<NodeId>>, StoredMembership<NodeId, BasicNode>), Err> {
        Ok((
            self.state_get(APPLIED_KEY).await?,
            self.state_get(MEMBERSHIP_KEY).await?.unwrap_or_default(),
        ))
    }

    async fn apply_to_state_machine(
        &mut self,
        entries: &[Entry<TypeConfig>],
    ) -> Result<Vec<Reply>, Err> {
        let mut tx = self.store.begin_write().await.map_err(io)?;
        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            let reply = match &e.payload {
                EntryPayload::Blank => Reply::Done,
                EntryPayload::Normal(req) => {
                    // A savepoint per entry, so a failed command rolls back alone
                    let mut sp = tx.begin().await.map_err(io)?;
                    match req.cmd.clone().apply(&mut sp, &req.now, true).await {
                        Ok(reply) => {
                            sp.commit().await.map_err(io)?;
                            reply
                        }
                        Err(err) => {
                            sp.rollback().await.map_err(io)?;
                            Reply::Err(format!("{err:#}"))
                        }
                    }
                }
                EntryPayload::Membership(m) => {
                    let stored = StoredMembership::new(Some(e.log_id), m.clone());
                    state_put(&mut tx, "", MEMBERSHIP_KEY, &stored).await?;
                    Reply::Done
                }
            };
            state_put(&mut tx, "", APPLIED_KEY, &e.log_id).await?;
            out.push(reply);
        }

        tx.commit().await.map_err(io)?;
        Ok(out)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }

    async fn begin_receiving_snapshot(&mut self) -> Result<Box<Cursor<Vec<u8>>>, Err> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<NodeId, BasicNode>,
        snapshot: Box<Cursor<Vec<u8>>>,
    ) -> Result<(), Err> {
        let data = snapshot.into_inner();
        let snap: SnapshotData = serde_json::from_slice(&data).map_err(io)?;
        let mut tx = self.store.begin_write().await.map_err(io)?;
        tasks::replace_replicated(&mut tx, &snap.tasks)
            .await
            .map_err(io_any)?;
        for table in replica::TABLES {
            let rows = snap
                .tables
                .get(table)
                .map(Vec::as_slice)
                .unwrap_or_default();
            replica::replace_table(&mut tx, table, rows)
                .await
                .map_err(io_any)?;
        }
        kv::replace_replicated(&mut tx, &snap.kv)
            .await
            .map_err(io_any)?;
        state_put(&mut tx, "", APPLIED_KEY, &meta.last_log_id).await?;
        state_put(&mut tx, "", MEMBERSHIP_KEY, &meta.last_membership).await?;
        tx.commit().await.map_err(io)?;
        self.put(
            "snapshot",
            &StoredSnapshot {
                meta: meta.clone(),
                data,
            },
        )
        .await
    }

    async fn get_current_snapshot(&mut self) -> Result<Option<Snapshot<TypeConfig>>, Err> {
        let s: Option<StoredSnapshot> = self.get("snapshot").await?;
        Ok(s.map(|s| Snapshot {
            meta: s.meta,
            snapshot: Box::new(Cursor::new(s.data)),
        }))
    }
}
