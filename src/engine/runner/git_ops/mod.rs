//! Git operations — auto-commit, push, PR creation.
//!
//! These run after the agent completes to ensure all changes
//! are committed, pushed, and a PR is created.
//!
//! All GitHub API operations use [`GhHttp`] exclusively. The `gh` CLI
//! is not used as a fallback to ensure consistent behavior across
//! environments and simplify testing.

mod pr;
mod push;

use super::commit;
use crate::cmd::CommandErrorContext;
use crate::github::token;
use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::process::Output;
use std::time::Duration;

pub use pr::create_pr_if_needed;
pub(crate) use pr::{
    is_invalid_base_error, is_transient_github_api_error, is_transient_github_error,
};
pub(crate) use push::{
    clear_push_lease, force_push_leased, record_push_lease, sanitize_git_output,
    sanitize_push_error,
};
pub use push::{is_missing_remote_ref_error, is_transient_network_error, push_branch};
use tokio::process::Command;
use tokio::time;

/// Format a task ID for display in PR/issue footers.
///
/// GitHub task IDs are displayed as `#123` (creates a GH issue hyperlink).
/// Internal task IDs (`internal:13`) are displayed as `internal-13` (no `#`
/// to avoid creating a false issue link).
pub(crate) fn format_task_ref(task_id: &str) -> String {
    if task_id.starts_with("internal:") {
        task_id.replace(':', "-")
    } else {
        format!("#{task_id}")
    }
}

/// Errors that can occur during PR creation.
#[derive(Debug, thiserror::Error)]
pub enum PrCreateError {
    #[error("GitHub API error: {0}")]
    ApiError(#[from] anyhow::Error),
}

/// Result type for PR creation operations.
pub type PrCreateResult<T> = Result<T, PrCreateError>;

/// Check if the current branch has any commits ahead of the default branch.
pub async fn has_commits_ahead(dir: &Path, default_branch: &str) -> bool {
    let output = Command::new("git")
        .args([
            "rev-list",
            "--count",
            &format!("origin/{default_branch}..HEAD"),
        ])
        .current_dir(dir)
        .output()
        .await;
    match output {
        Ok(o) if o.status.success() => {
            let count: u32 = String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse()
                .unwrap_or(0);
            count > 0
        }
        _ => {
            tracing::debug!(
                default_branch,
                "git rev-list failed for compare ref, assuming commits ahead"
            );
            // If we can't determine, assume there ARE commits ahead so we
            // attempt a push (better to try and fail loudly than silently
            // skip push and lose the PR creation path).
            true
        }
    }
}

pub(crate) fn find_stash_ref_by_hash(stash_list: &str, stash_hash: &str) -> Option<String> {
    stash_list.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next()) {
            (Some(hash), Some(reference)) if hash == stash_hash => Some(reference.to_string()),
            _ => None,
        }
    })
}

/// Options for [`stash_rebase_restore`].
pub(crate) struct StashRebaseOpts {
    /// Abort the rebase (via `git rebase --abort`) and restore the stash when the
    /// rebase fails.  Set to `false` when the caller intentionally leaves the working
    /// tree in the in-progress-rebase state so an agent can resolve conflicts.
    pub abort_on_failure: bool,
}

impl Default for StashRebaseOpts {
    fn default() -> Self {
        Self {
            abort_on_failure: true,
        }
    }
}

/// Outcome returned by [`stash_rebase_restore`].
#[derive(Debug)]
pub(crate) enum RebaseOutcome {
    /// Rebase completed successfully.
    Succeeded,
    /// Rebase failed.  When `abort_on_failure` is true the rebase was aborted and any
    /// stash was restored; when false the working tree is left in the in-progress-rebase
    /// state.
    Failed(String),
    /// A stash operation failed (push failed, or OID could not be captured); the
    /// rebase was skipped to preserve the dirty working tree.
    Skipped(String),
}

