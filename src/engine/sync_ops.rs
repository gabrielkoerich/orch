//! Sync operations — periodic maintenance tasks.
//!
//! This module contains operations that run on the sync tick (every 120s):
//! - `cleanup_done_worktrees()` — remove worktrees for completed tasks
//! - `check_merged_prs()` — detect merged PRs and update task status
//! - `scan_mentions()` — detect @mentions and create internal tasks
//! - `skills_sync()` — clone/pull skill repositories

use crate::backends::{ExternalBackend, ExternalId, Status};
use crate::cmd::CommandErrorContext;
use crate::db::{Db, TaskStatus};
use crate::sidecar;
use std::sync::Arc;
use tokio::process::Command;

/// Sync skill repositories from config.
///
/// Reads the `skills:` list from config and clones/pulls each repository
/// to `~/.orch/skills/{repo}/`. This keeps skill documentation up-to-date
/// for agents.
pub(crate) async fn skills_sync() -> anyhow::Result<()> {
    use tokio::process::Command;

    let skills = match crate::config::get_skills() {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(err = %e, "no skills configured");
            return Ok(());
        }
    };

    if skills.is_empty() {
        tracing::debug!("no skills configured, skipping sync");
        return Ok(());
    }

    let skills_base = crate::home::skills_dir()?;
    let git_timeout = std::time::Duration::from_secs(60);

    for skill in skills {
        // Validate repo format to prevent path traversal
        if skill.repo.contains("..") || skill.repo.matches('/').count() != 1 {
            tracing::warn!(repo = %skill.repo, "invalid skill repo format, expected 'owner/repo'");
            continue;
        }

        let repo_dir = skills_base.join(&skill.repo);
        let repo_url = format!("https://github.com/{}.git", skill.repo);

        if repo_dir.exists() {
            // Pull latest changes with timeout
            tracing::debug!(repo = %skill.repo, "pulling skill repo");
            let pull_result = tokio::time::timeout(
                git_timeout,
                Command::new("git")
                    .args(["pull", "--ff-only", "--prune"])
                    .current_dir(&repo_dir)
                    .output_with_context(),
            )
            .await;

            match pull_result {
                Ok(Ok(output)) if !output.status.success() => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    tracing::warn!(repo = %skill.repo, err = %stderr, "git pull failed");
                }
                Ok(Ok(_)) => {
                    tracing::debug!(repo = %skill.repo, "skill repo updated");
                }
                Ok(Err(e)) => {
                    tracing::warn!(repo = %skill.repo, err = %e, "git pull error");
                }
                Err(_) => {
                    tracing::warn!(repo = %skill.repo, "git pull timed out after 60s");
                }
            }
        } else {
            // Clone the repository (shallow for efficiency)
            tracing::debug!(repo = %skill.repo, "cloning skill repo");
            let parent = repo_dir
                .parent()
                .ok_or_else(|| anyhow::anyhow!("skill repo path has no parent directory"))?;
            std::fs::create_dir_all(parent)?;
            let repo_dir_str = repo_dir
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("skill repo path is not valid UTF-8"))?;

            let clone_result = tokio::time::timeout(
                git_timeout,
                Command::new("git")
                    .args(["clone", "--depth", "1", &repo_url, repo_dir_str])
                    .output_with_context(),
            )
            .await;

            match clone_result {
                Ok(Ok(output)) if !output.status.success() => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    tracing::warn!(repo = %skill.repo, err = %stderr, "git clone failed");
                    // Clean up partial clone to allow retry on next tick
                    let _ = std::fs::remove_dir_all(&repo_dir);
                }
                Ok(Ok(_)) => {
                    tracing::info!(repo = %skill.repo, "skill repo cloned");
                }
                Ok(Err(e)) => {
                    tracing::warn!(repo = %skill.repo, err = %e, "git clone error");
                    let _ = std::fs::remove_dir_all(&repo_dir);
                }
                Err(_) => {
                    tracing::warn!(repo = %skill.repo, "git clone timed out after 60s");
                    let _ = std::fs::remove_dir_all(&repo_dir);
                }
            }
        }
    }

    Ok(())
}

