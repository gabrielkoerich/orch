//! Context building for task execution.
//!
//! Assembles all contextual information needed for the agent prompt:
//! - Task-specific context files
//! - Parent task context (for subtasks)
//! - Project instructions (CLAUDE.md, AGENTS.md, README.md)
//! - Skills documentation
//! - Repository file tree
//! - Git diff (for retries)
//! - Issue comments

use crate::backends::{ExternalBackend, ExternalId, ExternalTask};
use crate::cmd::CommandErrorContext;
use crate::store;
use crate::store::TaskStore;
use std::path::Path;
use std::sync::Arc;
use tokio::process::Command;
use futures::future::join_all;

/// All assembled context for a task execution.
pub struct TaskContext {
    /// Task-specific context from previous runs
    pub task_context: String,
    /// Parent issue summary + sibling summaries
    pub parent_context: String,
    /// Project instructions (CLAUDE.md + AGENTS.md + README.md)
    pub project_instructions: String,
    /// Skills documentation for selected skills
    pub skills_docs: String,
    /// Repository file tree (git ls-files, capped at 200 lines)
    pub repo_tree: String,
    /// Git diff from base branch (only for retries)
    pub git_diff: String,
    /// Recent issue comments
    pub issue_comments: String,
    /// PR review context (for re-dispatching after review changes requested)
    pub pr_review_context: String,
    /// Memory from previous attempts (capped at last 3)
    pub memory: Vec<crate::store::MemoryEntry>,
}

/// Load task-specific context from context file.
pub fn load_task_context(task_id: &str) -> String {
    let contexts_dir = crate::home::contexts_dir().unwrap_or_default();

    let path = contexts_dir.join(format!("task-{task_id}.md"));
    std::fs::read_to_string(&path).unwrap_or_default()
}

/// Build parent task context for subtasks.
pub async fn build_parent_context(
    task: &ExternalTask,
    backend: &dyn ExternalBackend,
    store: &Option<Arc<TaskStore>>,
    repo: &str,
) -> String {
    // Check if task has a parent via store
    let parent_id = match store::opt_store_get_task(store, repo, &task.id.0)
        .await
        .and_then(|t| t.parent_id)
    {
        Some(id) => id.to_string(),
        None => return String::new(),
    };

    let mut ctx = String::new();

    // Get parent issue details
    let parent = match backend.get_task(&ExternalId(parent_id.clone())).await {
        Ok(t) => t,
        Err(_) => return String::new(),
    };

    ctx.push_str(&format!(
        "## Parent Task #{}\n\n**Title**: {}\n\n{}\n\n",
        parent.id.0, parent.title, parent.body
    ));

    // Get sibling tasks — fetch remote task objects and store summaries in parallel
    if let Ok(siblings) = backend.get_sub_issues(&ExternalId(parent_id)).await {
        // Filter out self early
        let siblings: Vec<ExternalId> = siblings
            .into_iter()
            .filter(|s| s.0 != task.id.0)
            .collect();

        if !siblings.is_empty() {
            ctx.push_str("## Sibling Tasks\n\n");

            // Fetch ExternalTask objects concurrently
            let task_futs = siblings.iter().map(|id| backend.get_task(id));
            let task_results = join_all(task_futs).await;

            // Fetch store task records concurrently (may be None for missing tasks)
            let store_futs = siblings
                .iter()
                .map(|id| store::opt_store_get_task(store, repo, &id.0));
            let store_results = join_all(store_futs).await;

            for (i, maybe_task_res) in task_results.into_iter().enumerate() {
                if let Ok(sib) = maybe_task_res {
                    let status = sib
                        .labels
                        .iter()
                        .find(|l| l.starts_with("status:"))
                        .map(|s| s.replace("status:", ""))
                        .unwrap_or_else(|| "unknown".to_string());
                    ctx.push_str(&format!("- #{} [{}]: {}\n", sib.id.0, status, sib.title));

                    // Include summary if available from the store result at same index
                    if let Some(Some(task_rec)) = store_results.get(i) {
                        let summary = &task_rec.summary;
                        if !summary.is_empty() {
                            ctx.push_str(&format!("  Summary: {}\n", summary));
                        }
                    }
                }
            }

            ctx.push('\n');
        }
    }

    ctx
}

/// Build project instructions from CLAUDE.md, AGENTS.md, README.md.
pub fn build_project_instructions(project_dir: &Path) -> String {
    let mut instructions = String::new();

    for filename in &["CLAUDE.md", "AGENTS.md", "README.md"] {
        let path = project_dir.join(filename);
        if let Ok(content) = std::fs::read_to_string(&path) {
            if !content.is_empty() {
                instructions.push_str(&format!("## {filename}\n\n{content}\n\n"));
            }
        }
    }

    instructions
}

