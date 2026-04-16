//! Git operations — auto-commit, push, PR creation.
//!
//! These run after the agent completes to ensure all changes
//! are committed, pushed, and a PR is created.
//!
//! All GitHub API operations use [`GhHttp`] exclusively. The `gh` CLI
//! is not used as a fallback to ensure consistent behavior across
//! environments and simplify testing.

use crate::cmd::CommandErrorContext;
use crate::github::http::GhHttp;
use std::path::Path;

use tokio::process::Command;

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

/// Check if there are uncommitted changes in the working directory.
pub async fn has_changes(dir: &Path) -> bool {
    // Check for staged, unstaged, and untracked files
    let diff = Command::new("git")
        .args(["diff", "--quiet"])
        .current_dir(dir)
        .status()
        .await;

    let cached = Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .current_dir(dir)
        .status()
        .await;

    let untracked = Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard"])
        .current_dir(dir)
        .output_with_context()
        .await;

    let has_diff = diff.map(|s| !s.success()).unwrap_or(false);
    let has_cached = cached.map(|s| !s.success()).unwrap_or(false);
    let has_untracked = untracked
        .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false);

    has_diff || has_cached || has_untracked
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
    /// Disable GPG commit signing during the rebase (passes `-c commit.gpgsign=false`).
    pub disable_gpg_signing: bool,
    /// Abort the rebase (via `git rebase --abort`) and restore the stash when the
    /// rebase fails.  Set to `false` when the caller intentionally leaves the working
    /// tree in the in-progress-rebase state so an agent can resolve conflicts.
    pub abort_on_failure: bool,
}