/// Stash uncommitted changes, rebase onto `target_ref`, then restore the stash.
///
/// This is the single canonical implementation of the stash → rebase → restore
/// pattern used in multiple call sites.  Callers are responsible for any
/// `git fetch` that should precede the rebase.
///
/// # Concurrency safety
/// Git stashes are repo-global; multiple worktrees share the same stash stack.
/// This function captures the stash OID immediately after pushing and applies by
/// that exact ref — never via `stash pop` — to avoid restoring a stash that
/// belongs to a different concurrently-running worktree.
///
/// # Stash failure handling
/// If the stash push fails, or the OID cannot be captured after a successful push,
/// the rebase is **skipped** and [`RebaseOutcome::Skipped`] is returned.  This
/// preserves the dirty working tree intact rather than risking data loss.
pub(crate) async fn stash_rebase_restore(
    dir: &Path,
    target_ref: &str,
    opts: StashRebaseOpts,
) -> anyhow::Result<RebaseOutcome> {
    let stash_ref: Option<String> = if commit::has_changes(dir).await {
        let stash_out = Command::new("git")
            .args(["stash", "push", "--include-untracked", "-m", "orch-rebase"])
            .current_dir(dir)
            .output_with_context()
            .await;

        match stash_out {
            Ok(o) if o.status.success() => {
                let stdout_preview = String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string();
                tracing::debug!(
                    dir = %dir.display(),
                    stdout = %stdout_preview,
                    "git stash push succeeded before rebase"
                );

                // Capture the OID of the stash object just created so we can
                // apply it back by that exact ref, regardless of any stashes
                // that other worktrees may push between now and then.
                let ref_out = Command::new("git")
                    .args(["rev-parse", "refs/stash@{0}"])
                    .current_dir(dir)
                    .output_with_context()
                    .await;
                let stash_hash = ref_out
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .filter(|s| !s.is_empty());

                if stash_hash.is_none() {
                    // Stash was created but we can't track its ref.
                    // Pop immediately to avoid orphaning the changes, then skip.
                    tracing::warn!(
                        dir = %dir.display(),
                        "git stash succeeded but rev-parse failed — popping stash and skipping rebase"
                    );
                    let _ = Command::new("git")
                        .args(["stash", "pop"])
                        .current_dir(dir)
                        .output_with_context()
                        .await;
                    return Ok(RebaseOutcome::Skipped(
                        "stash OID capture failed".to_string(),
                    ));
                }
                stash_hash
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
                tracing::warn!(
                    dir = %dir.display(),
                    code = ?o.status.code(),
                    stderr = %stderr,
                    "git stash push failed — skipping rebase to preserve dirty state"
                );
                return Ok(RebaseOutcome::Skipped(format!(
                    "stash push failed: {stderr}"
                )));
            }
            Err(e) => {
                tracing::warn!(
                    dir = %dir.display(),
                    error = %e,
                    "failed to run git stash push — skipping rebase to preserve dirty state"
                );
                return Ok(RebaseOutcome::Skipped(format!("stash push error: {e}")));
            }
        }
    } else {
        None
    };

    let output = git()
        .await
        .args(["rebase", target_ref])
        .current_dir(dir)
        .output_with_context()
        .await;

    match output {
        Ok(o) if o.status.success() => {
            tracing::debug!(dir = %dir.display(), target_ref, "rebase succeeded");
            restore_stash_by_hash(dir, stash_ref.as_deref()).await;
            Ok(RebaseOutcome::Succeeded)
        }
        Ok(o) => {
            let stderr = sanitize_push_error(&String::from_utf8_lossy(&o.stderr));
            tracing::warn!(dir = %dir.display(), target_ref, err = %stderr, "rebase failed");
            if opts.abort_on_failure {
                let abort = Command::new("git")
                    .args(["rebase", "--abort"])
                    .current_dir(dir)
                    .output_with_context()
                    .await;
                match abort {
                    Err(e) => {
                        tracing::warn!(dir = %dir.display(), error = %e, "git rebase --abort failed — worktree may be in inconsistent state");
                    }
                    Ok(o) if !o.status.success() => {
                        let stderr = String::from_utf8_lossy(&o.stderr);
                        tracing::warn!(dir = %dir.display(), stderr = %stderr, "git rebase --abort returned non-zero — worktree may be in inconsistent state");
                    }
                    _ => {}
                }
            }
            restore_stash_by_hash(dir, stash_ref.as_deref()).await;
            Ok(RebaseOutcome::Failed(stderr))
        }
        Err(e) => {
            if opts.abort_on_failure {
                let abort = Command::new("git")
                    .args(["rebase", "--abort"])
                    .current_dir(dir)
                    .output_with_context()
                    .await;
                match abort {
                    Err(e) => {
                        tracing::warn!(dir = %dir.display(), error = %e, "git rebase --abort failed — worktree may be in inconsistent state");
                    }
                    Ok(o) if !o.status.success() => {
                        let stderr = String::from_utf8_lossy(&o.stderr);
                        tracing::warn!(dir = %dir.display(), stderr = %stderr, "git rebase --abort returned non-zero — worktree may be in inconsistent state");
                    }
                    _ => {}
                }
            }
            restore_stash_by_hash(dir, stash_ref.as_deref()).await;
            Err(e)
        }
    }
}

