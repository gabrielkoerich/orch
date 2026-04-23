//! Git worktree management for task execution.
//!
//! Each task runs in an isolated worktree to prevent conflicts.
//! Worktrees are stored at `~/.orch/worktrees/<project>/<branch>/`.

use crate::cmd::CommandErrorContext;
use crate::store;
use crate::store::TaskStore;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::process::Command;

/// Result of worktree setup.
pub struct WorktreeSetup {
    /// The directory where the agent will work
    pub work_dir: PathBuf,
    /// The branch name
    pub branch: String,
    /// The default branch of the repository
    pub default_branch: String,
    /// The main project directory (original, not worktree)
    pub main_project_dir: PathBuf,
}

/// Canonical project name used under `~/.orch/worktrees/`.
pub fn project_name(project_dir: &Path) -> String {
    project_dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string())
        .trim_end_matches(".git")
        .to_string()
}

/// Base worktrees directory for a project.
pub fn project_worktrees_dir(project_dir: &Path) -> PathBuf {
    crate::home::worktrees_dir()
        .unwrap_or_default()
        .join(project_name(project_dir))
}

/// List all worktree directories for a project.
pub async fn list_project_worktrees(project_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let base = project_worktrees_dir(project_dir);
    let mut worktrees = Vec::new();

    if tokio::fs::metadata(&base).await.is_err() {
        return Ok(worktrees);
    }

    let mut entries = tokio::fs::read_dir(&base).await?;
    while let Some(entry) = entries.next_entry().await? {
        if entry.metadata().await.map(|m| m.is_dir()).unwrap_or(false) {
            worktrees.push(entry.path());
        }
    }

    Ok(worktrees)
}

/// Extract a task ID from a worktree directory name.
pub fn task_id_from_worktree_name(name: &str) -> Option<String> {
    if let Some(rest) = name.strip_prefix("internal-") {
        let num = rest.split('-').next()?;
        return num.parse::<u64>().ok().map(|n| format!("internal:{n}"));
    }

    for prefix in ["gh-issue-", "gh-task-"] {
        if let Some(rest) = name.strip_prefix(prefix) {
            let task_part = rest.split('-').next()?;
            if task_part == "internal" {
                let num = rest.split('-').nth(1)?;
                return num.parse::<u64>().ok().map(|n| format!("internal:{n}"));
            }
            return Some(task_part.to_string());
        }
    }

    None
}

/// Abort any rebase in progress for a worktree.
pub async fn abort_worktree_rebase(worktree_dir: &Path) {
    let _ = Command::new("git")
        .args(["rebase", "--abort"])
        .current_dir(worktree_dir)
        .output_with_context()
        .await;
}

/// Rebase a worktree on top of `origin/{default_branch}`.
///
/// Stashes any uncommitted changes before rebasing to avoid failing the
/// rebase due to "You have unstaged changes".  The stash is restored after
/// the rebase succeeds; on failure the stash is preserved for manual recovery.
pub async fn rebase_worktree_on_origin_main(
    worktree_dir: &Path,
    default_branch: &str,
) -> anyhow::Result<()> {
    let origin_branch = format!("origin/{default_branch}");

    match crate::engine::runner::git_ops::stash_rebase_restore(
        worktree_dir,
        &origin_branch,
        crate::engine::runner::git_ops::StashRebaseOpts::default(),
    )
    .await?
    {
        crate::engine::runner::git_ops::RebaseOutcome::Succeeded => Ok(()),
        crate::engine::runner::git_ops::RebaseOutcome::Failed(e) => {
            anyhow::bail!("git rebase {} failed: {}", origin_branch, e)
        }
        crate::engine::runner::git_ops::RebaseOutcome::Skipped(_) => Ok(()),
    }
}

/// Generate a branch name from task ID and title.
///
/// Format: `internal-{id}-{slug}` for internal tasks, `gh-issue-{id}-{slug}` for external.
/// Slug is lowercase, non-alphanum→`-`, max 40 chars.
pub fn branch_name(task_id: &str, title: &str) -> String {
    let (prefix, sanitized_id) = if let Some(num) = task_id.strip_prefix("internal:") {
        ("internal", num.to_string())
    } else {
        ("gh-issue", sanitize_task_id(task_id))
    };

    let raw: String = title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();

    // Collapse consecutive dashes and trim
    let mut slug = String::with_capacity(raw.len());
    let mut prev_dash = false;
    for c in raw.chars() {
        if c == '-' {
            if !prev_dash {
                slug.push('-');
            }
            prev_dash = true;
        } else {
            slug.push(c);
            prev_dash = false;
        }
    }
    let slug = slug.trim_matches('-').to_string();

    // Truncate slug to 40 chars
    let slug = if slug.len() > 40 { &slug[..40] } else { &slug };
    let slug = slug.trim_end_matches('-');

    if slug.is_empty() {
        format!("{prefix}-{sanitized_id}")
    } else {
        format!("{prefix}-{sanitized_id}-{slug}")
    }
}

