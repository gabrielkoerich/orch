//! `orch progress` on the daemon: commit and push what an agent left in its worktree, then record it

use crate::store::now_stamp;
use std::path::Path;
use std::sync::Arc;

use tokio::process::Command;

use crate::engine::runner::commit::When;
use crate::engine::runner::{git_ops, worktree};
use crate::store::TaskStore;
use crate::store::cluster::local::ProgressFn;
use crate::store::cluster::progress::{self, Record, Report};

/// The handler the local daemon endpoint calls for `orch progress`
pub fn handler() -> ProgressFn {
    Arc::new(|store, node, report| Box::pin(async move { handle(&store, &node, report).await }))
}

async fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
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
    let previous = progress::list(store, task_id).await?;
    let mut errors: Vec<String> = report.cli_error.iter().cloned().collect();

    let head_before = git_out(dir, &["rev-parse", "HEAD"])
        .await
        .unwrap_or_default();
    let mut committed_by = if report.cli_committed { "cli" } else { "none" };
    let ext = task.external_id.clone().unwrap_or_default();
    match git_ops::auto_commit(
        dir,
        &ext,
        &report.done,
        "orch",
        u32::try_from(task.attempts).unwrap_or(0),
        When::Tested,
    )
    .await
    {
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
        ts: now_stamp(),
        branch,
        commit,
        committed_by: committed_by.to_string(),
        pushed_by: pushed_by.to_string(),
        error: (!errors.is_empty()).then(|| progress::cut(&errors.join("; "))),
        done: progress::cut(&report.done),
        left: progress::cut(&report.left),
        state: progress::cut(&report.state),
    };
    progress::append(store, task_id, &record).await?;
    Ok(record)
}