/// Apply a stash entry by its OID and drop it from the stack on success.
///
/// On failure the entry is preserved so it can be recovered manually.
async fn restore_stash_by_hash(dir: &Path, stash_hash: Option<&str>) {
    let Some(stash_hash) = stash_hash else {
        return;
    };

    let apply = Command::new("git")
        .args(["stash", "apply", stash_hash])
        .current_dir(dir)
        .output_with_context()
        .await;
    if apply.is_ok_and(|o| o.status.success()) {
        let list = Command::new("git")
            .args(["stash", "list", "--format=%H %gd"])
            .current_dir(dir)
            .output_with_context()
            .await;
        if let Ok(list_out) = list
            && list_out.status.success()
        {
            let list_str = String::from_utf8_lossy(&list_out.stdout);
            if let Some(stash_ref) = find_stash_ref_by_hash(&list_str, stash_hash) {
                match Command::new("git")
                    .args(["stash", "drop", &stash_ref])
                    .current_dir(dir)
                    .output_with_context()
                    .await
                {
                    Ok(drop_out) if !drop_out.status.success() => {
                        let stderr = String::from_utf8_lossy(&drop_out.stderr);
                        tracing::warn!(
                            dir = %dir.display(),
                            stash_ref = %stash_ref,
                            err = %stderr,
                            "stash drop failed after apply — entry may remain on stack"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            dir = %dir.display(),
                            stash_ref = %stash_ref,
                            error = %e,
                            "stash drop command failed after apply — entry may remain on stack"
                        );
                    }
                    _ => {}
                }
            } else {
                tracing::warn!(
                    dir = %dir.display(),
                    stash = %stash_hash,
                    "stash applied but could not find ref by hash — stash entry may be orphaned on stack"
                );
            }
        }
    } else {
        tracing::warn!(
            dir = %dir.display(),
            stash = %stash_hash,
            "stash apply failed after rebase — stash preserved for manual recovery"
        );
    }
}

/// Commit what the agent left, with the tool the repository uses (see `commit`).
pub async fn auto_commit(
    dir: &Path,
    task_id: &str,
    title: &str,
    agent: &str,
    attempt: u32,
    when: commit::When,
) -> anyhow::Result<bool> {
    if !commit::has_changes(dir).await {
        return Ok(false);
    }

    tracing::info!(task_id, "auto-committing uncommitted changes");
    let task_ref = format_task_ref(task_id);
    let message = format!("{title}\n\nTask {task_ref}\nAgent: {agent}\nAttempt: {attempt}");
    match commit::commit_all(dir, &message, agent, when).await? {
        commit::Outcome::Committed => {
            tracing::info!(task_id, "auto-commit succeeded");
            Ok(true)
        }
        commit::Outcome::Nothing => Ok(false),
        commit::Outcome::Refused {
            by: commit::Tool::Git,
            reason,
        } => {
            tracing::warn!(task_id, err = %reason, "git commit failed");
            Ok(false)
        }
        commit::Outcome::Refused { reason, .. } => anyhow::bail!(reason),
    }
}

