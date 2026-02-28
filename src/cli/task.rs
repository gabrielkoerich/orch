use crate::backends::{ExternalId, Status};
use crate::cli::init_task_manager;
use crate::cmd::SyncCommandErrorContext;
use crate::config;
use crate::engine::router::Router;
use crate::engine::runner::TaskRunner;
use crate::engine::tasks::{CreateTaskRequest, Task, TaskFilter, TaskType};
use crate::sidecar;
use crate::tmux::TmuxManager;
use anyhow::Context;
use std::sync::Arc;

/// List tasks with optional filters.
pub async fn list(status: Option<String>, source: Option<String>) -> anyhow::Result<()> {
    let task_manager = init_task_manager().await?;
    let filter = TaskFilter { status, source };
    let tasks = task_manager.list_tasks(filter).await?;

    if tasks.is_empty() {
        println!("No tasks found.");
        return Ok(());
    }

    println!("{:<10} {:<12} {:<20} TITLE", "ID", "TYPE", "STATUS");
    println!("{}", "-".repeat(80));

    for task in tasks {
        match task {
            Task::External(ext) => {
                let status = ext
                    .labels
                    .iter()
                    .find(|l| l.starts_with("status:"))
                    .map(|s| s.replace("status:", ""))
                    .unwrap_or_else(|| "unknown".to_string());
                println!(
                    "{:<10} {:<12} {:<20} {}",
                    ext.id.0, "external", status, ext.title
                );
            }
            Task::Internal(int) => {
                println!(
                    "{:<10} {:<12} {:<20} {}",
                    int.id,
                    "internal",
                    int.status.as_str(),
                    int.title
                );
            }
        }
    }

    Ok(())
}

/// Create a new task.
pub async fn add(
    title: String,
    body: Option<String>,
    labels: Vec<String>,
    source: String,
) -> anyhow::Result<()> {
    let task_manager = init_task_manager().await?;

    // If labels are provided, create as external (GitHub) task
    let task_type = if !labels.is_empty() {
        TaskType::External
    } else {
        TaskType::Internal
    };

    let req = CreateTaskRequest {
        title,
        body: body.unwrap_or_default(),
        task_type,
        labels,
        source,
        source_id: String::new(),
    };
    let task = task_manager.create_task(req).await?;

    match task {
        Task::Internal(t) => {
            println!("Created internal task #{}: {}", t.id, t.title);
        }
        Task::External(t) => {
            println!("Created external task #{}: {}", t.id.0, t.title);
        }
    }

    Ok(())
}

/// Get task details by ID.
pub async fn get(id: i64) -> anyhow::Result<()> {
    let task_manager = init_task_manager().await?;
    let task = task_manager.get_task(id).await?;

    match task {
        Task::External(ext) => {
            println!("ID: {} (external)", ext.id.0);
            println!("Title: {}", ext.title);
            println!("State: {}", ext.state);
            println!("Labels: {}", ext.labels.join(", "));
            println!("Author: {}", ext.author);
            println!("URL: {}", ext.url);
            println!("Created: {}", ext.created_at);
            println!("Updated: {}", ext.updated_at);

            // Show sidecar info if available
            if let Ok(agent) = sidecar::get(&ext.id.0, "agent") {
                println!("Agent: {}", agent);
            }
            if let Ok(branch) = sidecar::get(&ext.id.0, "branch") {
                println!("Branch: {}", branch);
            }

            println!("\n{}", ext.body);
        }
        Task::Internal(int) => {
            println!("ID: {} (internal)", int.id);
            println!("Title: {}", int.title);
            println!("Status: {}", int.status.as_str());
            println!("Source: {}", int.source);
            if let Some(agent) = &int.agent {
                println!("Agent: {}", agent);
            }
            if let Some(reason) = &int.block_reason {
                println!("Block reason: {}", reason);
            }
            println!("Created: {}", int.created_at.to_rfc3339());
            println!("Updated: {}", int.updated_at.to_rfc3339());
            println!("\n{}", int.body);
        }
    }

    Ok(())
}

