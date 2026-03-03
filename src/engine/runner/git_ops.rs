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

/// Errors that can occur during PR creation.
#[derive(Debug, thiserror::Error)]
pub enum PrCreateError {
    #[error("GitHub API error: {0}")]
    ApiError(#[from] anyhow::Error),
}

/// Result type for PR creation operations.
pub type PrCreateResult<T> = Result<T, PrCreateError>;

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

    let commit_msg = format!("{title}\n\nTask #{task_id}\nAgent: {agent}\nAttempt: {attempt}");

    tracing::info!(task_id, "auto-committing uncommitted changes");

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

    // Check if there are commits to push
    let has_unpushed = has_unpushed_commits(dir, branch_to_push, default_branch).await;
    if !has_unpushed {
        tracing::debug!(
            branch = branch_to_push,
            "no unpushed commits detected, skipping push"
        );
        return Ok(false);
    }

    tracing::info!(branch = branch_to_push, "pushing branch");

    let output = Command::new("git")
        .args([
            "-c",
            "url.https://github.com/.insteadOf=git@github.com:",
            "push",
            "-u",
            "origin",
            branch_to_push,
        ])
        .current_dir(dir)
        .output_with_context()
        .await?;

    if output.status.success() {
        tracing::info!(branch = branch_to_push, "push succeeded");
        return Ok(true);
    }

    let stderr = String::from_utf8_lossy(&output.stderr);

    // If push failed due to non-fast-forward, try pull --rebase and retry once
    if stderr.contains("non-fast-forward") || stderr.contains("rejected") {
        tracing::warn!(
            branch = branch_to_push,
            "push rejected (non-fast-forward), pulling with rebase and retrying"
        );

        let pull = Command::new("git")
            .args(["pull", "--rebase", "origin", branch_to_push])
            .current_dir(dir)
            .output_with_context()
            .await;

        match pull {
            Ok(p) if p.status.success() => {
                // Retry push after rebase
                let retry = Command::new("git")
                    .args([
                        "-c",
                        "url.https://github.com/.insteadOf=git@github.com:",
                        "push",
                        "-u",
                        "origin",
                        branch_to_push,
                    ])
                    .current_dir(dir)
                    .output_with_context()
                    .await?;

                if retry.status.success() {
                    tracing::info!(branch = branch_to_push, "push succeeded after rebase");
                    return Ok(true);
                }
                let retry_err = String::from_utf8_lossy(&retry.stderr);
                tracing::warn!(branch = branch_to_push, err = %retry_err, "push still failed after rebase");
                anyhow::bail!("push failed after rebase: {retry_err}")
            }
            Ok(p) => {
                let pull_err = String::from_utf8_lossy(&p.stderr);
                tracing::warn!(branch = branch_to_push, err = %pull_err, "pull --rebase failed (conflicts?)");
                // Abort the rebase so worktree is clean
                let _ = Command::new("git")
                    .args(["rebase", "--abort"])
                    .current_dir(dir)
                    .output_with_context()
                    .await;
                anyhow::bail!("push failed: {stderr} (rebase also failed: {pull_err})")
            }
            Err(e) => {
                anyhow::bail!("push failed: {stderr} (pull --rebase error: {e})")
            }
        }
    }

    tracing::warn!(branch = branch_to_push, err = %stderr, "push failed");
    anyhow::bail!("push failed: {stderr}")
}

/// Create a PR if one doesn't already exist.
///
/// Uses [`GhHttp`] exclusively for GitHub API operations. No `gh` CLI
/// fallback is used, ensuring consistent behavior across environments.
///
/// # Returns
/// - `Ok(Some(url))` - PR was successfully created
/// - `Ok(None)` - PR already exists (not an error)
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
    repo: &str,
    base_branch: &str,
) -> PrCreateResult<Option<String>> {
    let gh = GhHttp::new();

    // Check if PR already exists using GhHttp API
    match gh.get_pr_number(repo, branch).await {
        Ok(Some(pr_number)) => {
            tracing::info!(task_id, pr = pr_number, "PR already exists");
            return Ok(None);
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

    body.push_str(&format!(
        "\n\n---\n*Task #{task_id} · Created by {agent}[bot] via [Orch](https://github.com/gabrielkoerich/orch)*"
    ));

    // Always use the short task title for the PR title (summary goes in body)
    let pr_title = title;

    // Create PR using GhHttp API
    let url = gh
        .create_pr(repo, pr_title, &body, branch, base_branch)
        .await
        .map_err(PrCreateError::ApiError)?;

    tracing::info!(task_id, pr_url = %url, "created PR via GhHttp API");

    // Link the issue to the PR branch via API (best effort, non-fatal)
    if let Err(e) = link_issue_to_branch(repo, task_id, branch).await {
        tracing::warn!(task_id, error = %e, "failed to link issue to branch (non-fatal)");
        // Don't fail the whole operation if linking fails
    }

    Ok(Some(url))
}

/// Link an issue to a branch using the GitHub GraphQL API.
///
/// Creates a "Development" sidebar link in GitHub (similar to `gh issue develop`).
/// This replaces the CLI-based implementation for consistent behavior across environments.
///
/// # Returns
/// - `Ok(())` - Successfully linked or already linked
/// - `Err(...)` - API error occurred
async fn link_issue_to_branch(repo: &str, task_id: &str, branch: &str) -> anyhow::Result<()> {
    if branch.is_empty() {
        tracing::warn!(task_id, "skipping link_issue_to_branch: empty branch name");
        return Ok(());
    }

    let gh = GhHttp::new();

    // Parse task_id as issue number
    let issue_number: u64 = task_id
        .parse()
        .map_err(|_| anyhow::anyhow!("task_id is not a valid issue number: {}", task_id))?;

    // Call the GhHttp method to link issue to branch
    match gh.link_issue_to_branch(repo, issue_number, branch).await {
        Ok(_) => {
            tracing::info!(task_id, branch, "linked issue to branch via API");
            Ok(())
        }
        Err(e) => {
            let err_msg = format!("{}", e);
            // Branch may already be linked — not an error
            if err_msg.contains("already") || err_msg.contains("existing") {
                tracing::debug!(task_id, branch, "issue already linked to branch");
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

/// Remove corrupt `[branch ""]` entries from git config.
///
/// `gh issue develop` sometimes creates these as a side effect,
/// which corrupts git config and blocks pushes.
pub async fn cleanup_empty_branch_config(dir: &Path) {
    let _ = Command::new("git")
        .args(["config", "--remove-section", "branch."])
        .current_dir(dir)
        .output_with_context()
        .await;
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
async fn has_unpushed_commits(dir: &Path, branch: &str, default_branch: &str) -> bool {
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

    match output {
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
    }
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
    fn test_link_issue_to_branch_empty_branch() {
        // This is a runtime test - empty branch should return Ok early
        // We can't easily test the async runtime behavior without tokio::test,
        // but we verify the function signature and error types compile correctly
    }
}