/// Maximum number of commits to replay during a rebase.
///
/// If a branch has diverged so far from the default branch that it would
/// require replaying more than this many commits, we skip the rebase entirely.
/// This prevents degenerate cases (e.g. a branch forked from the very first
/// commit that now sits 300+ commits behind main) from producing hundreds of
/// add/add conflicts that block the agent and leave the worktree unusable.
const MAX_REBASE_COMMITS: usize = 50;

/// Rebase the current branch on top of `origin/{branch}`.
///
/// Used for push-failure recovery: when another agent has pushed to the same
/// branch, we fetch the latest remote state and rebase local commits on top.
/// Returns `Ok(true)` if rebase succeeded, `Ok(false)` if there was nothing to
/// rebase, and `Err` if rebase failed (conflicts).
///
/// Similar to `rebase_on_default` but targets the feature branch instead of
/// the default branch, and is used after push failures rather than at startup.
pub async fn rebase_on_branch(dir: &Path, branch: &str) -> anyhow::Result<bool> {
    let origin_branch = format!("origin/{branch}");

    // Fetch the remote branch to get latest state.
    // 2-minute timeout mirrors the push timeout to prevent indefinite stalls.
    let fetch = time::timeout(
        Duration::from_secs(120),
        git()
            .await
            .args(["fetch", "origin", branch])
            .kill_on_drop(true)
            .current_dir(dir)
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("git fetch timed out after 120s"))
    .and_then(|r| r.map_err(Into::into));

    match fetch {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            let stderr = sanitize_push_error(&String::from_utf8_lossy(&o.stderr));
            anyhow::bail!("failed to fetch origin/{branch}: {stderr}");
        }
        Err(e) => anyhow::bail!("failed to fetch origin/{branch}: {e}"),
    }

    // Check if there are local commits to rebase.
    let count_output = Command::new("git")
        .args(["rev-list", "--count", &format!("{origin_branch}..HEAD")])
        .current_dir(dir)
        .output_with_context()
        .await;

    let commit_count: usize = match count_output {
        Ok(o) if o.status.success() => {
            let raw = String::from_utf8_lossy(&o.stdout);
            raw.trim().parse().map_err(|e| {
                anyhow::anyhow!(
                    "failed to parse commit count from git rev-list output {:?}: {e}",
                    raw.trim()
                )
            })?
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            anyhow::bail!("git rev-list --count failed: {stderr}");
        }
        Err(e) => anyhow::bail!("failed to run git rev-list --count: {e}"),
    };

    if commit_count == 0 {
        // No local commits to rebase — nothing to do.
        return Ok(false);
    }

    if commit_count > MAX_REBASE_COMMITS {
        anyhow::bail!(
            "refusing to rebase {commit_count} commits on origin/{branch} (max {MAX_REBASE_COMMITS})"
        );
    }

    tracing::info!(
        branch = branch,
        commit_count,
        "rebasing local commits on origin/{branch}"
    );

    match stash_rebase_restore(dir, &origin_branch, StashRebaseOpts::default()).await? {
        RebaseOutcome::Succeeded => Ok(true),
        RebaseOutcome::Failed(e) => anyhow::bail!("rebase failed: {e}"),
        RebaseOutcome::Skipped(reason) => anyhow::bail!("rebase skipped: {reason}"),
    }
}

