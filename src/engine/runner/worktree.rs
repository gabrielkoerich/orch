//! Git worktree management for task execution.
//!
//! Each task runs in an isolated worktree to prevent conflicts.
//! Worktrees are stored at `~/.orch/worktrees/<project>/<branch>/`.

use crate::cmd::CommandErrorContext;
use crate::sidecar;
use std::path::{Path, PathBuf};
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

/// Generate a branch name from task ID and title.
///
/// Format: `gh-task-{issue}-{slug}` where slug is lowercase, non-alphanum→`-`, max 40 chars.
pub fn branch_name(task_id: &str, title: &str) -> String {
    let task_id = sanitize_task_id(task_id);

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
        format!("gh-task-{task_id}")
    } else {
        format!("gh-task-{task_id}-{slug}")
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
) -> anyhow::Result<WorktreeSetup> {
    // Resolve to main repo (avoid nested worktrees)
    let main_dir = resolve_main_repo(project_dir).await;
    let default_branch = detect_default_branch(&main_dir).await;

    let project_name = main_dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string())
        .trim_end_matches(".git")
        .to_string();

    let worktrees_base = crate::home::worktrees_dir()
        .unwrap_or_default()
        .join(&project_name);
    std::fs::create_dir_all(&worktrees_base)?;

    // Check if we have a saved branch/worktree in sidecar
    let saved_branch = sidecar::get(task_id, "branch").ok();
    let saved_worktree = sidecar::get(task_id, "worktree").ok();

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

        // Fetch if bare repo
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
        }

        // Create branch from default
        let _ = Command::new("git")
            .args([
                "-C",
                &main_dir.to_string_lossy(),
                "branch",
                &branch_name_str,
                &default_branch,
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
            // Retry: prune and recreate
            tracing::warn!(task_id, "worktree creation failed, retrying after prune");

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
                    &default_branch,
                ])
                .output_with_context()
                .await;

            let _ = Command::new("git")
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

            if !worktree_dir.exists() {
                anyhow::bail!(
                    "failed to create worktree at {} for task {}",
                    worktree_dir.display(),
                    task_id
                );
            }
        }
    }

    // Save worktree info to sidecar
    sidecar::set(
        task_id,
        &[
            format!("worktree={}", worktree_dir.display()),
            format!("branch={branch_name_str}"),
        ],
    )?;

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
    let sanitized_id = sanitize_task_id(task_id);
    let prefix = format!("gh-task-{sanitized_id}-");

    if let Ok(entries) = std::fs::read_dir(worktrees_base) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(&prefix) && entry.path().is_dir() {
                return Some(entry.path());
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_name_basic() {
        let name = branch_name("42", "Fix login bug");
        assert_eq!(name, "gh-task-42-fix-login-bug");
    }

    #[test]
    fn branch_name_special_chars() {
        let name = branch_name("7", "Add OAuth2/OIDC (Google & GitHub)");
        assert_eq!(name, "gh-task-7-add-oauth2-oidc-google-github");
    }

    #[test]
    fn branch_name_truncates_long_slug() {
        let title =
            "This is a very long task title that should be truncated to forty characters maximum";
        let name = branch_name("1", title);
        // slug part should be max 40 chars
        let slug = name.strip_prefix("gh-task-1-").unwrap();
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
        assert_eq!(name, "gh-task-99");
    }

    #[test]
    fn branch_name_all_special_chars() {
        // All non-alphanumeric → empty slug → falls back to task-id only.
        // Ensures we never produce an empty branch name that would write
        // a corrupt `[branch ""]` entry to .git/config via gh issue develop.
        let name = branch_name("10", "--- ??? ---");
        assert_eq!(name, "gh-task-10");
        assert!(!name.is_empty());
    }

    #[test]
    fn branch_name_chinese_chars_no_panic() {
        // Non-ASCII alphanumeric chars must not be kept in the slug — byte-offset
        // truncation at &slug[..40] would panic if a multi-byte char straddles byte 40.
        let name = branch_name("265", "implement 用户认证 user auth");
        assert!(name.starts_with("gh-task-265-"));
        assert!(!name.is_empty());
        let slug = name.strip_prefix("gh-task-265-").unwrap();
        assert!(slug.len() <= 40, "slug too long: {}", slug.len());
        assert!(slug.is_ascii(), "slug contains non-ASCII: {slug}");
    }

    #[test]
    fn branch_name_emoji_no_panic() {
        let name = branch_name("265", "fix 🚀 deployment pipeline 🔥");
        assert!(name.starts_with("gh-task-265-"));
        let slug = name.strip_prefix("gh-task-265-").unwrap();
        assert!(slug.is_ascii(), "slug contains non-ASCII: {slug}");
    }

    #[test]
    fn branch_name_all_non_ascii_falls_back_to_task_id() {
        // Title with only non-ASCII chars → empty slug → task-id only fallback
        let name = branch_name("265", "用户认证 실행 тест");
        assert_eq!(name, "gh-task-265");
    }

    #[test]
    fn find_existing_worktree_returns_none_for_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(find_existing_worktree(dir.path(), "42").is_none());
    }

    #[test]
    fn find_existing_worktree_matches_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let wt_dir = dir.path().join("gh-task-42-fix-login-bug");
        std::fs::create_dir(&wt_dir).unwrap();

        let result = find_existing_worktree(dir.path(), "42");
        assert_eq!(result, Some(wt_dir));
    }

    #[test]
    fn find_existing_worktree_handles_sanitized_task_id() {
        let dir = tempfile::tempdir().unwrap();
        let wt_dir = dir.path().join("gh-task-internal-8-fix");
        std::fs::create_dir(&wt_dir).unwrap();

        let result = find_existing_worktree(dir.path(), "internal:8");
        assert_eq!(result, Some(wt_dir));
    }

    #[test]
    fn find_existing_worktree_ignores_other_tasks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("gh-task-43-other-task")).unwrap();

        assert!(find_existing_worktree(dir.path(), "42").is_none());
    }
}
