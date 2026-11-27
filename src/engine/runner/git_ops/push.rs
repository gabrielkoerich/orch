use super::{
    check_push_needed, get_current_branch, git, is_workflow_scope_error, rebase_on_branch,
    strip_workflow_files,
};
use crate::cmd::CommandErrorContext;
use crate::github::http;
use crate::security;
use regex::Regex;
use std::path::Path;
use std::process::Output;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::process::Command;
use tokio::time;

pub(super) const LEASE_ABSENT: &str = "absent";

/// Force pushes that found no recorded lease and fell back to the fetched tip
pub(super) static LEASE_FALLBACKS: AtomicU64 = AtomicU64::new(0);

async fn git_stdout(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output_with_context()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }

    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn lease_key(branch: &str) -> String {
    format!("branch.{branch}.orchPushLease")
}

pub(super) async fn read_push_lease(dir: &Path, branch: &str) -> Option<String> {
    git_stdout(dir, &["config", "--get", &lease_key(branch)]).await
}

async fn write_push_lease(dir: &Path, branch: &str, value: &str) {
    if git_stdout(dir, &["config", &lease_key(branch), value])
        .await
        .is_none()
    {
        tracing::warn!(branch, "failed to record push lease");
    }
}

async fn remote_tracking_sha(dir: &Path, branch: &str) -> Option<String> {
    let rev = format!("refs/remotes/origin/{branch}");
    git_stdout(dir, &["rev-parse", "--verify", "--quiet", &rev])
        .await
        .filter(|sha| http::is_full_commit_sha(sha))
}

async fn is_ancestor(dir: &Path, ancestor: &str, descendant: &str) -> bool {
    git_stdout(dir, &["merge-base", "--is-ancestor", ancestor, descendant])
        .await
        .is_some()
}

/// Record the remote tip this worktree may replace, before a fetch moves `origin/<branch>`. The lease
/// moves only to a tip HEAD contains, such as the agent's own push, never to one only fetched
pub(crate) async fn record_push_lease(dir: &Path, branch: &str, default_branch: &str) {
    if branch.is_empty() || branch == default_branch {
        return;
    }

    let tip = remote_tracking_sha(dir, branch).await;
    match (read_push_lease(dir, branch).await, tip) {
        (Some(lease), Some(tip)) if lease != tip && is_ancestor(dir, &tip, "HEAD").await => {
            write_push_lease(dir, branch, &tip).await;
        }
        (Some(_), _) => {}
        (None, tip) => {
            let expect = tip.unwrap_or_else(|| LEASE_ABSENT.to_string());
            write_push_lease(dir, branch, &expect).await;
        }
    }
}

/// Drop the lease of a branch whose remote was deleted, so the next dispatch records one for the recreated branch
pub(crate) async fn clear_push_lease(dir: &Path, branch: &str) {
    let _ = git_stdout(dir, &["config", "--unset", &lease_key(branch)]).await;
}

async fn note_pushed_head(dir: &Path, branch: &str) {
    let spec = format!("refs/heads/{branch}");
    if let Some(sha) = git_stdout(dir, &["rev-parse", "--verify", &spec]).await {
        write_push_lease(dir, branch, &sha).await;
    }
}

/// True when `tip` is GitHub's "Update branch" of `lease`: a merge with first parent `lease` and
/// second parent on the default branch, or a rebase whose commits all have a twin in `lease`
pub(super) async fn is_github_branch_update(
    dir: &Path,
    tip: &str,
    lease: &str,
    default_branch: &str,
) -> bool {
    let base = format!("origin/{default_branch}");
    let Some(line) = git_stdout(dir, &["rev-list", "--parents", "-n", "1", tip]).await else {
        return false;
    };
    let parents: Vec<&str> = line.split_whitespace().skip(1).collect();
    if let [first, second] = parents[..] {
        return first == lease && is_ancestor(dir, second, &base).await;
    }

    // `git cherry` marks each commit in base..tip with `-` when `lease` holds the same patch
    let Some(cherry) = git_stdout(dir, &["cherry", lease, tip, &base]).await else {
        return false;
    };
    is_ancestor(dir, &base, tip).await
        && !cherry.is_empty()
        && cherry.lines().all(|l| l.starts_with('-'))
}