/// Rebase the current branch on the default branch.
///
/// Run before the agent starts to ensure the worktree has the latest code.
/// Fetches both the default branch and the current branch so agents never
/// need to run `git fetch` themselves (important for sandboxed agents like
/// codex whose workspace-write sandbox blocks writes outside the worktree).
/// Non-fatal: if rebase fails (conflicts), the agent may still be able to work.
pub async fn rebase_on_default(dir: &Path, default_branch: &str) {
    // auto_merge leaves a conflicted rebase in place for the agent, aborting it here would undo that
    if rebase_in_progress(dir).await {
        tracing::info!("rebase already in progress, skipping pre-dispatch rebase");
        return;
    }

    // A later force push leases this tip, so fetching someone else's commits cannot permit replacing them
    let branch = get_current_branch(dir).await;
    record_push_lease(dir, &branch, default_branch).await;

    // Fetch current branch (for retries with existing remote commits)
    // and default branch (for rebase) in one call.
    let _ = git()
        .await
        .args(["fetch", "origin"])
        .current_dir(dir)
        .output_with_context()
        .await;

    // Count commits that would be replayed.  If the branch diverged too far
    // back we skip the rebase: replaying hundreds of historical commits produces
    // massive add/add conflicts (the branch adds files that main already has)
    // and leaves the worktree stuck.
    let commit_count = Command::new("git")
        .args([
            "rev-list",
            "--count",
            &format!("origin/{default_branch}..HEAD"),
        ])
        .current_dir(dir)
        .output_with_context()
        .await
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<usize>()
                .ok()
        })
        .unwrap_or(0);

    if commit_count > MAX_REBASE_COMMITS {
        tracing::warn!(
            default_branch,
            commit_count,
            max = MAX_REBASE_COMMITS,
            "skipping rebase: branch has too many commits to replay safely"
        );
        return;
    }

    match stash_rebase_restore(
        dir,
        &format!("origin/{default_branch}"),
        StashRebaseOpts::default(),
    )
    .await
    {
        Ok(RebaseOutcome::Succeeded) => {
            tracing::debug!(default_branch, "rebased worktree on default branch");
        }
        Ok(RebaseOutcome::Failed(e)) => {
            tracing::warn!(default_branch, err = %e, "rebase failed, continuing with current state");
        }
        Ok(RebaseOutcome::Skipped(reason)) => {
            tracing::warn!(default_branch, %reason, "stash failed before rebase, continuing with current state");
        }
        Err(e) => tracing::warn!(err = %e, "rebase error"),
    }
}

pub(crate) async fn rebase_in_progress(dir: &Path) -> bool {
    for name in ["rebase-merge", "rebase-apply"] {
        let out = Command::new("git")
            .args(["rev-parse", "--git-path", name])
            .current_dir(dir)
            .output_with_context()
            .await;
        if let Ok(o) = out {
            let p = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !p.is_empty() && dir.join(p).exists() {
                return true;
            }
        }
    }
    false
}

/// Tool patterns no agent may run: a worktree shares `.git/config` with the operator's checkout,
/// so a rewritten remote or URL rule would change the main repository
pub(crate) fn remote_change_tools() -> Vec<String> {
    [
        "Bash(git remote set-url*)",
        "Bash(git remote add*)",
        "Bash(git remote remove*)",
        "Bash(git remote rm*)",
        "Bash(git remote rename*)",
        "Bash(git config*remote.*)",
        "Bash(git config*url.*)",
    ]
    .map(String::from)
    .to_vec()
}

/// Credentialed git command whose stdout and stderr are sanitized when read
pub(crate) struct GitCommand {
    inner: Command,
}

impl GitCommand {
    pub(crate) fn arg<S: AsRef<OsStr>>(&mut self, arg: S) -> &mut Self {
        self.inner.arg(arg);
        self
    }

    pub(crate) fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.inner.args(args);
        self
    }

    pub(crate) fn current_dir<P: AsRef<Path>>(&mut self, dir: P) -> &mut Self {
        self.inner.current_dir(dir);
        self
    }

    pub(crate) fn env<K, V>(&mut self, key: K, val: V) -> &mut Self
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.inner.env(key, val);
        self
    }

    pub(crate) fn kill_on_drop(&mut self, kill: bool) -> &mut Self {
        self.inner.kill_on_drop(kill);
        self
    }

    pub(crate) async fn output(&mut self) -> io::Result<Output> {
        self.inner.output().await.map(sanitize_git_output)
    }

    pub(crate) async fn output_with_context(&mut self) -> anyhow::Result<Output> {
        self.inner
            .output_with_context()
            .await
            .map(sanitize_git_output)
    }
}

