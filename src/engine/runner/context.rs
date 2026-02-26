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
use crate::sidecar;
use std::path::Path;
use tokio::process::Command;

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
    /// Memory from previous attempts (capped at last 3)
    pub memory: Vec<crate::sidecar::MemoryEntry>,
}

/// Load task-specific context from context file.
pub fn load_task_context(task_id: &str) -> String {
    let contexts_dir = crate::home::contexts_dir().unwrap_or_default();

    let path = contexts_dir.join(format!("task-{task_id}.md"));
    std::fs::read_to_string(&path).unwrap_or_default()
}

/// Build parent task context for subtasks.
#[allow(dead_code)]
pub async fn build_parent_context(task: &ExternalTask, backend: &dyn ExternalBackend) -> String {
    // Check if task has a parent via sidecar
    let parent_id = match sidecar::get(&task.id.0, "parent_id") {
        Ok(id) if !id.is_empty() => id,
        _ => return String::new(),
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

    // Get sibling tasks
    if let Ok(siblings) = backend.get_sub_issues(&ExternalId(parent_id)).await {
        if !siblings.is_empty() {
            ctx.push_str("## Sibling Tasks\n\n");
            for sib_id in &siblings {
                if sib_id.0 == task.id.0 {
                    continue; // Skip self
                }
                if let Ok(sib) = backend.get_task(sib_id).await {
                    let status = sib
                        .labels
                        .iter()
                        .find(|l| l.starts_with("status:"))
                        .map(|s| s.replace("status:", ""))
                        .unwrap_or_else(|| "unknown".to_string());
                    ctx.push_str(&format!("- #{} [{}]: {}\n", sib.id.0, status, sib.title));

                    // Include sidecar summary if available
                    if let Ok(summary) = sidecar::get(&sib.id.0, "summary") {
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
        .output()
        .await;

    match output {
        Ok(o) if o.status.success() => {
            let full = String::from_utf8_lossy(&o.stdout);
            let lines: Vec<&str> = full.lines().take(200).collect();
            let result = lines.join("\n");
            if full.lines().count() > 200 {
                format!(
                    "{result}\n... (truncated, {} total files)",
                    full.lines().count()
                )
            } else {
                result
            }
        }
        _ => String::new(),
    }
}

/// Build git diff from base branch (only for retries).
pub async fn build_git_diff(project_dir: &Path, default_branch: &str) -> String {
    let output = Command::new("git")
        .args(["diff", default_branch])
        .current_dir(project_dir)
        .output()
        .await;

    match output {
        Ok(o) if o.status.success() => {
            let diff = String::from_utf8_lossy(&o.stdout);
            // Cap diff at 10000 chars to avoid blowing up context
            if diff.len() > 10000 {
                format!("{}...\n(diff truncated at 10000 chars)", &diff[..10000])
            } else {
                diff.to_string()
            }
        }
        _ => String::new(),
    }
}

/// Fetch recent issue comments for agent context.
#[allow(dead_code)]
pub async fn fetch_issue_comments(
    backend: &dyn ExternalBackend,
    task_id: &str,
    limit: usize,
) -> String {
    let since = chrono::Utc::now() - chrono::Duration::days(30);
    let since_str = since.format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let mentions = match backend.get_mentions(&since_str).await {
        Ok(m) => m,
        Err(_) => return String::new(),
    };

    // Filter to comments on this task
    let mut comments = String::new();
    let mut count = 0;
    for mention in mentions.iter().rev() {
        if count >= limit {
            break;
        }
        // Include if the mention references this task
        if mention.id.contains(task_id) || mention.body.contains(&format!("#{task_id}")) {
            comments.push_str(&format!(
                "**@{}** ({}):\n{}\n\n",
                mention.author, mention.created_at, mention.body
            ));
            count += 1;
        }
    }

    comments
}

/// Build memory context from previous attempts.
/// Returns formatted memory context string and the raw memory entries.
pub fn build_memory_context(task_id: &str) -> (String, Vec<crate::sidecar::MemoryEntry>) {
    const MAX_MEMORY_ENTRIES: usize = 3;

    let memory = match crate::sidecar::get_recent_memory(task_id, MAX_MEMORY_ENTRIES) {
        Ok(m) => m,
        Err(_) => return (String::new(), vec![]),
    };

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

/// Build the full task context.
#[allow(dead_code)]
pub async fn build_full_context(
    task: &ExternalTask,
    backend: &dyn ExternalBackend,
    project_dir: &Path,
    default_branch: &str,
    attempts: u32,
    selected_skills: &[String],
) -> TaskContext {
    let task_context = load_task_context(&task.id.0);
    let parent_context = build_parent_context(task, backend).await;
    let project_instructions = build_project_instructions(project_dir);
    let skills_docs = build_skills_docs(selected_skills);
    let repo_tree = build_repo_tree(project_dir).await;

    let git_diff = if attempts > 0 {
        build_git_diff(project_dir, default_branch).await
    } else {
        String::new()
    };

    let issue_comments = fetch_issue_comments(backend, &task.id.0, 10).await;

    // Load memory from previous attempts (only on retries)
    let (_, memory) = if attempts > 0 {
        build_memory_context(&task.id.0)
    } else {
        (String::new(), vec![])
    };

    TaskContext {
        task_context,
        parent_context,
        project_instructions,
        skills_docs,
        repo_tree,
        git_diff,
        issue_comments,
        memory,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_memory_context_empty() {
        let (context, memory) = build_memory_context("nonexistent-task-12345");
        assert!(context.is_empty());
        assert!(memory.is_empty());
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
            memory: vec![],
        };

        assert!(ctx.task_context.is_empty());
        assert!(ctx.memory.is_empty());
    }
}
