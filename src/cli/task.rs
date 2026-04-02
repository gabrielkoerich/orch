use crate::backends::{ExternalBackend, ExternalId, Status};
use crate::cli::init_task_manager;
use crate::cmd::SyncCommandErrorContext;
use crate::config;
use crate::engine::router::Router;
use crate::engine::runner::TaskRunner;
use crate::engine::tasks::{
    parse_internal_id, status_to_task_status, CreateTaskRequest, Task, TaskFilter, TaskType,
};
use crate::home;
use crate::store;
use crate::store::TaskStore;
use crate::tmux::TmuxManager;
use anyhow::Context;
use chrono::{DateTime, Utc};
use futures::future::join_all;
use std::sync::Arc;

/// Format a timestamp as a human-readable age (e.g. "5m", "2h", "3d").
fn format_age(updated_at: &str) -> String {
    let Ok(dt) = DateTime::parse_from_rfc3339(updated_at) else {
        return "-".to_string();
    };
    let secs = (Utc::now() - dt.with_timezone(&Utc)).num_seconds();
    if secs < 0 {
        return "now".to_string();
    }
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        let days = secs / 86400;
        if days >= 365 {
            let years = days / 365;
            format!("{}y", years)
        } else {
            format!("{}d", days)
        }
    }
}

/// Truncate an error message to a maximum length, appending an ellipsis when truncated.
fn truncate_err(err: &str, max: usize) -> String {
    if err.len() > max {
        format!("{}...", &err[..max])
    } else {
        err.to_string()
    }
}

fn format_run_duration(duration_secs: f64) -> String {
    if duration_secs < 60.0 {
        format!("{duration_secs:.0}s")
    } else {
        format!("{:.1}m", duration_secs / 60.0)
    }
}

fn format_run_cost(cost: f64) -> String {
    if cost <= 0.0 {
        "-".to_string()
    } else {
        format!("${cost:.2}")
    }
}

fn format_run_text(text: &str, verbose: bool, max: usize) -> String {
    if verbose {
        text.to_string()
    } else if text.is_empty() {
        String::new()
    } else {
        truncate_err(text, max)
    }
}

/// Store-first status update for CLI: updates SQLite first, then mirrors to backend.
async fn update_status_store_first(
    store: &Option<Arc<TaskStore>>,
    backend: &Arc<dyn ExternalBackend>,
    repo: &str,
    id: &ExternalId,
    status: Status,
) -> anyhow::Result<()> {
    let db_status = status_to_task_status(status);
    if let Some(ref s) = store {
        if let Ok(Some(store_id)) = s.resolve_task_id(repo, &id.0).await {
            s.update_status(store_id, db_status).await?;
        }
    }
    if let Err(e) = backend.update_status(id, status).await {
        tracing::warn!(
            task_id = id.0,
            ?status,
            err = %e,
            "failed to mirror status to backend — store is authoritative"
        );
    }
    Ok(())
}

fn matches_project_filter(repo: &str, project: &str) -> bool {
    let repo_name = repo.rsplit('/').next().unwrap_or(repo);
    repo == project || repo_name == project || repo.ends_with(&format!("/{project}"))
}

/// List tasks with optional filters.
pub async fn list(
    status: Option<String>,
    source: Option<String>,
    global: bool,
    project: Option<String>,
) -> anyhow::Result<()> {
    // When no project context is available (e.g. running from a worktree without
    // .orch.yml), fall back to a store-only listing across all configured projects
    // instead of printing a fatal error.
    if global || project.is_some() {
        return list_from_global_store(status, source, project.as_deref()).await;
    }

    let task_manager = match init_task_manager().await {
        Ok(tm) => tm,
        Err(e) => {
            tracing::debug!("no project context available, falling back to global store: {e:#}");
            return list_from_global_store(status, source, project.as_deref()).await;
        }
    };
    let filter = TaskFilter { status, source };
    let tasks = task_manager.list_tasks(filter).await?;

    if tasks.is_empty() {
        println!("No tasks found.");
        return Ok(());
    }

    let store: Option<Arc<TaskStore>> = crate::cli::init_store().await.ok().map(Arc::new);
    let repo = config::get_current_repo().unwrap_or_default();

    // Preload all store tasks for this repo into a map keyed by external_id.
    // This avoids 4 SQL queries per external task (resolve_id + get, twice for agent and attempts).
    let store_by_ext_id: std::collections::HashMap<String, crate::store::Task> =
        if let Some(ref s) = store {
            s.list_all(&repo)
                .await
                .unwrap_or_default()
                .into_iter()
                .filter_map(|t| t.external_id.clone().map(|eid| (eid, t)))
                .collect()
        } else {
            std::collections::HashMap::new()
        };

    println!(
        "{:<15} {:<12} {:<20} {:<24} {:<10} {:<6} {:<5} TITLE",
        "ID", "TYPE", "STATUS", "PROJECT", "AGENT", "AGE", "TRIES"
    );
    println!("{}", "-".repeat(125));

    for task in tasks {
        match task {
            Task::External(ext) => {
                let status = ext
                    .labels
                    .iter()
                    .find(|l| l.starts_with("status:"))
                    .map(|s| s.replace("status:", ""))
                    .unwrap_or_else(|| "unknown".to_string());
                let store_task = store_by_ext_id.get(&ext.id.0);
                let agent = store_task
                    .and_then(|t| t.agent.as_deref())
                    .unwrap_or("")
                    .to_string();
                let age = format_age(&ext.updated_at);
                let project = store_task.map(|t| t.repo.as_str()).unwrap_or(&repo);
                let tries = store_task
                    .map(|t| {
                        if t.attempts > 0 {
                            t.attempts.to_string()
                        } else {
                            String::new()
                        }
                    })
                    .unwrap_or_default();
                println!(
                    "{:<15} {:<12} {:<20} {:<24} {:<10} {:<6} {:<5} {}",
                    ext.id.0, "external", status, project, agent, age, tries, ext.title
                );
            }
            Task::Internal(int) => {
                let agent = int.agent.as_deref().unwrap_or("-");
                let age = format_age(&int.updated_at);
                let project = &int.repo;
                let tries = if int.attempts > 0 {
                    int.attempts.to_string()
                } else {
                    "-".to_string()
                };
                let title = if int.status == crate::store::TaskStatus::Blocked {
                    if let Some(ref reason) = int.block_reason {
                        format!("{} [blocked: {}]", int.title, reason)
                    } else {
                        int.title.clone()
                    }
                } else {
                    int.title.clone()
                };
                println!(
                    "{:<15} {:<12} {:<20} {:<24} {:<10} {:<6} {:<5} {}",
                    format!("internal:{}", int.id),
                    "internal",
                    int.status.as_str(),
                    project,
                    agent,
                    age,
                    tries,
                    title
                );
            }
        }
    }

    Ok(())
}