/// A `git` command for every call orch makes: GitHub over HTTPS with the gh token, never SSH,
/// and no commit or tag signing, whatever the owner's git config says
pub(crate) async fn git() -> GitCommand {
    let mut cmd = Command::new("git");
    cmd.envs(build_git_auth_env().await).kill_on_drop(true);
    GitCommand { inner: cmd }
}

/// Build git config environment variables that inject GitHub token credentials.
///
/// This handles two scenarios:
/// - **SSH remotes** (`git@github.com:user/repo.git`): rewritten to
///   `https://x-access-token:TOKEN@github.com/…` via `insteadOf`.
/// - **HTTPS remotes** (`https://github.com/user/repo.git`): rewritten to
///   `https://x-access-token:TOKEN@github.com/…` via a second `insteadOf`.
///
/// When no token is available, falls back to the legacy SSH→HTTPS conversion
/// without credentials (works for repos that use credential helpers or SSH keys).
///
/// Returns a `Vec<(String, String)>` of environment variable key-value pairs.
/// These are NOT visible in the process list (`ps`), unlike `-c` args.
///
/// Git environment variables:
/// - `GIT_CONFIG_COUNT` — number of config entries
/// - `GIT_CONFIG_KEY_N` — config key for entry N
/// - `GIT_CONFIG_VALUE_N` — config value for entry N
///
/// Signing is switched off too, `commit.gpgsign=false` and `tag.gpgsign=false`
pub(crate) async fn build_git_auth_env() -> Vec<(String, String)> {
    let token = token::shared().get_token().await.ok().flatten();
    build_git_auth_env_for_token(token)
}

fn build_git_auth_env_for_token(token: Option<String>) -> Vec<(String, String)> {
    let mut config: Vec<(String, String)> = match token {
        Some(t) if !t.is_empty() => {
            let authed = format!("url.https://x-access-token:{t}@github.com/.insteadOf");
            vec![
                (authed.clone(), "https://github.com/".into()),
                (authed, "git@github.com:".into()),
            ]
        }
        _ => vec![(
            "url.https://github.com/.insteadOf".into(),
            "git@github.com:".into(),
        )],
    };
    // orch's git never signs, a signing key would ask the owner for a passphrase or a touch
    config.push(("commit.gpgsign".into(), "false".into()));
    config.push(("tag.gpgsign".into(), "false".into()));

    let mut env = vec![("GIT_CONFIG_COUNT".to_string(), config.len().to_string())];
    for (n, (key, value)) in config.into_iter().enumerate() {
        env.push((format!("GIT_CONFIG_KEY_{n}"), key));
        env.push((format!("GIT_CONFIG_VALUE_{n}"), value));
    }
    env
}

/// Returns `true` if the push error indicates that the GitHub token lacks the
/// `workflow` OAuth scope, which is required to push changes to
/// `.github/workflows/` files.
///
/// GitHub rejects pushes with: "refusing to allow an OAuth App to create or
/// update workflow `…` without `workflow` scope"
pub(crate) fn is_workflow_scope_error(stderr: &str) -> bool {
    stderr.contains("without `workflow` scope")
        || stderr.contains("without 'workflow' scope")
        || (stderr.contains("refusing to allow")
            && stderr.contains("workflow")
            && stderr.contains("scope"))
}

