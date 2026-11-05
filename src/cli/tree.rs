//! Task tree visualization — ASCII tree view of parent-child relationships.

use crate::backends::ExternalTask;
use crate::engine::tasks::Task;
use crate::store;
use std::collections::HashMap;
use std::fmt::Write as _;

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
            .map_or_else(|| "unknown".to_string(), |s| s.replace("status:", ""));

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

    /// Create a tree node for an internal task (from store).
    pub fn from_internal(task: &store::Task) -> Self {
        Self {
            id: task
                .external_id
                .clone()
                .unwrap_or_else(|| task.id.to_string()),
            title: task.title.clone(),
            status: task.status.as_str().to_string(),
            agent: task.agent.clone(),
            children: Vec::new(),
        }
    }
}

/// Order nodes by numeric ID, with non-numeric IDs counting as 0.
fn sort_by_numeric_id(nodes: &mut [TreeNode]) {
    nodes.sort_by_key(|n| n.id.parse::<i64>().unwrap_or(0));
}

/// Build a forest (collection of trees) from a list of tasks.
///
/// Uses `parent:*` labels to determine hierarchy. Tasks without a parent label
/// are roots. Tasks with a parent label that doesn't exist in the list are
/// also treated as roots (orphaned children become top-level).
pub fn build_forest(tasks: &[Task]) -> Vec<TreeNode> {
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
        sort_by_numeric_id(&mut node.children);

        Some(node)
    }

    let mut nodes: HashMap<String, TreeNode> = HashMap::new();
    let mut parent_map: HashMap<String, Option<String>> = HashMap::new();

    // First pass: create nodes and extract parent relationships
    for task in tasks {
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
                let id = int
                    .external_id
                    .clone()
                    .unwrap_or_else(|| int.id.to_string());
                (id, None, node)
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
    let mut roots: Vec<TreeNode> = Vec::new();
    for root_id in root_ids {
        if let Some(tree) = build_node(&root_id, &nodes, &children_map) {
            roots.push(tree);
        }
    }

    // Sort roots by ID numerically for consistent output
    sort_by_numeric_id(&mut roots);

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
    let agent = node
        .agent
        .as_ref()
        .map(|a| format!("({a}) "))
        .unwrap_or_default();
    let label = format!("#{} [{}] {}{}", node.id, node.status, agent, node.title);
    if is_root {
        // Root node format: #30 [blocked] Implement auth system
        let _ = writeln!(output, "{label}");
    } else {
        // Child node format with tree branches
        let branch = if is_last { "└── " } else { "├── " };
        let _ = writeln!(output, "{prefix}{branch}{label}");
    }

    // Render children
    let child_prefix = if is_root {
        prefix.to_string()
    } else if is_last {
        format!("{prefix}    ")
    } else {
        format!("{prefix}│   ")
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

    fn create_test_task(id: &str, title: &str, _state: &str, labels: &[&str]) -> Task {
        Task::External(ExternalTask {
            id: ExternalId(id.to_string()),
            title: title.to_string(),
            body: String::new(),
            state: "open".to_string(),
            labels: labels.iter().map(ToString::to_string).collect(),
            author: "test".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: format!("https://github.com/test/test/issues/{id}"),
        })
    }

    #[test]
    fn test_build_forest_empty() {
        let forest = build_forest(&[]);
        assert!(forest.is_empty());
    }

    #[test]
    fn test_build_forest_single_root() {
        let tasks = vec![create_test_task(
            "30",
            "Implement auth",
            "open",
            &["status:blocked", "agent:claude"],
        )];

        let forest = build_forest(&tasks);
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
                &["status:blocked", "agent:claude"],
            ),
            create_test_task(
                "31",
                "Add user model",
                "open",
                &["status:done", "parent:30"],
            ),
            create_test_task(
                "32",
                "Add login API",
                "open",
                &["status:in_progress", "parent:30"],
            ),
        ];

        let forest = build_forest(&tasks);
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
            create_test_task("35", "Refactor DB", "open", &["status:in_progress"]),
            create_test_task("30", "Implement auth", "open", &["status:blocked"]),
        ];

        let forest = build_forest(&tasks);
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
            &["status:new", "parent:99"],
        )];

        let forest = build_forest(&tasks);
        assert_eq!(forest.len(), 1);
        assert_eq!(forest[0].id, "31");
        assert!(forest[0].children.is_empty());
    }

    fn node(id: &str, status: &str, agent: Option<&str>, children: Vec<TreeNode>) -> TreeNode {
        TreeNode {
            id: id.to_string(),
            title: format!("T{id}"),
            status: status.to_string(),
            agent: agent.map(ToString::to_string),
            children,
        }
    }

    #[test]
    fn render_tree_exact_output_for_nested_tree() {
        let root = node(
            "1",
            "blocked",
            Some("claude"),
            vec![
                node(
                    "2",
                    "done",
                    None,
                    vec![
                        node("4", "done", Some("codex"), vec![]),
                        node("5", "new", None, vec![]),
                    ],
                ),
                node("3", "new", None, vec![node("6", "new", None, vec![])]),
            ],
        );

        let expected = [
            "#1 [blocked] (claude) T1",
            "├── #2 [done] T2",
            "│   ├── #4 [done] (codex) T4",
            "│   └── #5 [new] T5",
            "└── #3 [new] T3",
            "    └── #6 [new] T6",
            "",
        ]
        .join("\n");
        assert_eq!(render_tree(&root), expected);
    }

    #[test]
    fn render_forest_separates_trees_with_one_blank_line() {
        let roots = vec![
            node("1", "new", None, vec![]),
            node("2", "done", None, vec![]),
        ];
        assert_eq!(render_forest(&roots), "#1 [new] T1\n\n#2 [done] T2\n");
    }

    #[test]
    fn render_forest_empty_says_no_tasks() {
        assert_eq!(render_forest(&[]), "No tasks found.");
    }

    #[test]
    fn render_single_tree_notes_missing_children_only_when_asked() {
        let leaf = node("1", "new", None, vec![]);
        assert_eq!(
            render_single_tree(&leaf, true),
            "#1 [new] T1\n    (no children)\n"
        );
        assert_eq!(render_single_tree(&leaf, false), "#1 [new] T1\n");

        let parent = node("1", "new", None, vec![node("2", "new", None, vec![])]);
        assert_eq!(
            render_single_tree(&parent, true),
            "#1 [new] T1\n└── #2 [new] T2\n"
        );
    }

    #[test]
    fn build_forest_sorts_numerically_and_non_numeric_ids_sort_first() {
        let tasks = vec![
            create_test_task("10", "ten", "open", &["status:new"]),
            create_test_task("9", "nine", "open", &["status:new"]),
            create_test_task("abc", "word", "open", &["status:new"]),
            create_test_task("2", "child b", "open", &["parent:10"]),
            create_test_task("11", "child a", "open", &["parent:10"]),
        ];

        let forest = build_forest(&tasks);
        let root_ids: Vec<&str> = forest.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(root_ids, ["abc", "9", "10"]);
        let child_ids: Vec<&str> = forest[2].children.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(child_ids, ["2", "11"]);
    }

    #[test]
    fn from_external_without_labels_has_unknown_status_and_no_agent() {
        let Task::External(ext) = create_test_task("7", "x", "open", &[]) else {
            unreachable!()
        };
        let node = TreeNode::from_external(&ext);
        assert_eq!(node.status, "unknown");
        assert_eq!(node.agent, None);
    }
}