/// Build skills documentation for selected skills.
pub fn build_skills_docs(selected_skills: &[String]) -> String {
    if selected_skills.is_empty() {
        return String::new();
    }

    let mut docs = String::new();
    let skills_dirs = [
        dirs::home_dir()
            .unwrap_or_default()
            .join(".claude")
            .join("skills"),
        crate::home::skills_dir().unwrap_or_default(),
    ];

    for skill in selected_skills {
        for dir in &skills_dirs {
            let skill_file = dir.join(skill).join("SKILL.md");
            if let Ok(content) = std::fs::read_to_string(&skill_file) {
                docs.push_str(&format!("## Skill: {skill}\n\n{content}\n\n"));
                break;
            }
        }
    }

    docs
}

/// Build repository file tree (git ls-files, capped at 200 lines).
pub async fn build_repo_tree(project_dir: &Path) -> String {
    let output = Command::new("git")
        .args(["ls-files"])
        .current_dir(project_dir)
        .output_with_context()
        .await;

    match output {
        Ok(o) if o.status.success() => {
            let full = String::from_utf8_lossy(&o.stdout);
            let all_lines: Vec<&str> = full.lines().collect();
            let total = all_lines.len();
            if total > 200 {
                format!(
                    "{}\n... (truncated, {} total files)",
                    all_lines[..200].join("\n"),
                    total
                )
            } else {
                all_lines.join("\n")
            }
        }
        _ => String::new(),
    }
}

/// Build git diff from base branch (only for retries).
pub async fn build_git_diff(project_dir: &Path, default_branch: &str) -> String {
    let output = Command::new("git")
        .args(["diff", &format!("origin/{default_branch}")])
        .current_dir(project_dir)
        .output_with_context()
        .await;

    match output {
        Ok(o) if o.status.success() => {
            let diff = String::from_utf8_lossy(&o.stdout);
            // Cap diff at 10000 chars to avoid blowing up context
            if diff.len() > 10000 {
                // Find safe UTF-8 boundary
                let mut boundary = 10000;
                while !diff.is_char_boundary(boundary) {
                    boundary -= 1;
                }
                format!("{}...\n(diff truncated at 10000 chars)", &diff[..boundary])
            } else {
                diff.to_string()
            }
        }
        _ => String::new(),
    }
}

/// Build git log from base branch to HEAD.
/// Shows commit history for the feature branch.
pub async fn build_git_log(project_dir: &Path, default_branch: &str) -> String {
    let output = Command::new("git")
        .args([
            "log",
            "--oneline",
            &format!("origin/{default_branch}..HEAD"),
        ])
        .current_dir(project_dir)
        .output_with_context()
        .await;

    match output {
        Ok(o) if o.status.success() => {
            let log = String::from_utf8_lossy(&o.stdout);
            let all_lines: Vec<&str> = log.lines().collect();
            let total = all_lines.len();
            if total > 100 {
                format!(
                    "{}\n... (truncated, {} total commits)",
                    all_lines[..100].join("\n"),
                    total
                )
            } else {
                all_lines.join("\n")
            }
        }
        _ => String::new(),
    }
}

/// Returns true if the comment is from an automated bot and should be skipped.
fn is_bot_comment(mention: &crate::backends::Mention) -> bool {
    // Skip GitHub Actions bots and other automated accounts
    if mention.author.ends_with("[bot]") {
        return true;
    }
    // Skip automated review comments posted by orch itself
    if mention.body.starts_with("## Automated Review") {
        return true;
    }
    false
}

/// Fetch issue comments for agent context.
///
/// Fetches all comments for the specific issue using the per-issue API endpoint,
/// then returns the last `limit` non-bot comments formatted for the prompt.
pub async fn fetch_issue_comments(
    backend: &dyn ExternalBackend,
    task_id: &str,
    limit: usize,
) -> String {
    let id = crate::backends::ExternalId(task_id.to_string());
    let all_comments = match backend.list_issue_comments(&id).await {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(task_id, error = %e, "failed to fetch issue comments");
            return String::new();
        }
    };

    // Filter bot comments, then take the last `limit` entries
    let human_comments: Vec<_> = all_comments
        .into_iter()
        .filter(|c| !is_bot_comment(c))
        .collect();

    let start = human_comments.len().saturating_sub(limit);
    let mut formatted = String::new();
    for comment in &human_comments[start..] {
        formatted.push_str(&format!(
            "**@{}** ({}):\n{}\n\n",
            comment.author, comment.created_at, comment.body
        ));
    }

    formatted
}