/// Fallback listing used when no project context is available (no `.orch.yml` in CWD or parents).
///
/// Queries the global SQLite store directly without scoping to a specific repo.
/// This suppresses the fatal "no valid projects" error that previously appeared
/// when agents ran `orch task list` from inside a worktree.
async fn list_from_global_store(
    status: Option<String>,
    source: Option<String>,
    project: Option<&str>,
) -> anyhow::Result<()> {
    let store = match crate::cli::init_store().await {
        Ok(s) => s,
        Err(_) => {
            println!("No tasks found.");
            return Ok(());
        }
    };

    let tasks = if let Some(ref status_str) = status {
        let task_status =
            crate::store::TaskStatus::from_str(status_str).unwrap_or(crate::store::TaskStatus::New);
        store.list_all_by_status_global(task_status).await?
    } else {
        store.list_all_active_global().await?
    };

    // Apply optional source filter.
    let tasks: Vec<_> = tasks
        .into_iter()
        .filter(|t| source.as_ref().map(|s| &t.source == s).unwrap_or(true))
        .filter(|t| {
            project
                .map(|p| matches_project_filter(&t.repo, p))
                .unwrap_or(true)
        })
        .collect();

    if tasks.is_empty() {
        println!("No tasks found.");
        return Ok(());
    }

    println!(
        "{:<15} {:<12} {:<20} {:<24} {:<10} {:<6} {:<5} TITLE",
        "ID", "TYPE", "STATUS", "PROJECT", "AGENT", "AGE", "TRIES"
    );
    println!("{}", "-".repeat(125));

    for task in tasks {
        let type_str = if task.origin == "internal" {
            "internal"
        } else {
            "external"
        };
        let project = task.repo.as_str();
        let id_str = if task.origin == "internal" {
            format!("internal:{}", task.id)
        } else {
            task.external_id
                .clone()
                .unwrap_or_else(|| task.id.to_string())
        };
        let agent = task.agent.as_deref().unwrap_or("-");
        let age = format_age(&task.updated_at);
        let tries = if task.attempts > 0 {
            task.attempts.to_string()
        } else {
            "-".to_string()
        };
        let title = if task.status == crate::store::TaskStatus::Blocked {
            if let Some(ref reason) = task.block_reason {
                format!("{} [blocked: {}]", task.title, reason)
            } else {
                task.title.clone()
            }
        } else {
            task.title.clone()
        };
        println!(
            "{:<15} {:<12} {:<20} {:<24} {:<10} {:<6} {:<5} {}",
            id_str,
            type_str,
            task.status.as_str(),
            project,
            agent,
            age,
            tries,
            title
        );
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

    let store: Option<Arc<TaskStore>> = crate::cli::init_store().await.ok().map(Arc::new);
    let repo = config::get_current_repo().unwrap_or_default();

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

            // Load all store fields in a single DB round-trip.
            if let Some(ref s) = store {
                if let Ok(Some(t)) = s.get_by_external_id(&repo, &ext.id.0).await {
                    if let Some(ref agent) = t.agent {
                        println!("Agent: {}", agent);
                    }
                    if let Some(ref model) = t.model {
                        println!("Model: {}", model);
                    }
                    if !t.complexity.is_empty() {
                        println!("Complexity: {}", t.complexity);
                    }
                    if t.route_attempts > 0 {
                        println!("Route attempts: {}", t.route_attempts);
                    }
                    if t.attempts > 0 {
                        println!("Attempts: {}", t.attempts);
                    }
                    if t.review_cycles > 0 {
                        println!("Review cycles: {}", t.review_cycles);
                    }
                    if let Some(pr) = t.pr_number {
                        if pr > 0 {
                            println!("PR: #{}", pr);
                        }
                    }
                    if !t.branch.is_empty() {
                        println!("Branch: {}", t.branch);
                    }
                    if !t.last_error.is_empty() {
                        println!("Last error: {}", truncate_err(&t.last_error, 200));
                    }
                }
            }
            // Cost summary (if any tokens recorded)
            let usage = store::get_token_usage(&store, &repo, &ext.id.0).await;
            let cost = store::get_cost_estimate(&store, &repo, &ext.id.0).await;
            if usage.total_tokens() > 0 || cost.total_cost_usd > 0.0 {
                println!(
                    "Tokens: {} in / {} out — ${:.6}",
                    usage.input_tokens, usage.output_tokens, cost.total_cost_usd
                );
            }

            println!("\n{}", ext.body);
        }
        Task::Internal(int) => {
            println!("ID: {} (internal)", int.id);
            println!("Title: {}", int.title);
            println!("Status: {}", int.status.as_str());
            println!("Source: {}", int.source);
            if let Some(model) = &int.model {
                println!("Model: {}", model);
            }
            if !int.complexity.is_empty() {
                println!("Complexity: {}", int.complexity);
            }
            if let Some(reason) = &int.block_reason {
                println!("Block reason: {}", reason);
            }
            // Routing & execution counters
            if int.route_attempts > 0 {
                println!("Route attempts: {}", int.route_attempts);
            }
            if int.attempts > 0 {
                println!("Attempts: {}", int.attempts);
            }
            if int.review_cycles > 0 {
                println!("Review cycles: {}", int.review_cycles);
            }
            // PR info
            if let Some(pr) = int.pr_number {
                println!("PR: #{}", pr);
            }
            // Branch / worktree
            if !int.branch.is_empty() {
                println!("Branch: {}", int.branch);
            }
            // Last error (truncated to 200 chars)
            if !int.last_error.is_empty() {
                let err = truncate_err(&int.last_error, 200);
                println!("Last error: {}", err);
            }
            // Parent
            if let Some(parent) = int.parent_id {
                println!("Parent: internal:{}", parent);
            }
            // Cost summary (if any tokens recorded)
            if int.input_tokens > 0 || int.output_tokens > 0 {
                println!(
                    "Tokens: {} in / {} out — ${:.6}",
                    int.input_tokens, int.output_tokens, int.total_cost_usd
                );
            }
            println!("Created: {}", int.created_at);
            println!("Updated: {}", int.updated_at);
            println!("\n{}", int.body);
        }
    }

    Ok(())
}