/// Show task status summary.
pub async fn status(json: bool) -> anyhow::Result<()> {
    use crate::backends::github::GitHubBackend;
    use crate::backends::ExternalBackend;

    let repo =
        config::get_current_repo().context("'repo' not set — ensure .orch.yml has gh.repo")?;
    let backend: Arc<dyn ExternalBackend> = Arc::new(GitHubBackend::new(repo));

    let statuses = [
        Status::New,
        Status::Routed,
        Status::InProgress,
        Status::Done,
        Status::Blocked,
        Status::InReview,
        Status::NeedsReview,
    ];

    let mut counts = Vec::new();
    for s in &statuses {
        let tasks = backend.list_by_status(*s).await?;
        counts.push((s, tasks.len()));
    }

    // Calculate total cost across all tasks
    let mut total_input_tokens: u64 = 0;
    let mut total_output_tokens: u64 = 0;
    let mut total_cost: f64 = 0.0;

    for s in &statuses {
        let tasks = backend.list_by_status(*s).await?;
        for task in tasks {
            total_input_tokens += sidecar::get_u64(&task.id.0, "input_tokens");
            total_output_tokens += sidecar::get_u64(&task.id.0, "output_tokens");
            total_cost += sidecar::get_f64(&task.id.0, "total_cost_usd");
        }
    }

    if json {
        let mut map: serde_json::Map<String, serde_json::Value> = counts
            .iter()
            .map(|(s, c)| {
                (
                    s.as_label().replace("status:", ""),
                    serde_json::Value::Number((*c).into()),
                )
            })
            .collect();

        // Add cost summary
        map.insert(
            "total_cost_usd".to_string(),
            serde_json::Value::Number(
                serde_json::Number::from_f64(total_cost).unwrap_or(serde_json::Number::from(0)),
            ),
        );
        map.insert(
            "total_input_tokens".to_string(),
            serde_json::Value::Number(total_input_tokens.into()),
        );
        map.insert(
            "total_output_tokens".to_string(),
            serde_json::Value::Number(total_output_tokens.into()),
        );
        map.insert(
            "total_tokens".to_string(),
            serde_json::Value::Number((total_input_tokens + total_output_tokens).into()),
        );

        println!("{}", serde_json::to_string_pretty(&map)?);
    } else {
        println!("{:<20} COUNT", "STATUS");
        println!("{}", "-".repeat(30));
        let total: usize = counts.iter().map(|(_, c)| c).sum();
        for (s, count) in &counts {
            if *count > 0 {
                println!("{:<20} {}", s.as_label().replace("status:", ""), count);
            }
        }
        println!("{}", "-".repeat(30));
        println!("{:<20} {}", "total", total);

        // Show cost summary if any
        if total_cost > 0.0 {
            println!();
            println!("Cost summary:");
            println!(
                "  Total tokens: {}",
                total_input_tokens + total_output_tokens
            );
            println!("  Total cost:   ${:.6}", total_cost);
        }
    }

    Ok(())
}

/// Route a task to an agent.
pub async fn route(id: i64) -> anyhow::Result<()> {
    use crate::backends::github::GitHubBackend;
    use crate::backends::ExternalBackend;

    let repo =
        config::get_current_repo().context("'repo' not set — ensure .orch.yml has gh.repo")?;
    let backend: Arc<dyn ExternalBackend> = Arc::new(GitHubBackend::new(repo));

    let ext_id = ExternalId(id.to_string());
    let task = backend.get_task(&ext_id).await?;

    let router = Router::from_config();
    let result = router.route(&task).await?;

    // Store in sidecar
    router.store_route_result(&ext_id.0, &result)?;

    // Set labels
    let labels = vec![
        format!("agent:{}", result.agent),
        format!("complexity:{}", result.complexity),
    ];
    backend.set_labels(&ext_id, &labels).await?;
    backend.update_status(&ext_id, Status::Routed).await?;

    println!(
        "Routed task #{} → {} (complexity: {}, reason: {})",
        id, result.agent, result.complexity, result.reason
    );

    Ok(())
}

