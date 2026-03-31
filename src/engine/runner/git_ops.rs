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

    // git commit
    let commit = Command::new("git")
        .args(["commit", "-m", &commit_msg])
        .current_dir(dir)
        .output_with_context()
        .await?;

    if !commit.status.success() {
        let stderr = String::from_utf8_lossy(&commit.stderr);
        tracing::warn!(task_id, err = %stderr, "git commit failed");
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

    // Stash any uncommitted changes so the rebase can proceed cleanly.
    // Worktrees killed mid-run (e.g. service restart) may have leftover
    // unstaged changes from a previous attempt that block `git rebase`.
    //
    // Safety: git stashes are repo-global. With multiple worktrees running
    // concurrently we must capture the exact stash ref created here and apply
    // it by that ref — NOT with `stash pop`, which would apply stash@{0}
    // (the most-recent stash) and could restore a stash from a different
    // worktree running in parallel.
    let stash_ref: Option<String> = if has_changes(dir).await {
        let stash_out = Command::new("git")
            .args(["stash", "--include-untracked"])
            .current_dir(dir)
            .output_with_context()
            .await;
        match stash_out {
            Ok(o) if o.status.success() => {
                // Capture the OID of the stash object just created so we can
                // apply it back by its exact ref, regardless of any stashes
                // that other worktrees may push between now and then.
                let ref_out = Command::new("git")
                    .args(["rev-parse", "refs/stash"])
                    .current_dir(dir)
                    .output_with_context()
                    .await;
                ref_out
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .filter(|s| !s.is_empty())
            }
            _ => None,
        }
    } else {
        None
    };

    let output = Command::new("git")
        .args(["rebase", &format!("origin/{default_branch}")])
        .current_dir(dir)
        .output_with_context()
        .await;

    match output {
        Ok(o) if o.status.success() => {
            tracing::debug!(default_branch, "rebased worktree on default branch");
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            tracing::warn!(
                default_branch,
                err = %stderr,
                "rebase failed, aborting and continuing with current state"
            );
            // Abort failed rebase so the worktree is in a clean state
            let _ = Command::new("git")
                .args(["rebase", "--abort"])
                .current_dir(dir)
                .output_with_context()
                .await;
        }
        Err(e) => {
            tracing::warn!(err = %e, "failed to run rebase");
        }
    }

    // Restore stashed changes using the captured ref so we never accidentally
    // pop a stash that belongs to a different concurrently-running worktree.
    if let Some(ref stash_hash) = stash_ref {
        let apply = Command::new("git")
            .args(["stash", "apply", stash_hash])
            .current_dir(dir)
            .output_with_context()
            .await;
        if apply.map(|o| o.status.success()).unwrap_or(false) {
            let _ = Command::new("git")
                .args(["stash", "drop", stash_hash])
                .current_dir(dir)
                .output_with_context()
                .await;
        } else {
            tracing::warn!(stash = %stash_hash, "stash apply failed after rebase — stash preserved for manual recovery");
        }
    }
}

