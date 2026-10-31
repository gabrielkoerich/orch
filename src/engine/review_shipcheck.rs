//! Runs shipcheck before a review that has no CI result, so the reviewer's `shipcheck run` returns
//! the stored result instead of starting the suite inside the review's time limit (#3943)

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

/// A full run that takes longer than this is killed and the review starts without a result
const PRERUN_TIMEOUT_SECS: u64 = 60 * 60;

#[derive(Debug, PartialEq)]
pub(crate) enum Prerun {
    /// The head has a result, or a run ended without one, so the review starts
    Ready,
    /// A run of the head is still going, the review waits
    Running,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum RunState {
    Running,
    Finished,
}

/// Runs this process started, keyed by worktree and head commit
static RUNS: LazyLock<Mutex<HashMap<(PathBuf, String), RunState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn run_state(key: &(PathBuf, String)) -> Option<RunState> {
    RUNS.lock().unwrap().get(key).copied()
}

fn set_run_state(key: (PathBuf, String), state: Option<RunState>) {
    let mut runs = RUNS.lock().unwrap();
    match state {
        Some(state) => runs.insert(key, state),
        None => runs.remove(&key),
    };
}

/// Whether the review in `worktree` may start. Starts one `shipcheck run` per head when the head
/// has no stored result, and holds the review until it ends
pub(crate) async fn prerun(worktree: &Path) -> Prerun {
    let bin = which::which("shipcheck").unwrap_or_else(|_| "shipcheck".into());
    prerun_with(&bin, worktree).await
}

async fn prerun_with(bin: &Path, worktree: &Path) -> Prerun {
    let Some(head) = crate::engine::review::worktree_head_sha(worktree).await else {
        return Prerun::Ready;
    };
    let key = (worktree.to_path_buf(), head);
    match run_state(&key) {
        Some(RunState::Running) => return Prerun::Running,
        // A run that ended without a result does not run again, the reviewer handles it
        Some(RunState::Finished) => {
            set_run_state(key, None);
            return Prerun::Ready;
        }
        None => {}
    }

    if has_result(bin, worktree).await {
        return Prerun::Ready;
    }

    {
        let mut runs = RUNS.lock().unwrap();
        if runs.contains_key(&key) {
            return Prerun::Running;
        }

        runs.insert(key.clone(), RunState::Running);
    }

    tracing::info!(worktree = %worktree.display(), head = %key.1, "running shipcheck before the review");
    let bin = bin.to_path_buf();
    tokio::spawn(async move {
        let run = tokio::process::Command::new(&bin)
            .arg("run")
            .current_dir(&key.0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .status();
        let timeout = std::time::Duration::from_secs(PRERUN_TIMEOUT_SECS);
        match tokio::time::timeout(timeout, run).await {
            Ok(Ok(status)) => {
                tracing::info!(worktree = %key.0.display(), head = %key.1, code = ?status.code(), "shipcheck run before the review ended");
            }
            Ok(Err(e)) => {
                tracing::warn!(worktree = %key.0.display(), err = %e, "shipcheck run before the review could not start");
            }
            Err(_) => {
                tracing::warn!(worktree = %key.0.display(), "shipcheck run before the review timed out, the review starts without it");
            }
        }
        set_run_state(key, Some(RunState::Finished));
    });

    Prerun::Running
}

/// `shipcheck status` exits 2 when the working copy's tree has no stored result. Any other
/// outcome, an error included, does not hold the review
async fn has_result(bin: &Path, worktree: &Path) -> bool {
    let status = tokio::process::Command::new(bin)
        .args(["status", "--json"])
        .current_dir(worktree)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .status();
    match tokio::time::timeout(std::time::Duration::from_secs(30), status).await {
        Ok(Ok(status)) => status.code() != Some(2),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-b", "main"]);
        std::fs::write(dir.path().join("a.txt"), "a").unwrap();
        git(dir.path(), &["add", "a.txt"]);
        git(
            dir.path(),
            &["-c", "commit.gpgsign=false", "commit", "-m", "a"],
        );
        dir
    }

    /// A stand-in for shipcheck: `status` has a result once `run` wrote the marker, and `run`
    /// waits for the `go` file so a test can watch it in progress
    fn fake_shipcheck(dir: &Path) -> PathBuf {
        let bin = dir.join("shipcheck");
        let body = format!(
            "#!/bin/sh\nd='{}'\necho \"$1\" >> \"$d/calls\"\nif [ \"$1\" = status ]; then [ -f \"$d/result\" ] && exit 0; exit 2; fi\nwhile [ ! -f \"$d/go\" ]; do sleep 0.05; done\ntouch \"$d/result\"\n",
            dir.display()
        );
        std::fs::write(&bin, body).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    fn runs(dir: &Path) -> usize {
        std::fs::read_to_string(dir.join("calls"))
            .unwrap_or_default()
            .lines()
            .filter(|l| *l == "run")
            .count()
    }

    async fn wait_for_finish(worktree: &Path) {
        let head = crate::engine::review::worktree_head_sha(worktree)
            .await
            .unwrap();
        let key = (worktree.to_path_buf(), head);
        for _ in 0..200 {
            if run_state(&key) == Some(RunState::Finished) {
                return;
            }

            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("the shipcheck run never finished");
    }

    #[tokio::test]
    async fn a_head_without_a_result_runs_once_and_holds_the_review_until_it_ends() {
        let wt = repo();
        let bins = tempfile::tempdir().unwrap();
        let bin = fake_shipcheck(bins.path());

        assert_eq!(prerun_with(&bin, wt.path()).await, Prerun::Running);
        assert_eq!(prerun_with(&bin, wt.path()).await, Prerun::Running);
        std::fs::write(bins.path().join("go"), "").unwrap();
        wait_for_finish(wt.path()).await;
        assert_eq!(prerun_with(&bin, wt.path()).await, Prerun::Ready);
        assert_eq!(runs(bins.path()), 1);

        // The stored result now starts the next review at once
        assert_eq!(prerun_with(&bin, wt.path()).await, Prerun::Ready);
        assert_eq!(runs(bins.path()), 1);
    }

    #[tokio::test]
    async fn a_run_that_leaves_no_result_does_not_hold_the_review_again() {
        let wt = repo();
        let bins = tempfile::tempdir().unwrap();
        let bin = bins.path().join("shipcheck");
        std::fs::write(&bin, "#!/bin/sh\n[ \"$1\" = status ] && exit 2\nexit 1\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(prerun_with(&bin, wt.path()).await, Prerun::Running);
        wait_for_finish(wt.path()).await;
        assert_eq!(prerun_with(&bin, wt.path()).await, Prerun::Ready);
    }

    #[tokio::test]
    async fn a_missing_shipcheck_or_worktree_never_holds_the_review() {
        let wt = repo();
        let missing = Path::new("/nonexistent/shipcheck");
        assert_eq!(prerun_with(missing, wt.path()).await, Prerun::Ready);
        let gone = Path::new("/nonexistent/worktree");
        assert_eq!(prerun_with(missing, gone).await, Prerun::Ready);
    }
}
