//! Engine side retry of a push that failed on a GitHub or network error (#4025, #4051), with no agent run

use crate::engine::review;
use crate::engine::runner::{git_ops, worktree};
use crate::store;
use crate::store::{Task, TaskStore, opt_store_get_task};
use chrono::Utc;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The push failure count that blocks the task, the run counts as the first failure
pub(crate) const MAX_PUSH_FAILURES: u64 = 4;

/// Wait after the first, second and third failure
const BACKOFF_SECS: [i64; 3] = [60, 5 * 60, 15 * 60];

/// What a retry attempt did
#[derive(Debug, PartialEq)]
pub(crate) enum PushRetry {
    /// The task has no transient push failure to retry
    NotPending,
    /// The backoff has not ended
    Wait,
    /// The branch reached the remote
    Pushed,
    /// The push failed again, a later tick tries it again
    Failed,
    /// The last allowed push failed
    Exhausted(String),
    /// The push failed for a reason that needs the task agent
    NeedsAgent(String),
}

fn retry_key(repo: &str, task_id: &str) -> String {
    format!("push_retry_at:{repo}#{task_id}")
}

/// Seconds to wait after the given number of failed pushes
pub(crate) fn backoff_secs(push_failures: i64) -> i64 {
    let idx = usize::try_from(push_failures.max(1) - 1)
        .unwrap_or(0)
        .min(BACKOFF_SECS.len() - 1);
    BACKOFF_SECS[idx]
}

/// A task whose last push failed on a GitHub or network error, including a later commit on an existing PR
pub(crate) fn is_pending(task: &Task) -> bool {
    task.push_failures > 0
        && u64::try_from(task.push_failures).is_ok_and(|n| n < MAX_PUSH_FAILURES)
        && !task.worktree.trim().is_empty()
        && !task.branch.trim().is_empty()
        && task.last_error.contains("push failed")
        && !task.last_error.contains("push failed and rebase failed:")
        && git_ops::is_transient_network_error(&task.last_error)
}

fn push_error_text(err: &str) -> String {
    if err.starts_with("push failed") {
        err.to_string()
    } else {
        format!("push failed: {err}")
    }
}

/// Push the branch of a task that waits on a failed push, once its backoff ended
pub(crate) async fn retry_pending_push(
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
) -> PushRetry {
    let opt_store = Some(Arc::clone(store));
    let Some(task) = opt_store_get_task(&opt_store, repo, task_id).await else {
        return PushRetry::NotPending;
    };
    let worktree_path = PathBuf::from(&task.worktree);
    if !is_pending(&task) || !worktree_path.exists() {
        return PushRetry::NotPending;
    }

    let key = retry_key(repo, task_id);
    let now = Utc::now().timestamp();
    match store.kv_get(&key).await {
        Ok(Some(due)) if now >= due.parse().unwrap_or(0) => {}
        Ok(Some(_)) => return PushRetry::Wait,
        Ok(None) => {
            let due = now + backoff_secs(i64::from(task.push_failures));
            if let Err(e) = store.kv_set(&key, &due.to_string()).await {
                tracing::warn!(task_id, err = %e, "failed to store the push retry time");
            }
            return PushRetry::Wait;
        }
        Err(e) => {
            tracing::warn!(task_id, err = %e, "failed to read the push retry time");
            return PushRetry::Wait;
        }
    }

    let default_branch = worktree::detect_default_branch(&worktree_path).await;
    tracing::info!(
        task_id,
        branch = %task.branch,
        push_failures = task.push_failures,
        "retrying the push from the engine"
    );
    match git_ops::push_branch(&worktree_path, &task.branch, &default_branch).await {
        Ok(true) => {
            log_push(store, repo, task_id, &task, "ok", None).await;
            store.kv_delete(&key).await.ok();
            store::store_set(
                &opt_store,
                repo,
                task_id,
                &[
                    ("last_error", serde_json::json!("")),
                    ("push_failures", serde_json::json!(0)),
                ],
            )
            .await;
            if task.pr_number.is_none() {
                open_pr(store, repo, task_id, &task, &worktree_path, &default_branch).await;
            }
            PushRetry::Pushed
        }
        Ok(false) => {
            let err = "push skipped".to_string();
            log_push(store, repo, task_id, &task, "skipped", Some(&err)).await;
            store.kv_delete(&key).await.ok();
            PushRetry::NeedsAgent(err)
        }
        Err(e) => {
            let err = push_error_text(&e.to_string());
            log_push(store, repo, task_id, &task, "error", Some(&err)).await;
            store::store_set(
                &opt_store,
                repo,
                task_id,
                &[("last_error", serde_json::json!(err))],
            )
            .await;
            if !git_ops::is_transient_network_error(&err) {
                store.kv_delete(&key).await.ok();
                return PushRetry::NeedsAgent(err);
            }
            let failures = store::store_increment(&opt_store, repo, task_id, "push_failures")
                .await
                .unwrap_or_else(|_| u64::try_from(task.push_failures).unwrap_or(0) + 1);
            if failures >= MAX_PUSH_FAILURES {
                store.kv_delete(&key).await.ok();
                return PushRetry::Exhausted(err);
            }
            let due = Utc::now().timestamp() + backoff_secs(i64::try_from(failures).unwrap_or(1));
            if let Err(e) = store.kv_set(&key, &due.to_string()).await {
                tracing::warn!(task_id, err = %e, "failed to store the push retry time");
            }
            PushRetry::Failed
        }
    }
}