/// Replace characters in `task_id` that are not safe for branch/worktree names.
fn sanitize_task_id(task_id: &str) -> String {
    task_id.replace(':', "-")
}

/// Detect the default branch of a repository.
pub async fn detect_default_branch(project_dir: &Path) -> String {
    let output = Command::new("git")
        .args(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .current_dir(project_dir)
        .output_with_context()
        .await;

    match output {
        Ok(o) if o.status.success() => {
            let branch = String::from_utf8_lossy(&o.stdout).trim().to_string();
            // Handle both "remotes/origin/<branch>" and "origin/<branch>" formats
            let stripped = branch
                .strip_prefix("remotes/origin/")
                .or_else(|| branch.strip_prefix("origin/"))
                .unwrap_or(&branch)
                .trim();
            if stripped.is_empty() {
                "main".to_string()
            } else {
                stripped.to_string()
            }
        }
        _ => "main".to_string(),
    }
}

/// Resolve PROJECT_DIR to the main repo if it's inside a worktree.
///
/// This prevents nested worktrees when subtasks inherit a parent's worktree dir.
pub async fn resolve_main_repo(project_dir: &Path) -> PathBuf {
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(project_dir)
        .output_with_context()
        .await;

    if let Ok(o) = output {
        if o.status.success() {
            let stdout = String::from_utf8_lossy(&o.stdout);
            if let Some(first_line) = stdout.lines().next() {
                if let Some(path) = first_line.strip_prefix("worktree ") {
                    let main_path = PathBuf::from(path);
                    if main_path != project_dir {
                        tracing::info!(
                            worktree = %project_dir.display(),
                            main = %main_path.display(),
                            "resolved worktree to main repo"
                        );
                        return main_path;
                    }
                }
            }
        }
    }

    project_dir.to_path_buf()
}

/// Resolve the starting point for creating a new local branch.
///
/// If `origin/<branch>` exists in the repository (e.g. cleanup kept the
/// remote branch after deleting the local one), use it so the new worktree
/// starts from the agent's committed work rather than the default branch tip.
/// Falls back to `default_branch` when no remote tracking ref is found.
async fn resolve_branch_start_point(repo_root: &str, branch: &str, default_branch: &str) -> String {
    let origin_ref = format!("origin/{branch}");
    let has_remote = Command::new("git")
        .args(["-C", repo_root, "rev-parse", "--verify", &origin_ref])
        .output_with_context()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);
    if has_remote {
        origin_ref
    } else {
        default_branch.to_string()
    }
}

/// Validate that a worktree directory has a working gitdir link.
///
/// In a git worktree, `.git` is a file containing `gitdir: /path/to/.git/worktrees/<name>`.
/// Returns `false` when the `.git` file is missing or points to a non-existent gitdir,
/// indicating the worktree metadata is broken and the directory should be cleaned up.
pub async fn validate_worktree_gitdir(worktree_dir: &Path) -> bool {
    let git_file = worktree_dir.join(".git");
    // Use tokio::fs::metadata for async-safe existence check
    if tokio::fs::metadata(&git_file).await.is_err() {
        return false;
    }
    // In a linked worktree .git is a file; in the main repo it's a directory (always valid).
    if tokio::fs::metadata(&git_file)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        return true;
    }

    let content = match tokio::fs::read_to_string(&git_file).await {
        Ok(c) => c,
        Err(_) => return false,
    };
    // Expected format: "gitdir: /absolute/path/to/.git/worktrees/<name>\n"
    // Use spawn_blocking for the final exists check on the resolved path
    if let Some(gitdir_path) = content
        .lines()
        .next()
        .and_then(|l| l.strip_prefix("gitdir: "))
    {
        // Use async metadata for the gitdir check too.
        return tokio::fs::metadata(gitdir_path.trim())
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false);
    }
    false
}

