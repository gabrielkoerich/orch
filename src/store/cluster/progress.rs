//! `orch progress` records: what an agent finished, what is left, and where the work is. The record
//! goes through the log, the daemon's engine commits and pushes before it writes one

use serde::{Deserialize, Serialize};

use crate::store::TaskStore;

/// Each text field is cut to this many characters, a record is a pointer and not a log
const MAX_TEXT: usize = 500;
/// Records kept per task, oldest dropped first
const KEEP: usize = 5;

/// What the CLI sends after it tried to commit in the worktree itself
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub worktree: String,
    pub done: String,
    pub left: String,
    pub state: String,
    pub cli_committed: bool,
    #[serde(default)]
    pub cli_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub node: String,
    pub ts: String,
    pub branch: String,
    pub commit: String,
    /// `agent`, `cli`, `orch` or `none`
    pub committed_by: String,
    /// `agent`, `orch` or `none`
    pub pushed_by: String,
    #[serde(default)]
    pub error: Option<String>,
    pub done: String,
    pub left: String,
    pub state: String,
}

/// A text field cut to the length a record keeps
pub fn cut(s: &str) -> String {
    s.chars().take(MAX_TEXT).collect()
}

fn key(task_id: i64) -> String {
    format!("progress:{task_id}")
}

pub async fn list(store: &TaskStore, task_id: i64) -> anyhow::Result<Vec<Record>> {
    match store.kv_get(&key(task_id)).await? {
        Some(json) => Ok(serde_json::from_str(&json)?),
        None => Ok(Vec::new()),
    }
}

/// Add `record` to the task's records, keeping the newest few
pub async fn append(store: &TaskStore, task_id: i64, record: &Record) -> anyhow::Result<()> {
    let mut records = list(store, task_id).await?;
    records.push(record.clone());
    let skip = records.len().saturating_sub(KEEP);
    let records: Vec<Record> = records.into_iter().skip(skip).collect();
    store
        .kv_set(&key(task_id), &serde_json::to_string(&records)?)
        .await?;
    Ok(())
}