/// Show task status summary.
pub async fn status(json: bool) -> anyhow::Result<()> {
    use crate::store::TaskStatus;

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
    let all_internal = task_manager.list_all_internal().await.unwrap_or_default();

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

    // Calculate total cost using a single aggregate query instead of per-task lookups.
    let store: Option<Arc<TaskStore>> = crate::cli::init_store().await.ok().map(Arc::new);
    let repo = config::get_current_repo().unwrap_or_default();
    let mut total_input_tokens: u64 = 0;
    let mut total_output_tokens: u64 = 0;
    let mut total_cost: f64 = 0.0;

    if let Some(ref s) = store {
        if let Ok((input, output, cost)) = s.cost_summary(&repo).await {
            total_input_tokens = input as u64;
            total_output_tokens = output as u64;
            total_cost = cost;
        }
    } else {
        // No store available: sum up internal task costs directly from already-loaded data.
        for task in &all_internal {
            total_input_tokens += task.input_tokens as u64;
            total_output_tokens += task.output_tokens as u64;
            total_cost += task.total_cost_usd;
        }
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
    let backend: Arc<dyn ExternalBackend> = Arc::new(GitHubBackend::new(repo.clone())?);
    let store: Arc<TaskStore> = Arc::new(crate::cli::init_store().await?);

    let ext_id = ExternalId(id.to_string());
    let task = backend.get_task(&ext_id).await?;

    let mut router = Router::from_config();
    let result = router.route(&task, &store, &repo).await?;

    // Store route result
    router
        .store_route_result(&ext_id.0, &result, &store, &repo)
        .await?;

    // Set labels
    let labels = vec![
        format!("agent:{}", result.agent),
        format!("complexity:{}", result.complexity),
    ];
    backend.set_labels(&ext_id, &labels).await?;
    update_status_store_first(&Some(store), &backend, &repo, &ext_id, Status::Routed).await?;

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
    let backend: Arc<dyn ExternalBackend> = Arc::new(GitHubBackend::new(repo.clone())?);
    let store: Arc<TaskStore> = Arc::new(crate::cli::init_store().await?);

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
    let route_result = get_route_result(&store, &repo, &task_id).await.ok();
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
    let started_at = Utc::now();
    update_status_store_first(
        &Some(store.clone()),
        &backend,
        &repo,
        &ext_id,
        Status::InProgress,
    )
    .await?;

    // Run via TaskRunner (with store for audit trail + token tracking)
    let runner = TaskRunner::new(repo).with_store(store);
    runner
        .run(
            &task_id,
            agent.as_deref(),
            model.as_deref(),
            Some(&*backend),
            &started_at,
        )
        .await?;

    println!("Task #{} completed", task_id);
    Ok(())
}

/// Retry a task (reset to new).
pub async fn retry(id: i64) -> anyhow::Result<()> {
    use crate::backends::github::GitHubBackend;
    use crate::backends::ExternalBackend;
    use crate::store::TaskStatus;

    let store = crate::cli::init_store().await.ok().map(std::sync::Arc::new);
    let repo = config::get_current_repo().unwrap_or_default();

    // Check if this is an internal task in the store.
    if let Some(ref s) = store {
        if let Ok(task) = s.get(id).await {
            if task.origin == "internal" {
                let internal_id = task
                    .external_id
                    .unwrap_or_else(|| format!("internal:{}", task.id));
                // Reset store counters
                store::store_reset_counters(&store, &repo, &internal_id).await;
                // Reset status to New
                s.update_status(id, TaskStatus::New).await?;
                println!(
                    "Task #{} reset to new (attempts reset, will be re-routed)",
                    id
                );
                return Ok(());
            }
        }
    }

    let backend: Arc<dyn ExternalBackend> = Arc::new(GitHubBackend::new(repo.clone())?);

    let ext_id = ExternalId(id.to_string());

    // Remove agent label
    let task = backend.get_task(&ext_id).await?;
    for label in &task.labels {
        if label.starts_with("agent:") {
            backend.remove_label(&ext_id, label).await?;
        }
    }

    // Reset store state (attempts + all failure counters)
    store::store_reset_counters(&store, &repo, &ext_id.0).await;

    // Reset to new
    update_status_store_first(&store, &backend, &repo, &ext_id, Status::New).await?;

    println!(
        "Task #{} reset to new (attempts reset, will be re-routed)",
        id
    );
    Ok(())
}

async fn reset_counters(
    id: i64,
    store: &Option<std::sync::Arc<crate::store::TaskStore>>,
    repo: &str,
) {
    let internal_id = format!("internal:{}", id);
    store::store_reset_counters(store, repo, &internal_id).await;
}

/// Unblock a task or all blocked tasks.
pub async fn unblock(id: &str) -> anyhow::Result<()> {
    use crate::backends::github::GitHubBackend;
    use crate::backends::ExternalBackend;
    use crate::store::TaskStatus;

    let repo =
        config::get_current_repo().context("'repo' not set — ensure .orch.yml has gh.repo")?;
    let backend: Arc<dyn ExternalBackend> = Arc::new(GitHubBackend::new(repo.clone())?);
    let store = crate::cli::init_store().await.ok().map(std::sync::Arc::new);

    if id == "all" {
        let blocked = backend.list_by_status(Status::Blocked).await?;
        let needs_review = backend.list_by_status(Status::NeedsReview).await?;

        let tasks_to_unblock: Vec<_> = blocked.iter().chain(needs_review.iter()).collect();
        let external_count = tasks_to_unblock.len();
        let ext_futs = tasks_to_unblock.into_iter().map(|task| {
            let store = store.clone();
            let backend = backend.clone();
            let repo = repo.clone();
            let id = task.id.clone();
            async move {
                store::store_reset_counters(&store, &repo, &id.0).await;
                update_status_store_first(&store, &backend, &repo, &id, Status::New).await
            }
        });
        for result in join_all(ext_futs).await {
            result?;
        }

        let mut internal_count = 0;
        if let Some(ref s) = store {
            let internal_blocked = s
                .list_internal_by_status(&repo, TaskStatus::Blocked)
                .await
                .unwrap_or_default();
            let internal_needs_review = s
                .list_internal_by_status(&repo, TaskStatus::NeedsReview)
                .await
                .unwrap_or_default();
            let tasks_internal: Vec<_> = internal_blocked
                .iter()
                .chain(internal_needs_review.iter())
                .collect();
            internal_count = tasks_internal.len();
            let int_futs = tasks_internal.into_iter().map(|task| {
                let store = store.clone();
                let s = s.clone();
                let repo = repo.clone();
                let ext_id = task
                    .external_id
                    .clone()
                    .unwrap_or_else(|| format!("internal:{}", task.id));
                let task_id = task.id;
                async move {
                    store::store_reset_counters(&store, &repo, &ext_id).await;
                    s.update_status(task_id, TaskStatus::New).await
                }
            });
            for result in join_all(int_futs).await {
                result?;
            }
        }

        let total = external_count + internal_count;
        println!(
            "Unblocked {} tasks ({} external, {} internal) (attempts reset)",
            total, external_count, internal_count
        );
        return Ok(());
    }

    // Try to resolve as a store task (internal or external)
    if let Some(ref s) = store {
        if let Some(stripped) = id.strip_prefix("internal:") {
            let parsed = stripped
                .parse::<i64>()
                .with_context(|| format!("internal task id '{}' is not numeric", stripped))?;
            if let Ok(Some(store_id)) = s.resolve_task_id(&repo, id).await {
                reset_counters(store_id, &store, &repo).await;
                s.update_status(store_id, TaskStatus::New).await?;
                println!(
                    "Unblocked internal task #{} (attempts reset, will be re-routed)",
                    parsed
                );
                return Ok(());
            }
        }

        if let Ok(parsed) = id.parse::<i64>() {
            if let Ok(task) = s.get(parsed).await {
                if task.origin == "internal" {
                    reset_counters(parsed, &store, &repo).await;
                    s.update_status(parsed, TaskStatus::New).await?;
                    println!(
                        "Unblocked internal task #{} (attempts reset, will be re-routed)",
                        parsed
                    );
                    return Ok(());
                }
            }
        }
    }

    let ext_id = ExternalId(id.to_string());
    store::store_reset_counters(&store, &repo, &ext_id.0).await;
    update_status_store_first(&store, &backend, &repo, &ext_id, Status::New).await?;
    println!("Unblocked task #{} (attempts reset)", id);

    Ok(())
}

/// Kill tmux sessions and remove the worktree for a closed task.
///
/// Sessions are killed first (before worktree removal) so that the tmux-active
/// guard in `cleanup_task_worktree_with_opts` does not skip the removal.
/// Errors are logged but do not fail the close operation.
async fn cleanup_sessions_and_worktree(
    task_id: &str,
    repo: &str,
    store: &Option<Arc<crate::store::TaskStore>>,
) {
    let tmux = TmuxManager::new();

    // Session names follow `orch-{project}-{task_id}` and optionally
    // `orch-{project}-{task_id}-{suffix}` (e.g. `-review`).
    // Build a pattern that matches the task_id as a complete dash-delimited segment.
    let safe_id = task_id.replace(':', "-");
    let pattern = format!("-{safe_id}");

    match tmux.list_sessions().await {
        Ok(sessions) => {
            for sess in sessions {
                let name = &sess.name;
                if let Some(pos) = name.find(&pattern) {
                    let after = &name[pos + pattern.len()..];
                    if after.is_empty() || after.starts_with('-') {
                        tracing::info!(
                            session = name,
                            task_id,
                            "killing tmux session for closed task"
                        );
                        if let Err(e) = tmux.kill_session(name).await {
                            tracing::warn!(session = name, err = %e, "failed to kill session on close");
                        } else {
                            println!("  killed session {name}");
                        }
                    }
                }
            }
        }
        Err(e) => {
            tracing::warn!(task_id, err = %e, "could not list tmux sessions during close cleanup");
        }
    }

    // Remove the worktree (sessions are already gone so the tmux guard won't block).
    if let Some(ref s) = store {
        match crate::engine::cleanup::cleanup_task_worktree(task_id, repo, s).await {
            Ok(()) => {}
            Err(e) => {
                tracing::warn!(task_id, err = %e, "worktree cleanup failed after task close");
            }
        }
    }
}

/// Mark a task as done (without running an agent).
pub async fn close(id: &str, note: Option<&str>) -> anyhow::Result<()> {
    use crate::backends::github::GitHubBackend;
    use crate::backends::ExternalBackend;
    use crate::store::TaskStatus;

    let repo = config::get_current_repo().unwrap_or_default();
    let store = crate::cli::init_store().await.ok().map(std::sync::Arc::new);

    // Handle internal: prefix
    if let Some(stripped) = id.strip_prefix("internal:") {
        let parsed = stripped
            .parse::<i64>()
            .with_context(|| format!("internal task id '{}' is not numeric", stripped))?;
        if let Some(ref s) = store {
            if let Ok(Some(store_id)) = s.resolve_task_id(&repo, id).await {
                s.update_status(store_id, TaskStatus::Done).await?;
                println!("Closed internal task #{} (marked done)", parsed);
                cleanup_sessions_and_worktree(id, &repo, &store).await;
                return Ok(());
            }
        }
        anyhow::bail!("internal task '{}' not found", id);
    }

    // Try numeric: check store first (may be internal)
    if let Ok(parsed) = id.parse::<i64>() {
        if let Some(ref s) = store {
            if let Ok(task) = s.get(parsed).await {
                if task.origin == "internal" {
                    s.update_status(parsed, TaskStatus::Done).await?;
                    println!("Closed internal task #{} (marked done)", parsed);
                    cleanup_sessions_and_worktree(id, &repo, &store).await;
                    return Ok(());
                }
            }
        }
    }

    // External (GitHub) task
    let backend: Arc<dyn ExternalBackend> = Arc::new(
        GitHubBackend::new(repo.clone())
            .context("'repo' not set — ensure .orch.yml has gh.repo")?,
    );
    let ext_id = ExternalId(id.to_string());

    if let Some(text) = note {
        backend.post_comment(&ext_id, text).await?;
    }

    update_status_store_first(&store, &backend, &repo, &ext_id, Status::Done).await?;
    println!("Closed task #{} (marked done)", id);
    cleanup_sessions_and_worktree(id, &repo, &store).await;
    Ok(())
}

/// Reopen a done/blocked task atomically:
/// 1. Update SQLite status → New
/// 2. Reset failure counters
/// 3. Reopen GitHub issue (if external)
/// 4. Sync GitHub labels to status:new
/// 5. Link PR number if branch has a PR
pub async fn reopen(id: &str) -> anyhow::Result<()> {
    use crate::backends::github::GitHubBackend;
    use crate::backends::ExternalBackend;
    use crate::github::http::GhHttp;
    use crate::store::TaskStatus;

    let repo =
        config::get_current_repo().context("'repo' not set — ensure .orch.yml has gh.repo")?;
    let store = crate::cli::init_store().await.ok().map(std::sync::Arc::new);
    let gh = GhHttp::new()?;

    // Resolve the task in the store
    let (store_id, task) = if let Some(ref s) = store {
        if let Some(stripped) = id.strip_prefix("internal:") {
            let parsed = stripped
                .parse::<i64>()
                .with_context(|| format!("internal task id '{}' is not numeric", stripped))?;
            if let Ok(Some(sid)) = s.resolve_task_id(&repo, id).await {
                let task = s.get(sid).await?;
                (sid, task)
            } else {
                anyhow::bail!("internal task '{}' not found", parsed);
            }
        } else if let Ok(parsed) = id.parse::<i64>() {
            if let Ok(task) = s.get(parsed).await {
                if task.origin == "internal" {
                    (parsed, task)
                } else if let Ok(Some(sid)) = s.resolve_task_id(&repo, id).await {
                    let task = s.get(sid).await?;
                    (sid, task)
                } else {
                    anyhow::bail!("task '{}' not found in store", id);
                }
            } else if let Ok(Some(sid)) = s.resolve_task_id(&repo, id).await {
                let task = s.get(sid).await?;
                (sid, task)
            } else {
                anyhow::bail!("task '{}' not found", id);
            }
        } else {
            anyhow::bail!("invalid task id: {}", id);
        }
    } else {
        anyhow::bail!("could not open task store");
    };

    let s = match store.as_ref() {
        Some(s) => s,
        None => anyhow::bail!("task store unexpectedly None"),
    };

    // 1. Reset failure counters
    let ext_id_str = task
        .external_id
        .clone()
        .unwrap_or_else(|| format!("internal:{}", task.id));
    crate::store::store_reset_counters(&store, &repo, &ext_id_str).await;

    // 2. Update SQLite status → New
    s.update_status(store_id, TaskStatus::New).await?;

    // 3 & 4. For external tasks: reopen issue + sync labels
    if task.origin != "internal" {
        if let Some(ref ext_id) = task.external_id {
            // Reopen the GitHub issue
            if let Err(e) = gh.reopen_issue(&repo, ext_id).await {
                eprintln!("warning: failed to reopen GitHub issue: {}", e);
            }

            // Sync labels to status:new
            let backend: Arc<dyn ExternalBackend> = Arc::new(GitHubBackend::new(repo.clone())?);
            if let Err(e) = backend
                .update_status(&crate::backends::ExternalId(ext_id.clone()), Status::New)
                .await
            {
                eprintln!("warning: failed to sync labels: {}", e);
            }
        }
    }

    // 5. Link PR number if branch has a PR and pr_number is not set
    if task.pr_number.is_none() && !task.branch.is_empty() {
        if let Ok(Some(pr_num)) = gh.get_pr_number(&repo, &task.branch).await {
            let _ = s
                .set_fields(store_id, &[("pr_number", serde_json::json!(pr_num as i64))])
                .await;
            println!("Linked PR #{} from branch '{}'", pr_num, task.branch);
        }
    }

    println!(
        "Reopened task #{} (status → new, counters reset, will be re-routed)",
        id
    );
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

    let store: Option<Arc<TaskStore>> = crate::cli::init_store().await.ok().map(Arc::new);
    let repo = config::get_current_repo().unwrap_or_default();

    println!("{:<30} {:<15} {:<10} ACTIVE", "SESSION", "TASK", "AGENT");
    println!("{}", "-".repeat(70));

    let active_map = tmux.batch_session_active().await;
    let agent_futs = sessions.iter().map(|session| {
        let store = store.clone();
        let repo = repo.clone();
        let task_id = session.task_id.clone();
        async move {
            store::opt_store_get_task(&store, &repo, &task_id)
                .await
                .and_then(|t| t.agent)
                .unwrap_or_default()
        }
    });
    let agents = join_all(agent_futs).await;
    for (session, agent) in sessions.iter().zip(agents.iter()) {
        let active = active_map.get(&session.name).copied().unwrap_or(false);
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
pub async fn cost(id: &str) -> anyhow::Result<()> {
    // Delegate to cli::cost::show_task for consistent formatting
    super::cost::show_task(id).await
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
    let store: Option<Arc<TaskStore>> = crate::cli::init_store().await.ok().map(Arc::new);
    let repo = config::get_current_repo().unwrap_or_default();

    // Resolve task key for store lookups.
    let mut task_key = id.to_string();

    // If user passed `internal:N`, use `internal:{n}` format for store lookups.
    if let Some(n) = parse_internal_id(id) {
        task_key = format!("internal:{}", n);
        // Fetch internal task metadata from store
        if let Some(ref s) = store {
            if let Ok(Some(store_id)) = s.resolve_task_id(&repo, id).await {
                if let Ok(internal) = s.get(store_id).await {
                    println!("ID: {} (internal)", internal.id);
                    println!("Title: {}", internal.title);
                    println!("Status: {}", internal.status.as_str());
                    if let Some(agent) = &internal.agent {
                        println!("Agent: {}", agent);
                    }
                    if let Some(reason) = &internal.block_reason {
                        println!("Block reason: {}", reason);
                    }
                    println!("Created: {}", internal.created_at);
                    println!("Updated: {}", internal.updated_at);
                }
            }
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
                task_key = ext.id.0.clone();
            }
            Ok(Task::Internal(int)) => {
                println!("ID: {} (internal)", int.id);
                println!("Title: {}", int.title);
                println!("Status: {}", int.status.as_str());
                if let Some(agent) = &int.agent {
                    println!("Agent: {}", agent);
                }
                println!("Created: {}", int.created_at);
                println!("Updated: {}", int.updated_at);
                task_key = int
                    .external_id
                    .clone()
                    .unwrap_or_else(|| format!("internal:{}", int.id));
            }
            Err(e) => {
                println!("ID: {}", id);
                println!("(could not resolve task metadata: {})", e);
            }
        }
    } else {
        println!("ID: {}", id);
    }

    // Task store fields
    {
        {
            // Agent, model, and attempts from store — single DB round-trip.
            if let Some(ref s) = store {
                if let Ok(Some(t)) = s.get_by_external_id(&repo, &task_key).await {
                    if let Some(ref agent) = t.agent {
                        println!("Agent: {}", agent);
                    }
                    if let Some(ref model) = t.model {
                        println!("Model: {}", model);
                    }
                    if t.attempts > 0 {
                        println!("Attempts: {}", t.attempts);
                    }
                }
            }

            // Token & cost summary
            let usage = store::get_token_usage(&store, &repo, &task_key).await;
            let cost = store::get_cost_estimate(&store, &repo, &task_key).await;
            let total_tokens = usage.total_tokens();
            if total_tokens > 0 || cost.total_cost_usd > 0.0 {
                println!("\nCost summary:");
                println!("  input tokens:  {}", usage.input_tokens);
                println!("  output tokens: {}", usage.output_tokens);
                println!("  total tokens:  {}", total_tokens);
                println!("  estimated $:   ${:.6}", cost.total_cost_usd);
            }

            // Memory (recent attempts)
            {
                let mem = store::get_recent_memory(&store, &repo, &task_key, 10).await;
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

    // Show agent output from last attempt if available
    let attempt_n: u32 = store::opt_store_get_task(&store, &repo, &task_key)
        .await
        .map(|t| t.attempts)
        .unwrap_or(0) as u32;
    if attempt_n > 0 {
        match home::task_attempt_dir(&repo, &task_key, attempt_n) {
            Ok(attempt_dir) => {
                let output_file = attempt_dir.join("output.json");
                let exit_file = attempt_dir.join("exit.txt");

                if output_file.exists() {
                    if let Ok(content) = std::fs::read_to_string(&output_file) {
                        let tail =
                            crate::engine::runner::response_handler::safe_utf8_tail(&content, 2000);
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

    // Show review agent output if available
    let review_task_id = format!("{}-review", task_key);
    if let Ok(review_attempt_dir) = home::task_attempt_dir(&repo, &review_task_id, 1) {
        let review_output_file = review_attempt_dir.join("output.json");
        if review_output_file.exists() {
            if let Ok(content) = std::fs::read_to_string(&review_output_file) {
                println!("\n--- Review agent output ---");
                println!("{}", content);
            }
        }
    }

    // Show audit trail from task_runs table (if available in store)
    if let Ok(store) = crate::cli::init_store().await {
        if let Ok(Some(store_id)) = store.resolve_task_id(&repo, &task_key).await {
            if let Ok(runs) = store.get_runs(store_id).await {
                if !runs.is_empty() {
                    println!("\n--- Run audit trail ({} runs) ---", runs.len());
                    for run in &runs {
                        println!(
                            "\n  Run #{} [{}] agent={} model={} outcome={}",
                            run.attempt, run.run_type, run.agent, run.model, run.outcome,
                        );
                        if let Some(code) = run.exit_code {
                            println!("    exit_code: {}", code);
                        }
                        if !run.error.is_empty() {
                            println!("    error: {}", run.error);
                        }
                        if run.total_cost_usd > 0.0 {
                            println!(
                                "    tokens: {}in/{}out  cost: ${:.6}  duration: {:.1}s",
                                run.input_tokens,
                                run.output_tokens,
                                run.total_cost_usd,
                                run.duration_secs,
                            );
                        }
                        println!("    started: {}", run.started_at);
                        if let Some(ref completed) = run.completed_at {
                            println!("    completed: {}", completed);
                        }
                    }
                }
            }
        }
    }

    // If tmux session is live, append recent pane capture
    let tmux = TmuxManager::new();
    let session = tmux.session_name(&repo, &task_key);
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

/// Show run history for a task.
pub async fn runs(id: &str, verbose: bool) -> anyhow::Result<()> {
    let store = crate::cli::init_store().await?;
    let repo = config::get_current_repo().unwrap_or_default();
    let Some(task_id) = store.resolve_task_id(&repo, id).await? else {
        anyhow::bail!("task not found: {id}");
    };

    let runs = store.get_runs(task_id).await?;
    if runs.is_empty() {
        println!("No runs found for task {id}.");
        return Ok(());
    }

    println!(
        "{:<4} {:<8} {:<18} {:<12} {:<8} {:<8} ERROR",
        "RUN", "AGENT", "MODEL", "OUTCOME", "DURATION", "COST"
    );
    println!("{}", "-".repeat(90));

    for run in runs {
        let model = if run.model.is_empty() {
            "-"
        } else {
            &run.model
        };
        let error = format_run_text(&run.error, verbose, 48);
        println!(
            "{:<4} {:<8} {:<18} {:<12} {:<8} {:<8} {}",
            run.attempt,
            run.agent,
            model,
            run.outcome,
            format_run_duration(run.duration_secs),
            format_run_cost(run.total_cost_usd),
            error,
        );

        if verbose {
            if !run.stdout.is_empty() {
                println!("  stdout:\n{}", run.stdout);
            }
            if !run.stderr.is_empty() {
                println!("  stderr:\n{}", run.stderr);
            }
            if !run.parsed_response.is_empty() {
                println!("  parsed_response:\n{}", run.parsed_response);
            }
            println!();
        }
    }

    Ok(())
}

/// Show lifecycle activity timeline for a task.
pub async fn activity_log(id: &str, limit: Option<usize>, json: bool) -> anyhow::Result<()> {
    let store = crate::cli::init_store().await?;
    let repo = config::get_current_repo().unwrap_or_default();
    let Some(task_id) = store.resolve_task_id(&repo, id).await? else {
        anyhow::bail!("task not found: {id}");
    };

    let events = store.get_activity(task_id, limit).await?;
    if events.is_empty() {
        println!("No activity found for task {id}.");
        return Ok(());
    }

    println!(
        "{:<20} {:<16} {:<12} {:<12} {:<10} {:<14} DETAILS",
        "TIME", "EVENT", "FROM", "TO", "AGENT", "MODEL"
    );
    println!("{}", "-".repeat(120));

    for event in events {
        let details = if json {
            serde_json::to_string(&event.details).unwrap_or_else(|_| "{}".to_string())
        } else if event.details.is_null() || event.details == serde_json::json!({}) {
            String::new()
        } else {
            let compact =
                serde_json::to_string(&event.details).unwrap_or_else(|_| "{}".to_string());
            truncate_err(&compact, 80)
        };
        println!(
            "{:<20} {:<16} {:<12} {:<12} {:<10} {:<14} {}",
            event.timestamp,
            event.event_type,
            event.from_status.as_deref().unwrap_or("-"),
            event.to_status.as_deref().unwrap_or("-"),
            event.agent.as_deref().unwrap_or("-"),
            event.model.as_deref().unwrap_or("-"),
            details
        );
    }

    Ok(())
}

/// Show task routing history with attempt timeline.
pub async fn history(id: &str) -> anyhow::Result<()> {
    let store = crate::cli::init_store().await?;
    let repo = config::get_current_repo().unwrap_or_default();
    let Some(task_id) = store.resolve_task_id(&repo, id).await? else {
        anyhow::bail!("task not found: {id}");
    };

    let task = store.get(task_id).await?;
    let events = store.get_activity(task_id, None).await?;

    // Header
    println!("Routing History for Task {}", id);
    println!("{}", "=".repeat(60));
    println!("Title: {}", task.title);
    println!("Status: {:?}", task.status);
    println!();

    // Find routing events (routed, rerouted)
    let routing_events: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == "routed" || e.event_type == "rerouted")
        .collect();

    if routing_events.is_empty() {
        println!("No routing history found. Task has not been routed yet.");
        return Ok(());
    }

    // Display routing timeline
    println!("Attempt Timeline:");
    println!("{}", "-".repeat(60));

    for (i, event) in routing_events.iter().enumerate() {
        let attempt_num = i + 1;
        let agent = event.agent.as_deref().unwrap_or("-");
        let model = event.model.as_deref().unwrap_or("-");
        let timestamp = &event.timestamp;

        if event.event_type == "routed" {
            println!("\nAttempt #{}", attempt_num);
            println!("  Time:     {}", timestamp);
            println!("  Agent:    {}", agent);
            println!("  Model:    {}", model);

            // Extract routing details
            if let Some(reason) = event.details.get("reason").and_then(|r| r.as_str()) {
                println!("  Reason:   {}", reason);
            }
            if let Some(complexity) = event.details.get("complexity").and_then(|c| c.as_str()) {
                println!("  Complexity: {}", complexity);
            }
        } else if event.event_type == "rerouted" {
            println!("  → Rerouted at {}", timestamp);

            // Check for failure reason
            if let Some(failure) = event.details.get("failure_reason").and_then(|f| f.as_str()) {
                println!("    Failure: {}", failure);
            } else if let Some(failure) = event.details.get("silence_duration_secs") {
                println!("    Failure: Agent silent for {}s", failure);
            }

            // Show from/to agent if available
            if let Some(from) = event.details.get("from_agent").and_then(|a| a.as_str()) {
                if let Some(to) = event.details.get("to_agent").and_then(|a| a.as_str()) {
                    println!("    Change:   {} → {}", from, to);
                }
            }

            // Show if agents were exhausted
            if event.details.get("agents_exhausted").is_some() {
                println!("    Note:     All agents exhausted");
            }
            if event.details.get("no_fallback_available").is_some() {
                println!("    Note:     No fallback available");
            }
        }
    }

    // Summary
    println!("\n{}", "-".repeat(60));
    println!("Summary:");
    println!("  Total routing attempts: {}", routing_events.len());

    // Get cost summary from task
    let total_tokens = task.input_tokens + task.output_tokens;
    if total_tokens > 0 {
        println!("  Input tokens:  {}", task.input_tokens);
        println!("  Output tokens: {}", task.output_tokens);
        println!(
            "  Total cost:    ${:.6}",
            task.input_cost_usd + task.output_cost_usd
        );
    }

    // Show current assigned agent/model if available
    if let Some(ref agent) = task.agent {
        println!("  Current agent: {}", agent);
    }
    if let Some(ref model) = task.model {
        println!("  Current model: {}", model);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_age_seconds() {
        let ts = (Utc::now() - chrono::Duration::seconds(30)).to_rfc3339();
        let age = format_age(&ts);
        assert!(age.ends_with('s'), "expected seconds format, got {age}");
    }

    #[test]
    fn format_age_minutes() {
        let ts = (Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
        let age = format_age(&ts);
        assert!(age.ends_with('m'), "expected minutes format, got {age}");
    }

    #[test]
    fn format_age_hours() {
        let ts = (Utc::now() - chrono::Duration::hours(3)).to_rfc3339();
        let age = format_age(&ts);
        assert!(age.ends_with('h'), "expected hours format, got {age}");
    }

    #[test]
    fn format_age_days() {
        let ts = (Utc::now() - chrono::Duration::days(2)).to_rfc3339();
        let age = format_age(&ts);
        assert!(age.ends_with('d'), "expected days format, got {age}");
    }

    #[test]
    fn format_age_invalid_returns_dash() {
        let age = format_age("not-a-timestamp");
        assert_eq!(age, "-");
    }

    #[test]
    fn matches_project_filter_accepts_exact_repo_slug() {
        assert!(matches_project_filter(
            "gabrielkoerich/orch",
            "gabrielkoerich/orch"
        ));
    }

    #[test]
    fn matches_project_filter_accepts_repo_name_suffix() {
        assert!(matches_project_filter("gabrielkoerich/orch", "orch"));
    }

    #[test]
    fn matches_project_filter_rejects_non_matching_repo() {
        assert!(!matches_project_filter("gabrielkoerich/orch", "bean"));
    }
}
