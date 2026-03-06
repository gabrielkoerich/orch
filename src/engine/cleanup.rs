//! Worktree cleanup and merged-PR detection.
//!
//! Contains the post-merge cleanup pipeline: removing git worktrees,
//! deleting local/remote branches, pulling main, and detecting
//! already-merged PRs so their tasks can be marked done.

use crate::backends::{ExternalBackend, ExternalId, Status};
use crate::cmd::CommandErrorContext;
use crate::db::TaskStatus;
use crate::engine::tasks::TaskManager;
use crate::sidecar;
use std::sync::Arc;
use tokio::process::Command;

/// Cleanup worktrees for done tasks.
///
/// Queries status:done tasks, checks if worktree exists, removes it,
/// deletes the local branch, and marks the worktree as cleaned.
///
/// Note: This is a fallback. Primary cleanup happens inline in
/// `auto_merge_pr` via `cleanup_task_worktree`. This catches edge cases
/// where the inline cleanup missed (e.g., manual merges).
pub(crate) async fn cleanup_done_worktrees(
    backend: &Arc<dyn ExternalBackend>,
    repo: &str,
    task_manager: &Arc<TaskManager>,
) -> anyhow::Result<()> {
    let done_tasks = backend.list_by_status(Status::Done).await?;
    tracing::debug!(count = done_tasks.len(), "checking done tasks for cleanup");

    // Collect task IDs from external done tasks.
    let mut task_ids: Vec<String> = done_tasks.iter().map(|t| t.id.0.clone()).collect();

    // Also include internal done tasks.
    if let Ok(internal_done) = task_manager
        .db_list_internal_by_status(TaskStatus::Done)
        .await
    {
        for t in internal_done {
            task_ids.push(format!("internal:{}", t.id));
        }
    }

    tracing::debug!(
        count = task_ids.len(),
        "checking all done tasks for cleanup"
    );

    for task_id in &task_ids {
        // Skip if already cleaned
        let worktree_cleaned = sidecar::get(task_id, "worktree_cleaned").ok();
        if worktree_cleaned.as_deref() == Some("true") || worktree_cleaned.as_deref() == Some("1") {
            continue;
        }

        if let Err(e) = cleanup_task_worktree(task_id, repo).await {
            tracing::warn!(task_id, err = %e, "worktree cleanup failed for task");
        }
    }

    Ok(())
}

/// Cleanup a single task's worktree and branches.
///
/// Removes the git worktree, deletes local + remote branches,
/// pulls main to stay up-to-date, and marks sidecar as cleaned.
pub(crate) async fn cleanup_task_worktree(task_id: &str, repo: &str) -> anyhow::Result<()> {
    let worktree = sidecar::get(task_id, "worktree").ok();
    let branch = sidecar::get(task_id, "branch").ok();

    let worktree_path = worktree.as_ref().map(std::path::PathBuf::from);

    // Get worktrees base path
    let worktrees_base = crate::home::worktrees_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from(".orch/worktrees"));

    // Try to construct default worktree path if not in sidecar
    let worktree_to_remove = if let Some(ref wt) = worktree_path {
        if wt.exists() {
            Some(wt.clone())
        } else {
            None
        }
    } else if let (Some(b), Some(dir)) = (&branch, worktree.as_ref()) {
        // Try: worktrees_base/{project}/{branch}
        let project = std::path::Path::new(dir)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| repo.replace('/', "__"));
        let wt = worktrees_base.join(&project).join(b);
        if wt.exists() {
            Some(wt)
        } else {
            None
        }
    } else {
        None
    };

    let repo_root = resolve_repo_root(repo).await?;

    if let Some(wt) = worktree_to_remove {
        tracing::info!(task_id, worktree = %wt.display(), "removing worktree");

        // Remove worktree FIRST, then delete the branch.
        // Git refuses to remove a worktree if its branch is already deleted.
        // Both commands run from the main repo root (not the worktree dir).
        let wt_str = wt.to_string_lossy().to_string();
        let remove_result = Command::new("git")
            .args(["-C", &repo_root, "worktree", "remove", &wt_str, "--force"])
            .output_with_context()
            .await;

        match remove_result {
            Ok(output) if output.status.success() => {
                tracing::info!(task_id, "worktree removed");
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::warn!(task_id, err = %stderr, "failed to remove worktree");
            }
            Err(e) => {
                tracing::warn!(task_id, err = %e, "failed to remove worktree");
            }
        }

        // Delete local and remote branch from the main repo root (worktree is already gone)
        if let Some(ref br) = branch {
            let branch_delete_result = Command::new("git")
                .args(["-C", &repo_root, "branch", "-D", br])
                .output_with_context()
                .await;

            match branch_delete_result {
                Ok(output) if output.status.success() => {
                    tracing::info!(task_id, branch = %br, "local branch deleted");
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    tracing::debug!(task_id, err = %stderr, "local branch delete skipped (may not exist)");
                }
                Err(e) => {
                    tracing::warn!(task_id, err = %e, "failed to delete local branch");
                }
            }

            // Delete remote branch
            let remote_delete = Command::new("git")
                .args(["-C", &repo_root, "push", "origin", "--delete", br])
                .output_with_context()
                .await;

            match remote_delete {
                Ok(output) if output.status.success() => {
                    tracing::info!(task_id, branch = %br, "remote branch deleted");
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    tracing::debug!(task_id, err = %stderr, "remote branch delete skipped");
                }
                Err(e) => {
                    tracing::warn!(task_id, err = %e, "failed to delete remote branch");
                }
            }
        }
    }

    // Pull main to keep local repo up-to-date for future worktrees
    let pull_result = Command::new("git")
        .args(["-C", &repo_root, "pull", "--ff-only"])
        .output_with_context()
        .await;
    match pull_result {
        Ok(output) if output.status.success() => {
            tracing::info!(task_id, "pulled main after cleanup");
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::debug!(task_id, err = %stderr, "git pull skipped");
        }
        Err(e) => {
            tracing::debug!(task_id, err = %e, "git pull failed");
        }
    }

    // Mark as cleaned in sidecar
    if let Err(e) = sidecar::set(task_id, &["worktree_cleaned=true".to_string()]) {
        tracing::warn!(task_id, err = %e, "failed to mark worktree_cleaned");
    }

    Ok(())
}

