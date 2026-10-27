use std::path::Path;

use crate::cluster::{local, progress};
use crate::engine::runner::git_ops;

async fn git(dir: &Path, args: &[&str]) -> anyhow::Result<std::process::Output> {
    Ok(tokio::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .await?)
}

/// Commit in the worktree, then hand the report to the service, which commits what is left,
/// pushes, and records it for every node
pub async fn run(done: &str, left: &str, state: &str) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let top = git(&cwd, &["rev-parse", "--show-toplevel"]).await?;
    anyhow::ensure!(
        top.status.success(),
        "orch progress runs inside a task worktree"
    );
    let worktree = String::from_utf8_lossy(&top.stdout).trim().to_string();
    let dir = Path::new(&worktree);

    let task_id = std::env::var("TASK_ID").unwrap_or_default();
    let agent = std::env::var("ORCH_AGENT").unwrap_or_else(|_| "agent".to_string());
    let (cli_committed, cli_error) =
        match git_ops::auto_commit(dir, &task_id, done, &agent, 0).await {
            Ok(committed) => (committed, None),
            Err(e) => (false, Some(format!("{e:#}"))),
        };

    let report = progress::Report {
        worktree,
        done: done.to_string(),
        left: left.to_string(),
        state: state.to_string(),
        cli_committed,
        cli_error,
    };
    let Some(endpoint) = local::daemon_endpoint().await else {
        eprintln!("orch service is not running: committed locally, nothing recorded or pushed");
        return Ok(());
    };
    let record = local::send_progress(&endpoint, &report).await?;
    println!(
        "{} {} committed by {}, pushed by {}",
        record.branch,
        record.commit.chars().take(12).collect::<String>(),
        record.committed_by,
        record.pushed_by
    );
    if let Some(err) = record.error {
        eprintln!("{err}");
    }

    Ok(())
}