/// Remove `.github/workflows/` files from committed changes and amend the
/// commits so the branch can be pushed without the `workflow` OAuth scope.
///
/// Strategy: find all commits ahead of `origin/{default_branch}`, identify
/// which ones touch `.github/workflows/`, and rewrite them to exclude those
/// files. Uses `git filter-branch`-style approach via interactive rebase
/// with `exec` steps.
///
/// Returns `Ok(true)` if workflow files were removed and commits rewritten,
/// `Ok(false)` if no workflow files were found in the commits.
async fn strip_workflow_files(dir: &Path, default_branch: &str) -> anyhow::Result<bool> {
    // Find workflow files in commits ahead of the default branch
    let output = Command::new("git")
        .args([
            "diff",
            "--name-only",
            &format!("origin/{default_branch}...HEAD"),
            "--",
            ".github/workflows/",
        ])
        .current_dir(dir)
        .output_with_context()
        .await?;

    let workflow_files: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(ToString::to_string)
        .collect();

    if workflow_files.is_empty() {
        return Ok(false);
    }

    tracing::info!(
        files = ?workflow_files,
        "stripping workflow files from commits to bypass missing workflow scope"
    );

    // Remove workflow files from the index and amend each commit.
    // We use a soft reset → remove files → re-commit approach for simplicity.
    //
    // Count commits ahead of default branch
    let count_output = Command::new("git")
        .args([
            "rev-list",
            "--count",
            &format!("origin/{default_branch}..HEAD"),
        ])
        .current_dir(dir)
        .output_with_context()
        .await?;

    let commit_count: usize = String::from_utf8_lossy(&count_output.stdout)
        .trim()
        .parse()
        .unwrap_or(0);

    if commit_count == 0 {
        return Ok(false);
    }

    // Save the commit messages before resetting.
    // Use a null byte as the record prefix — null bytes cannot appear in git commit
    // messages, so this separator is unambiguous (unlike a string like ---COMMIT_SEP---
    // which an agent could write into a commit body, corrupting the parse).
    let log_output = Command::new("git")
        .args([
            "log",
            "--format=%x00%H%n%B",
            &format!("origin/{default_branch}..HEAD"),
        ])
        .current_dir(dir)
        .output_with_context()
        .await?;
    let log_text = String::from_utf8_lossy(&log_output.stdout).to_string();

    // Parse commits in reverse order (oldest first).
    // Each record starts with \0 (from %x00 in the format), so split on \0 and
    // discard any leading empty fragment before the first record.
    let commits: Vec<(String, String)> = log_text
        .split('\0')
        .filter(|s| !s.trim().is_empty())
        .map(|block| {
            let block = block.trim();
            let first_nl = block.find('\n').unwrap_or(block.len());
            let hash = block[..first_nl].to_string();
            let msg = block[first_nl..].trim().to_string();
            (hash, msg)
        })
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    if commits.is_empty() {
        return Ok(false);
    }

    // Soft reset to the base (origin/default_branch)
    let reset = Command::new("git")
        .args(["reset", "--soft", &format!("origin/{default_branch}")])
        .current_dir(dir)
        .output_with_context()
        .await?;

    if !reset.status.success() {
        let stderr = String::from_utf8_lossy(&reset.stderr);
        anyhow::bail!("git reset --soft failed during workflow file stripping: {stderr}");
    }

    // Remove workflow files from the index
    let mut rm_args = vec!["rm", "--cached", "--ignore-unmatch", "--"];
    let file_refs: Vec<&str> = workflow_files.iter().map(String::as_str).collect();
    rm_args.extend(file_refs);

    let rm = Command::new("git")
        .args(&rm_args)
        .current_dir(dir)
        .output_with_context()
        .await?;

    if !rm.status.success() {
        let stderr = String::from_utf8_lossy(&rm.stderr);
        tracing::warn!("git rm --cached for workflow files failed (non-fatal): {stderr}");
    }

    // Re-commit all remaining changes as a single commit preserving the first message.
    // This squashes multiple commits but preserves the work minus workflow files.
    let has_staged = Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .current_dir(dir)
        .status()
        .await
        .is_ok_and(|s| !s.success());

    if !has_staged {
        tracing::warn!(
            "after removing workflow files, no staged changes remain — \
             the only changes were workflow files"
        );
        // Reset back to where we were — nothing to push
        // Use the original HEAD from the first (newest) commit
        if let Some((hash, _)) = commits.last() {
            let _ = Command::new("git")
                .args(["reset", "--soft", hash])
                .current_dir(dir)
                .output_with_context()
                .await;
        }
        return Ok(false);
    }

    // Use the first commit's message (oldest commit)
    let commit_msg = if commits.len() == 1 {
        commits[0].1.clone()
    } else {
        // Combine messages when squashing
        let mut combined = String::new();
        for (i, (_, msg)) in commits.iter().enumerate() {
            if i > 0 {
                combined.push_str("\n\n");
            }
            combined.push_str(msg);
        }
        combined.push_str("\n\n[workflow files stripped: token lacks `workflow` scope]");
        combined
    };

    // A repair of commits already made, so it lands through git even where shipcheck commits
    let commit = commit::git_commit(dir, &commit_msg, "orchestrator").await?;

    if !commit.status.success() {
        let stderr = String::from_utf8_lossy(&commit.stderr);
        // Recovery: restore the original commits so the worktree isn't stuck
        if let Some((original_head, _)) = commits.last() {
            let _ = Command::new("git")
                .args(["reset", "--hard", original_head])
                .current_dir(dir)
                .output_with_context()
                .await;
        }
        anyhow::bail!("git commit failed during workflow file stripping: {stderr}");
    }

    tracing::info!(
        stripped_files = ?workflow_files,
        original_commits = commit_count,
        "successfully stripped workflow files from commits"
    );

    Ok(true)
}