async fn log_push(
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
    task: &Task,
    status: &str,
    error: Option<&str>,
) {
    store::store_log_activity(
        &Some(Arc::clone(store)),
        repo,
        task_id,
        "push",
        None,
        None,
        task.agent.as_deref(),
        task.model.as_deref(),
        Some(&serde_json::json!({
            "status": status,
            "branch": task.branch,
            "phase": "engine_retry",
            "error": error,
        })),
    )
    .await;
}

/// Open the PR the run could not open, the review gate opens it when this fails
async fn open_pr(
    store: &Arc<TaskStore>,
    repo: &str,
    task_id: &str,
    task: &Task,
    worktree_path: &Path,
    default_branch: &str,
) {
    if repo.is_empty() {
        return;
    }
    let opt_store = Some(Arc::clone(store));
    let created = git_ops::create_pr_if_needed(
        worktree_path,
        &task.branch,
        &task.title,
        &task.summary,
        &[],
        &[],
        &[],
        task_id,
        task.agent.as_deref().unwrap_or_default(),
        task.model.as_deref(),
        repo,
        default_branch,
    )
    .await;
    match created {
        Ok(url) => match review::parse_pr_number_from_url(&url) {
            Ok(pr_number) => {
                store::store_set(
                    &opt_store,
                    repo,
                    task_id,
                    &[("pr_number", serde_json::json!(pr_number))],
                )
                .await;
            }
            Err(e) => {
                tracing::warn!(task_id, url, err = %e, "pushed, but the PR number is unreadable");
            }
        },
        Err(e) => {
            tracing::warn!(task_id, err = %e, "pushed, but the PR is not open yet");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::NewTask;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    const FAILED_PUSH: &str = "push failed: remote: fatal error in commit_refs\n ! [remote rejected] feature -> feature (pre-receive hook declined)";

    /// A clone with one commit ahead on a feature branch, and the bare remote it pushes to
    struct Fixture {
        _tmp: tempfile::TempDir,
        clone: PathBuf,
        remote: PathBuf,
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let remote = tmp.path().join("remote.git");
        let clone = tmp.path().join("clone");
        fs::create_dir_all(&remote).unwrap();
        git(&remote, &["init", "--bare", "-b", "main"]);
        git(tmp.path(), &["clone", remote.to_str().unwrap(), "clone"]);
        git(&clone, &["config", "user.email", "t@example.com"]);
        git(&clone, &["config", "user.name", "t"]);
        git(&clone, &["checkout", "-b", "main"]);
        fs::write(clone.join("a.txt"), "a").unwrap();
        git(&clone, &["add", "."]);
        git(&clone, &["commit", "-m", "base"]);
        git(&clone, &["push", "-u", "origin", "main"]);
        git(&clone, &["checkout", "-b", "feature"]);
        fs::write(clone.join("b.txt"), "b").unwrap();
        git(&clone, &["add", "."]);
        git(&clone, &["commit", "-m", "work"]);
        Fixture {
            _tmp: tmp,
            clone,
            remote,
        }
    }

    /// Make the remote fail the way GitHub did during the outage
    fn break_remote(remote: &Path) {
        let hook = remote.join("hooks/pre-receive");
        fs::write(
            &hook,
            "#!/bin/sh\necho 'fatal error in commit_refs' >&2\nexit 1\n",
        )
        .unwrap();
        #[cfg(unix)]
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    }

    async fn pending_task(store: &Arc<TaskStore>, fx: &Fixture, failures: i32) -> String {
        let id = store
            .create(&NewTask {
                title: "Work".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        store
            .set_fields(
                id,
                &[
                    ("worktree", serde_json::json!(fx.clone.to_str().unwrap())),
                    ("branch", serde_json::json!("feature")),
                    ("last_error", serde_json::json!(FAILED_PUSH)),
                    ("push_failures", serde_json::json!(failures)),
                ],
            )
            .await
            .unwrap();
        format!("internal:{id}")
    }

    async fn set_pr_number(store: &Arc<TaskStore>, task_id: &str, pr_number: i32) {
        let store_id = store.resolve_task_id("", task_id).await.unwrap().unwrap();
        store
            .set_fields(store_id, &[("pr_number", serde_json::json!(pr_number))])
            .await
            .unwrap();
    }

    /// The first feature commit is already on the remote, as it is for an open PR
    fn extra_unpushed_commit(fx: &Fixture) {
        git(&fx.clone, &["push", "-u", "origin", "feature"]);
        fs::write(fx.clone.join("c.txt"), "c").unwrap();
        git(&fx.clone, &["add", "."]);
        git(&fx.clone, &["commit", "-m", "more"]);
    }

    fn remote_log(fx: &Fixture) -> String {
        let out = Command::new("git")
            .args(["log", "feature", "--oneline"])
            .current_dir(&fx.remote)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    async fn stored(store: &Arc<TaskStore>, task_id: &str) -> Task {
        opt_store_get_task(&Some(Arc::clone(store)), "", task_id)
            .await
            .unwrap()
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff_secs(0), 60);
        assert_eq!(backoff_secs(1), 60);
        assert_eq!(backoff_secs(2), 300);
        assert_eq!(backoff_secs(3), 900);
        assert_eq!(backoff_secs(9), 900);
    }

    #[tokio::test]
    async fn waits_for_the_backoff_without_pushing() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let fx = fixture();
        break_remote(&fx.remote);
        let task_id = pending_task(&store, &fx, 1).await;

        // The first look starts the timer, the second finds it running
        assert_eq!(
            retry_pending_push(&store, "", &task_id).await,
            PushRetry::Wait
        );
        assert_eq!(
            retry_pending_push(&store, "", &task_id).await,
            PushRetry::Wait
        );
        assert_eq!(stored(&store, &task_id).await.push_failures, 1);
    }

    #[tokio::test]
    async fn failures_inside_one_outage_do_not_block_and_the_next_push_lands() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let fx = fixture();
        break_remote(&fx.remote);
        let task_id = pending_task(&store, &fx, 1).await;
        let key = retry_key("", &task_id);

        for expected in [2, 3] {
            store.kv_set(&key, "0").await.unwrap();
            assert_eq!(
                retry_pending_push(&store, "", &task_id).await,
                PushRetry::Failed
            );
            let task = stored(&store, &task_id).await;
            assert_eq!(task.push_failures, expected);
            assert!(is_pending(&task));
        }

        fs::remove_file(fx.remote.join("hooks/pre-receive")).unwrap();
        store.kv_set(&key, "0").await.unwrap();
        assert_eq!(
            retry_pending_push(&store, "", &task_id).await,
            PushRetry::Pushed
        );
        let task = stored(&store, &task_id).await;
        assert_eq!(task.push_failures, 0);
        assert!(task.last_error.is_empty());
        assert!(store.kv_get(&key).await.unwrap().is_none());
        let heads = Command::new("git")
            .args(["branch", "--list", "feature"])
            .current_dir(&fx.remote)
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&heads.stdout).contains("feature"));
    }

    #[tokio::test]
    async fn the_last_allowed_failure_exhausts() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let fx = fixture();
        break_remote(&fx.remote);
        let task_id = pending_task(&store, &fx, 3).await;
        store.kv_set(&retry_key("", &task_id), "0").await.unwrap();

        assert!(matches!(
            retry_pending_push(&store, "", &task_id).await,
            PushRetry::Exhausted(_)
        ));
    }

    #[tokio::test]
    async fn a_failure_that_is_not_transient_is_left_alone() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let fx = fixture();
        let task_id = pending_task(&store, &fx, 1).await;
        let store_id = store.resolve_task_id("", &task_id).await.unwrap().unwrap();
        store
            .set_fields(
                store_id,
                &[(
                    "last_error",
                    serde_json::json!("push failed: ! [rejected] feature -> feature (fetch first)"),
                )],
            )
            .await
            .unwrap();

        assert_eq!(
            retry_pending_push(&store, "", &task_id).await,
            PushRetry::NotPending
        );
    }

    #[tokio::test]
    async fn is_pending_when_the_task_already_has_a_pr() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let fx = fixture();
        let task_id = pending_task(&store, &fx, 1).await;
        set_pr_number(&store, &task_id, 12).await;
        assert!(is_pending(&stored(&store, &task_id).await));
    }

    /// A later commit on a task that already has a PR still retries its push (#4051)
    #[tokio::test]
    async fn retries_an_unpushed_commit_when_the_task_already_has_a_pr() {
        let store = Arc::new(TaskStore::open_memory().await.unwrap());
        let fx = fixture();
        extra_unpushed_commit(&fx);
        let task_id = pending_task(&store, &fx, 1).await;
        set_pr_number(&store, &task_id, 12).await;
        let key = retry_key("", &task_id);

        assert_eq!(
            retry_pending_push(&store, "", &task_id).await,
            PushRetry::Wait
        );
        assert!(!remote_log(&fx).contains("more"));

        store.kv_set(&key, "0").await.unwrap();
        assert_eq!(
            retry_pending_push(&store, "", &task_id).await,
            PushRetry::Pushed
        );
        let task = stored(&store, &task_id).await;
        assert_eq!(task.push_failures, 0);
        assert!(task.last_error.is_empty());
        assert_eq!(task.pr_number, Some(12));
        assert!(remote_log(&fx).contains("more"));
    }
}