/// Build git `-c` config args that inject GitHub token credentials.
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
/// Returns a `Vec<String>` of alternating `-c KEY=VALUE` pairs suitable for
/// prepending to any `git` command's argument list.
pub(crate) fn build_git_auth_args() -> Vec<String> {
    // Use a fresh resolver here so tests that temporarily set env vars
    // are not affected by a process-wide cached resolver. The global
    // `shared()` resolver intentionally caches tokens for the running
    // process, but tests frequently mutate `GH_TOKEN`/`GITHUB_TOKEN` and
    // expecting immediate visibility. Creating a local resolver reads the
    // current environment without relying on the cached singleton.
    let token = crate::github::token::TokenResolver::default_env()
        .get_token_sync()
        .ok()
        .flatten();

    match token {
        Some(t) if !t.is_empty() => {
            // Map both SSH and HTTPS origins → HTTPS with token auth.
            // Having two insteadOf rules pointing at the same replacement is
            // valid: git picks the longest matching prefix.
            let authed = format!("url.https://x-access-token:{t}@github.com/.insteadOf");
            vec![
                "-c".to_string(),
                format!("{authed}=https://github.com/"),
                "-c".to_string(),
                format!("{authed}=git@github.com:"),
            ]
        }
        _ => {
            // No token: keep the legacy SSH→HTTPS conversion so SSH-origin
            // repos can still push via credential helpers or SSH keys.
            vec![
                "-c".to_string(),
                "url.https://github.com/.insteadOf=git@github.com:".to_string(),
            ]
        }
    }
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

    let auth_args = build_git_auth_args();

    // First attempt: normal push (no force).
    let push_args = build_push_args(&auth_args, branch_to_push, false);
    let output = Command::new("git")
        .args(&push_args)
        .current_dir(dir)
        .output_with_context()
        .await?;

    if output.status.success() {
        tracing::info!(branch = branch_to_push, "push succeeded");
        return Ok(true);
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    // If non-fast-forward, the branch was likely rebased on the default branch
    // by `rebase_on_default()` before the agent started — the local history is
    // correct but diverges from the remote. Force-push with lease to update it
    // without pulling (which would duplicate commits from main).
    if push_needs_rebase(&stderr) && remote_exists {
        tracing::info!(
            branch = branch_to_push,
            "push rejected (non-fast-forward), force-pushing with lease"
        );
        let force_args = build_push_args(&auth_args, branch_to_push, true);
        let output = Command::new("git")
            .args(&force_args)
            .current_dir(dir)
            .output_with_context()
            .await?;

        if output.status.success() {
            tracing::info!(branch = branch_to_push, "force push succeeded");
            return Ok(true);
        }

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!("force push failed: {stderr}");
    }

    anyhow::bail!("push failed: {stderr}")
}

fn build_push_args(auth_args: &[String], branch: &str, force: bool) -> Vec<String> {
    let mut args: Vec<String> = auth_args.to_vec();
    args.push("push".to_string());
    if force {
        args.push("--force-with-lease".to_string());
    }
    args.push("-u".to_string());
    args.push("origin".to_string());
    args.push(branch.to_string());
    args
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
    _dir: &Path,
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

    if !files.is_empty() {
        body.push_str("\n\n### Files changed\n\n");
        for file in files {
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

    // Create PR using GhHttp API
    let create_result = gh
        .create_pr(repo, pr_title, &body, branch, base_branch)
        .await;

    let url = match create_result {
        Ok(url) => url,
        Err(e) => {
            let err_str = format!("{e}");
            // For transient 5xx errors, GitHub may have created the PR despite
            // returning an error (e.g. 502 Bad Gateway after writing the PR).
            // Re-check for an existing PR before propagating the failure so we
            // don't orphan the PR from the task.
            if is_transient_github_error(&err_str) {
                tracing::warn!(
                    task_id,
                    error = %e,
                    "transient GitHub API error during PR creation — checking if PR was actually created"
                );
                match gh.get_pr_number(repo, branch).await {
                    Ok(Some(pr_number)) => {
                        tracing::info!(
                            task_id,
                            pr = pr_number,
                            "PR was created despite transient error — recovering"
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
                            "PR was not created despite transient error — propagating original error"
                        );
                        return Err(PrCreateError::ApiError(e));
                    }
                    Err(check_err) => {
                        tracing::warn!(
                            task_id,
                            error = %check_err,
                            "failed to verify PR existence after transient error — propagating original error"
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
/// (HTTP 5xx), where the PR may have been created despite the error response.
fn is_transient_github_error(err_str: &str) -> bool {
    // Match the error format produced by GhHttp: "GitHub API POST ... failed (5XX): ..."
    // This covers 500, 502, 503, 504, etc.
    err_str.contains("failed (5")
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
    fn build_git_auth_args_without_token_falls_back_to_ssh_https_conversion() {
        // Without a token in env the function must still return the legacy
        // SSH→HTTPS insteadOf rule so SSH-origin repos can push.
        let saved_gh = std::env::var("GH_TOKEN").ok();
        let saved_gh2 = std::env::var("GITHUB_TOKEN").ok();
        std::env::remove_var("GH_TOKEN");
        std::env::remove_var("GITHUB_TOKEN");

        let args = build_git_auth_args();

        // Restore env
        if let Some(v) = saved_gh {
            std::env::set_var("GH_TOKEN", v);
        }
        if let Some(v) = saved_gh2 {
            std::env::set_var("GITHUB_TOKEN", v);
        }

        // When no token is available the fallback must include the SSH insteadOf rule.
        let joined = args.join(" ");
        assert!(
            joined.contains("insteadOf=git@github.com:"),
            "expected SSH insteadOf fallback, got: {joined}"
        );
    }

    #[test]
    fn build_git_auth_args_with_token_covers_both_ssh_and_https() {
        // Temporarily inject a fake token so we can verify both insteadOf rules.
        let saved = std::env::var("GH_TOKEN").ok();
        std::env::set_var("GH_TOKEN", "ghp_testtoken1234");

        let args = build_git_auth_args();

        // Restore env
        match saved {
            Some(v) => std::env::set_var("GH_TOKEN", v),
            None => std::env::remove_var("GH_TOKEN"),
        }

        let joined = args.join(" ");
        // Must contain the token in the replacement URL
        assert!(
            joined.contains("x-access-token:ghp_testtoken1234@github.com"),
            "expected token in auth URL, got: {joined}"
        );
        // Must cover HTTPS origins
        assert!(
            joined.contains("insteadOf=https://github.com/"),
            "expected HTTPS insteadOf rule, got: {joined}"
        );
        // Must cover SSH origins
        assert!(
            joined.contains("insteadOf=git@github.com:"),
            "expected SSH insteadOf rule, got: {joined}"
        );
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
}