/// Run a task (manual execution).
pub async fn run(id: Option<String>) -> anyhow::Result<()> {
    use crate::backends::github::GitHubBackend;
    use crate::backends::ExternalBackend;
    use crate::engine::router::get_route_result;

    let repo =
        config::get_current_repo().context("'repo' not set — ensure .orch.yml has gh.repo")?;
    let backend: Arc<dyn ExternalBackend> = Arc::new(GitHubBackend::new(repo.clone()));

    // Resolve task ID
    let task_id = match id {
        Some(id) => id,
        None => {
            // Find next routed task
            let routed = backend.list_by_status(Status::Routed).await?;
            if let Some(task) = routed.first() {
                task.id.0.clone()
            } else {
                let new = backend.list_by_status(Status::New).await?;
                if let Some(task) = new.first() {
                    task.id.0.clone()
                } else {
                    anyhow::bail!("no runnable tasks found");
                }
            }
        }
    };

    // Get routing info
    let route_result = get_route_result(&task_id).ok();
    let agent = route_result.as_ref().map(|r| r.agent.clone());
    let model = route_result.as_ref().and_then(|r| r.model.clone());

    println!(
        "Running task #{} (agent: {}, model: {})",
        task_id,
        agent.as_deref().unwrap_or("default"),
        model.as_deref().unwrap_or("default")
    );

    // Mark in progress
    let ext_id = ExternalId(task_id.clone());
    backend.update_status(&ext_id, Status::InProgress).await?;

    // Run via TaskRunner
    let runner = TaskRunner::new(repo);
    runner
        .run(&task_id, agent.as_deref(), model.as_deref())
        .await?;

    println!("Task #{} completed", task_id);
    Ok(())
}

/// Retry a task (reset to new).
pub async fn retry(id: i64) -> anyhow::Result<()> {
    use crate::backends::github::GitHubBackend;
    use crate::backends::ExternalBackend;

    let repo =
        config::get_current_repo().context("'repo' not set — ensure .orch.yml has gh.repo")?;
    let backend: Arc<dyn ExternalBackend> = Arc::new(GitHubBackend::new(repo));

    let ext_id = ExternalId(id.to_string());

    // Remove agent label
    let task = backend.get_task(&ext_id).await?;
    for label in &task.labels {
        if label.starts_with("agent:") {
            backend.remove_label(&ext_id, label).await?;
        }
    }

    // Reset sidecar state so the task starts fresh
    crate::sidecar::set(
        &ext_id.0,
        &["attempts=0".to_string(), "route_attempts=0".to_string()],
    )
    .ok();

    // Reset to new
    backend.update_status(&ext_id, Status::New).await?;

    println!(
        "Task #{} reset to new (attempts reset, will be re-routed)",
        id
    );
    Ok(())
}

/// Unblock a task or all blocked tasks.
pub async fn unblock(id: &str) -> anyhow::Result<()> {
    use crate::backends::github::GitHubBackend;
    use crate::backends::ExternalBackend;

    let repo =
        config::get_current_repo().context("'repo' not set — ensure .orch.yml has gh.repo")?;
    let backend: Arc<dyn ExternalBackend> = Arc::new(GitHubBackend::new(repo));

    if id == "all" {
        let blocked = backend.list_by_status(Status::Blocked).await?;
        let needs_review = backend.list_by_status(Status::NeedsReview).await?;

        let mut count = 0;
        for task in blocked.iter().chain(needs_review.iter()) {
            crate::sidecar::set(
                &task.id.0,
                &["attempts=0".to_string(), "route_attempts=0".to_string()],
            )
            .ok();
            backend.update_status(&task.id, Status::New).await?;
            count += 1;
        }
        println!("Unblocked {} tasks (attempts reset)", count);
    } else {
        let ext_id = ExternalId(id.to_string());
        crate::sidecar::set(
            &ext_id.0,
            &["attempts=0".to_string(), "route_attempts=0".to_string()],
        )
        .ok();
        backend.update_status(&ext_id, Status::New).await?;
        println!("Unblocked task #{} (attempts reset)", id);
    }

    Ok(())
}