impl Default for StashRebaseOpts {
    fn default() -> Self {
        Self {
            disable_gpg_signing: false,
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
    let stash_ref: Option<String> = if has_changes(dir).await {
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

    let mut rebase_args: Vec<&str> = Vec::new();
    if opts.disable_gpg_signing {
        rebase_args.extend(["-c", "commit.gpgsign=false"]);
    }
    rebase_args.extend(["rebase", target_ref]);

    let output = Command::new("git")
        .args(&rebase_args)
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
            let stderr = String::from_utf8_lossy(&o.stderr).to_string();
            tracing::warn!(dir = %dir.display(), target_ref, err = %stderr, "rebase failed");
            if opts.abort_on_failure {
                let abort = Command::new("git")
                    .args(["rebase", "--abort"])
                    .current_dir(dir)
                    .output_with_context()
                    .await;
                match abort {
                    Err(e) => {
                        tracing::warn!(dir = %dir.display(), error = %e, "git rebase --abort failed — worktree may be in inconsistent state")
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
                        tracing::warn!(dir = %dir.display(), error = %e, "git rebase --abort failed — worktree may be in inconsistent state")
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
    if apply.map(|o| o.status.success()).unwrap_or(false) {
        let list = Command::new("git")
            .args(["stash", "list", "--format=%H %gd"])
            .current_dir(dir)
            .output_with_context()
            .await;
        if let Ok(list_out) = list {
            if list_out.status.success() {
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
        }
    } else {
        tracing::warn!(
            dir = %dir.display(),
            stash = %stash_hash,
            "stash apply failed after rebase — stash preserved for manual recovery"
        );
    }
}

/// Resolve the git author identity from config, falling back to `{agent}[bot]`.
fn git_identity(agent: &str) -> (String, String) {
    let name = crate::config::get("git.name").unwrap_or_else(|_| format!("{agent}[bot]"));
    let email = crate::config::get("git.email")
        .unwrap_or_else(|_| format!("{agent}[bot]@users.noreply.github.com"));
    (name, email)
}

/// Auto-commit any uncommitted changes.
pub async fn auto_commit(
    dir: &Path,
    task_id: &str,
    title: &str,
    agent: &str,
    attempt: u32,
) -> anyhow::Result<bool> {
    // Check for changes first (before creating span to avoid Send issues)
    if !has_changes(dir).await {
        return Ok(false);
    }

    tracing::info!(task_id, "auto-committing uncommitted changes");

    let task_ref = format_task_ref(task_id);
    let commit_msg = format!("{title}\n\nTask {task_ref}\nAgent: {agent}\nAttempt: {attempt}");

    // git add -A
    let add = Command::new("git")
        .args(["add", "-A"])
        .current_dir(dir)
        .output_with_context()
        .await?;

    if !add.status.success() {
        tracing::warn!(task_id, "git add -A failed");
        return Ok(false);
    }

    // git commit — override author/committer to match the agent identity
    let (git_name, git_email) = git_identity(agent);
    let commit = Command::new("git")
        .args(["commit", "-m", &commit_msg])
        .env("GIT_AUTHOR_NAME", &git_name)
        .env("GIT_COMMITTER_NAME", &git_name)
        .env("GIT_AUTHOR_EMAIL", &git_email)
        .env("GIT_COMMITTER_EMAIL", &git_email)
        .current_dir(dir)
        .output_with_context()
        .await?;

    if !commit.status.success() {
        let stderr = String::from_utf8_lossy(&commit.stderr);
        tracing::warn!(task_id, err = %stderr, "git commit failed");
        // Unstage files so has_changes() sees them as unstaged on the next
        // check and the workflow can retry the commit cleanly.
        let restore = Command::new("git")
            .args(["restore", "--staged", "."])
            .current_dir(dir)
            .output_with_context()
            .await;
        match restore {
            Ok(o) if !o.status.success() => {
                tracing::warn!(
                    task_id,
                    stderr = %String::from_utf8_lossy(&o.stderr),
                    "git restore --staged failed after commit failure — files remain staged"
                );
            }
            Err(e) => {
                tracing::warn!(
                    task_id,
                    error = %e,
                    "git restore --staged command failed after commit failure — files remain staged"
                );
            }
            _ => {}
        }
        return Ok(false);
    }

    tracing::info!(task_id, "auto-commit succeeded");
    Ok(true)
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
    let fetch = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        Command::new("git")
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
            let stderr = String::from_utf8_lossy(&o.stderr);
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
            "refusing to rebase {commit_count} commits on origin/{branch} (max {})",
            MAX_REBASE_COMMITS
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
    // Fetch current branch (for retries with existing remote commits)
    // and default branch (for rebase) in one call.
    let _ = Command::new("git")
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
            tracing::debug!(default_branch, "rebased worktree on default branch")
        }
        Ok(RebaseOutcome::Failed(e)) => {
            tracing::warn!(default_branch, err = %e, "rebase failed, continuing with current state")
        }
        Ok(RebaseOutcome::Skipped(reason)) => {
            tracing::warn!(default_branch, %reason, "stash failed before rebase, continuing with current state")
        }
        Err(e) => tracing::warn!(err = %e, "rebase error"),
    }
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
pub(crate) fn build_git_auth_env() -> Vec<(String, String)> {
    let token = crate::github::token::TokenResolver::default_env()
        .get_token_sync()
        .ok()
        .flatten();
    build_git_auth_env_for_token(token)
}

fn build_git_auth_env_for_token(token: Option<String>) -> Vec<(String, String)> {
    match token {
        Some(t) if !t.is_empty() => {
            let authed = format!("url.https://x-access-token:{t}@github.com/.insteadOf");
            vec![
                ("GIT_CONFIG_COUNT".into(), "2".into()),
                ("GIT_CONFIG_KEY_0".into(), authed.clone()),
                ("GIT_CONFIG_VALUE_0".into(), "https://github.com/".into()),
                ("GIT_CONFIG_KEY_1".into(), authed),
                ("GIT_CONFIG_VALUE_1".into(), "git@github.com:".into()),
            ]
        }
        _ => {
            vec![
                ("GIT_CONFIG_COUNT".into(), "1".into()),
                (
                    "GIT_CONFIG_KEY_0".into(),
                    "url.https://github.com/.insteadOf".into(),
                ),
                ("GIT_CONFIG_VALUE_0".into(), "git@github.com:".into()),
            ]
        }
    }
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
        .map(|s| s.to_string())
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
    let file_refs: Vec<&str> = workflow_files.iter().map(|s| s.as_str()).collect();
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
        .map(|s| !s.success())
        .unwrap_or(false);

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

    let (git_name, git_email) = git_identity("orchestrator");
    let commit = Command::new("git")
        .args(["commit", "-m", &commit_msg])
        .env("GIT_AUTHOR_NAME", &git_name)
        .env("GIT_COMMITTER_NAME", &git_name)
        .env("GIT_AUTHOR_EMAIL", &git_email)
        .env("GIT_COMMITTER_EMAIL", &git_email)
        .current_dir(dir)
        .output_with_context()
        .await?;

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

/// Push the branch to origin.
pub async fn push_branch(dir: &Path, branch: &str, default_branch: &str) -> anyhow::Result<bool> {
    let current = get_current_branch(dir).await;
    let branch_to_push = if !current.is_empty() {
        &current
    } else {
        branch
    };

    // Guard against empty branch names (prevents git config corruption)
    if branch_to_push.is_empty() {
        anyhow::bail!("cannot push: branch name is empty");
    }

    // Skip push for main/master
    if branch_to_push == "main" || branch_to_push == "master" {
        return Ok(false);
    }

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

    let auth_env = build_git_auth_env();

    // First attempt: normal push (no force).
    // A 2-minute timeout prevents an indefinitely-stalled push from blocking
    // post-processing and triggering a stuck-task re-dispatch race.
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        Command::new("git")
            .args(["push", "-u", "origin", branch_to_push])
            .kill_on_drop(true)
            .envs(auth_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .current_dir(dir)
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("git push timed out after 120s"))??;

    if output.status.success() {
        tracing::info!(branch = branch_to_push, "push succeeded");
        return Ok(true);
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

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
                let retry_output = tokio::time::timeout(
                    std::time::Duration::from_secs(120),
                    Command::new("git")
                        .args(["push", "-u", "origin", branch_to_push])
                        .kill_on_drop(true)
                        .envs(auth_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
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
                    return Ok(true);
                }

                let retry_stderr = String::from_utf8_lossy(&retry_output.stderr)
                    .trim()
                    .to_string();

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

    // If non-fast-forward, the branch was likely rebased on the default branch
    // by `rebase_on_default()` before the agent started — the local history is
    // correct but diverges from the remote. Force-push with lease to update it
    // without pulling (which would duplicate commits from main).
    if push_needs_rebase(&stderr) && remote_exists {
        tracing::info!(
            branch = branch_to_push,
            "push rejected (non-fast-forward), force-pushing with lease"
        );
        let output = Command::new("git")
            .args(["push", "--force-with-lease", "-u", "origin", branch_to_push])
            .envs(auth_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .current_dir(dir)
            .output_with_context()
            .await?;

        if output.status.success() {
            tracing::info!(branch = branch_to_push, "force push succeeded");
            return Ok(true);
        }

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

        // Check if force push also hit the workflow scope error
        if is_workflow_scope_error(&stderr) {
            tracing::warn!(
                branch = branch_to_push,
                "force push also rejected due to workflow scope — stripping and retrying"
            );
            match strip_workflow_files(dir, default_branch).await {
                Ok(true) => {
                    let retry_output = Command::new("git")
                        .args(["push", "--force-with-lease", "-u", "origin", branch_to_push])
                        .envs(auth_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
                        .current_dir(dir)
                        .output_with_context()
                        .await?;

                    if retry_output.status.success() {
                        tracing::info!(
                            branch = branch_to_push,
                            "force push succeeded after stripping workflow files"
                        );
                        return Ok(true);
                    }

                    let retry_stderr = String::from_utf8_lossy(&retry_output.stderr)
                        .trim()
                        .to_string();
                    anyhow::bail!(
                        "force push failed after stripping workflow files: {retry_stderr}"
                    );
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

        anyhow::bail!("force push failed: {stderr}");
    }

    anyhow::bail!("push failed: {stderr}")
}

fn push_needs_rebase(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("non-fast-forward")
        || lower.contains("rejected")
        || lower.contains("fetch first")
        || lower.contains("behind")
}

/// Create a PR if one doesn't already exist.
///
/// Uses [`GhHttp`] exclusively for GitHub API operations. No `gh` CLI
/// fallback is used, ensuring consistent behavior across environments.
///
/// # Returns
/// - `Ok(url)` - PR URL (newly created or already existed)
/// - `Err(PrCreateError)` - API error or other failure
#[allow(clippy::too_many_arguments)]
pub async fn create_pr_if_needed(
    dir: &Path,
    branch: &str,
    title: &str,
    summary: &str,
    accomplished: &[String],
    remaining: &[String],
    files: &[String],
    task_id: &str,
    agent: &str,
    model: Option<&str>,
    repo: &str,
    base_branch: &str,
) -> PrCreateResult<String> {
    let gh = GhHttp::new()?;

    // Check if PR already exists using GhHttp API
    match gh.get_pr_number(repo, branch).await {
        Ok(Some(pr_number)) => {
            tracing::info!(task_id, pr = pr_number, "PR already exists");
            // Append attribution footer if the agent created the PR without one.
            append_pr_footer_if_missing(&gh, repo, pr_number, task_id, agent, model).await;
            // Fetch the existing PR to get its URL so the caller can store pr_number.
            let pr = match gh.get_pr(repo, pr_number).await {
                Ok(pr) => pr,
                Err(e) => {
                    tracing::warn!(task_id, error = %e, "get_pr API call failed after detecting existing PR");
                    return Err(PrCreateError::ApiError(e));
                }
            };
            return Ok(pr.html_url);
        }
        Ok(None) => {
            // No PR exists, proceed to create one
        }
        Err(e) => {
            tracing::warn!(task_id, error = %e, "get_pr_number API call failed");
            return Err(PrCreateError::ApiError(e));
        }
    }

    // Build PR body
    let mut body = format!(
        "## Summary\n\n{}",
        if summary.is_empty() { title } else { summary }
    );

    if !accomplished.is_empty() {
        body.push_str("\n\n### What was done\n\n");
        for item in accomplished {
            body.push_str(&format!("- {item}\n"));
        }
    }

    if !remaining.is_empty() {
        body.push_str("\n\n### Remaining\n\n");
        for item in remaining {
            body.push_str(&format!("- {item}\n"));
        }
    }

    // When the agent didn't self-report files, derive them from git.
    let git_files: Vec<String>;
    let effective_files: &[String] = if files.is_empty() {
        match tokio::process::Command::new("git")
            .args([
                "diff",
                &format!("origin/{base_branch}...HEAD"),
                "--name-only",
            ])
            .current_dir(dir)
            .output()
            .await
        {
            Ok(output) if output.status.success() => {
                git_files = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(|l| l.to_string())
                    .collect();
                &git_files
            }
            _ => files,
        }
    } else {
        files
    };

    if !effective_files.is_empty() {
        body.push_str("\n\n### Files changed\n\n");
        for file in effective_files {
            body.push_str(&format!("- `{file}`\n"));
        }
    }

    // Link PR to the issue so GitHub shows the connection in the sidebar.
    // Only for external tasks (issue numbers), not internal ones.
    if !task_id.starts_with("internal:") {
        if let Ok(issue_num) = task_id.parse::<u64>() {
            body.push_str(&format!("\n\nCloses #{issue_num}"));
        }
    }

    let model_str = model.map(|m| format!(" using `{m}`")).unwrap_or_default();
    let task_ref = format_task_ref(task_id);
    body.push_str(&format!(
        "\n\n---\n*Task {task_ref} · Created by {agent}[bot] via [Orch](https://github.com/gabrielkoerich/orch){model_str}*"
    ));

    // Always use the short task title for the PR title (summary goes in body)
    let pr_title = title;

    // Create PR using GhHttp API with exponential backoff retry for transient 5xx errors.
    const MAX_RETRIES: u32 = 3;
    let mut last_error: Option<anyhow::Error> = None;
    let mut created_url: Option<String> = None;
    let mut had_transient_error = false;

    for attempt in 1..=MAX_RETRIES {
        match gh
            .create_pr(repo, pr_title, &body, branch, base_branch)
            .await
        {
            Ok(u) => {
                created_url = Some(u);
                break;
            }
            Err(e) => {
                let err_str = format!("{e}");
                if is_transient_github_error(&err_str) {
                    had_transient_error = true;
                    if attempt < MAX_RETRIES {
                        let delay = std::time::Duration::from_secs(2u64.pow(attempt));
                        tracing::warn!(
                            task_id,
                            attempt,
                            delay_secs = delay.as_secs(),
                            error = %e,
                            "transient GitHub API error during PR creation — retrying"
                        );
                        tokio::time::sleep(delay).await;
                    }
                }
                last_error = Some(e);
            }
        }
    }

    let url = match created_url {
        Some(u) => u,
        None => {
            let e = last_error.unwrap_or_else(|| {
                anyhow::anyhow!("PR creation failed after retries (no error captured)")
            });
            let err_str = format!("{e}");
            // For transient 5xx errors, GitHub may have created the PR despite returning
            // an error (e.g. 502 after the write succeeded). Re-check for an existing PR
            // before propagating the failure so we don't orphan the PR from the task.
            // Also check when the final error is non-transient but a prior attempt had a
            // transient error — the PR may have been created on that earlier attempt (e.g.
            // attempt 1 returns 502, attempt 2 returns 422 "already exists").
            if is_transient_github_error(&err_str) || had_transient_error {
                tracing::warn!(
                    task_id,
                    error = %e,
                    "transient GitHub API error after all retries — checking if PR was actually created"
                );
                match gh.get_pr_number(repo, branch).await {
                    Ok(Some(pr_number)) => {
                        tracing::info!(
                            task_id,
                            pr = pr_number,
                            "PR was created despite transient errors — recovering"
                        );
                        append_pr_footer_if_missing(&gh, repo, pr_number, task_id, agent, model)
                            .await;
                        match gh.get_pr(repo, pr_number).await {
                            Ok(pr) => pr.html_url,
                            Err(get_err) => {
                                tracing::warn!(
                                    task_id,
                                    error = %get_err,
                                    "failed to fetch PR URL after transient-error recovery"
                                );
                                return Err(PrCreateError::ApiError(e));
                            }
                        }
                    }
                    Ok(None) => {
                        tracing::warn!(
                            task_id,
                            "PR was not created after all retries — propagating original error"
                        );
                        return Err(PrCreateError::ApiError(e));
                    }
                    Err(check_err) => {
                        tracing::warn!(
                            task_id,
                            error = %check_err,
                            "failed to verify PR existence after transient error retries — propagating original error"
                        );
                        return Err(PrCreateError::ApiError(e));
                    }
                }
            } else {
                return Err(PrCreateError::ApiError(e));
            }
        }
    };

    tracing::info!(task_id, pr_url = %url, "created PR via GhHttp API");

    Ok(url)
}

/// Returns true if the error string indicates a transient GitHub API failure
/// (HTTP 5xx, network errors, DNS failures, TLS issues, rate limits), where
/// the operation may succeed on retry.
pub(crate) fn is_transient_github_error(err_str: &str) -> bool {
    // Treat transport/send failures and explicit GhHttp server-error messages
    // as transient. Older callers relied on parsing the "failed (NNN)" pattern
    // to detect 5xx — keep that, but also recognize the newer wrappers.
    if err_str.contains("HTTP send failed") {
        return true;
    }
    if err_str.contains("GitHub API server error") {
        return true;
    }

    // Check for HTTP 5xx status codes
    if extract_github_http_status(err_str)
        .map(|s| (500..600).contains(&s))
        .unwrap_or(false)
    {
        return true;
    }

    // Check for common network-level transient errors
    let lower = err_str.to_lowercase();

    // Connection and transport errors
    if lower.contains("connection reset")
        || lower.contains("broken pipe")
        || lower.contains("connection refused")
        || lower.contains("connection closed")
        || lower.contains("transport error")
    {
        return true;
    }

    // Timeout errors
    if lower.contains("timeout") {
        return true;
    }

    // DNS resolution errors
    if lower.contains("dns error")
        || lower.contains("resolve")
        || lower.contains("name resolution")
        || lower.contains("no such host")
    {
        return true;
    }

    // TLS/SSL errors
    if lower.contains("tls handshake")
        || lower.contains("certificate")
        || lower.contains("ssl error")
    {
        return true;
    }

    // Network unreachable errors
    if lower.contains("network is unreachable")
        || lower.contains("host unreachable")
        || lower.contains("temporary failure")
    {
        return true;
    }

    // EOF errors (connection dropped mid-request)
    if lower.contains("unexpected eof") || lower.contains("unexpected end of file") {
        return true;
    }

    // Internal circuit-breaker state (orch's own GitHub 5xx protection)
    if lower.contains("circuit-breaker") || lower.contains("circuit breaker") {
        return true;
    }

    // Rate limit errors (transient with cooldown/retry)
    if lower.contains("rate limit") || lower.contains("too many requests") {
        return true;
    }

    false
}

/// Returns true when an error looks like a transient *GitHub API/CLI* failure.
///
/// This is stricter than [`is_transient_github_error`]: in addition to matching
/// a transient transport pattern, it requires GitHub-specific context in the
/// message. This prevents generic agent/runtime transport failures (for
/// example "broken pipe" from a model stream disconnect) from being treated as
/// GitHub retry signals in review reroute logic.
pub(crate) fn is_transient_github_api_error(err_str: &str) -> bool {
    if !is_transient_github_error(err_str) {
        return false;
    }

    if extract_github_http_status(err_str).is_some() {
        return true;
    }

    let lower = err_str.to_lowercase();
    lower.contains("github")
        || lower.contains("api.github.com")
        || lower.contains("gh api")
        || lower.contains("gh pr")
        || lower.contains("gh error")
}

/// Extract the HTTP status code from a GhHttp error string.
///
/// GhHttp formats errors as: `"GitHub API POST https://... failed (NNN): body"`
/// This parses the numeric status code from that format.
fn extract_github_http_status(err_str: &str) -> Option<u16> {
    let marker = "failed (";
    let start = err_str.find(marker)? + marker.len();
    let rest = &err_str[start..];
    let end = rest.find(')')?;
    rest[..end].parse().ok()
}

/// Append the Orch attribution footer to an existing PR body if not already present.
///
/// Called when the agent created the PR directly (bypassing orch's
/// `create_pr_if_needed`), so the footer was never added by `build_pr_body`.
async fn append_pr_footer_if_missing(
    gh: &GhHttp,
    repo: &str,
    pr_number: u64,
    task_id: &str,
    agent: &str,
    model: Option<&str>,
) {
    let pr = match gh.get_pr(repo, pr_number).await {
        Ok(pr) => pr,
        Err(e) => {
            tracing::debug!(task_id, error = %e, "failed to fetch PR for footer check");
            return;
        }
    };

    let body = pr.body.unwrap_or_default();
    if body.contains("via [Orch]") {
        return; // Footer already present
    }

    let model_str = model.map(|m| format!(" using `{m}`")).unwrap_or_default();
    let task_ref = format_task_ref(task_id);
    let footer = format!(
        "\n\n---\n*Task {task_ref} · Created by {agent}[bot] via [Orch](https://github.com/gabrielkoerich/orch){model_str}*"
    );
    let new_body = format!("{body}{footer}");

    if let Err(e) = gh.update_pr_body(repo, pr_number, &new_body).await {
        tracing::warn!(task_id, error = %e, "failed to append footer to PR (non-fatal)");
    } else {
        tracing::info!(
            task_id,
            pr = pr_number,
            "appended attribution footer to agent-created PR"
        );
    }
}

/// Get the current branch name.
async fn get_current_branch(dir: &Path) -> String {
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
        .map(|o| o.status.success())
        .unwrap_or(false);

    let compare_ref = if remote_exists {
        format!("origin/{branch}..HEAD")
    } else {
        // Compare against default branch (passed in, NOT detected from
        // current HEAD — in a worktree HEAD is the feature branch itself)
        format!("origin/{default_branch}..HEAD")
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
mod tests {
    use super::*;

    #[test]
    fn pr_create_error_display() {
        let err = PrCreateError::ApiError(anyhow::anyhow!("test error"));
        assert_eq!(format!("{}", err), "GitHub API error: test error");
    }

    #[test]
    fn pr_create_error_from_anyhow() {
        let anyhow_err = anyhow::anyhow!("source error");
        let pr_err: PrCreateError = anyhow_err.into();
        assert!(matches!(pr_err, PrCreateError::ApiError(_)));
    }

    #[test]
    fn build_git_auth_env_without_token_falls_back_to_ssh_https_conversion() {
        // Pass None directly to test the no-token path without relying on env vars
        // or the gh CLI, keeping the test deterministic regardless of local auth state.
        let env = build_git_auth_env_for_token(None);

        // When no token is available the fallback must include the SSH insteadOf rule.
        // The SSH→HTTPS conversion sets the target URL (the rewrite destination) to git@github.com:.
        let has_ssh_rule = env
            .iter()
            .any(|(k, v)| k == "GIT_CONFIG_VALUE_0" && v == "git@github.com:");
        assert!(
            has_ssh_rule,
            "expected SSH insteadOf fallback (value is git@github.com:), got: {env:?}"
        );
    }

    #[test]
    fn build_git_auth_env_with_token_covers_both_ssh_and_https() {
        // Pass a pre-built token directly to test the token path without touching env vars.
        let env = build_git_auth_env_for_token(Some("ghp_testtoken1234".into()));

        // Must contain the token in the auth URL (the token is in the value of GIT_CONFIG_KEY_N).
        let has_token = env
            .iter()
            .any(|(_, v)| v.contains("x-access-token:ghp_testtoken1234@github.com"));
        assert!(has_token, "expected token in auth URL, got: {env:?}");
        // Must cover HTTPS origins (GIT_CONFIG_VALUE_0 is the HTTPS source URL).
        let has_https_rule = env
            .iter()
            .any(|(k, v)| k == "GIT_CONFIG_VALUE_0" && v == "https://github.com/");
        assert!(
            has_https_rule,
            "expected HTTPS insteadOf rule, got: {env:?}"
        );
        // Must cover SSH origins (GIT_CONFIG_VALUE_1 is the SSH source URL).
        let has_ssh_rule = env
            .iter()
            .any(|(k, v)| k == "GIT_CONFIG_VALUE_1" && v == "git@github.com:");
        assert!(has_ssh_rule, "expected SSH insteadOf rule, got: {env:?}");
    }

    #[test]
    fn find_stash_ref_by_hash_returns_matching_reference() {
        let stash_list = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa stash@{1}\nbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb stash@{0}";

        assert_eq!(
            find_stash_ref_by_hash(stash_list, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            Some("stash@{1}".to_string())
        );
        assert_eq!(
            find_stash_ref_by_hash(stash_list, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            Some("stash@{0}".to_string())
        );
    }

    #[test]
    fn find_stash_ref_by_hash_returns_none_when_missing_or_malformed() {
        let stash_list = "malformed-line\naaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa stash@{2}";

        assert_eq!(
            find_stash_ref_by_hash(stash_list, "cccccccccccccccccccccccccccccccccccccccc"),
            None
        );
        assert_eq!(find_stash_ref_by_hash("", "anything"), None);
    }

    #[test]
    fn push_needs_rebase_detects_common_rejections() {
        assert!(push_needs_rebase(
            "! [rejected] branch -> branch (fetch first)"
        ));
        assert!(push_needs_rebase("non-fast-forward update was rejected"));
        assert!(push_needs_rebase(
            "push rejected because the remote contains work"
        ));
    }

    #[test]
    fn extract_github_http_status_parses_ghhttp_error_format() {
        let err =
            "GitHub API POST https://api.github.com/repos/foo/bar/pulls failed (502): Bad Gateway";
        assert_eq!(extract_github_http_status(err), Some(502));

        let err500 = "GitHub API POST https://api.github.com/repos/foo/bar/pulls failed (500): Internal Server Error";
        assert_eq!(extract_github_http_status(err500), Some(500));

        let err422 = "GitHub API POST https://api.github.com/repos/foo/bar/pulls failed (422): Unprocessable Entity";
        assert_eq!(extract_github_http_status(err422), Some(422));

        let not_github = "some other error";
        assert_eq!(extract_github_http_status(not_github), None);
    }

    #[test]
    fn is_transient_github_error_matches_5xx_only() {
        let err502 =
            "GitHub API POST https://api.github.com/repos/foo/bar/pulls failed (502): Bad Gateway";
        assert!(is_transient_github_error(err502));

        let err503 = "GitHub API POST https://api.github.com/repos/foo/bar/pulls failed (503): Service Unavailable";
        assert!(is_transient_github_error(err503));

        let err422 = "GitHub API POST https://api.github.com/repos/foo/bar/pulls failed (422): Unprocessable Entity";
        assert!(!is_transient_github_error(err422));

        let err404 =
            "GitHub API POST https://api.github.com/repos/foo/bar/pulls failed (404): Not Found";
        assert!(!is_transient_github_error(err404));
    }

    #[test]
    fn is_transient_github_error_matches_connection_errors() {
        // Connection-related transient errors
        assert!(is_transient_github_error("connection refused"));
        assert!(is_transient_github_error("Connection reset by peer"));
        assert!(is_transient_github_error("broken pipe"));
        assert!(is_transient_github_error("connection closed unexpectedly"));
        assert!(is_transient_github_error(
            "transport error: connection lost"
        ));
    }

    #[test]
    fn is_transient_github_error_matches_dns_errors() {
        // DNS resolution failures
        assert!(is_transient_github_error("dns error: failed to lookup"));
        assert!(is_transient_github_error("failed to resolve hostname"));
        assert!(is_transient_github_error("name resolution failed"));
        assert!(is_transient_github_error("no such host: api.github.com"));
    }

    #[test]
    fn is_transient_github_error_matches_tls_errors() {
        // TLS/SSL handshake failures
        assert!(is_transient_github_error("tls handshake failed"));
        assert!(is_transient_github_error("certificate verification failed"));
        assert!(is_transient_github_error("ssl error: unknown ca"));
    }

    #[test]
    fn is_transient_github_error_matches_network_unreachable() {
        // Network unreachable errors
        assert!(is_transient_github_error("network is unreachable"));
        assert!(is_transient_github_error("host unreachable"));
        assert!(is_transient_github_error(
            "temporary failure in name resolution"
        ));
    }

    #[test]
    fn is_transient_github_error_matches_eof_errors() {
        // EOF/connection dropped errors
        assert!(is_transient_github_error("unexpected eof while reading"));
        assert!(is_transient_github_error("unexpected end of file"));
    }

    #[test]
    fn is_transient_github_error_matches_rate_limit() {
        // Rate limit errors (transient with cooldown)
        assert!(is_transient_github_error("rate limit exceeded"));
        assert!(is_transient_github_error("429 Too Many Requests"));
        assert!(is_transient_github_error("too many requests, retry later"));
    }

    #[test]
    fn is_transient_github_error_matches_timeout() {
        // Timeout errors
        assert!(is_transient_github_error("operation timeout"));
        assert!(is_transient_github_error("request timeout"));
    }

    #[test]
    fn is_transient_github_error_rejects_non_transient() {
        // Non-transient errors should not match
        assert!(!is_transient_github_error("permission denied"));
        assert!(!is_transient_github_error("file not found"));
        assert!(!is_transient_github_error("invalid argument"));
        assert!(!is_transient_github_error("authentication failed"));
        assert!(!is_transient_github_error("bad credentials"));
    }

    #[test]
    fn is_transient_github_api_error_requires_github_context() {
        assert!(is_transient_github_api_error(
            "GitHub API POST https://api.github.com/repos/foo/bar/pulls failed (502): Bad Gateway"
        ));
        assert!(is_transient_github_api_error(
            "gh error: transport error: connection reset by peer"
        ));
        assert!(!is_transient_github_api_error(
            "codex failed: Reconnecting... (stream disconnected before completion: Broken pipe)"
        ));
    }

    #[tokio::test]
    async fn auto_commit_unstages_files_on_commit_failure() {
        // Create a temp git repo with one initial commit
        let dir =
            std::env::temp_dir().join(format!("orch_test_auto_commit_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Init repo
        let _ = Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .await;
        let _ = Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&dir)
            .output()
            .await;
        let _ = Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&dir)
            .output()
            .await;
        // Create initial file and commit
        std::fs::write(dir.join("init.txt"), "init").unwrap();
        let _ = Command::new("git")
            .args(["add", "init.txt"])
            .current_dir(&dir)
            .output()
            .await;
        let _ = Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&dir)
            .output()
            .await;

        // Create a new file (unstaged)
        std::fs::write(dir.join("new_file.txt"), "content").unwrap();

        // Verify has_changes sees it
        assert!(has_changes(&dir).await);

        // auto_commit should return false (no git user.email configured for this
        // test repo would cause commit to fail if we hadn't set it, but we did —
        // so this test verifies the happy path too).
        // For this test, the commit should succeed since we set user.email/name.
        let result = auto_commit(&dir, "1", "Test title", "test", 1).await;
        assert!(result.unwrap());

        // Verify no changes remain
        assert!(!has_changes(&dir).await);

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn auto_commit_unstages_on_pre_commit_hook_failure() {
        // Create a temp git repo where commit always fails (via pre-commit hook)
        let dir =
            std::env::temp_dir().join(format!("orch_test_auto_commit_hook_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Init repo
        let _ = Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .await;
        let _ = Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&dir)
            .output()
            .await;
        let _ = Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&dir)
            .output()
            .await;
        // Create initial commit
        std::fs::write(dir.join("init.txt"), "init").unwrap();
        let _ = Command::new("git")
            .args(["add", "init.txt"])
            .current_dir(&dir)
            .output()
            .await;
        let _ = Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&dir)
            .output()
            .await;

        // Install a pre-commit hook that always fails
        let hooks_dir = dir.join(".git/hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        std::fs::write(hooks_dir.join("pre-commit"), "#!/bin/sh\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(hooks_dir.join("pre-commit"))
                .unwrap()
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(hooks_dir.join("pre-commit"), perms).unwrap();
        }

        // Create a new file
        std::fs::write(dir.join("staged_file.txt"), "content").unwrap();

        // Verify has_changes sees it
        assert!(has_changes(&dir).await);

        // auto_commit should return false (pre-commit hook blocks it)
        let result = auto_commit(&dir, "2", "Test title", "test", 1).await;
        assert!(!result.unwrap());

        // After commit failure, files should be unstaged — has_changes should
        // still return true because the unstaged file is still there.
        assert!(has_changes(&dir).await);

        // Verify the file is NOT staged (git diff --cached --quiet should succeed)
        let cached = Command::new("git")
            .args(["diff", "--cached", "--quiet"])
            .current_dir(&dir)
            .status()
            .await;
        assert!(
            cached.map(|s| s.success()).unwrap_or(false),
            "files should be unstaged after commit failure"
        );

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_workflow_scope_error_detects_backtick_format() {
        let err = "! [remote rejected] branch -> branch (refusing to allow an OAuth App to create or update workflow `.github/workflows/ci.yml` without `workflow` scope)";
        assert!(is_workflow_scope_error(err));
    }

    #[test]
    fn is_workflow_scope_error_detects_single_quote_format() {
        let err = "refusing to allow an OAuth App to create or update workflow '.github/workflows/ci.yml' without 'workflow' scope";
        assert!(is_workflow_scope_error(err));
    }

    #[test]
    fn is_workflow_scope_error_detects_refusing_to_allow_variant() {
        let err = "remote: refusing to allow a Personal Access Token to create or update workflow `.github/workflows/scan.yml` without `workflow` scope";
        assert!(is_workflow_scope_error(err));
    }

    #[test]
    fn is_workflow_scope_error_rejects_unrelated_errors() {
        assert!(!is_workflow_scope_error("non-fast-forward"));
        assert!(!is_workflow_scope_error("permission denied"));
        assert!(!is_workflow_scope_error("authentication failed"));
        assert!(!is_workflow_scope_error("rejected"));
    }

    #[tokio::test]
    async fn strip_workflow_files_removes_workflow_from_commits() {
        // Create a temp git repo simulating the scenario
        let dir = std::env::temp_dir().join(format!("orch_test_strip_wf_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Init repo with initial commit
        let _ = Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .await;
        let _ = Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&dir)
            .output()
            .await;
        let _ = Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&dir)
            .output()
            .await;
        std::fs::write(dir.join("init.txt"), "init").unwrap();
        let _ = Command::new("git")
            .args(["add", "."])
            .current_dir(&dir)
            .output()
            .await;
        let _ = Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&dir)
            .output()
            .await;

        // Create a "remote" by adding the repo as its own origin (for rev-list)
        let _ = Command::new("git")
            .args(["remote", "add", "origin", dir.to_str().unwrap()])
            .current_dir(&dir)
            .output()
            .await;
        let _ = Command::new("git")
            .args(["fetch", "origin"])
            .current_dir(&dir)
            .output()
            .await;

        // Create a branch off main
        let _ = Command::new("git")
            .args(["checkout", "-b", "feature"])
            .current_dir(&dir)
            .output()
            .await;

        // Add both a normal file and a workflow file
        std::fs::write(dir.join("feature.txt"), "feature work").unwrap();
        let wf_dir = dir.join(".github/workflows");
        std::fs::create_dir_all(&wf_dir).unwrap();
        std::fs::write(wf_dir.join("ci.yml"), "name: CI\non: push").unwrap();
        let _ = Command::new("git")
            .args(["add", "."])
            .current_dir(&dir)
            .output()
            .await;
        let _ = Command::new("git")
            .args(["commit", "-m", "add feature and workflow"])
            .current_dir(&dir)
            .output()
            .await;

        // Get the default branch name
        let branch_out = Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "origin/HEAD"])
            .current_dir(&dir)
            .output()
            .await;
        // Fallback to "main" if origin/HEAD is not set
        let default_branch = branch_out
            .ok()
            .filter(|o| o.status.success())
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .strip_prefix("origin/")
                    .unwrap_or("main")
                    .to_string()
            })
            .unwrap_or_else(|| "main".to_string());

        // Strip workflow files
        let stripped = strip_workflow_files(&dir, &default_branch).await.unwrap();
        assert!(stripped, "should have stripped workflow files");

        // Verify workflow file is no longer in the committed tree
        let ls_output = Command::new("git")
            .args(["ls-tree", "-r", "--name-only", "HEAD"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        let files = String::from_utf8_lossy(&ls_output.stdout);
        assert!(
            !files.contains(".github/workflows/ci.yml"),
            "workflow file should be removed from commits"
        );
        assert!(
            files.contains("feature.txt"),
            "non-workflow files should be preserved"
        );

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn strip_workflow_files_returns_false_when_no_workflows() {
        let dir =
            std::env::temp_dir().join(format!("orch_test_strip_wf_none_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Init repo
        let _ = Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .await;
        let _ = Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&dir)
            .output()
            .await;
        let _ = Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&dir)
            .output()
            .await;
        std::fs::write(dir.join("init.txt"), "init").unwrap();
        let _ = Command::new("git")
            .args(["add", "."])
            .current_dir(&dir)
            .output()
            .await;
        let _ = Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&dir)
            .output()
            .await;

        let _ = Command::new("git")
            .args(["remote", "add", "origin", dir.to_str().unwrap()])
            .current_dir(&dir)
            .output()
            .await;
        let _ = Command::new("git")
            .args(["fetch", "origin"])
            .current_dir(&dir)
            .output()
            .await;

        let _ = Command::new("git")
            .args(["checkout", "-b", "feature2"])
            .current_dir(&dir)
            .output()
            .await;

        // Only non-workflow files
        std::fs::write(dir.join("feature.txt"), "feature").unwrap();
        let _ = Command::new("git")
            .args(["add", "."])
            .current_dir(&dir)
            .output()
            .await;
        let _ = Command::new("git")
            .args(["commit", "-m", "add feature only"])
            .current_dir(&dir)
            .output()
            .await;

        let stripped = strip_workflow_files(&dir, "main").await.unwrap();
        assert!(!stripped, "should not strip when no workflow files exist");

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }
}