/// Check if a directory is a bare git repository.
async fn is_bare_repo(dir: &Path) -> bool {
    let output = Command::new("git")
        .args([
            "-C",
            &dir.to_string_lossy(),
            "rev-parse",
            "--is-bare-repository",
        ])
        .output_with_context()
        .await;

    matches!(output, Ok(o) if o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true")
}

/// Resolve the branch and worktree directory from a parent task, if available.
///
/// When a child task is created from a mention on a PR, it should work on the
/// parent task's branch so changes are committed to the existing PR rather than
/// a new one being opened.
async fn resolve_parent_branch(
    store: &Option<Arc<TaskStore>>,
    parent_id: Option<i64>,
    worktrees_base: &Path,
) -> Option<(String, PathBuf)> {
    let parent_id = parent_id?;
    let parent = store::opt_store_get_task_by_id(store, parent_id).await?;
    if parent.branch.is_empty() {
        return None;
    }
    let wt = if !parent.worktree.is_empty() {
        // Use async metadata to avoid blocking the reactor thread.
        let exists = tokio::fs::metadata(&parent.worktree)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false);
        if exists {
            PathBuf::from(&parent.worktree)
        } else {
            worktrees_base.join(&parent.branch)
        }
    } else {
        worktrees_base.join(&parent.branch)
    };
    tracing::info!(
        parent_id,
        branch = %parent.branch,
        worktree = %wt.display(),
        "child task inheriting parent branch"
    );
    Some((parent.branch, wt))
}

