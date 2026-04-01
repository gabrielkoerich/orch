use crate::backends::Status;
use crate::cli::init_task_manager;
use crate::config;
use crate::store;
use crate::store::TaskStatus;
use crate::store::TaskStore;
use crate::tmux::TmuxManager;
use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;
use std::sync::Arc;

/// Simple dashboard command combining task status, active sessions, and recent activity.
pub async fn dashboard(global: bool, project: Option<String>) -> anyhow::Result<()> {
    // Use global store when --global flag is set, a project filter is given,
    // or when no project context is available (running outside a project dir).
    if global || project.is_some() {
        return dashboard_from_global_store(project.as_deref()).await;
    }

    let task_manager = match init_task_manager().await {
        Ok(tm) => tm,
        Err(e) => {
            tracing::debug!("no project context available, falling back to global store: {e:#}");
            return dashboard_from_global_store(None).await;
        }
    };

    let statuses = [
        Status::New,
        Status::Routed,
        Status::InProgress,
        Status::InReview,
        Status::Done,
        Status::NeedsReview,
        Status::Blocked,
    ];

    // Fetch all tasks from both backends
    let all_external = task_manager.list_all_external_tasks().await?;
    let all_internal = task_manager.list_all_internal().await.unwrap_or_default();

    let mut counts: Vec<(Status, usize, usize)> = Vec::new(); // (status, external, internal)
    let mut total = 0usize;
    let mut done_tasks = Vec::new();
    for s in &statuses {
        let label = s.as_label();
        let ext_filtered: Vec<_> = all_external
            .iter()
            .filter(|t| t.labels.contains(&label.to_string()))
            .cloned()
            .collect();
        let ts = match s {
            Status::New => TaskStatus::New,
            Status::Routed => TaskStatus::Routed,
            Status::InProgress => TaskStatus::InProgress,
            Status::Done => TaskStatus::Done,
            Status::Blocked => TaskStatus::Blocked,
            Status::InReview => TaskStatus::InReview,
            Status::NeedsReview => TaskStatus::NeedsReview,
        };
        let int_count = all_internal.iter().filter(|t| t.status == ts).count();
        total += ext_filtered.len() + int_count;
        counts.push((*s, ext_filtered.len(), int_count));
        if *s == Status::Done {
            done_tasks = ext_filtered;
        }
    }

    println!("Tasks ({} total)", total);

    for (s, ext, int) in &counts {
        let count = ext + int;
        if count > 0 {
            println!(
                "  {:<12} {:>3}{}",
                s.as_label().replace("status:", ""),
                count,
                if *int > 0 {
                    format!(" ({} internal)", int)
                } else {
                    String::new()
                }
            );
        }
    }

    let store: Option<Arc<TaskStore>> = crate::cli::init_store().await.ok().map(Arc::new);
    let repo = config::get_current_repo().unwrap_or_default();

    println!("\nActive Sessions");
    let tmux = TmuxManager::new();
    let sessions = tmux.list_sessions().await.unwrap_or_default();
    for s in sessions.iter() {
        let agent = store::opt_store_get_task(&store, &repo, &s.task_id)
            .await
            .and_then(|t| t.agent)
            .unwrap_or_default();
        println!("  {:<25} {:<8} #{}", s.name, agent, s.task_id);
    }

    println!("\nRecent (last 24h)");
    let cutoff: DateTime<Utc> = Utc::now() - Duration::hours(24);
    for r in done_tasks.iter().take(10) {
        if let Ok(dt) = DateTime::parse_from_rfc3339(&r.updated_at) {
            let dt_utc = dt.with_timezone(&Utc);
            if dt_utc >= cutoff {
                let agent = store::opt_store_get_task(&store, &repo, &r.id.0)
                    .await
                    .and_then(|t| t.agent)
                    .unwrap_or_default();
                let elapsed = Utc::now() - dt_utc;
                let mins = elapsed.num_minutes();
                println!(
                    "  ✅ #{:<4} {:<30} {:<8} done {:>4}m ago",
                    r.id.0, r.title, agent, mins
                );
            }
        }
    }

    Ok(())
}

/// Dashboard using the global store — works from any directory, across all projects.
async fn dashboard_from_global_store(project: Option<&str>) -> anyhow::Result<()> {
    let store = match crate::cli::init_store().await {
        Ok(s) => Arc::new(s),
        Err(_) => {
            println!("Tasks (0 total)\n\nActive Sessions\n\nRecent (last 24h)");
            return Ok(());
        }
    };

    // Load all active tasks and recent done tasks from the store.
    let mut active = store.list_all_active_global().await.unwrap_or_default();
    let mut done = store
        .list_all_by_status_global(TaskStatus::Done)
        .await
        .unwrap_or_default();

    // Apply optional project filter.
    if let Some(p) = project {
        active.retain(|t| matches_project_filter(&t.repo, p));
        done.retain(|t| matches_project_filter(&t.repo, p));
    }

    // Build a map from external_id -> agent for quick session lookups.
    let agent_by_ext_id: HashMap<String, String> = active
        .iter()
        .filter_map(|t| {
            let ext_id = t.external_id.as_deref()?;
            let agent = t.agent.as_deref()?;
            Some((ext_id.to_string(), agent.to_string()))
        })
        .collect();

    // Count by status.
    let status_order = [
        TaskStatus::New,
        TaskStatus::Routed,
        TaskStatus::InProgress,
        TaskStatus::InReview,
        TaskStatus::NeedsReview,
        TaskStatus::Blocked,
        TaskStatus::Done,
    ];

    let done_count = done.len();
    let total = active.len() + done_count;
    println!("Tasks ({} total)", total);

    for ts in &status_order {
        let count = if *ts == TaskStatus::Done {
            done_count
        } else {
            active.iter().filter(|t| t.status == *ts).count()
        };
        if count > 0 {
            println!("  {:<12} {:>3}", ts.as_str().replace('_', ""), count);
        }
    }

    println!("\nActive Sessions");
    let tmux = TmuxManager::new();
    let sessions = tmux.list_sessions().await.unwrap_or_default();
    for s in sessions.iter() {
        let agent = agent_by_ext_id
            .get(&s.task_id)
            .map(String::as_str)
            .unwrap_or_default();
        println!("  {:<25} {:<8} #{}", s.name, agent, s.task_id);
    }

    println!("\nRecent (last 24h)");
    let cutoff: DateTime<Utc> = Utc::now() - Duration::hours(24);
    for r in done.iter().take(10) {
        if let Ok(dt) = DateTime::parse_from_rfc3339(&r.updated_at) {
            let dt_utc = dt.with_timezone(&Utc);
            if dt_utc >= cutoff {
                let agent = r.agent.as_deref().unwrap_or_default();
                let elapsed = Utc::now() - dt_utc;
                let mins = elapsed.num_minutes();
                let id_str = r.external_id.clone().unwrap_or_else(|| r.id.to_string());
                println!(
                    "  ✅ #{:<4} {:<30} {:<8} done {:>4}m ago",
                    id_str, r.title, agent, mins
                );
            }
        }
    }

    Ok(())
}

fn matches_project_filter(repo: &str, project: &str) -> bool {
    let repo_name = repo.rsplit('/').next().unwrap_or(repo);
    repo == project || repo_name == project || repo.ends_with(&format!("/{project}"))
}
