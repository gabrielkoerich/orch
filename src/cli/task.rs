use crate::backends::{ExternalId, Status};
use crate::cli::init_task_manager;
use crate::cmd::SyncCommandErrorContext;
use crate::config;
use crate::engine::router::Router;
use crate::engine::runner::TaskRunner;
use crate::engine::tasks::{parse_internal_id, CreateTaskRequest, Task, TaskFilter, TaskType};
use crate::home;
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

    println!(
        "{:<15} {:<12} {:<20} {:<10} TITLE",
        "ID", "TYPE", "STATUS", "AGENT"
    );
    println!("{}", "-".repeat(90));

    for task in tasks {
        match task {
            Task::External(ext) => {
                let status = ext
                    .labels
                    .iter()
                    .find(|l| l.starts_with("status:"))
                    .map(|s| s.replace("status:", ""))
                    .unwrap_or_else(|| "unknown".to_string());
                let agent = sidecar::get(&ext.id.0, "agent").unwrap_or_default();
                println!(
                    "{:<15} {:<12} {:<20} {:<10} {}",
                    ext.id.0, "external", status, agent, ext.title
                );
            }
            Task::Internal(int) => {
                let agent = int.agent.as_deref().unwrap_or("-");
                let title = if int.status == crate::db::TaskStatus::Blocked {
                    if let Some(ref reason) = int.block_reason {
                        format!("{} [blocked: {}]", int.title, reason)
                    } else {
                        int.title.clone()
                    }
                } else {
                    int.title.clone()
                };
                println!(
                    "{:<15} {:<12} {:<20} {:<10} {}",
                    format!("internal:{}", int.id),
                    "internal",
                    int.status.as_str(),
                    agent,
                    title
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
    use crate::db::TaskStatus;

    let task_manager = init_task_manager().await?;

    let statuses = [
        Status::New,
        Status::Routed,
        Status::InProgress,
        Status::Done,
        Status::Blocked,
        Status::InReview,
        Status::NeedsReview,
    ];

    // Fetch all tasks from both backends
    let all_external = task_manager.list_all_external_tasks().await?;
    let all_internal = task_manager.db.list_all_internal_tasks().await?;

    // Count external tasks per status via labels
    let ext_counts: Vec<usize> = statuses
        .iter()
        .map(|s| {
            let label = s.as_label().to_string();
            all_external
                .iter()
                .filter(|t| t.labels.contains(&label))
                .count()
        })
        .collect();

    // Count internal tasks per status
    let int_counts: Vec<usize> = statuses
        .iter()
        .map(|s| {
            let ts = match s {
                Status::New => TaskStatus::New,
                Status::Routed => TaskStatus::Routed,
                Status::InProgress => TaskStatus::InProgress,
                Status::Done => TaskStatus::Done,
                Status::Blocked => TaskStatus::Blocked,
                Status::InReview => TaskStatus::InReview,
                Status::NeedsReview => TaskStatus::NeedsReview,
            };
            all_internal.iter().filter(|t| t.status == ts).count()
        })
        .collect();

    // Calculate total cost across all external tasks
    let mut total_input_tokens: u64 = 0;
    let mut total_output_tokens: u64 = 0;
    let mut total_cost: f64 = 0.0;

    for task in &all_external {
        total_input_tokens += sidecar::get_u64(&task.id.0, "input_tokens");
        total_output_tokens += sidecar::get_u64(&task.id.0, "output_tokens");
        total_cost += sidecar::get_f64(&task.id.0, "total_cost_usd");
    }
    for task in &all_internal {
        let id = format!("internal:{}", task.id);
        total_input_tokens += sidecar::get_u64(&id, "input_tokens");
        total_output_tokens += sidecar::get_u64(&id, "output_tokens");
        total_cost += sidecar::get_f64(&id, "total_cost_usd");
    }

    if json {
        let mut map = serde_json::Map::new();
        for (i, s) in statuses.iter().enumerate() {
            let key = s.as_label().replace("status:", "");
            let mut entry = serde_json::Map::new();
            entry.insert(
                "external".to_string(),
                serde_json::Value::Number(ext_counts[i].into()),
            );
            entry.insert(
                "internal".to_string(),
                serde_json::Value::Number(int_counts[i].into()),
            );
            map.insert(key, serde_json::Value::Object(entry));
        }

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
        println!("{:<20} {:>8}  {:>8}", "STATUS", "EXTERNAL", "INTERNAL");
        println!("{}", "-".repeat(42));
        let total_ext: usize = ext_counts.iter().sum();
        let total_int: usize = int_counts.iter().sum();
        for (i, s) in statuses.iter().enumerate() {
            let ext = ext_counts[i];
            let int = int_counts[i];
            if ext > 0 || int > 0 {
                println!(
                    "{:<20} {:>8}  {:>8}",
                    s.as_label().replace("status:", ""),
                    ext,
                    int
                );
            }
        }
        println!("{}", "-".repeat(42));
        println!("{:<20} {:>8}  {:>8}", "total", total_ext, total_int);

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
        .run(
            &task_id,
            agent.as_deref(),
            model.as_deref(),
            Some(&*backend),
        )
        .await?;

    println!("Task #{} completed", task_id);
    Ok(())
}

/// Retry a task (reset to new).
pub async fn retry(id: i64) -> anyhow::Result<()> {
    use crate::backends::github::GitHubBackend;
    use crate::backends::ExternalBackend;
    use crate::db::{Db, TaskStatus};
    use crate::home;

    // Check if this is an internal task in the DB first.
    let db_path = home::db_path().context("could not resolve DB path")?;
    let db = Db::open(&db_path)?;
    if let Ok(task) = db.get_internal_task(id).await {
        let internal_id = format!("internal:{}", task.id);
        // Reset sidecar
        crate::sidecar::set(
            &internal_id,
            &["attempts=0".to_string(), "route_attempts=0".to_string()],
        )
        .ok();
        // Reset DB status to New
        db.update_internal_task_status(id, TaskStatus::New).await?;
        println!(
            "Task #{} reset to new (attempts reset, will be re-routed)",
            id
        );
        return Ok(());
    }

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

async fn reset_internal_sidecar(id: i64) {
    let internal_id = format!("internal:{}", id);
    crate::sidecar::set(
        &internal_id,
        &["attempts=0".to_string(), "route_attempts=0".to_string()],
    )
    .ok();
}

/// Unblock a task or all blocked tasks.
pub async fn unblock(id: &str) -> anyhow::Result<()> {
    use crate::backends::github::GitHubBackend;
    use crate::backends::ExternalBackend;
    use crate::db::{Db, TaskStatus};
    use crate::home;

    let repo =
        config::get_current_repo().context("'repo' not set — ensure .orch.yml has gh.repo")?;
    let backend: Arc<dyn ExternalBackend> = Arc::new(GitHubBackend::new(repo));
    let db_path = home::db_path().context("could not resolve DB path")?;
    let db = Db::open(&db_path)?;

    if id == "all" {
        let blocked = backend.list_by_status(Status::Blocked).await?;
        let needs_review = backend.list_by_status(Status::NeedsReview).await?;

        let mut external_count = 0;
        for task in blocked.iter().chain(needs_review.iter()) {
            crate::sidecar::set(
                &task.id.0,
                &["attempts=0".to_string(), "route_attempts=0".to_string()],
            )
            .ok();
            backend.update_status(&task.id, Status::New).await?;
            external_count += 1;
        }

        let internal_blocked = db
            .list_internal_tasks_by_status(TaskStatus::Blocked)
            .await?;
        let internal_needs_review = db
            .list_internal_tasks_by_status(TaskStatus::NeedsReview)
            .await?;
        let mut internal_count = 0;
        for task in internal_blocked.iter().chain(internal_needs_review.iter()) {
            reset_internal_sidecar(task.id).await;
            db.update_internal_task_status(task.id, TaskStatus::New)
                .await?;
            internal_count += 1;
        }

        let total = external_count + internal_count;
        println!(
            "Unblocked {} tasks ({} external, {} internal) (attempts reset)",
            total, external_count, internal_count
        );
        return Ok(());
    }

    if let Some(stripped) = id.strip_prefix("internal:") {
        let parsed = stripped
            .parse::<i64>()
            .with_context(|| format!("internal task id '{}' is not numeric", stripped))?;
        db.get_internal_task(parsed).await?;
        reset_internal_sidecar(parsed).await;
        db.update_internal_task_status(parsed, TaskStatus::New)
            .await?;
        println!(
            "Unblocked internal task #{} (attempts reset, will be re-routed)",
            parsed
        );
        return Ok(());
    }

    if let Ok(parsed) = id.parse::<i64>() {
        if db.get_internal_task(parsed).await.is_ok() {
            reset_internal_sidecar(parsed).await;
            db.update_internal_task_status(parsed, TaskStatus::New)
                .await?;
            println!(
                "Unblocked internal task #{} (attempts reset, will be re-routed)",
                parsed
            );
            return Ok(());
        }
    }

    let ext_id = ExternalId(id.to_string());
    crate::sidecar::set(
        &ext_id.0,
        &["attempts=0".to_string(), "route_attempts=0".to_string()],
    )
    .ok();
    backend.update_status(&ext_id, Status::New).await?;
    println!("Unblocked task #{} (attempts reset)", id);

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

    println!("{:<30} {:<15} {:<10} ACTIVE", "SESSION", "TASK", "AGENT");
    println!("{}", "-".repeat(70));

    for session in &sessions {
        let active = tmux.is_session_active(&session.name).await;
        let agent = sidecar::get(&session.task_id, "agent").unwrap_or_default();
        println!(
            "{:<30} {:<15} {:<10} {}",
            session.name,
            session.task_id,
            agent,
            if active { "yes" } else { "no" },
        );
    }

    Ok(())
}

/// Kill a running agent tmux session.
pub async fn kill(id: &str) -> anyhow::Result<()> {
    use crate::backends::github::GitHubBackend;
    use crate::backends::ExternalBackend;
    use crate::db::{Db, TaskStatus};

    let tmux = TmuxManager::new();
    let repo = crate::config::get_current_repo().unwrap_or_default();
    let session = tmux.session_name(&repo, id);
    tmux.kill_session(&session).await?;
    println!("Killed session for task #{}", id);

    // Immediately reset the task status to New so the engine can re-dispatch it
    // without waiting for the no_session_stuck_timeout (default 10 min).
    if let Some(n) = parse_internal_id(id) {
        let db_path = home::db_path().context("could not resolve DB path")?;
        let db = Db::open(&db_path)?;
        let internal_id = format!("internal:{}", n);
        sidecar::set(
            &internal_id,
            &["attempts=0".to_string(), "route_attempts=0".to_string()],
        )
        .ok();
        db.update_internal_task_status(n, TaskStatus::New).await?;
        println!("Task #{} reset to new (will be re-dispatched)", n);
    } else if let Ok(num) = id.parse::<i64>() {
        // Try internal DB first, then fall back to external
        let db_path = home::db_path().context("could not resolve DB path")?;
        let db = Db::open(&db_path)?;
        if db.get_internal_task(num).await.is_ok() {
            let internal_id = format!("internal:{}", num);
            sidecar::set(
                &internal_id,
                &["attempts=0".to_string(), "route_attempts=0".to_string()],
            )
            .ok();
            db.update_internal_task_status(num, TaskStatus::New).await?;
            println!("Task #{} reset to new (will be re-dispatched)", num);
        } else {
            let ext_id = ExternalId(id.to_string());
            let backend: Arc<dyn ExternalBackend> = Arc::new(GitHubBackend::new(repo));
            sidecar::set(
                &ext_id.0,
                &["attempts=0".to_string(), "route_attempts=0".to_string()],
            )
            .ok();
            backend.update_status(&ext_id, Status::New).await?;
            println!("Task #{} reset to new (will be re-dispatched)", id);
        }
    } else {
        // Non-numeric external id
        let ext_id = ExternalId(id.to_string());
        let backend: Arc<dyn ExternalBackend> = Arc::new(GitHubBackend::new(repo));
        sidecar::set(
            &ext_id.0,
            &["attempts=0".to_string(), "route_attempts=0".to_string()],
        )
        .ok();
        backend.update_status(&ext_id, Status::New).await?;
        println!("Task #{} reset to new (will be re-dispatched)", id);
    }

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

/// Show logs / post-mortem for a task (internal or external).
pub async fn logs(id: &str) -> anyhow::Result<()> {
    let task_manager = init_task_manager().await?;

    // Resolve sidecar key and print basic metadata where available.
    let mut sidecar_key = id.to_string();

    // If user passed `internal:N` or the task resolves to an internal task,
    // prefer the internal sidecar key format `internal:{n}` so we read the
    // correct file.
    if let Some(n) = parse_internal_id(id) {
        sidecar_key = format!("internal:{}", n);
        // Fetch internal task metadata
        if let Ok(internal) = task_manager.db.get_internal_task(n).await {
            println!("ID: {} (internal)", internal.id);
            println!("Title: {}", internal.title);
            println!("Status: {}", internal.status.as_str());
            if let Some(agent) = &internal.agent {
                println!("Agent: {}", agent);
            }
            if let Some(reason) = &internal.block_reason {
                println!("Block reason: {}", reason);
            }
            println!("Created: {}", internal.created_at.to_rfc3339());
            println!("Updated: {}", internal.updated_at.to_rfc3339());
        }
    } else if let Ok(num) = id.parse::<i64>() {
        // Numeric ID: try DB first (TaskManager::get_task will check both)
        match task_manager.get_task(num).await {
            Ok(Task::External(ext)) => {
                println!("ID: {} (external)", ext.id.0);
                println!("Title: {}", ext.title);
                println!("State: {}", ext.state);
                println!("Labels: {}", ext.labels.join(", "));
                println!("Author: {}", ext.author);
                println!("URL: {}", ext.url);
                println!("Created: {}", ext.created_at);
                println!("Updated: {}", ext.updated_at);
                sidecar_key = ext.id.0.clone();
            }
            Ok(Task::Internal(int)) => {
                println!("ID: {} (internal)", int.id);
                println!("Title: {}", int.title);
                println!("Status: {}", int.status.as_str());
                if let Some(agent) = &int.agent {
                    println!("Agent: {}", agent);
                }
                println!("Created: {}", int.created_at.to_rfc3339());
                println!("Updated: {}", int.updated_at.to_rfc3339());
                sidecar_key = format!("internal:{}", int.id);
            }
            Err(e) => {
                // Could not resolve task metadata; continue and try sidecar only
                println!("ID: {}", id);
                println!("(could not resolve task metadata: {})", e);
            }
        }
    } else {
        // Non-numeric external id (rare) — use as-is for sidecar lookup
        println!("ID: {}", id);
    }

    // Sidecar path and existence
    match sidecar::sidecar_path(&sidecar_key) {
        Ok(path) => {
            if !path.exists() {
                println!(
                    "\nNo sidecar found for task {} — looked at: {}",
                    sidecar_key,
                    path.display()
                );
            } else {
                println!("\nSidecar: {}", path.display());

                // Agent & model
                if let Ok(agent) = sidecar::get(&sidecar_key, "agent") {
                    println!("Agent: {}", agent);
                }
                let model = sidecar::get_model(&sidecar_key);
                if !model.is_empty() {
                    println!("Model: {}", model);
                }

                // Attempts
                if let Ok(attempts) = sidecar::get(&sidecar_key, "attempts") {
                    println!("Attempts: {}", attempts);
                }

                // Token & cost summary
                let usage = sidecar::get_token_usage(&sidecar_key);
                let cost = sidecar::get_cost_estimate(&sidecar_key);
                let total_tokens = usage.total_tokens();
                if total_tokens > 0 || cost.total_cost_usd > 0.0 {
                    println!("\nCost summary:");
                    println!("  input tokens:  {}", usage.input_tokens);
                    println!("  output tokens: {}", usage.output_tokens);
                    println!("  total tokens:  {}", total_tokens);
                    println!("  estimated $:   ${:.6}", cost.total_cost_usd);
                }

                // Memory (recent attempts)
                if let Ok(mem) = sidecar::get_recent_memory(&sidecar_key, 10) {
                    if !mem.is_empty() {
                        println!("\nMemory (recent attempts):");
                        for m in mem {
                            println!(
                                "- Attempt {} @ {}: agent={} model={}",
                                m.attempt,
                                m.timestamp,
                                m.agent,
                                m.model.clone().unwrap_or_default()
                            );
                            if let Some(err) = m.error.as_ref() {
                                println!("    Error: {}", err);
                            }
                            if !m.learnings.is_empty() {
                                println!("    Learnings:");
                                for l in &m.learnings {
                                    println!("      - {}", l);
                                }
                            }
                            if !m.files_modified.is_empty() {
                                println!("    Files modified:");
                                for f in &m.files_modified {
                                    println!("      - {}", f);
                                }
                            }
                        }
                    }
                }
            }
        }
        Err(e) => {
            println!(
                "\nCould not resolve sidecar path for {}: {}",
                sidecar_key, e
            );
        }
    }

    // Show agent output from last attempt if available
    let repo = config::get_current_repo().unwrap_or_default();
    if let Ok(attempts_str) = sidecar::get(&sidecar_key, "attempts") {
        if let Ok(attempt_n) = attempts_str.parse::<u32>() {
            if attempt_n > 0 {
                match home::task_attempt_dir(&repo, &sidecar_key, attempt_n) {
                    Ok(attempt_dir) => {
                        let output_file = attempt_dir.join("output.json");
                        let exit_file = attempt_dir.join("exit.txt");

                        if output_file.exists() {
                            if let Ok(content) = std::fs::read_to_string(&output_file) {
                                let tail = crate::engine::runner::response_handler::safe_utf8_tail(
                                    &content, 2000,
                                );
                                let truncated = if content.len() > 2000 {
                                    format!(
                                        "...({} chars truncated)...\n{}",
                                        content.len() - tail.len(),
                                        tail
                                    )
                                } else {
                                    content.clone()
                                };
                                println!("\n--- Last attempt output (attempt {}) ---", attempt_n);
                                println!("{}", truncated);
                            }
                        }

                        if exit_file.exists() {
                            if let Ok(exit_code) = std::fs::read_to_string(&exit_file) {
                                println!("Exit code: {}", exit_code.trim());
                            }
                        }
                    }
                    Err(e) => {
                        println!("(could not resolve attempt dir: {})", e);
                    }
                }
            }
        }
    }

    // Show review agent output if available
    let review_task_id = format!("{}-review", sidecar_key);
    if let Ok(review_attempt_dir) = home::task_attempt_dir(&repo, &review_task_id, 1) {
        let review_output_file = review_attempt_dir.join("output.json");
        if review_output_file.exists() {
            if let Ok(content) = std::fs::read_to_string(&review_output_file) {
                println!("\n--- Review agent output ---");
                println!("{}", content);
            }
        }
    }

    // If tmux session is live, append recent pane capture
    let tmux = TmuxManager::new();
    let session = tmux.session_name(&repo, &sidecar_key);
    if tmux.session_exists(&session).await {
        println!(
            "\nLive tmux session detected: {} — appending recent output:\n---",
            session
        );
        match tmux.capture_pane(&session, 200).await {
            Ok(output) => {
                println!("{}", output);
            }
            Err(e) => {
                println!("(tmux capture failed: {})", e);
            }
        }
    }

    Ok(())
}