/// Set up a worktree for task execution.
///
/// Returns the working directory, branch name, and default branch.
pub async fn setup_worktree(
    task_id: &str,
    title: &str,
    project_dir: &Path,
    store: &Option<Arc<TaskStore>>,
    repo: &str,
) -> anyhow::Result<WorktreeSetup> {
    // Resolve to main repo (avoid nested worktrees)
    let main_dir = resolve_main_repo(project_dir).await;
    let default_branch = detect_default_branch(&main_dir).await;

    let worktrees_base = project_worktrees_dir(&main_dir);
    tokio::fs::create_dir_all(&worktrees_base).await?;

    // Check if we have a saved branch/worktree in store
    let task_record = store::opt_store_get_task(store, repo, task_id).await;
    let (saved_branch, saved_worktree, parent_id) = task_record
        .as_ref()
        .map(|t| {
            (
                Some(t.branch.clone()),
                Some(t.worktree.clone()),
                t.parent_id,
            )
        })
        .unwrap_or((None, None, None));

    let (branch_name_str, worktree_dir) = if let Some(ref saved) = saved_branch {
        if !saved.is_empty() {
            // Use spawn_blocking for blocking Path::exists call in async context
            let wt = match &saved_worktree {
                Some(wt) if !wt.is_empty() => {
                    let wt_str = wt.clone();
                    let exists = tokio::task::spawn_blocking(move || Path::new(&wt_str).exists())
                        .await
                        .unwrap_or(false);
                    if exists {
                        PathBuf::from(wt)
                    } else {
                        worktrees_base.join(saved)
                    }
                }
                _ => worktrees_base.join(saved),
            };
            (saved.clone(), wt)
        } else if let Some((parent_branch, parent_wt)) =
            resolve_parent_branch(store, parent_id, &worktrees_base).await
        {
            // Child task with no saved branch: inherit parent's branch so we
            // commit onto the existing PR instead of opening a new one.
            (parent_branch, parent_wt)
        } else {
            let bn = branch_name(task_id, title);
            (bn.clone(), worktrees_base.join(&bn))
        }
    } else {
        // No store record — check parent branch, then existing worktree by prefix
        if let Some((parent_branch, parent_wt)) =
            resolve_parent_branch(store, parent_id, &worktrees_base).await
        {
            (parent_branch, parent_wt)
        } else {
            let existing = find_existing_worktree(&worktrees_base, task_id).await;
            if let Some(existing_dir) = existing {
                let bn = existing_dir
                    .file_name()
                    .filter(|n| !n.is_empty())
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| branch_name(task_id, title));
                tracing::info!(task_id, worktree = %existing_dir.display(), "found existing worktree");
                (bn, existing_dir)
            } else {
                let bn = branch_name(task_id, title);
                (bn.clone(), worktrees_base.join(&bn))
            }
        }
    };

    if branch_name_str.is_empty() {
        anyhow::bail!("empty branch name for task {task_id}");
    }

    // Abort any in-progress rebase in an existing worktree.
    //
    // Agents can leave a worktree stuck mid-rebase (e.g. after running
    // `git rebase -i` interactively).  The next dispatch attempt would
    // inherit that broken state, causing all subsequent git operations to
    // fail.  Aborting here is safe: if no rebase is in progress git exits
    // with a non-zero code which we silently ignore.
    // Use tokio::fs for async-safe existence check
    if tokio::fs::metadata(&worktree_dir).await.is_ok() {
        let _ = Command::new("git")
            .args(["rebase", "--abort"])
            .current_dir(&worktree_dir)
            .output_with_context()
            .await;

        // Check for a corrupted git index (e.g. "error: could not write index").
        // `git status` reads the index; a non-zero exit with index-related stderr
        // indicates corruption that will cause every subsequent git operation to
        // fail, leading to an infinite stuck-recovery loop.  When detected, nuke
        // the worktree and recreate it from scratch below.
        let index_ok = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&worktree_dir)
            .output_with_context()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false);

        if !index_ok {
            let stderr_hint = Command::new("git")
                .args(["status", "--porcelain"])
                .current_dir(&worktree_dir)
                .output_with_context()
                .await
                .map(|o| String::from_utf8_lossy(&o.stderr).trim().to_string())
                .unwrap_or_default();
            tracing::warn!(
                task_id,
                worktree = %worktree_dir.display(),
                stderr = %stderr_hint,
                "corrupted worktree index detected — removing and recreating worktree"
            );
            // Remove git worktree reference first, then delete the directory.
            let _ = Command::new("git")
                .args([
                    "-C",
                    &main_dir.to_string_lossy(),
                    "worktree",
                    "remove",
                    "--force",
                    &worktree_dir.to_string_lossy(),
                ])
                .output_with_context()
                .await;
            // If the directory still exists (e.g. worktree remove failed), remove it
            // manually so the worktree-creation block below re-creates it cleanly.
            if tokio::fs::metadata(&worktree_dir).await.is_ok() {
                let _ = tokio::fs::remove_dir_all(&worktree_dir).await;
            }
            // Prune stale worktree metadata so git won't complain on re-add.
            let _ = Command::new("git")
                .args(["-C", &main_dir.to_string_lossy(), "worktree", "prune"])
                .output_with_context()
                .await;
        }
    }

    // Create worktree if it doesn't exist
    if tokio::fs::metadata(&worktree_dir).await.is_err() {
        tracing::info!(task_id, worktree = %worktree_dir.display(), "creating worktree");

        // Pull/fetch latest so the new branch starts from up-to-date main.
        if is_bare_repo(&main_dir).await {
            let _ = Command::new("git")
                .args([
                    "-C",
                    &main_dir.to_string_lossy(),
                    "fetch",
                    "--all",
                    "--prune",
                ])
                .output_with_context()
                .await;
        } else {
            let _ = Command::new("git")
                .args(["-C", &main_dir.to_string_lossy(), "pull", "--ff-only"])
                .output_with_context()
                .await;
        }

        // Create local branch. Prefer `origin/<branch>` when it already exists
        // (recovery case: cleanup deleted local branch but kept the remote),
        // so the new worktree starts from the agent's committed work instead
        // of the default branch tip.
        let start_point = resolve_branch_start_point(
            &main_dir.to_string_lossy(),
            &branch_name_str,
            &default_branch,
        )
        .await;
        let _ = Command::new("git")
            .args([
                "-C",
                &main_dir.to_string_lossy(),
                "branch",
                &branch_name_str,
                &start_point,
            ])
            .output_with_context()
            .await;

        // Create worktree
        if let Some(parent) = worktree_dir.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let output = Command::new("git")
            .args([
                "-C",
                &main_dir.to_string_lossy(),
                "worktree",
                "add",
                &worktree_dir.to_string_lossy(),
                &branch_name_str,
            ])
            .output_with_context()
            .await?;

        if !output.status.success() && tokio::fs::metadata(&worktree_dir).await.is_err() {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            // Retry: prune and recreate
            tracing::warn!(
                task_id,
                stdout = %stdout,
                stderr = %stderr,
                "worktree creation failed, retrying after prune"
            );

            let _ = Command::new("git")
                .args(["-C", &main_dir.to_string_lossy(), "worktree", "prune"])
                .output_with_context()
                .await;

            let _ = Command::new("git")
                .args([
                    "-C",
                    &main_dir.to_string_lossy(),
                    "branch",
                    "-D",
                    &branch_name_str,
                ])
                .output_with_context()
                .await;

            let _ = Command::new("git")
                .args([
                    "-C",
                    &main_dir.to_string_lossy(),
                    "branch",
                    &branch_name_str,
                    &start_point,
                ])
                .output_with_context()
                .await;

            let retry_output = Command::new("git")
                .args([
                    "-C",
                    &main_dir.to_string_lossy(),
                    "worktree",
                    "add",
                    &worktree_dir.to_string_lossy(),
                    &branch_name_str,
                ])
                .output_with_context()
                .await;

            let mut retry_stdout = String::new();
            let mut retry_stderr = String::new();
            let mut retry_error = String::new();
            match retry_output {
                Ok(output) => {
                    retry_stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    retry_stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                    tracing::warn!(
                        task_id,
                        stdout = %retry_stdout,
                        stderr = %retry_stderr,
                        "worktree creation retry failed"
                    );
                }
                Err(err) => {
                    retry_error = err.to_string();
                    tracing::warn!(
                        task_id,
                        error = %retry_error,
                        "worktree creation retry failed to run"
                    );
                }
            }

            if tokio::fs::metadata(&worktree_dir).await.is_err() {
                anyhow::bail!(
                    "failed to create worktree at {} for task {} (stdout: {}, stderr: {}, retry stdout: {}, retry stderr: {}, retry error: {})",
                    worktree_dir.display(),
                    task_id,
                    stdout,
                    stderr,
                    retry_stdout,
                    retry_stderr,
                    retry_error
                );
            }
        }
    }

    // Link issue to branch via GitHub API (Development sidebar).
    // Must happen before the agent pushes — createLinkedBranch only works
    // for branches that don't yet exist on GitHub.
    if !task_id.starts_with("internal:") {
        if let Ok(issue_num) = task_id.parse::<u64>() {
            if let Ok(gh) = crate::github::http::GhHttp::new() {
                match gh
                    .link_issue_to_branch(repo, issue_num, &branch_name_str)
                    .await
                {
                    Ok(id) => {
                        tracing::info!(task_id, branch = %branch_name_str, linked = %id, "linked issue to branch");
                    }
                    Err(e) => {
                        tracing::debug!(task_id, error = %e, "failed to link issue to branch (non-fatal)");
                    }
                }
            }
        }
    }

    // Save worktree info to store
    if let Err(e) = store::store_set_result(
        store,
        repo,
        task_id,
        &[
            (
                "worktree",
                serde_json::json!(worktree_dir.display().to_string()),
            ),
            ("branch", serde_json::json!(branch_name_str)),
        ],
    )
    .await
    {
        tracing::warn!(task_id, err = %e, "failed to save worktree info to store (non-fatal)");
    }

    tracing::info!(
        task_id,
        worktree = %worktree_dir.display(),
        branch = %branch_name_str,
        "worktree ready"
    );

    Ok(WorktreeSetup {
        work_dir: worktree_dir,
        branch: branch_name_str,
        default_branch,
        main_project_dir: main_dir,
    })
}

