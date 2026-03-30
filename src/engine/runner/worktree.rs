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
pub fn list_project_worktrees(project_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let base = project_worktrees_dir(project_dir);
    let mut worktrees = Vec::new();

    if !base.exists() {
        return Ok(worktrees);
    }

    for entry in std::fs::read_dir(&base)? {
        let entry = entry?;
        if entry.path().is_dir() {
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
/// rebase due to "You have unstaged changes".  The stash is popped after
/// the rebase succeeds; if the pop fails the stash is left on the stack
/// for manual recovery rather than destroying the worktree.
pub async fn rebase_worktree_on_origin_main(
    worktree_dir: &Path,
    default_branch: &str,
) -> anyhow::Result<()> {
    let worktree_str = worktree_dir.to_string_lossy();
    let origin_branch = format!("origin/{default_branch}");

    // Stash uncommitted changes before rebasing. Unlike the older naive
    // approach we capture the exact stash ref (OID) so concurrent worktrees
    // cannot interfere by popping the wrong stash. If the stash command
    // fails we log the stderr and SKIP the startup rebase to avoid
    // destroying a worktree due to an un-stashed dirty state.
    let stash_ref: Option<String> = if crate::engine::runner::git_ops::has_changes(worktree_dir)
        .await
    {
        let stash_out = Command::new("git")
            .args([
                "-C",
                &worktree_str,
                "stash",
                "push",
                "--include-untracked",
                "-m",
                "orch-startup-rebase",
            ])
            .output_with_context()
            .await;

        match stash_out {
            Ok(o) if o.status.success() => {
                // Log the stash push stdout for diagnostics (helps debug index.lock / refs/stash.lock races).
                let stdout_preview = String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string();
                tracing::debug!(worktree = %worktree_str, stdout = %stdout_preview, "git stash push succeeded");

                // Capture the OID of the stash object just created so we can
                // apply it back by that ref, regardless of most concurrent
                // operations. We use `refs/stash@{0}` to explicitly resolve the
                // newest stash entry. There is still a tiny race if another
                // process pushes a stash between the push and the rev-parse;
                // parsing `git stash push` output for the created OID would be
                // ideal, but `git stash push` does not reliably emit the OID
                // across git versions. This approach reduces the window and
                // is an acceptable pragmatic improvement.
                let ref_out = Command::new("git")
                    .args(["-C", &worktree_str, "rev-parse", "refs/stash@{0}"])
                    .output_with_context()
                    .await;
                ref_out
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .filter(|s| !s.is_empty())
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
                tracing::warn!(worktree = %worktree_str, code = ?o.status.code(), stderr = %stderr, "git stash push failed during startup rebase — skipping rebase to avoid worktree loss");
                // Skip rebase when stash fails: leave dirty worktree intact.
                return Ok(());
            }
            Err(e) => {
                tracing::warn!(worktree = %worktree_str, error = %e, "failed to run git stash push during startup rebase — skipping rebase to avoid worktree loss");
                return Ok(());
            }
        }
    } else {
        None
    };

    let rebase = Command::new("git")
        .args(["-C", &worktree_str, "rebase", &origin_branch])
        .output_with_context()
        .await?;

    if !rebase.status.success() {
        anyhow::bail!(
            "git rebase {} failed: {}",
            origin_branch,
            String::from_utf8_lossy(&rebase.stderr).trim()
        );
    }

    // Restore stashed changes using the captured ref so we never accidentally
    // pop a stash that belongs to a different concurrently-running worktree.
    if let Some(ref stash_hash) = stash_ref {
        let apply = Command::new("git")
            .args(["-C", &worktree_str, "stash", "apply", stash_hash])
            .output_with_context()
            .await;
        if apply.map(|o| o.status.success()).unwrap_or(false) {
            let _ = Command::new("git")
                .args(["-C", &worktree_str, "stash", "drop", stash_hash])
                .output_with_context()
                .await;
        } else {
            tracing::warn!(stash = %stash_hash, worktree = %worktree_str, "stash apply failed after rebase — stash preserved for manual recovery");
        }
    }

    Ok(())
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
            // strip "origin/" prefix if present
            branch
                .strip_prefix("origin/")
                .unwrap_or(&branch)
                .to_string()
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
    std::fs::create_dir_all(&worktrees_base)?;

    // Check if we have a saved branch/worktree in store
    let (saved_branch, saved_worktree) = store::opt_store_get_task(store, repo, task_id)
        .await
        .map(|t| (Some(t.branch), Some(t.worktree)))
        .unwrap_or((None, None));

    let (branch_name_str, worktree_dir) = if let Some(ref saved) = saved_branch {
        if !saved.is_empty() {
            let wt = match &saved_worktree {
                Some(wt) if !wt.is_empty() && Path::new(wt).exists() => PathBuf::from(wt),
                _ => worktrees_base.join(saved),
            };
            (saved.clone(), wt)
        } else {
            let bn = branch_name(task_id, title);
            (bn.clone(), worktrees_base.join(&bn))
        }
    } else {
        // Check for existing worktree by prefix pattern
        let existing = find_existing_worktree(&worktrees_base, task_id);
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
    if worktree_dir.exists() {
        let _ = Command::new("git")
            .args(["rebase", "--abort"])
            .current_dir(&worktree_dir)
            .output_with_context()
            .await;
    }

    // Create worktree if it doesn't exist
    if !worktree_dir.exists() {
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
            std::fs::create_dir_all(parent)?;
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

        if !output.status.success() && !worktree_dir.exists() {
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

            if !worktree_dir.exists() {
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
    store::store_set(
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
    .await;

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
fn find_existing_worktree(worktrees_base: &Path, task_id: &str) -> Option<PathBuf> {
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

    if let Ok(entries) = std::fs::read_dir(worktrees_base) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let matches = name.starts_with(&prefix_new)
                || legacy_prefixes.iter().any(|p| name.starts_with(p));
            if matches && entry.path().is_dir() {
                return Some(entry.path());
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

    #[test]
    fn find_existing_worktree_returns_none_for_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(find_existing_worktree(dir.path(), "42").is_none());
    }

    #[test]
    fn find_existing_worktree_matches_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let wt_dir = dir.path().join("gh-issue-42-fix-login-bug");
        std::fs::create_dir(&wt_dir).unwrap();

        let result = find_existing_worktree(dir.path(), "42");
        assert_eq!(result, Some(wt_dir));
    }

    #[test]
    fn find_existing_worktree_matches_internal_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let wt_dir = dir.path().join("internal-8-fix");
        std::fs::create_dir(&wt_dir).unwrap();

        let result = find_existing_worktree(dir.path(), "internal:8");
        assert_eq!(result, Some(wt_dir));
    }

    #[test]
    fn find_existing_worktree_matches_legacy_gh_task_prefix() {
        // Old worktrees created before the rename should still be found
        let dir = tempfile::tempdir().unwrap();
        let wt_dir = dir.path().join("gh-task-internal-8-fix");
        std::fs::create_dir(&wt_dir).unwrap();

        let result = find_existing_worktree(dir.path(), "internal:8");
        assert_eq!(result, Some(wt_dir));
    }

    #[test]
    fn find_existing_worktree_matches_legacy_external_prefix() {
        // Old external worktrees with gh-task- prefix should still be found
        let dir = tempfile::tempdir().unwrap();
        let wt_dir = dir.path().join("gh-task-42-fix-login-bug");
        std::fs::create_dir(&wt_dir).unwrap();

        let result = find_existing_worktree(dir.path(), "42");
        assert_eq!(result, Some(wt_dir));
    }

    #[test]
    fn find_existing_worktree_ignores_other_tasks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("gh-issue-43-other-task")).unwrap();

        assert!(find_existing_worktree(dir.path(), "42").is_none());
    }
}
