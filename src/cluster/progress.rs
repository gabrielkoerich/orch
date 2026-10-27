//! `orch progress`: what an agent finished, what is left, and where the work is. The record goes
//! through the log, the session and the worktree stay on the node that runs the task

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::engine::runner::{git_ops, worktree};
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

fn cut(s: &str) -> String {
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

async fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let out = tokio::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .await
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Commit what is still uncommitted, push the branch, and record it. Only a known task's worktree
/// is touched, so a caller cannot point the daemon at another directory
pub async fn handle(store: &TaskStore, node: &str, report: Report) -> anyhow::Result<Record> {
    let task_id = store
        .resolve_id_by_worktree(&report.worktree)
        .await?
        .ok_or_else(|| anyhow::anyhow!("{} is not the worktree of a task", report.worktree))?;
    let task = store.get(task_id).await?;
    let dir = Path::new(&report.worktree);
    let previous = list(store, task_id).await?;
    let mut errors: Vec<String> = report.cli_error.iter().cloned().collect();

    let head_before = git_out(dir, &["rev-parse", "HEAD"])
        .await
        .unwrap_or_default();
    let mut committed_by = if report.cli_committed { "cli" } else { "none" };
    let ext = task.external_id.clone().unwrap_or_default();
    match git_ops::auto_commit(dir, &ext, &report.done, "orch", task.attempts as u32).await {
        Ok(true) => committed_by = "orch",
        Ok(false) => {}
        Err(e) => errors.push(format!("commit: {e:#}")),
    }

    if committed_by == "none"
        && previous
            .last()
            .is_some_and(|last| !head_before.is_empty() && last.commit != head_before)
    {
        committed_by = "agent";
    }

    let branch = git_ops::get_current_branch(dir).await;
    let commit = git_out(dir, &["rev-parse", "HEAD"])
        .await
        .unwrap_or_default();
    let remote = git_out(
        dir,
        &["rev-parse", &format!("refs/remotes/origin/{branch}")],
    )
    .await;
    let pushed_by = if remote.as_deref() == Some(commit.as_str()) {
        "agent"
    } else {
        let default_branch = worktree::detect_default_branch(dir).await;
        match git_ops::push_branch(dir, &branch, &default_branch).await {
            Ok(true) => "orch",
            Ok(false) => "none",
            Err(e) => {
                errors.push(format!("push: {e:#}"));
                "none"
            }
        }
    };

    let record = Record {
        node: node.to_string(),
        ts: super::now_stamp(),
        branch,
        commit,
        committed_by: committed_by.to_string(),
        pushed_by: pushed_by.to_string(),
        error: (!errors.is_empty()).then(|| cut(&errors.join("; "))),
        done: cut(&report.done),
        left: cut(&report.left),
        state: cut(&report.state),
    };
    let mut records = previous;
    records.push(record.clone());
    let skip = records.len().saturating_sub(KEEP);
    let records: Vec<Record> = records.into_iter().skip(skip).collect();
    store
        .kv_set(&key(task_id), &serde_json::to_string(&records)?)
        .await?;
    Ok(record)
}