/// Get the current branch name.
pub(crate) async fn get_current_branch(dir: &Path) -> String {
    let output = Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(dir)
        .output_with_context()
        .await;

    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => String::new(),
    }
}

/// Count the number of changed files in the working directory.
pub async fn count_changed_files(dir: &Path) -> anyhow::Result<usize> {
    // Count modified and new files (excluding deleted)
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(dir)
        .output_with_context()
        .await?;

    let status_output = String::from_utf8_lossy(&output.stdout);
    let count = status_output
        .lines()
        .filter(|line| {
            let prefix = line.get(0..2).unwrap_or("");
            // Count modified (M), added (A), renamed (R), copied (C), untracked (??)
            prefix.starts_with('M')
                || prefix.starts_with('A')
                || prefix.starts_with('R')
                || prefix.starts_with('C')
                || prefix.starts_with('?')
        })
        .count();

    Ok(count)
}

/// Check if there are unpushed commits.
/// Returns `(remote_exists, has_unpushed_commits)`.
///
/// - `remote_exists` — whether `origin/<branch>` is known to the local repo.
/// - `has_unpushed_commits` — whether there are local commits not yet on the
///   remote.  When the remote branch doesn't exist the comparison is against
///   `origin/<default_branch>` so we can still detect new work.
async fn check_push_needed(dir: &Path, branch: &str, default_branch: &str) -> (bool, bool) {
    // Check if remote tracking branch exists
    let remote_exists = Command::new("git")
        .args(["rev-parse", &format!("origin/{branch}")])
        .current_dir(dir)
        .output_with_context()
        .await
        .is_ok_and(|o| o.status.success());

    let compare_ref = if remote_exists {
        format!("origin/{branch}..{branch}")
    } else {
        // Compare the named branch, not HEAD: a protected checkout still pushes the task branch
        format!("origin/{default_branch}..{branch}")
    };

    tracing::debug!(
        branch,
        compare_ref,
        remote_exists,
        "checking unpushed commits"
    );

    let output = Command::new("git")
        .args(["log", &compare_ref, "--oneline"])
        .current_dir(dir)
        .output_with_context()
        .await;

    let has_unpushed = match output {
        Ok(o) if o.status.success() => {
            let out = String::from_utf8_lossy(&o.stdout);
            let has = !out.trim().is_empty();
            tracing::debug!(branch, has_unpushed = has, "unpushed commits check result");
            has
        }
        _ => {
            tracing::debug!(branch, "git log failed for compare ref, assuming unpushed");
            // If we can't determine, assume there ARE commits to push
            // (better to attempt push and let it fail than silently skip)
            true
        }
    };

    (remote_exists, has_unpushed)
}

#[cfg(test)]
mod tests;