/// Attach to a running agent's tmux session.
pub fn attach(id: &str) -> anyhow::Result<()> {
    let tmux = TmuxManager::new();
    let repo = crate::config::get_current_repo().unwrap_or_default();
    let session = tmux.session_name(&repo, id);
    let status = std::process::Command::new("tmux")
        .args(["attach-session", "-t", &session])
        .status_with_context()?;

    if !status.success() {
        anyhow::bail!("no active session for task {}", id);
    }
    Ok(())
}

/// List active agent tmux sessions.
pub async fn live() -> anyhow::Result<()> {
    let tmux = TmuxManager::new();
    let sessions = tmux.list_sessions().await?;

    if sessions.is_empty() {
        println!("No active agent sessions.");
        return Ok(());
    }

    println!("{:<20} {:<12} {:<10} CREATED", "SESSION", "TASK", "ACTIVE");
    println!("{}", "-".repeat(60));

    for session in &sessions {
        let active = tmux.is_session_active(&session.name).await;
        println!(
            "{:<20} {:<12} {:<10} {}",
            session.name,
            session.task_id,
            if active { "yes" } else { "no" },
            session.created_at.format("%Y-%m-%d %H:%M"),
        );
    }

    Ok(())
}

/// Kill a running agent tmux session.
pub async fn kill(id: &str) -> anyhow::Result<()> {
    let tmux = TmuxManager::new();
    let repo = crate::config::get_current_repo().unwrap_or_default();
    let session = tmux.session_name(&repo, id);
    tmux.kill_session(&session).await?;
    println!("Killed session for task #{}", id);
    Ok(())
}

/// Publish an internal task to GitHub.
pub async fn publish(id: i64, labels: Vec<String>) -> anyhow::Result<()> {
    let task_manager = init_task_manager().await?;
    let ext_id = task_manager.publish_task(id, &labels).await?;
    println!("Published task #{} as GitHub issue #{}", id, ext_id.0);
    Ok(())
}

/// Show token cost breakdown for a task.
pub fn cost(id: &str) -> anyhow::Result<()> {
    // Delegate to cli::cost::show_task for consistent formatting
    super::cost::show_task(id)
}

/// Show task tree view (parent-child relationships).
pub async fn tree(id: Option<i64>) -> anyhow::Result<()> {
    use crate::cli::tree::{build_forest, render_forest, render_single_tree};
    use crate::engine::tasks::TaskFilter;

    let task_manager = init_task_manager().await?;

    // If a specific task ID is provided, show just that tree
    if let Some(task_id) = id {
        let task = task_manager.get_task(task_id).await?;

        // For a single task, we need to find its children
        // First get all tasks to build the full hierarchy
        let all_tasks = task_manager.list_tasks(TaskFilter::default()).await?;
        let forest = build_forest(all_tasks);

        // Find the requested task in the forest
        fn find_node_in_forest<'a>(
            forest: &'a [crate::cli::tree::TreeNode],
            id: &str,
        ) -> Option<&'a crate::cli::tree::TreeNode> {
            for root in forest {
                if root.id == id {
                    return Some(root);
                }
                if let Some(found) = find_node_in_forest(&root.children, id) {
                    return Some(found);
                }
            }
            None
        }

        let target_id = task_id.to_string();
        if let Some(node) = find_node_in_forest(&forest, &target_id) {
            let output = render_single_tree(node, true);
            print!("{}", output);
        } else {
            // Task exists but not in any tree - show as standalone
            match &task {
                Task::External(ext) => {
                    let node = crate::cli::tree::TreeNode::from_external(ext);
                    let output = render_single_tree(&node, true);
                    print!("{}", output);
                }
                Task::Internal(int) => {
                    let node = crate::cli::tree::TreeNode::from_internal(int);
                    let output = render_single_tree(&node, true);
                    print!("{}", output);
                }
            }
        }
        return Ok(());
    }

    // No ID provided - show all root tasks
    let filter = TaskFilter::default();
    let tasks = task_manager.list_tasks(filter).await?;

    if tasks.is_empty() {
        println!("No tasks found.");
        return Ok(());
    }

    let forest = build_forest(tasks);
    let output = render_forest(&forest);
    print!("{}", output);

    Ok(())
}