/// Build memory context from previous attempts.
/// Returns formatted memory context string and the raw memory entries.
pub async fn build_memory_context(
    task_id: &str,
    store: &Option<Arc<TaskStore>>,
    repo: &str,
) -> (String, Vec<crate::store::MemoryEntry>) {
    const MAX_MEMORY_ENTRIES: usize = 3;

    let memory = store::get_recent_memory(store, repo, task_id, MAX_MEMORY_ENTRIES).await;

    if memory.is_empty() {
        return (String::new(), vec![]);
    }

    let mut context = String::new();
    context.push_str("## Previous Attempts Memory\n\n");
    context.push_str("Learnings from previous task attempts:\n\n");

    for entry in &memory {
        context.push_str(&format!(
            "### Attempt #{} (Agent: {})",
            entry.attempt, entry.agent
        ));

        if let Some(ref model) = entry.model {
            context.push_str(&format!(", Model: {}", model));
        }
        context.push('\n');

        if !entry.approach.is_empty() {
            context.push_str(&format!("**Approach**: {}\n", entry.approach));
        }

        if !entry.learnings.is_empty() {
            context.push_str("**Key Learnings**:\n");
            for learning in &entry.learnings {
                context.push_str(&format!("- {}\n", learning));
            }
        }

        if let Some(ref error) = entry.error {
            context.push_str(&format!("**Error**: {}\n", error));
        }

        if !entry.files_modified.is_empty() {
            context.push_str(&format!(
                "**Files Modified**: {}\n",
                entry.files_modified.join(", ")
            ));
        }

        context.push('\n');
    }

    (context, memory)
}

/// Load PR review context from store.
pub async fn load_pr_review_context(
    task_id: &str,
    store: &Option<Arc<TaskStore>>,
    repo: &str,
) -> String {
    store::opt_store_get_task(store, repo, task_id)
        .await
        .map(|t| t.pr_review_context)
        .unwrap_or_default()
}

/// Build the full task context.
#[allow(clippy::too_many_arguments)]
pub async fn build_full_context(
    task: &ExternalTask,
    backend: Option<&dyn ExternalBackend>,
    project_dir: &Path,
    default_branch: &str,
    attempts: u32,
    selected_skills: &[String],
    store: &Option<Arc<TaskStore>>,
    repo: &str,
) -> TaskContext {
    let task_context = load_task_context(&task.id.0);

    let parent_context = if let Some(b) = backend {
        build_parent_context(task, b, store, repo).await
    } else {
        String::new()
    };

    let project_instructions = build_project_instructions(project_dir);
    let skills_docs = build_skills_docs(selected_skills);
    let repo_tree = build_repo_tree(project_dir).await;

    let git_diff = if attempts > 0 {
        build_git_diff(project_dir, default_branch).await
    } else {
        String::new()
    };

    let issue_comments = if let Some(b) = backend {
        fetch_issue_comments(b, &task.id.0, 10).await
    } else {
        String::new()
    };

    // Load PR review context from store
    let pr_review_context = load_pr_review_context(&task.id.0, store, repo).await;

    // Always load memory from previous attempts (empty on first run, populated on retries)
    let (_, memory) = build_memory_context(&task.id.0, store, repo).await;

    TaskContext {
        task_context,
        parent_context,
        project_instructions,
        skills_docs,
        repo_tree,
        git_diff,
        issue_comments,
        pr_review_context,
        memory,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_build_memory_context_empty() {
        let (context, memory) = build_memory_context("nonexistent-task-12345", &None, "").await;
        assert!(context.is_empty());
        assert!(memory.is_empty());
    }

    #[test]
    fn test_build_project_instructions_empty_dir() {
        let dir = std::env::temp_dir().join("orch_test_nonexistent");
        let instructions = build_project_instructions(&dir);
        assert!(instructions.is_empty());
    }

    #[test]
    fn test_build_skills_docs_empty() {
        let docs = build_skills_docs(&[]);
        assert!(docs.is_empty());
    }

    #[test]
    fn test_task_context_default_values() {
        let ctx = TaskContext {
            task_context: String::new(),
            parent_context: String::new(),
            project_instructions: String::new(),
            skills_docs: String::new(),
            repo_tree: String::new(),
            git_diff: String::new(),
            issue_comments: String::new(),
            pr_review_context: String::new(),
            memory: vec![],
        };

        assert!(ctx.task_context.is_empty());
        assert!(ctx.memory.is_empty());
    }
}
