//! Task tree visualization — ASCII tree view of parent-child relationships.

use crate::backends::ExternalTask;
use crate::engine::tasks::Task;
use std::collections::HashMap;

/// A node in the task tree.
#[derive(Debug, Clone)]
pub struct TreeNode {
    pub id: String,
    pub title: String,
    pub status: String,
    pub agent: Option<String>,
    pub children: Vec<TreeNode>,
}

impl TreeNode {
    /// Create a new tree node from an external task.
    pub fn from_external(task: &ExternalTask) -> Self {
        let status = task
            .labels
            .iter()
            .find(|l| l.starts_with("status:"))
            .map(|s| s.replace("status:", ""))
            .unwrap_or_else(|| "unknown".to_string());

        let agent = task
            .labels
            .iter()
            .find(|l| l.starts_with("agent:"))
            .map(|s| s.replace("agent:", ""));

        Self {
            id: task.id.0.clone(),
            title: task.title.clone(),
            status,
            agent,
            children: Vec::new(),
        }
    }

    /// Create a tree node for an internal task (limited info).
    pub fn from_internal(task: &crate::db::InternalTask) -> Self {
        Self {
            id: task.id.to_string(),
            title: task.title.clone(),
            status: task.status.as_str().to_string(),
            agent: task.agent.clone(),
            children: Vec::new(),
        }
    }
}

/// Build a forest (collection of trees) from a list of tasks.
///
/// Uses `parent:*` labels to determine hierarchy. Tasks without a parent label
/// are roots. Tasks with a parent label that doesn't exist in the list are
/// also treated as roots (orphaned children become top-level).
pub fn build_forest(tasks: Vec<Task>) -> Vec<TreeNode> {
    let mut nodes: HashMap<String, TreeNode> = HashMap::new();
    let mut parent_map: HashMap<String, Option<String>> = HashMap::new();

    // First pass: create nodes and extract parent relationships
    for task in &tasks {
        let (id, parent_id, node) = match task {
            Task::External(ext) => {
                let parent = ext
                    .labels
                    .iter()
                    .find(|l| l.starts_with("parent:"))
                    .map(|l| {
                        let parent_num = l.trim_start_matches("parent:");
                        parent_num.to_string()
                    });
                let node = TreeNode::from_external(ext);
                (ext.id.0.clone(), parent, node)
            }
            Task::Internal(int) => {
                // Internal tasks don't have parent labels (for now)
                let node = TreeNode::from_internal(int);
                (int.id.to_string(), None, node)
            }
        };

        parent_map.insert(id.clone(), parent_id);
        nodes.insert(id, node);
    }

    // Second pass: build the tree structure
    let mut root_ids: Vec<String> = Vec::new();
    let mut children_map: HashMap<String, Vec<String>> = HashMap::new();

    for (id, parent_opt) in &parent_map {
        match parent_opt {
            Some(parent_id) => {
                // If parent exists in our task list, add as child
                if nodes.contains_key(parent_id) {
                    children_map
                        .entry(parent_id.clone())
                        .or_default()
                        .push(id.clone());
                } else {
                    // Parent not in list, treat as root
                    root_ids.push(id.clone());
                }
            }
            None => {
                // No parent, this is a root
                root_ids.push(id.clone());
            }
        }
    }

    // Third pass: recursively build the tree
    fn build_node(
        id: &str,
        nodes: &HashMap<String, TreeNode>,
        children_map: &HashMap<String, Vec<String>>,
    ) -> Option<TreeNode> {
        let mut node = nodes.get(id)?.clone();

        if let Some(children_ids) = children_map.get(id) {
            for child_id in children_ids {
                if let Some(child_node) = build_node(child_id, nodes, children_map) {
                    node.children.push(child_node);
                }
            }
        }

        // Sort children by numeric ID for deterministic output
        node.children.sort_by(|a, b| {
            let a_num = a.id.parse::<i64>().unwrap_or(0);
            let b_num = b.id.parse::<i64>().unwrap_or(0);
            a_num.cmp(&b_num)
        });

        Some(node)
    }

    let mut roots: Vec<TreeNode> = Vec::new();
    for root_id in root_ids {
        if let Some(tree) = build_node(&root_id, &nodes, &children_map) {
            roots.push(tree);
        }
    }

    // Sort roots by ID numerically for consistent output
    roots.sort_by(|a, b| {
        let a_num = a.id.parse::<i64>().unwrap_or(0);
        let b_num = b.id.parse::<i64>().unwrap_or(0);
        a_num.cmp(&b_num)
    });

    roots
}

/// Render a tree as ASCII with unicode box-drawing characters.
pub fn render_tree(root: &TreeNode) -> String {
    let mut output = String::new();
    render_node(root, &mut output, "", true, true);
    output
}

/// Render a tree node and its children recursively.
fn render_node(node: &TreeNode, output: &mut String, prefix: &str, is_last: bool, is_root: bool) {
    if is_root {
        // Root node format: #30 [blocked] Implement auth system
        output.push_str(&format!(
            "#{} [{}] {}{}\n",
            node.id,
            node.status,
            node.agent
                .as_ref()
                .map(|a| format!("({}) ", a))
                .unwrap_or_default(),
            node.title
        ));
    } else {
        // Child node format with tree branches
        let branch = if is_last { "└── " } else { "├── " };
        output.push_str(&format!(
            "{}{}#{} [{}] {}{}\n",
            prefix,
            branch,
            node.id,
            node.status,
            node.agent
                .as_ref()
                .map(|a| format!("({}) ", a))
                .unwrap_or_default(),
            node.title
        ));
    }

    // Render children
    let child_prefix = if is_root {
        prefix.to_string()
    } else if is_last {
        format!("{}    ", prefix)
    } else {
        format!("{}│   ", prefix)
    };

    let child_count = node.children.len();
    for (i, child) in node.children.iter().enumerate() {
        let child_is_last = i == child_count - 1;
        render_node(child, output, &child_prefix, child_is_last, false);
    }
}