/// Cleanup worktrees for done tasks.
///
/// Queries status:done tasks, checks if worktree exists, removes it,
/// deletes the local branch, and marks the worktree as cleaned.
pub(crate) async fn cleanup_done_worktrees(
    backend: &Arc<dyn ExternalBackend>,
    repo: &str,
) -> anyhow::Result<()> {
    let done_tasks = backend.list_by_status(Status::Done).await?;
    tracing::debug!(count = done_tasks.len(), "checking done tasks for cleanup");

    // Resolve the main repository root for git operations.
    // We need this because worktree removal and branch deletion must run
    // from the main repo, not from the (soon-to-be-deleted) worktree dir.
    let repo_root = resolve_repo_root(repo).await?;

    // Get worktrees base path
    let worktrees_base = crate::home::worktrees_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from(".orch/worktrees"));

    for task in done_tasks {
        let task_id = &task.id.0;

        // Get worktree and branch from sidecar
        let worktree = sidecar::get(task_id, "worktree").ok();
        let branch = sidecar::get(task_id, "branch").ok();
        let worktree_cleaned = sidecar::get(task_id, "worktree_cleaned").ok();

        // Skip if already cleaned
        if worktree_cleaned.as_deref() == Some("true") || worktree_cleaned.as_deref() == Some("1") {
            continue;
        }

        let worktree_path = worktree.as_ref().map(std::path::PathBuf::from);

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

            // Delete branch from the main repo root (worktree is already gone)
            if let Some(ref br) = branch {
                let branch_delete_result = Command::new("git")
                    .args(["-C", &repo_root, "branch", "-D", br])
                    .output_with_context()
                    .await;

                match branch_delete_result {
                    Ok(output) if output.status.success() => {
                        tracing::info!(task_id, branch = %br, "branch deleted");
                    }
                    Ok(output) => {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        tracing::debug!(task_id, err = %stderr, "branch delete skipped (may not exist)");
                    }
                    Err(e) => {
                        tracing::warn!(task_id, err = %e, "failed to delete branch");
                    }
                }
            }

            // Mark as cleaned in sidecar
            if let Err(e) = sidecar::set(task_id, &["worktree_cleaned=true".to_string()]) {
                tracing::warn!(task_id, err = %e, "failed to mark worktree_cleaned");
            }
        }
    }

    Ok(())
}

/// Resolve the main git repository root path for a project.
///
/// Looks up the local project path from config, then verifies it's a git repo.
/// This avoids relying on cwd (which is undefined under launchd services).
async fn resolve_repo_root(repo: &str) -> anyhow::Result<String> {
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
    let bare = crate::home::projects_dir()
        .map(|d| d.join(repo.replace('/', "__")))
        .unwrap_or_default();
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
/// Queries status:in_review tasks, checks if their PR is merged,
/// and updates status to done if merged.
pub(crate) async fn check_merged_prs(backend: &Arc<dyn ExternalBackend>) -> anyhow::Result<()> {
    let in_review_tasks = backend.list_by_status(Status::InReview).await?;
    tracing::debug!(
        count = in_review_tasks.len(),
        "checking in_review tasks for merged PRs"
    );

    for task in in_review_tasks {
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

/// Scan for @mentions and create internal tasks.
///
/// Checks recent issue comments for @orchestrator mentions,
/// creates internal tasks, and acknowledges them.
pub(crate) async fn scan_mentions(backend: &Arc<dyn ExternalBackend>, db: &Arc<Db>) -> anyhow::Result<()> {
    // Get the current user (for mention detection)
    let current_user = match backend.get_authenticated_user().await {
        Ok(Some(u)) => format!("@{}", u),
        Ok(None) => {
            tracing::debug!("backend does not support user identity, skipping mentions");
            return Ok(());
        }
        Err(e) => {
            tracing::warn!(err = %e, "failed to get current user, skipping mentions");
            return Ok(());
        }
    };

    // Use persisted cursor if available, otherwise fall back to 24h ago
    let fallback = chrono::Utc::now() - chrono::Duration::hours(24);
    let since_str = match db.kv_get("mentions_last_checked").await {
        Ok(Some(ts)) => ts,
        _ => fallback.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    };

    let mentions = match backend.get_mentions(&since_str).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(err = %e, "failed to get mentions");
            return Ok(());
        }
    };

    // Get existing mention tasks across ALL statuses to avoid duplicates.
    // Only checking New status would miss tasks that progressed to InProgress/Done,
    // causing duplicate tasks on the next sync tick within the 24h window.
    let mut existing_mentions = std::collections::HashSet::new();
    for status in &[
        TaskStatus::New,
        TaskStatus::InProgress,
        TaskStatus::Done,
        TaskStatus::Blocked,
        TaskStatus::Routed,
        TaskStatus::InReview,
        TaskStatus::NeedsReview,
    ] {
        let tasks = db.list_internal_tasks_by_status(*status).await?;
        for t in tasks {
            if t.source == "mention" {
                existing_mentions.insert(t.source_id.clone());
            }
        }
    }

    for mention in mentions {
        // Skip if already processed
        if existing_mentions.contains(&mention.id) {
            continue;
        }

        if !mention.body.contains(&current_user) && !mention.body.contains("@orchestrator") {
            continue;
        }

        // Create internal task for this mention
        let title = format!("Respond to mention by @{}", mention.author);
        let task_body = format!("Mention by @{}:\n\n{}", mention.author, mention.body);

        let task_id = db
            .create_internal_task(&title, &task_body, "mention", &mention.id)
            .await?;

        tracing::info!(task_id, mention_id = %mention.id, "created mention task");
    }

    // Persist cursor so the next sync tick only fetches newer comments
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    if let Err(e) = db.kv_set("mentions_last_checked", &now).await {
        tracing::warn!(err = %e, "failed to persist mentions cursor");
    }

    Ok(())
}
