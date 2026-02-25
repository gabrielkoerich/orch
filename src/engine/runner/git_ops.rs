//! Git operations — auto-commit, push, PR creation.
//!
//! These run after the agent completes to ensure all changes
//! are committed, pushed, and a PR is created.

use std::path::Path;
use tokio::process::Command;

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
        .output()
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
    if !has_changes(dir).await {
        return Ok(false);
    }

    let commit_msg = format!(
        "feat: {title}\n\nTask #{task_id}\nAgent: {agent}\nAttempt: {attempt}"
    );

    tracing::info!(task_id, "auto-committing uncommitted changes");

    // git add -A
    let add = Command::new("git")
        .args(["add", "-A"])
        .current_dir(dir)
        .output()
        .await?;

    if !add.status.success() {
        tracing::warn!(task_id, "git add -A failed");
        return Ok(false);
    }

    // git commit
    let commit = Command::new("git")
        .args(["commit", "-m", &commit_msg])
        .current_dir(dir)
        .output()
        .await?;

    if !commit.status.success() {
        let stderr = String::from_utf8_lossy(&commit.stderr);
        tracing::warn!(task_id, err = %stderr, "git commit failed");
        return Ok(false);
    }

    tracing::info!(task_id, "auto-commit succeeded");
    Ok(true)
}

/// Push the branch to origin.
pub async fn push_branch(dir: &Path, branch: &str) -> anyhow::Result<bool> {
    let current = get_current_branch(dir).await;
    let branch_to_push = if !current.is_empty() {
        &current
    } else {
        branch
    };

    // Skip push for main/master
    if branch_to_push == "main" || branch_to_push == "master" {
        return Ok(false);
    }

    // Check if there are commits to push
    let has_unpushed = has_unpushed_commits(dir, branch_to_push).await;
    if !has_unpushed {
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
        .output()
        .await?;

    if output.status.success() {
        tracing::info!(branch = branch_to_push, "push succeeded");
        Ok(true)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!(branch = branch_to_push, err = %stderr, "push failed");
        Ok(false)
    }
}

/// Create a PR if one doesn't already exist.
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
) -> anyhow::Result<Option<String>> {
    // Check if PR already exists
    let existing = Command::new("gh")
        .args([
            "pr",
            "list",
            "--head",
            branch,
            "--json",
            "number",
            "-q",
            ".[0].number",
        ])
        .current_dir(dir)
        .output()
        .await?;

    let existing_number = String::from_utf8_lossy(&existing.stdout).trim().to_string();
    if !existing_number.is_empty() {
        tracing::info!(task_id, pr = %existing_number, "PR already exists");
        return Ok(None);
    }

    // Build PR body
    let mut body = format!("## Summary\n\n{}", if summary.is_empty() { title } else { summary });

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
        "\n\nCloses #{task_id}\n\n---\n*Created by {agent}[bot] via [Orch](https://github.com/gabrielkoerich/orch)*"
    ));

    let pr_title = if summary.is_empty() { title } else { summary };

    let output = Command::new("gh")
        .args([
            "pr",
            "create",
            "--title",
            pr_title,
            "--body",
            &body,
            "--head",
            branch,
        ])
        .current_dir(dir)
        .output()
        .await?;

    if output.status.success() {
        let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
        tracing::info!(task_id, pr_url = %url, "created PR");
        Ok(Some(url))
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!(task_id, err = %stderr, "failed to create PR");
        Ok(None)
    }
}

/// Check if there's an open PR for the branch and override done→in_review.
pub async fn check_pr_override(dir: &Path, branch: &str) -> bool {
    if branch == "main" || branch == "master" {
        return false;
    }

    let output = Command::new("gh")
        .args([
            "pr",
            "list",
            "--head",
            branch,
            "--json",
            "number,state",
            "-q",
            ".[] | select(.state == \"OPEN\") | .number",
        ])
        .current_dir(dir)
        .output()
        .await;

    match output {
        Ok(o) if o.status.success() => {
            let number = String::from_utf8_lossy(&o.stdout).trim().to_string();
            !number.is_empty()
        }
        _ => false,
    }
}

/// Get the current branch name.
async fn get_current_branch(dir: &Path) -> String {
    let output = Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(dir)
        .output()
        .await;

    match output {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        }
        _ => String::new(),
    }
}

/// Check if there are unpushed commits.
async fn has_unpushed_commits(dir: &Path, branch: &str) -> bool {
    // Check if remote tracking branch exists
    let remote_exists = Command::new("git")
        .args(["rev-parse", &format!("origin/{branch}")])
        .current_dir(dir)
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);

    let compare_ref = if remote_exists {
        format!("origin/{branch}..HEAD")
    } else {
        // Compare against default branch
        let default = super::worktree::detect_default_branch(dir).await;
        format!("{default}..HEAD")
    };

    let output = Command::new("git")
        .args(["log", &compare_ref, "--oneline"])
        .current_dir(dir)
        .output()
        .await;

    match output {
        Ok(o) if o.status.success() => {
            !String::from_utf8_lossy(&o.stdout).trim().is_empty()
        }
        _ => false,
    }
}