/// Render multiple trees (a forest).
pub fn render_forest(roots: &[TreeNode]) -> String {
    if roots.is_empty() {
        return "No tasks found.".to_string();
    }

    let mut output = String::new();
    for (i, root) in roots.iter().enumerate() {
        output.push_str(&render_tree(root));

        // Add blank line between trees, except for the last one
        if i < roots.len() - 1 {
            output.push('\n');
        }
    }

    output
}

/// Render a single tree by ID (for when user requests a specific task tree).
pub fn render_single_tree(root: &TreeNode, show_no_children_msg: bool) -> String {
    let mut output = render_tree(root);

    if root.children.is_empty() && show_no_children_msg {
        output.push_str("    (no children)\n");
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::{ExternalId, ExternalTask};

    fn create_test_task(id: &str, title: &str, _state: &str, labels: Vec<&str>) -> Task {
        Task::External(ExternalTask {
            id: ExternalId(id.to_string()),
            title: title.to_string(),
            body: String::new(),
            state: "open".to_string(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            author: "test".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: format!("https://github.com/test/test/issues/{}", id),
        })
    }

    #[test]
    fn test_build_forest_empty() {
        let forest = build_forest(vec![]);
        assert!(forest.is_empty());
    }

    #[test]
    fn test_build_forest_single_root() {
        let tasks = vec![create_test_task(
            "30",
            "Implement auth",
            "open",
            vec!["status:blocked", "agent:claude"],
        )];

        let forest = build_forest(tasks);
        assert_eq!(forest.len(), 1);
        assert_eq!(forest[0].id, "30");
        assert_eq!(forest[0].status, "blocked");
        assert_eq!(forest[0].agent, Some("claude".to_string()));
        assert!(forest[0].children.is_empty());
    }

    #[test]
    fn test_build_forest_with_children() {
        let tasks = vec![
            create_test_task(
                "30",
                "Implement auth",
                "open",
                vec!["status:blocked", "agent:claude"],
            ),
            create_test_task(
                "31",
                "Add user model",
                "open",
                vec!["status:done", "parent:30"],
            ),
            create_test_task(
                "32",
                "Add login API",
                "open",
                vec!["status:in_progress", "parent:30"],
            ),
        ];

        let forest = build_forest(tasks);
        assert_eq!(forest.len(), 1);
        assert_eq!(forest[0].id, "30");
        assert_eq!(forest[0].children.len(), 2);

        // Children should be sorted by ID
        assert_eq!(forest[0].children[0].id, "31");
        assert_eq!(forest[0].children[1].id, "32");
    }

    #[test]
    fn test_build_forest_multiple_roots() {
        let tasks = vec![
            create_test_task("35", "Refactor DB", "open", vec!["status:in_progress"]),
            create_test_task("30", "Implement auth", "open", vec!["status:blocked"]),
        ];

        let forest = build_forest(tasks);
        assert_eq!(forest.len(), 2);
        // Should be sorted by ID: 30, 35
        assert_eq!(forest[0].id, "30");
        assert_eq!(forest[1].id, "35");
    }

    #[test]
    fn test_render_tree() {
        let root = TreeNode {
            id: "30".to_string(),
            title: "Implement auth system".to_string(),
            status: "blocked".to_string(),
            agent: Some("claude".to_string()),
            children: vec![
                TreeNode {
                    id: "31".to_string(),
                    title: "Add user model".to_string(),
                    status: "done".to_string(),
                    agent: None,
                    children: vec![],
                },
                TreeNode {
                    id: "32".to_string(),
                    title: "Add login API".to_string(),
                    status: "in_progress".to_string(),
                    agent: Some("codex".to_string()),
                    children: vec![],
                },
            ],
        };

        let output = render_tree(&root);
        assert!(output.contains("#30 [blocked] (claude) Implement auth system"));
        assert!(output.contains("├── #31 [done] Add user model"));
        assert!(output.contains("└── #32 [in_progress] (codex) Add login API"));
    }

    #[test]
    fn test_render_forest() {
        let roots = vec![TreeNode {
            id: "35".to_string(),
            title: "Refactor database".to_string(),
            status: "in_progress".to_string(),
            agent: None,
            children: vec![],
        }];

        let output = render_forest(&roots);
        assert!(output.contains("#35 [in_progress] Refactor database"));
    }

    #[test]
    fn test_orphaned_child_becomes_root() {
        // Task has parent:99 but 99 doesn't exist in our task list
        let tasks = vec![create_test_task(
            "31",
            "Orphan task",
            "open",
            vec!["status:new", "parent:99"],
        )];

        let forest = build_forest(tasks);
        assert_eq!(forest.len(), 1);
        assert_eq!(forest[0].id, "31");
        assert!(forest[0].children.is_empty());
    }
}
