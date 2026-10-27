use serde::{Deserialize, Serialize};
use sqlx::SqliteConnection;

use crate::store::kv;

/// One replicated write. The proposer stamps `now`, so every node applies the same value
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Request {
    pub now: String,
    pub cmd: Command,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Command {
    KvSet { key: String, value: String },
    KvIncrement { key: String },
    KvInsertIfAbsent { key: String, value: String },
    KvDelete { key: String },
}

/// A failed command is `Err` on every node, so one bad command cannot stop the state machine
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Reply {
    Done,
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
        })
    }
}