pub(super) async fn expected_force_sha(
    dir: &Path,
    branch: &str,
    default_branch: &str,
) -> anyhow::Result<String> {
    let tip = remote_tracking_sha(dir, branch).await;
    let base = format!("origin/{default_branch}");
    if let Some(tip) = &tip {
        // A tip the default branch already holds has no commits of its own, so a force push loses nothing
        if is_ancestor(dir, tip, &base).await {
            tracing::info!(branch, %tip, "remote tip is contained in the default branch, leasing it");
            return Ok(tip.clone());
        }
    }

    match read_push_lease(dir, branch).await {
        Some(lease) if lease == LEASE_ABSENT => anyhow::bail!(
            "force push rejected: remote branch {branch} was absent when the lease was recorded"
        ),
        Some(lease) => match tip {
            Some(tip)
                if tip != lease
                    && is_github_branch_update(dir, &tip, &lease, default_branch).await =>
            {
                tracing::info!(branch, %lease, %tip, "remote tip is GitHub's update of the leased branch, leasing it");
                Ok(tip)
            }
            _ => Ok(lease),
        },
        None => {
            let Some(tip) = tip else {
                anyhow::bail!("force push rejected: no remote sha recorded for {branch}");
            };
            let fallbacks = LEASE_FALLBACKS.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::warn!(
                branch,
                fallbacks,
                "no push lease recorded, using the fetched origin tip"
            );
            Ok(tip)
        }
    }
}

/// Force-push `branch` only if the remote still holds the leased sha
pub(crate) async fn force_push_leased(
    dir: &Path,
    branch: &str,
    default_branch: &str,
) -> anyhow::Result<()> {
    let expected = expected_force_sha(dir, branch, default_branch).await?;
    let lease = format!("--force-with-lease={branch}:{expected}");
    tracing::info!(branch, expected_sha = %expected, "force-pushing with an expected remote sha");
    let output = git()
        .await
        .args([
            "-c",
            "lfs.locksverify=false",
            "push",
            &lease,
            "-u",
            "origin",
            branch,
        ])
        .current_dir(dir)
        .kill_on_drop(true)
        .output_with_context()
        .await?;
    if output.status.success() {
        note_pushed_head(dir, branch).await;
        return Ok(());
    }

    let stderr = sanitize_push_error(String::from_utf8_lossy(&output.stderr).trim());
    anyhow::bail!("{stderr}")
}

/// True for a branch orch must never push: the repo default, plus `main` and `master`
fn is_protected_push_branch(name: &str, default_branch: &str) -> bool {
    !name.is_empty() && (name == default_branch || name == "main" || name == "master")
}

/// Pick the ref to push. A protected checkout falls back to the task branch when that name is safe
pub(super) fn resolve_push_branch<'a>(
    current: &'a str,
    branch: &'a str,
    default_branch: &str,
) -> anyhow::Result<&'a str> {
    let chosen = if !current.is_empty() && !is_protected_push_branch(current, default_branch) {
        current
    } else if !branch.is_empty() && !is_protected_push_branch(branch, default_branch) {
        branch
    } else {
        let name = if current.is_empty() { branch } else { current };
        if name.is_empty() {
            anyhow::bail!("cannot push: branch name is empty");
        }
        anyhow::bail!("push refused: '{name}' is a default branch");
    };
    Ok(chosen)
}