/// Find an existing worktree by task ID prefix.
async fn find_existing_worktree(worktrees_base: &Path, task_id: &str) -> Option<PathBuf> {
    let (prefix_new, legacy_prefixes) = if let Some(num) = task_id.strip_prefix("internal:") {
        (
            format!("internal-{num}-"),
            vec![
                format!("gh-task-internal-{num}-"),
                format!("gh-issue-internal-{num}-"),
            ],
        )
    } else {
        let sanitized_id = sanitize_task_id(task_id);
        (
            format!("gh-issue-{sanitized_id}-"),
            vec![format!("gh-task-{sanitized_id}-")],
        )
    };

    let mut entries = match tokio::fs::read_dir(worktrees_base).await {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %worktrees_base.display(),
                "failed to read worktrees directory while searching for existing worktree"
            );
            return None;
        }
    };
    loop {
        let next_entry = match entries.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(e) => {
                // Log the error but continue scanning - a single entry error shouldn't stop us
                tracing::warn!(error = %e, "failed to read directory entry while searching for worktree");
                continue;
            }
        };

        let name = next_entry.file_name().to_string_lossy().to_string();
        let matches =
            name.starts_with(&prefix_new) || legacy_prefixes.iter().any(|p| name.starts_with(p));
        if matches {
            match next_entry.metadata().await {
                Ok(metadata) if metadata.is_dir() => return Some(next_entry.path()),
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        path = %next_entry.path().display(),
                        "failed to read metadata for candidate worktree entry"
                    );
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: run a git command in `dir`, panicking on failure.
    fn git(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap_or_else(|e| panic!("git {:?} failed to start: {e}", args));
        if !out.status.success() {
            panic!(
                "git {:?} failed:\nstdout: {}\nstderr: {}",
                args,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    /// When `origin/<branch>` exists, `resolve_branch_start_point` should
    /// return `"origin/<branch>"` so that the worktree starts from the
    /// already-committed work, not from the default branch tip.
    #[tokio::test]
    async fn resolve_branch_start_point_prefers_remote_over_default() {
        let remote = tempfile::tempdir().unwrap();
        git(remote.path(), &["init", "--bare"]);

        // Set up a working clone to push branches to the remote.
        let setup = tempfile::tempdir().unwrap();
        git(
            setup.path(),
            &["clone", remote.path().to_str().unwrap(), "."],
        );
        git(setup.path(), &["config", "user.email", "t@t.com"]);
        git(setup.path(), &["config", "user.name", "T"]);

        // Initial commit on main.
        std::fs::write(setup.path().join("README.md"), "main").unwrap();
        git(setup.path(), &["add", "."]);
        git(setup.path(), &["commit", "-m", "init"]);
        git(setup.path(), &["push", "origin", "HEAD:main"]);

        // Feature branch with a distinct commit.
        git(
            setup.path(),
            &["checkout", "-b", "gh-issue-42-fix-login-bug"],
        );
        std::fs::write(setup.path().join("feature.txt"), "work").unwrap();
        git(setup.path(), &["add", "."]);
        git(setup.path(), &["commit", "-m", "feature work"]);
        git(
            setup.path(),
            &["push", "origin", "gh-issue-42-fix-login-bug"],
        );

        // Local repo: clone + fetch, but NO local branch (simulates cleanup
        // with keep_remote_branch=true where local branch was deleted).
        let local = tempfile::tempdir().unwrap();
        git(
            local.path(),
            &["clone", remote.path().to_str().unwrap(), "."],
        );
        git(local.path(), &["config", "user.email", "t@t.com"]);
        git(local.path(), &["config", "user.name", "T"]);
        git(local.path(), &["fetch", "--all"]);
        // Confirm the local branch does NOT exist; only the remote tracking ref.
        let has_local = std::process::Command::new("git")
            .args([
                "-C",
                local.path().to_str().unwrap(),
                "rev-parse",
                "--verify",
                "gh-issue-42-fix-login-bug",
            ])
            .output()
            .unwrap()
            .status
            .success();
        assert!(!has_local, "local branch should not exist before the test");

        let repo_root = local.path().to_str().unwrap();

        // With remote branch present → should prefer origin/<branch>.
        let start =
            resolve_branch_start_point(repo_root, "gh-issue-42-fix-login-bug", "main").await;
        assert_eq!(
            start, "origin/gh-issue-42-fix-login-bug",
            "should use remote branch as start point when origin/<branch> exists"
        );
    }

    /// When `origin/<branch>` does NOT exist, `resolve_branch_start_point`
    /// should fall back to the default branch.
    #[tokio::test]
    async fn resolve_branch_start_point_falls_back_to_default() {
        let remote = tempfile::tempdir().unwrap();
        git(remote.path(), &["init", "--bare"]);

        let local = tempfile::tempdir().unwrap();
        git(
            local.path(),
            &["clone", remote.path().to_str().unwrap(), "."],
        );
        git(local.path(), &["config", "user.email", "t@t.com"]);
        git(local.path(), &["config", "user.name", "T"]);

        // Commit something so origin/main exists.
        std::fs::write(local.path().join("README.md"), "main").unwrap();
        git(local.path(), &["add", "."]);
        git(local.path(), &["commit", "-m", "init"]);
        git(local.path(), &["push", "origin", "HEAD:main"]);
        git(local.path(), &["fetch", "--all"]);

        let repo_root = local.path().to_str().unwrap();

        // Branch "no-such-branch" has never been pushed → should fall back.
        let start = resolve_branch_start_point(repo_root, "no-such-branch", "main").await;
        assert_eq!(
            start, "main",
            "should fall back to default_branch when remote does not exist"
        );
    }

    #[test]
    fn branch_name_basic() {
        let name = branch_name("42", "Fix login bug");
        assert_eq!(name, "gh-issue-42-fix-login-bug");
    }

    #[test]
    fn branch_name_internal_task() {
        let name = branch_name("internal:8", "Fix login bug");
        assert_eq!(name, "internal-8-fix-login-bug");
    }

    #[test]
    fn branch_name_special_chars() {
        let name = branch_name("7", "Add OAuth2/OIDC (Google & GitHub)");
        assert_eq!(name, "gh-issue-7-add-oauth2-oidc-google-github");
    }

    #[test]
    fn branch_name_truncates_long_slug() {
        let title =
            "This is a very long task title that should be truncated to forty characters maximum";
        let name = branch_name("1", title);
        // slug part should be max 40 chars
        let slug = name.strip_prefix("gh-issue-1-").unwrap();
        assert!(slug.len() <= 40, "slug length {} > 40", slug.len());
    }

    #[test]
    fn branch_name_trims_trailing_dashes() {
        let name = branch_name("5", "Fix bug---");
        assert!(
            !name.ends_with('-'),
            "branch name should not end with dash: {name}"
        );
    }

    #[test]
    fn branch_name_empty_title() {
        let name = branch_name("99", "");
        assert_eq!(name, "gh-issue-99");
    }

    #[test]
    fn branch_name_empty_title_internal() {
        let name = branch_name("internal:99", "");
        assert_eq!(name, "internal-99");
    }

    #[test]
    fn branch_name_all_special_chars() {
        let name = branch_name("10", "--- ??? ---");
        assert_eq!(name, "gh-issue-10");
        assert!(!name.is_empty());
    }

    #[test]
    fn branch_name_chinese_chars_no_panic() {
        let name = branch_name("265", "implement 用户认证 user auth");
        assert!(name.starts_with("gh-issue-265-"));
        assert!(!name.is_empty());
        let slug = name.strip_prefix("gh-issue-265-").unwrap();
        assert!(slug.len() <= 40, "slug too long: {}", slug.len());
        assert!(slug.is_ascii(), "slug contains non-ASCII: {slug}");
    }

    #[test]
    fn task_id_from_worktree_name_parses_issue_branches() {
        assert_eq!(
            task_id_from_worktree_name("gh-issue-42-fix-login"),
            Some("42".to_string())
        );
        assert_eq!(
            task_id_from_worktree_name("gh-task-42-fix-login"),
            Some("42".to_string())
        );
    }

    #[test]
    fn task_id_from_worktree_name_parses_internal_branches() {
        assert_eq!(
            task_id_from_worktree_name("internal-8-fix-login"),
            Some("internal:8".to_string())
        );
        assert_eq!(
            task_id_from_worktree_name("gh-issue-internal-8-fix-login"),
            Some("internal:8".to_string())
        );
    }

    #[test]
    fn task_id_from_worktree_name_returns_none_for_unknown_names() {
        assert!(task_id_from_worktree_name("random-worktree").is_none());
    }

    #[test]
    fn branch_name_emoji_no_panic() {
        let name = branch_name("265", "fix 🚀 deployment pipeline 🔥");
        assert!(name.starts_with("gh-issue-265-"));
        let slug = name.strip_prefix("gh-issue-265-").unwrap();
        assert!(slug.is_ascii(), "slug contains non-ASCII: {slug}");
    }

    #[test]
    fn branch_name_all_non_ascii_falls_back_to_task_id() {
        let name = branch_name("265", "用户认证 실행 тест");
        assert_eq!(name, "gh-issue-265");
    }

    #[tokio::test]
    async fn find_existing_worktree_returns_none_for_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(find_existing_worktree(dir.path(), "42").await.is_none());
    }

    #[tokio::test]
    async fn find_existing_worktree_matches_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let wt_dir = dir.path().join("gh-issue-42-fix-login-bug");
        tokio::fs::create_dir(&wt_dir).await.unwrap();

        let result = find_existing_worktree(dir.path(), "42").await;
        assert_eq!(result, Some(wt_dir));
    }

    #[tokio::test]
    async fn find_existing_worktree_matches_internal_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let wt_dir = dir.path().join("internal-8-fix");
        tokio::fs::create_dir(&wt_dir).await.unwrap();

        let result = find_existing_worktree(dir.path(), "internal:8").await;
        assert_eq!(result, Some(wt_dir));
    }

    #[tokio::test]
    async fn find_existing_worktree_matches_legacy_gh_task_prefix() {
        // Old worktrees created before the rename should still be found
        let dir = tempfile::tempdir().unwrap();
        let wt_dir = dir.path().join("gh-task-internal-8-fix");
        tokio::fs::create_dir(&wt_dir).await.unwrap();

        let result = find_existing_worktree(dir.path(), "internal:8").await;
        assert_eq!(result, Some(wt_dir));
    }

    #[tokio::test]
    async fn find_existing_worktree_matches_legacy_external_prefix() {
        // Old external worktrees with gh-task- prefix should still be found
        let dir = tempfile::tempdir().unwrap();
        let wt_dir = dir.path().join("gh-task-42-fix-login-bug");
        tokio::fs::create_dir(&wt_dir).await.unwrap();

        let result = find_existing_worktree(dir.path(), "42").await;
        assert_eq!(result, Some(wt_dir));
    }

    #[tokio::test]
    async fn find_existing_worktree_ignores_other_tasks() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::create_dir(dir.path().join("gh-issue-43-other-task"))
            .await
            .unwrap();

        assert!(find_existing_worktree(dir.path(), "42").await.is_none());
    }

    // --- resolve_parent_branch tests ---

    /// No store → always returns None.
    #[tokio::test]
    async fn resolve_parent_branch_no_store_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let result = resolve_parent_branch(&None, Some(1), dir.path()).await;
        assert!(result.is_none());
    }

    /// parent_id = None → always returns None regardless of store.
    #[tokio::test]
    async fn resolve_parent_branch_no_parent_id_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let result = resolve_parent_branch(&None, None, dir.path()).await;
        assert!(result.is_none());
    }

    /// Parent exists but has empty branch → returns None so child gets its own branch.
    #[tokio::test]
    async fn resolve_parent_branch_empty_parent_branch_returns_none() {
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("test.db");
        let store = Arc::new(
            crate::store::TaskStore::open_single(&db_path)
                .await
                .unwrap(),
        );

        // Create parent task — branch defaults to empty string
        let parent_id = store
            .create_internal("owner/repo", "parent task", "", "issue", "99", None)
            .await
            .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let store_opt = Some(store);
        let result = resolve_parent_branch(&store_opt, Some(parent_id), dir.path()).await;
        assert!(result.is_none());
    }

    /// Parent has a branch set → child inherits that branch and worktree path.
    #[tokio::test]
    async fn resolve_parent_branch_returns_parent_branch_and_worktree() {
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("test.db");
        let store = Arc::new(
            crate::store::TaskStore::open_single(&db_path)
                .await
                .unwrap(),
        );

        // Create parent task and set its branch
        let parent_id = store
            .create_internal("owner/repo", "parent task", "", "issue", "99", None)
            .await
            .unwrap();
        store
            .set_fields(
                parent_id,
                &[("branch", serde_json::json!("gh-issue-99-parent-task"))],
            )
            .await
            .unwrap();

        let worktrees_base = tempfile::tempdir().unwrap();
        let store_opt = Some(store);
        let result =
            resolve_parent_branch(&store_opt, Some(parent_id), worktrees_base.path()).await;

        let (branch, wt) = result.expect("should return parent branch");
        assert_eq!(branch, "gh-issue-99-parent-task");
        assert_eq!(wt, worktrees_base.path().join("gh-issue-99-parent-task"));
    }

    /// When parent has both branch and a worktree path that exists, use the worktree path.
    #[tokio::test]
    async fn resolve_parent_branch_prefers_existing_worktree_path() {
        let db_dir = tempfile::tempdir().unwrap();
        let db_path = db_dir.path().join("test.db");
        let store = Arc::new(
            crate::store::TaskStore::open_single(&db_path)
                .await
                .unwrap(),
        );

        let worktrees_base = tempfile::tempdir().unwrap();
        // Create the parent worktree directory
        let parent_wt_dir = worktrees_base.path().join("custom-worktree-path");
        tokio::fs::create_dir_all(&parent_wt_dir).await.unwrap();

        let parent_id = store
            .create_internal("owner/repo", "parent task", "", "issue", "99", None)
            .await
            .unwrap();
        store
            .set_fields(
                parent_id,
                &[
                    ("branch", serde_json::json!("gh-issue-99-parent-task")),
                    (
                        "worktree",
                        serde_json::json!(parent_wt_dir.to_string_lossy().as_ref()),
                    ),
                ],
            )
            .await
            .unwrap();

        let store_opt = Some(store);
        let result =
            resolve_parent_branch(&store_opt, Some(parent_id), worktrees_base.path()).await;

        let (branch, wt) = result.expect("should return parent branch");
        assert_eq!(branch, "gh-issue-99-parent-task");
        assert_eq!(wt, parent_wt_dir);
    }

    /// Regression test: concurrent calls to list_project_worktrees and
    /// validate_worktree_gitdir should not starve the Tokio worker thread
    /// pool. This was broken by blocking Path::exists/is_dir calls in async
    /// functions (issue #2467).
    #[tokio::test]
    async fn concurrent_worktree_checks_no_starvation() {
        let temp_dir = tempfile::tempdir().unwrap();
        let worktrees_base = temp_dir.path().join("worktrees");
        tokio::fs::create_dir_all(&worktrees_base).await.unwrap();

        // Create a fake worktree with valid gitdir
        let wt_dir = worktrees_base.join("gh-issue-123-test");
        tokio::fs::create_dir_all(&wt_dir).await.unwrap();
        // Create .git as a file pointing to a real directory (worktree link)
        let gitdir_target = temp_dir
            .path()
            .join(".git")
            .join("worktrees")
            .join("gh-issue-123-test");
        tokio::fs::create_dir_all(&gitdir_target).await.unwrap();
        let git_file = wt_dir.join(".git");
        tokio::fs::write(&git_file, format!("gitdir: {}\n", gitdir_target.display()))
            .await
            .unwrap();

        // Spawn many concurrent tasks - if any use blocking calls, they'll
        // block the worker thread and cause timeout or deadlock.
        let handles: Vec<_> = (0..20)
            .map(|i| {
                let wb = worktrees_base.clone();
                let wd = wt_dir.clone();
                tokio::spawn(async move {
                    if i % 2 == 0 {
                        list_project_worktrees(&wb).await.ok();
                    } else {
                        validate_worktree_gitdir(&wd).await;
                    }
                })
            })
            .collect();

        // Wait with timeout to detect starvation
        let results = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            futures::future::join_all(handles).await
        })
        .await;

        assert!(
            results.is_ok(),
            "concurrent worktree checks timed out (starvation?)"
        );
    }
}