/// Resolve the main git repository root path for a project.
///
/// Looks up the local project path from config, then verifies it's a git repo.
/// This avoids relying on cwd (which is undefined under launchd services).
pub(crate) async fn resolve_repo_root(repo: &str) -> anyhow::Result<String> {
    // Look up the local path from registered projects
    let paths = crate::config::get_project_paths().unwrap_or_default();
    for path_str in &paths {
        let path = std::path::Path::new(path_str);
        let orch_yml = path.join(".orch.yml");
        let legacy = path.join(".orchestrator.yml");
        let config_file = if orch_yml.exists() {
            orch_yml
        } else if legacy.exists() {
            legacy
        } else {
            continue;
        };
        if let Ok(content) = std::fs::read_to_string(&config_file) {
            if let Ok(doc) = serde_yml::from_str::<serde_yml::Value>(&content) {
                if let Some(r) = doc
                    .get("gh")
                    .and_then(|gh| gh.get("repo"))
                    .and_then(|r| r.as_str())
                {
                    if r == repo {
                        return Ok(path_str.clone());
                    }
                }
            }
        }
    }

    // Fallback: try bare clone in ~/.orch/projects/
    let parts: Vec<&str> = repo.split('/').collect();
    let bare = if parts.len() == 2 {
        crate::home::projects_dir()
            .map(|d| d.join(parts[0]).join(format!("{}.git", parts[1])))
            .unwrap_or_default()
    } else {
        std::path::PathBuf::new()
    };
    if bare.exists() {
        return Ok(bare.display().to_string());
    }

    anyhow::bail!(
        "cannot find local path for project {repo} — \
         checked {} registered project(s) and bare clone at {}",
        paths.len(),
        bare.display()
    )
}

/// Check for merged PRs and update task status accordingly.
///
/// Queries status:in_review and status:needs_review tasks, checks if their PR
/// is merged, and updates status to done if merged.
pub(crate) async fn check_merged_prs(backend: &Arc<dyn ExternalBackend>) -> anyhow::Result<()> {
    let in_review_tasks = backend.list_by_status(Status::InReview).await?;
    let needs_review_tasks = backend.list_by_status(Status::NeedsReview).await?;
    let all_review_tasks: Vec<_> = in_review_tasks
        .into_iter()
        .chain(needs_review_tasks)
        .collect();
    tracing::debug!(
        count = all_review_tasks.len(),
        "checking review tasks for merged PRs"
    );

    for task in all_review_tasks {
        let task_id = &task.id.0;

        // Get branch from sidecar
        let branch = match sidecar::get(task_id, "branch") {
            Ok(b) if !b.is_empty() => b,
            _ => {
                tracing::debug!(task_id, "no branch info, skipping PR check");
                continue;
            }
        };

        // Check if PR is merged via the backend trait
        match backend.is_pr_merged(&branch).await {
            Ok(true) => {
                tracing::info!(task_id, branch = %branch, "PR merged, marking task complete");

                // Update status to done
                let id = ExternalId(task_id.clone());
                if let Err(e) = backend.update_status(&id, Status::Done).await {
                    tracing::warn!(task_id, err = %e, "failed to update status to done");
                    continue;
                }

                // Post comment
                let comment = "PR merged, marking task complete";
                if let Err(e) = backend.post_comment(&id, comment).await {
                    tracing::warn!(task_id, err = %e, "failed to post comment");
                }
            }
            Ok(false) => {
                // PR not merged, continue
            }
            Err(e) => {
                tracing::warn!(task_id, branch = %branch, err = %e, "failed to check PR merge status");
            }
        }
    }

    Ok(())
}
