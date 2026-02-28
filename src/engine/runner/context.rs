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

use crate::cmd::CommandErrorContext;
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
    /// PR review context (for re-dispatching after review changes requested)
    pub pr_review_context: String,
    /// Memory from previous attempts (capped at last 3)
    pub memory: Vec<crate::sidecar::MemoryEntry>,
}

/// Load task-specific context from context file.
pub fn load_task_context(task_id: &str) -> String {
    let contexts_dir = crate::home::contexts_dir().unwrap_or_default();

    let path = contexts_dir.join(format!("task-{task_id}.md"));
    std::fs::read_to_string(&path).unwrap_or_default()
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
        .args(["diff", default_branch])
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
        .args(["log", "--oneline", &format!("{}..HEAD", default_branch)])
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

/// Load PR review context from sidecar (for re-dispatching after review changes requested).
pub fn load_pr_review_context(task_id: &str) -> String {
    sidecar::get(task_id, "pr_review_context").unwrap_or_default()
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