/// Push the branch to origin.
///
/// Returns `Ok(true)` when the branch is on the remote, `Ok(false)` when the push was skipped,
/// and `Err` when the push was refused (default branch) or failed
pub async fn push_branch(dir: &Path, branch: &str, default_branch: &str) -> anyhow::Result<bool> {
    let current = get_current_branch(dir).await;
    let branch_to_push = resolve_push_branch(&current, branch, default_branch)?;

    // Check if there are commits to push and whether the remote branch exists
    let (remote_exists, has_unpushed) =
        check_push_needed(dir, branch_to_push, default_branch).await;
    if remote_exists && !has_unpushed {
        // Remote already has this branch and it's up to date — nothing to push.
        // Return true so callers know the branch IS on the remote and PR creation
        // is safe to attempt (e.g. a PR may already exist or can be created).
        tracing::debug!(
            branch = branch_to_push,
            "remote branch exists and is up to date, skipping push"
        );
        note_pushed_head(dir, branch_to_push).await;
        return Ok(true);
    }
    if !remote_exists && !has_unpushed {
        // Branch doesn't exist on the remote and has no commits ahead of the
        // default branch — push it anyway so the branch is created on the remote.
        // PR creation will then fail with "No commits between …" (422) which is
        // handled downstream, rather than with "head invalid" (422) which is
        // harder to distinguish from a real error.
        tracing::debug!(
            branch = branch_to_push,
            "remote branch missing with no new commits — pushing to create it on remote"
        );
    }

    tracing::info!(branch = branch_to_push, "pushing branch");

    // First attempt: normal push (no force).
    // lfs.locksverify=false skips the auxiliary LFS locks/verify roundtrip in
    // the pre-push hook; agents never hold LFS locks, and a transient timeout
    // on that endpoint must not fail the whole push.
    // A 2-minute timeout prevents an indefinitely-stalled push from blocking
    // post-processing and triggering a stuck-task re-dispatch race.
    let output = time::timeout(
        Duration::from_secs(120),
        git()
            .await
            .args([
                "-c",
                "lfs.locksverify=false",
                "push",
                "-u",
                "origin",
                branch_to_push,
            ])
            .kill_on_drop(true)
            .current_dir(dir)
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("git push timed out after 120s"))??;

    if output.status.success() {
        tracing::info!(branch = branch_to_push, "push succeeded");
        note_pushed_head(dir, branch_to_push).await;
        return Ok(true);
    }

    let stderr = sanitize_push_error(String::from_utf8_lossy(&output.stderr).trim());

    // Transient network blip (timeout, reset) rather than a ref rejection —
    // retry the identical push once before classifying the run as failed.
    if is_transient_network_error(&stderr) {
        tracing::warn!(
            branch = branch_to_push,
            stderr = %stderr,
            "push failed with transient network error — retrying once"
        );
        let retry_output = time::timeout(
            Duration::from_secs(120),
            git()
                .await
                .args([
                    "-c",
                    "lfs.locksverify=false",
                    "push",
                    "-u",
                    "origin",
                    branch_to_push,
                ])
                .kill_on_drop(true)
                .current_dir(dir)
                .output(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("git push (network retry) timed out after 120s"))??;

        if retry_output.status.success() {
            tracing::info!(branch = branch_to_push, "push succeeded on network retry");
            note_pushed_head(dir, branch_to_push).await;
            return Ok(true);
        }

        let retry_stderr =
            sanitize_push_error(String::from_utf8_lossy(&retry_output.stderr).trim());
        tracing::warn!(
            branch = branch_to_push,
            stderr = %retry_stderr,
            "push retry also failed"
        );
        anyhow::bail!("push failed: {retry_stderr}");
    }

    // Handle workflow scope error: the token lacks the `workflow` OAuth scope
    // required to push changes to `.github/workflows/`. Strip workflow files
    // from commits and retry the push.
    if is_workflow_scope_error(&stderr) {
        tracing::warn!(
            branch = branch_to_push,
            "push rejected: token lacks `workflow` scope — stripping workflow files and retrying"
        );

        match strip_workflow_files(dir, default_branch).await {
            Ok(true) => {
                // Workflow files stripped, retry push
                let retry_output = time::timeout(
                    Duration::from_secs(120),
                    git()
                        .await
                        .args([
                            "-c",
                            "lfs.locksverify=false",
                            "push",
                            "-u",
                            "origin",
                            branch_to_push,
                        ])
                        .kill_on_drop(true)
                        .current_dir(dir)
                        .output(),
                )
                .await
                .map_err(|_| anyhow::anyhow!("git push (retry) timed out after 120s"))??;

                if retry_output.status.success() {
                    tracing::info!(
                        branch = branch_to_push,
                        "push succeeded after stripping workflow files"
                    );
                    note_pushed_head(dir, branch_to_push).await;
                    return Ok(true);
                }

                let retry_stderr =
                    sanitize_push_error(String::from_utf8_lossy(&retry_output.stderr).trim());

                // If still failing with workflow scope, bail with clear message
                if is_workflow_scope_error(&retry_stderr) {
                    anyhow::bail!(
                        "push failed: token lacks `workflow` OAuth scope and workflow files \
                         could not be fully stripped — add `workflow` scope to your GitHub \
                         token or use a GitHub App for authentication"
                    );
                }

                anyhow::bail!("push failed after stripping workflow files: {retry_stderr}");
            }
            Ok(false) => {
                // No workflow files found to strip (shouldn't happen if error was detected)
                anyhow::bail!(
                    "push failed: token lacks `workflow` OAuth scope — add `workflow` scope \
                     to your GitHub token or use a GitHub App for authentication. \
                     Error: {stderr}"
                );
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to strip workflow files from commits");
                anyhow::bail!(
                    "push failed: token lacks `workflow` OAuth scope and automatic \
                     recovery failed ({e}) — add `workflow` scope to your GitHub token \
                     or use a GitHub App for authentication"
                );
            }
        }
    }

    // Non-fast-forward after `rebase_on_default`. The lease was recorded before that fetch,
    // so this replaces only a tip this worktree already built on
    if push_needs_rebase(&stderr) && remote_exists {
        tracing::info!(
            branch = branch_to_push,
            "push rejected (non-fast-forward), force-pushing with lease"
        );
        let stderr = match force_push_leased(dir, branch_to_push, default_branch).await {
            Ok(()) => {
                tracing::info!(branch = branch_to_push, "force push succeeded");
                return Ok(true);
            }
            Err(e) => e.to_string(),
        };

        // Check if force push also hit the workflow scope error
        if is_workflow_scope_error(&stderr) {
            tracing::warn!(
                branch = branch_to_push,
                "force push also rejected due to workflow scope — stripping and retrying"
            );
            match strip_workflow_files(dir, default_branch).await {
                Ok(true) => {
                    if let Err(e) = force_push_leased(dir, branch_to_push, default_branch).await {
                        anyhow::bail!("force push failed after stripping workflow files: {e}");
                    }

                    tracing::info!(
                        branch = branch_to_push,
                        "force push succeeded after stripping workflow files"
                    );
                    return Ok(true);
                }
                Ok(false) => {
                    anyhow::bail!(
                        "force push failed: token lacks `workflow` OAuth scope — \
                         add `workflow` scope to your GitHub token or use a GitHub App. \
                         Error: {stderr}"
                    );
                }
                Err(e) => {
                    anyhow::bail!(
                        "force push failed: token lacks `workflow` scope and recovery \
                         failed ({e})"
                    );
                }
            }
        }

        // The remote branch moved since the last fetch (GitHub's "Update branch", another
        // clone), so the lease no longer matches. Replay local commits on the new tip once
        if is_stale_lease_error(&stderr) && rebase_on_branch(dir, branch_to_push).await? {
            let output = git()
                .await
                .args([
                    "-c",
                    "lfs.locksverify=false",
                    "push",
                    "-u",
                    "origin",
                    branch_to_push,
                ])
                .current_dir(dir)
                .output_with_context()
                .await?;
            if output.status.success() {
                tracing::info!(
                    branch = branch_to_push,
                    "push succeeded after rebasing on the moved remote branch"
                );
                note_pushed_head(dir, branch_to_push).await;
                return Ok(true);
            }

            let stderr = sanitize_push_error(String::from_utf8_lossy(&output.stderr).trim());
            anyhow::bail!("push failed after rebasing on the moved remote branch: {stderr}");
        }

        if is_stale_lease_error(&stderr) {
            tracing::info!(
                branch = branch_to_push,
                "remote branch already holds every local commit, nothing to push"
            );
            return Ok(true);
        }

        anyhow::bail!("force push failed: {stderr}");
    }

    anyhow::bail!("push failed: {stderr}")
}

pub fn is_missing_remote_ref_error(err: &anyhow::Error) -> bool {
    err.to_string().contains("couldn't find remote ref")
}

pub(super) fn push_needs_rebase(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("non-fast-forward")
        || lower.contains("rejected")
        || lower.contains("fetch first")
        || lower.contains("behind")
}

/// True when `--force-with-lease` refused because the remote ref is not the one last fetched
pub(super) fn is_stale_lease_error(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("stale info") || lower.contains("remote ref updated since checkout")
}

/// True when a push failure looks like a transient network error (dial
/// timeout, i/o timeout, connection reset) rather than a ref rejection.
/// A refused ref must not be retried, but a blipped endpoint deserves one
/// immediate retry before the run is classified as `push_failed`.
pub fn is_transient_network_error(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("i/o timeout")
        || lower.contains("dial tcp")
        || lower.contains("connection reset")
        || lower.contains("connection refused")
        || lower.contains("no route to host")
        || lower.contains("temporary failure in name resolution")
        || lower.contains("eof")
        || lower.contains("fatal error in commit_refs")
        || (lower.contains("[remote rejected]") && lower.contains("(failure)"))
}

/// Strip URL userinfo (`https://x-access-token:TOKEN@…`) and leak-pattern secrets
/// from credentialed git stdout and stderr before it is logged or stored
pub(crate) fn sanitize_push_error(stderr: &str) -> String {
    static URL_USERINFO: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"://[^/@\s]+@").expect("valid url userinfo regex"));
    let no_userinfo = URL_USERINFO.replace_all(stderr, "://***@");
    security::redact(&no_userinfo)
}

/// Sanitize both streams of a credentialed git `Output` so callers cannot log the raw bytes
pub(crate) fn sanitize_git_output(mut output: Output) -> Output {
    output.stdout = sanitize_push_error(&String::from_utf8_lossy(&output.stdout)).into_bytes();
    output.stderr = sanitize_push_error(&String::from_utf8_lossy(&output.stderr)).into_bytes();
    output
}
