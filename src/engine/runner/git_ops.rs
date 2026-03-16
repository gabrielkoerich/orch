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
fn build_git_auth_args() -> Vec<String> {
    let token = crate::github::token::shared()
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

    let auth_args = build_git_auth_args();
    let push_args: Vec<&str> = auth_args
        .iter()
        .map(String::as_str)
        .chain(["push", "-u", "origin", branch_to_push])
        .collect();

    let output = Command::new("git")
        .args(&push_args)
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
                // Retry push after rebase with the same auth args
                let retry_push_args: Vec<&str> = auth_args
                    .iter()
                    .map(String::as_str)
                    .chain(["push", "-u", "origin", branch_to_push])
                    .collect();
                let retry = Command::new("git")
                    .args(&retry_push_args)
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
    model: Option<&str>,
    repo: &str,
    base_branch: &str,
) -> PrCreateResult<Option<String>> {
    let gh = GhHttp::new()?;

    // Check if PR already exists using GhHttp API
    match gh.get_pr_number(repo, branch).await {
        Ok(Some(pr_number)) => {
            tracing::info!(task_id, pr = pr_number, "PR already exists");
            // Append attribution footer if the agent created the PR without one.
            append_pr_footer_if_missing(&gh, repo, pr_number, task_id, agent, model).await;
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
    let url = gh
        .create_pr(repo, pr_title, &body, branch, base_branch)
        .await
        .map_err(PrCreateError::ApiError)?;

    tracing::info!(task_id, pr_url = %url, "created PR via GhHttp API");

    Ok(Some(url))
}

/// Append the Orch attribution footer to an existing PR body if not already present.
///
/// Called when the agent created the PR directly (bypassing the orchestrator's
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
}
