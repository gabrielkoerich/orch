use crate::backends::{ExternalBackend, ExternalId, Status};
use crate::cli::init_task_manager;
use crate::cmd::SyncCommandErrorContext;
use crate::config;
use crate::engine::router::Router;
use crate::engine::runner::TaskRunner;
use crate::engine::tasks::{status_to_task_status, CreateTaskRequest, Task, TaskFilter, TaskType};
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
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        let days = secs / 86400;
        if days >= 365 {
            let years = days / 365;
            format!("{years}y")
        } else {
            format!("{days}d")
        }
    }
}

/// Truncate an error message to a maximum length, appending an ellipsis when truncated.
/// Format one `review_attempt_failed` activity row for `orch task inspect`.
fn review_attempt_line(activity: &store::TaskActivity) -> String {
    let field = |key: &str| {
        activity
            .details
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("-")
            .to_string()
    };
    let external = activity
        .details
        .get("external")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    format!(
        "{}  {}{}: {}",
        activity.timestamp,
        field("cause"),
        if external { " (external)" } else { "" },
        truncate_err(field("reason").trim(), 160)
    )
}

fn truncate_err(err: &str, max: usize) -> String {
    if err.len() > max {
        let mut end = max;
        while end > 0 && !err.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &err[..end])
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
    store: Option<&Arc<TaskStore>>,
    backend: &Arc<dyn ExternalBackend>,
    repo: &str,
    id: &ExternalId,
    status: Status,
) -> anyhow::Result<()> {
    let db_status = status_to_task_status(status);
    if let Some(s) = store {
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

fn waiting_title(title: &str, status: &str, blockers: &[String]) -> String {
    if status == "waiting" && !blockers.is_empty() {
        format!("{title} [waiting on {}]", blockers.join(", "))
    } else {
        title.to_string()
    }
}

fn dep_label(task_repo: &str, dep_repo: &str, number: i64) -> String {
    if task_repo == dep_repo {
        format!("#{number}")
    } else {
        format!("{dep_repo}#{number}")
    }
}

async fn refuse_external_claim(
    store: &TaskStore,
    repo: &str,
    external_id: &str,
) -> anyhow::Result<()> {
    if let Some(id) = store.resolve_task_id(repo, external_id).await? {
        crate::engine::dependencies::refuse_if_waiting(store, id).await?;
    }
    Ok(())
}

/// Refuse to move a done task back into the pipeline.
///
/// Done is terminal: a done task keeps the body it was created with, so
/// reviving one re-runs a prompt that may be months stale (#3780)
fn refuse_if_done(resolved: &store::CliTaskRef, action: &str) -> anyhow::Result<()> {
    if resolved.status != crate::store::TaskStatus::Done.as_str() {
        return Ok(());
    }
    anyhow::bail!(
        "{} is done — {action} does not revive done tasks; file a new issue or task for follow-up work",
        resolved.display_spec()
    )
}

/// True when the task still has an open dependency and must stay `waiting`.
async fn release_or_keep_waiting(store: &TaskStore, task_id: i64) -> anyhow::Result<bool> {
    Ok(store.status_after_unblock(task_id).await? == crate::store::TaskStatus::Waiting)
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
    let waiting_on = if let Some(ref s) = store {
        s.unmet_labels_by_task().await.unwrap_or_default()
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
                    .map_or_else(|| "unknown".to_string(), |s| s.replace("status:", ""));
                let store_task = store_by_ext_id.get(&ext.id.0);
                let agent = store_task
                    .and_then(|t| t.agent.as_deref())
                    .unwrap_or("")
                    .to_string();
                let age = format_age(&ext.updated_at);
                let project = store_task.map_or(repo.as_str(), |t| t.repo.as_str());
                let tries = store_task
                    .map(|t| {
                        if t.attempts > 0 {
                            t.attempts.to_string()
                        } else {
                            String::new()
                        }
                    })
                    .unwrap_or_default();
                let blockers = store_task
                    .and_then(|row| waiting_on.get(&row.id))
                    .map_or(&[][..], Vec::as_slice);
                let title = waiting_title(&ext.title, &status, blockers);
                println!(
                    "{:<15} {:<12} {:<20} {:<24} {:<10} {:<6} {:<5} {}",
                    ext.id.0, "external", status, project, agent, age, tries, title
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
                let blockers = waiting_on.get(&int.id).map_or(&[][..], Vec::as_slice);
                let title = if int.status == crate::store::TaskStatus::Blocked {
                    if let Some(ref reason) = int.block_reason {
                        format!("{} [blocked: {}]", int.title, reason)
                    } else {
                        int.title.clone()
                    }
                } else {
                    waiting_title(&int.title, int.status.as_str(), blockers)
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
    let Ok(store) = crate::cli::init_store().await else {
        println!("No tasks found.");
        return Ok(());
    };

    let tasks = if let Some(ref status_str) = status {
        if status_str == "all" {
            store.list_all_global().await?
        } else {
            let task_status = crate::store::TaskStatus::from_str(status_str)
                .ok_or_else(|| anyhow::anyhow!(
                    "invalid status: '{status_str}'. allowed: new, routed, in_progress, needs_review, in_review, approved, blocked, waiting, done, all"
                ))?;
            store.list_all_by_status_global(task_status).await?
        }
    } else {
        store.list_all_active_global().await?
    };

    // Apply optional source filter.
    let tasks: Vec<_> = tasks
        .into_iter()
        .filter(|t| source.as_ref().is_none_or(|s| &t.source == s))
        .filter(|t| project.is_none_or(|p| matches_project_filter(&t.repo, p)))
        .collect();

    if tasks.is_empty() {
        println!("No tasks found.");
        return Ok(());
    }

    let waiting_on = store.unmet_labels_by_task().await.unwrap_or_default();

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
        let blockers = waiting_on.get(&task.id).map_or(&[][..], Vec::as_slice);
        let title = if task.status == crate::store::TaskStatus::Blocked {
            if let Some(ref reason) = task.block_reason {
                format!("{} [blocked: {}]", task.title, reason)
            } else {
                task.title.clone()
            }
        } else {
            waiting_title(&task.title, task.status.as_str(), blockers)
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
    depends_on: Vec<String>,
) -> anyhow::Result<()> {
    let task_manager = init_task_manager().await?;

    // Labels or dependencies create a GitHub issue. Dependencies need the native link
    let task_type = if !labels.is_empty() || !depends_on.is_empty() {
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
            if !depends_on.is_empty() {
                link_created_dependencies(&t, &depends_on).await?;
            }
            println!("Created external task #{}: {}", t.id.0, t.title);
        }
    }

    Ok(())
}

async fn link_created_dependencies(
    task: &crate::backends::ExternalTask,
    depends_on: &[String],
) -> anyhow::Result<()> {
    use crate::backends::github::GitHubBackend;
    use crate::engine::dependencies::parse_depends_on_token;
    use crate::store::{DepSource, NewDependency, UpsertExternal};

    let repo =
        config::get_current_repo().context("'repo' not set — ensure .orch.yml has gh.repo")?;
    let issue_number: i64 = task
        .id
        .0
        .parse()
        .with_context(|| format!("created issue id '{}' is not a number", task.id.0))?;
    let backend = GitHubBackend::new(repo.clone())?;
    let store = crate::cli::init_store().await?;
    let store_id = store
        .upsert_external(&UpsertExternal {
            repo: &repo,
            ext_id: &task.id.0,
            title: &task.title,
            body: &task.body,
            author: &task.author,
            url: &task.url,
            labels: &task.labels,
            origin: "github",
        })
        .await?;
    let mut rows = Vec::new();
    for token in depends_on {
        let Some(dep) = parse_depends_on_token(token, &repo) else {
            anyhow::bail!("invalid --depends-on value '{token}'");
        };
        let source = match backend
            .add_dependency(issue_number, &dep.repo, dep.number)
            .await
        {
            Ok(()) => DepSource::Github,
            Err(err) => {
                eprintln!(
                    "warning: could not link {}#{} on GitHub ({err}). Stored the dependency locally.",
                    dep.repo, dep.number
                );
                DepSource::Cli
            }
        };
        rows.push(NewDependency {
            repo: dep.repo,
            number: dep.number,
            source,
        });
    }
    store.replace_dependencies(store_id, &rows).await?;
    Ok(())
}

/// Get task details by ID.
pub async fn get(id: &str) -> anyhow::Result<()> {
    use crate::backends::github::GitHubBackend;
    use crate::engine::tasks::store_task_to_external;

    let store = crate::cli::init_store().await?;
    let resolved = crate::cli::task_ref::resolve(&store, id).await?;
    let store_arc: Option<Arc<TaskStore>> = Some(Arc::new(store.clone()));
    let repo = resolved.repo.clone();

    if resolved.is_internal() {
        let store_id = resolved
            .store_id
            .context("internal task missing store id")?;
        let int = store.get(store_id).await?;
        println!("ID: {} (internal)", int.id);
        println!("Title: {}", int.title);
        println!("Status: {}", int.status.as_str());
        println!("Source: {}", int.source);
        if let Some(model) = &int.model {
            println!("Model: {model}");
        }
        if !int.complexity.is_empty() {
            println!("Complexity: {}", int.complexity);
        }
        if let Some(reason) = &int.block_reason {
            println!("Block reason: {reason}");
        }
        if int.status == crate::store::TaskStatus::Waiting {
            let blockers = store.unmet_blocker_labels(int.id).await.unwrap_or_default();
            if !blockers.is_empty() {
                println!("Waiting on: {}", blockers.join(", "));
            }
        }
        if int.route_attempts > 0 {
            println!("Route attempts: {}", int.route_attempts);
        }
        if int.attempts > 0 {
            println!("Attempts: {}", int.attempts);
        }
        if int.review_cycles > 0 {
            println!("Review cycles: {}", int.review_cycles);
        }
        if let Some(pr) = int.pr_number {
            println!("PR: #{pr}");
        }
        if !int.branch.is_empty() {
            println!("Branch: {}", int.branch);
        }
        if !int.last_error.is_empty() {
            let err = truncate_err(&int.last_error, 200);
            println!("Last error: {err}");
        }
        if let Some(parent) = int.parent_id {
            println!("Parent: internal:{parent}");
        }
        if int.input_tokens > 0 || int.output_tokens > 0 {
            println!(
                "Tokens: {} in / {} out — ${:.6}",
                int.input_tokens, int.output_tokens, int.total_cost_usd
            );
        }
        println!("Created: {}", int.created_at);
        println!("Updated: {}", int.updated_at);
        println!("\n{}", int.body);
        return Ok(());
    }

    let ext_id = ExternalId(resolved.external_id.clone());
    let backend = GitHubBackend::new(repo.clone())?;
    let ext = match backend.get_task(&ext_id).await {
        Ok(ext) => ext,
        Err(err) => match resolved.store_id {
            Some(store_id) => store_task_to_external(&store.get(store_id).await?),
            None => return Err(err),
        },
    };

    println!("ID: {} (external)", ext.id.0);
    println!("Title: {}", ext.title);
    println!("State: {}", ext.state);
    println!("Labels: {}", ext.labels.join(", "));
    println!("Author: {}", ext.author);
    println!("URL: {}", ext.url);
    println!("Created: {}", ext.created_at);
    println!("Updated: {}", ext.updated_at);

    if let Some(ref s) = store_arc {
        if let Ok(Some(t)) = s.get_by_external_id(&repo, &ext.id.0).await {
            if let Some(ref agent) = t.agent {
                println!("Agent: {agent}");
            }
            if let Some(ref model) = t.model {
                println!("Model: {model}");
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
                    println!("PR: #{pr}");
                }
            }
            if !t.branch.is_empty() {
                println!("Branch: {}", t.branch);
            }
            if !t.last_error.is_empty() {
                println!("Last error: {}", truncate_err(&t.last_error, 200));
            }
            if t.status == crate::store::TaskStatus::Waiting {
                let blockers = s.unmet_blocker_labels(t.id).await.unwrap_or_default();
                if !blockers.is_empty() {
                    println!("Waiting on: {}", blockers.join(", "));
                }
            }
        }
    }
    let usage = store::get_token_usage(&store_arc, &repo, &ext.id.0).await;
    let cost = store::get_cost_estimate(&store_arc, &repo, &ext.id.0).await;
    if usage.total_tokens() > 0 || cost.total_cost_usd > 0.0 {
        println!(
            "Tokens: {} in / {} out — ${:.6}",
            usage.input_tokens, usage.output_tokens, cost.total_cost_usd
        );
    }

    println!("\n{}", ext.body);

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
        Status::Approved,
        Status::NeedsReview,
        Status::Waiting,
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
                Status::Approved => TaskStatus::Approved,
                Status::NeedsReview => TaskStatus::NeedsReview,
                Status::Waiting => TaskStatus::Waiting,
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
            total_input_tokens = u64::try_from(input).unwrap_or(0);
            total_output_tokens = u64::try_from(output).unwrap_or(0);
            total_cost = cost;
        }
    } else {
        // No store available: sum up internal task costs directly from already-loaded data.
        for task in &all_internal {
            total_input_tokens += u64::try_from(task.input_tokens).unwrap_or(0);
            total_output_tokens += u64::try_from(task.output_tokens).unwrap_or(0);
            total_cost += task.total_cost_usd;
        }
    }

    // The engine process owns the live rate-limit state, so it leaves the reset epoch in
    // KV for this one. Stale entries read as expired and are dropped.
    let rate_limited_until = match store.as_ref() {
        Some(s) => s
            .kv_get(crate::engine::RATE_LIMITED_UNTIL_KEY)
            .await
            .ok()
            .flatten()
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|reset_at| *reset_at > chrono::Utc::now().timestamp()),
        None => None,
    };

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

        if let Some(reset_at) = rate_limited_until {
            map.insert(
                "github_rate_limited_until".to_string(),
                serde_json::Value::Number(reset_at.into()),
            );
        }

        println!("{}", serde_json::to_string_pretty(&map)?);
    } else {
        if let Some(reset_at) = rate_limited_until {
            println!(
                "GitHub: rate limited until {}",
                crate::engine::format_rate_limited_until(reset_at)
            );
            println!();
        }

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
            println!("  Total cost:   ${total_cost:.6}");
        }
    }

    Ok(())
}

/// Route a task to an agent.
pub async fn route(id: &str) -> anyhow::Result<()> {
    use crate::backends::github::GitHubBackend;
    use crate::backends::ExternalBackend;
    use crate::engine::tasks::store_task_to_external;
    use crate::store::TaskStatus;

    let store: Arc<TaskStore> = Arc::new(crate::cli::init_store().await?);
    let resolved = crate::cli::task_ref::resolve(&store, id).await?;
    let repo = resolved.repo.clone();
    let ext_id = ExternalId(resolved.external_id.clone());

    let (task, backend) = if resolved.is_internal() {
        let store_id = resolved
            .store_id
            .context("internal task missing store id")?;
        crate::engine::dependencies::refuse_if_waiting(&store, store_id).await?;
        (
            store_task_to_external(&store.get(store_id).await?),
            None::<Arc<dyn ExternalBackend>>,
        )
    } else {
        let backend: Arc<dyn ExternalBackend> = Arc::new(GitHubBackend::new(repo.clone())?);
        refuse_external_claim(&store, &repo, &ext_id.0).await?;
        (backend.get_task(&ext_id).await?, Some(backend))
    };

    let mut router = Router::from_config();
    let result = router.route(&task, &store, &repo).await?;

    router
        .store_route_result(&ext_id.0, &result, &store, &repo)
        .await?;

    if let Some(backend) = backend {
        let labels = vec![
            format!("agent:{}", result.agent),
            format!("complexity:{}", result.complexity),
        ];
        backend.set_labels(&ext_id, &labels).await?;
        update_status_store_first(Some(&store), &backend, &repo, &ext_id, Status::Routed).await?;
    } else if let Some(store_id) = resolved.store_id {
        store.update_status(store_id, TaskStatus::Routed).await?;
    }

    println!(
        "Routed task {} → {} (complexity: {}, reason: {})",
        resolved.display_spec(),
        result.agent,
        result.complexity,
        result.reason
    );

    Ok(())
}

/// Run a task (manual execution).
pub async fn run(id: Option<String>) -> anyhow::Result<()> {
    use crate::backends::github::GitHubBackend;
    use crate::backends::ExternalBackend;
    use crate::engine::router::get_route_result;
    use crate::store::TaskStatus;

    let store: Arc<TaskStore> = Arc::new(crate::cli::init_store().await?);

    let (repo, task_id, backend) = if let Some(id) = id {
        let resolved = crate::cli::task_ref::resolve(&store, &id).await?;
        let backend: Option<Arc<dyn ExternalBackend>> = if resolved.is_internal() {
            None
        } else {
            Some(Arc::new(GitHubBackend::new(resolved.repo.clone())?))
        };
        (resolved.repo, resolved.external_id, backend)
    } else {
        let repo =
            config::get_current_repo().context("'repo' not set — ensure .orch.yml has gh.repo")?;
        let backend: Arc<dyn ExternalBackend> = Arc::new(GitHubBackend::new(repo.clone())?);
        let routed = backend.list_by_status(Status::Routed).await?;
        let task_id = if let Some(task) = routed.first() {
            task.id.0.clone()
        } else {
            let new = backend.list_by_status(Status::New).await?;
            if let Some(task) = new.first() {
                task.id.0.clone()
            } else {
                anyhow::bail!("no runnable tasks found");
            }
        };
        (repo, task_id, Some(backend))
    };

    let route_result = get_route_result(&store, &repo, &task_id).await.ok();
    let agent = route_result.as_ref().map(|r| r.agent.clone());
    let model = route_result.as_ref().and_then(|r| r.model.clone());

    println!(
        "Running task #{} (agent: {}, model: {})",
        task_id,
        agent.as_deref().unwrap_or("default"),
        model.as_deref().unwrap_or("default")
    );

    refuse_external_claim(&store, &repo, &task_id).await?;

    let ext_id = ExternalId(task_id.clone());
    let started_at = Utc::now();
    if let Some(ref backend) = backend {
        update_status_store_first(Some(&store), backend, &repo, &ext_id, Status::InProgress)
            .await?;
    } else if let Some(store_id) = store.resolve_task_id(&repo, &task_id).await? {
        store
            .update_status(store_id, TaskStatus::InProgress)
            .await?;
    }

    let runner = TaskRunner::new(repo).with_store(store);
    runner
        .run(
            &task_id,
            agent.as_deref(),
            model.as_deref(),
            backend.as_deref(),
            &started_at,
        )
        .await?;

    println!("Task #{task_id} completed");
    Ok(())
}

/// Retry a task (reset to new).
///
/// When `agent` is provided, the task is force-routed to that agent/model and
/// set directly to `Routed` status, bypassing the router entirely.
pub async fn retry(id: &str, agent: Option<String>, model: Option<String>) -> anyhow::Result<()> {
    use crate::backends::github::GitHubBackend;
    use crate::backends::ExternalBackend;
    use crate::store::TaskStatus;

    let s = Arc::new(crate::cli::init_store().await?);
    let resolved = crate::cli::task_ref::resolve(&s, id).await?;
    refuse_if_done(&resolved, "retry")?;
    let store = Some(std::sync::Arc::clone(&s));
    let repo = resolved.repo.clone();
    let shown = resolved.display_spec();

    if resolved.is_internal() {
        let store_id = resolved
            .store_id
            .context("internal task missing store id")?;
        crate::engine::dependencies::refuse_if_waiting(&s, store_id).await?;
        store::store_reset_counters(&store, &repo, &resolved.external_id).await;

        if let Some(ref forced_agent) = agent {
            let fields = retry_force_fields(forced_agent, model.as_deref());
            s.set_fields(store_id, &fields).await?;
            s.update_status(store_id, TaskStatus::Routed).await?;
            let model_hint = model.as_deref().unwrap_or("(router default)");
            println!(
                "Task {shown} force-routed to {forced_agent}:{model_hint} (set to routed, dispatches on next tick)"
            );
        } else {
            s.update_status(store_id, TaskStatus::New).await?;
            println!("Task {shown} reset to new (attempts reset, will be re-routed)");
        }
        return Ok(());
    }

    let backend: Arc<dyn ExternalBackend> = Arc::new(GitHubBackend::new(repo.clone())?);
    let ext_id = ExternalId(resolved.external_id.clone());
    refuse_external_claim(&s, &repo, &ext_id.0).await?;

    let task = backend.get_task(&ext_id).await?;
    for label in &task.labels {
        if label.starts_with("agent:") {
            backend.remove_label(&ext_id, label).await?;
        }
    }

    store::store_reset_counters(&store, &repo, &ext_id.0).await;

    if let Some(ref forced_agent) = agent {
        backend
            .set_labels(&ext_id, &[format!("agent:{forced_agent}")])
            .await?;

        if let Some(store_id) = resolved.store_id {
            let fields = retry_force_fields(forced_agent, model.as_deref());
            s.set_fields(store_id, &fields).await?;
        }

        update_status_store_first(store.as_ref(), &backend, &repo, &ext_id, Status::Routed).await?;
        let model_hint = model.as_deref().unwrap_or("(router default)");
        println!(
            "Task {shown} force-routed to {forced_agent}:{model_hint} (set to routed, dispatches on next tick)"
        );
    } else {
        update_status_store_first(store.as_ref(), &backend, &repo, &ext_id, Status::New).await?;
        println!("Task {shown} reset to new (attempts reset, will be re-routed)");
    }

    Ok(())
}

fn retry_force_fields<'a>(
    forced_agent: &'a str,
    model: Option<&'a str>,
) -> Vec<(&'a str, serde_json::Value)> {
    let mut fields = vec![
        ("agent", serde_json::json!(forced_agent)),
        (
            "route_reason",
            serde_json::json!(format!("forced via retry --agent {forced_agent}")),
        ),
    ];
    if let Some(model) = model {
        fields.push(("model", serde_json::json!(model)));
    }
    fields
}

/// Unblock a task or all blocked tasks.
pub async fn unblock(id: &str) -> anyhow::Result<()> {
    use crate::backends::github::GitHubBackend;
    use crate::backends::ExternalBackend;
    use crate::store::TaskStatus;

    if id == "all" {
        let repo =
            config::get_current_repo().context("'repo' not set — ensure .orch.yml has gh.repo")?;
        let backend: Arc<dyn ExternalBackend> = Arc::new(GitHubBackend::new(repo.clone())?);
        let store = crate::cli::init_store().await.ok().map(std::sync::Arc::new);
        let blocked = backend.list_by_status(Status::Blocked).await?;
        let needs_review = backend.list_by_status(Status::NeedsReview).await?;

        let tasks_to_unblock: Vec<_> = blocked.iter().chain(needs_review.iter()).collect();
        let external_count = tasks_to_unblock.len();
        let ext_futs = tasks_to_unblock.into_iter().map(|task| {
            let store = store.clone();
            let backend = std::sync::Arc::clone(&backend);
            let repo = repo.clone();
            let id = task.id.clone();
            async move {
                store::store_reset_counters(&store, &repo, &id.0).await;
                if let Some(ref s) = store {
                    if let Ok(Some(store_id)) = s.resolve_task_id(&repo, &id.0).await {
                        match s.status_after_unblock(store_id).await {
                            Ok(TaskStatus::Waiting) => {
                                let _ = backend.update_status(&id, Status::Waiting).await;
                                return Ok(());
                            }
                            Ok(_) => {}
                            Err(err) => return Err(err),
                        }
                    }
                }
                update_status_store_first(store.as_ref(), &backend, &repo, &id, Status::New).await
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
                let s = std::sync::Arc::clone(s);
                let repo = repo.clone();
                let ext_id = task
                    .external_id
                    .clone()
                    .unwrap_or_else(|| format!("internal:{}", task.id));
                let task_id = task.id;
                async move {
                    store::store_reset_counters(&store, &repo, &ext_id).await;
                    match s.status_after_unblock(task_id).await? {
                        TaskStatus::Waiting => Ok(()),
                        _ => s.update_status(task_id, TaskStatus::New).await,
                    }
                }
            });
            for result in join_all(int_futs).await {
                result?;
            }
        }

        let total = external_count + internal_count;
        println!(
            "Unblocked {total} tasks ({external_count} external, {internal_count} internal) (attempts reset)"
        );
        return Ok(());
    }

    let s = Arc::new(crate::cli::init_store().await?);
    let resolved = crate::cli::task_ref::resolve(&s, id).await?;
    refuse_if_done(&resolved, "unblock")?;
    let store = Some(std::sync::Arc::clone(&s));
    let repo = resolved.repo.clone();
    let shown = resolved.display_spec();

    if resolved.is_internal() {
        let store_id = resolved
            .store_id
            .context("internal task missing store id")?;
        store::store_reset_counters(&store, &repo, &resolved.external_id).await;
        if release_or_keep_waiting(&s, store_id).await? {
            println!("Task {shown} stays waiting on an open dependency");
        } else {
            s.update_status(store_id, TaskStatus::New).await?;
            println!("Unblocked {shown} (attempts reset, will be re-routed)");
        }
        return Ok(());
    }

    let backend: Arc<dyn ExternalBackend> = Arc::new(GitHubBackend::new(repo.clone())?);
    let ext_id = ExternalId(resolved.external_id.clone());
    store::store_reset_counters(&store, &repo, &ext_id.0).await;
    if let Some(store_id) = resolved.store_id {
        if release_or_keep_waiting(&s, store_id).await? {
            let _ = backend.update_status(&ext_id, Status::Waiting).await;
            println!("Task {shown} stays waiting on an open dependency");
            return Ok(());
        }
    }
    update_status_store_first(store.as_ref(), &backend, &repo, &ext_id, Status::New).await?;
    println!("Unblocked {shown} (attempts reset)");

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
    store: Option<&Arc<crate::store::TaskStore>>,
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
    if let Some(s) = store {
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

    let s = crate::cli::init_store().await?;
    let resolved = crate::cli::task_ref::resolve(&s, id).await?;
    let store = Some(Arc::new(s.clone()));
    let repo = resolved.repo.clone();
    let shown = resolved.display_spec();

    if resolved.is_internal() {
        let store_id = resolved
            .store_id
            .context("internal task missing store id")?;
        s.update_status(store_id, TaskStatus::Done).await?;
        println!("Closed {shown} (marked done)");
        cleanup_sessions_and_worktree(&resolved.external_id, &repo, store.as_ref()).await;
        return Ok(());
    }

    let backend: Arc<dyn ExternalBackend> = Arc::new(GitHubBackend::new(repo.clone())?);
    let ext_id = ExternalId(resolved.external_id.clone());

    if let Some(text) = note {
        backend.post_comment(&ext_id, text).await?;
    }

    update_status_store_first(store.as_ref(), &backend, &repo, &ext_id, Status::Done).await?;
    println!("Closed {shown} (marked done)");
    cleanup_sessions_and_worktree(&resolved.external_id, &repo, store.as_ref()).await;
    Ok(())
}

/// Attach to a running agent's tmux session.
pub async fn attach(id: &str) -> anyhow::Result<()> {
    let store = crate::cli::init_store().await?;
    let task = crate::cli::task_ref::resolve(&store, id).await?;
    let tmux = TmuxManager::new();
    let session = tmux.session_name(&task.repo, &task.external_id);
    let status = std::process::Command::new("tmux")
        .args(["attach-session", "-t", &session])
        .status_with_context()?;

    if !status.success() {
        anyhow::bail!("no active session for task {}", task.display_spec());
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
    let store = crate::cli::init_store().await?;
    let task = crate::cli::task_ref::resolve(&store, id).await?;
    let tmux = TmuxManager::new();
    let session = tmux.session_name(&task.repo, &task.external_id);
    if let Some(store_id) = task.store_id {
        store
            .finalize_incomplete_runs(store_id, "killed", "killed by orch task kill")
            .await?;
    }
    tmux.kill_session(&session).await?;
    println!("Killed session for task {}", task.display_spec());
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
pub async fn tree(id: Option<i64>, deps: bool) -> anyhow::Result<()> {
    use crate::cli::tree::{build_forest, render_forest, render_single_tree};
    use crate::engine::tasks::TaskFilter;

    if deps {
        return dependency_tree(id).await;
    }

    let task_manager = init_task_manager().await?;

    // If a specific task ID is provided, show just that tree
    if let Some(task_id) = id {
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

        let task = task_manager.get_task(task_id).await?;

        // For a single task, we need to find its children
        // First get all tasks to build the full hierarchy
        let all_tasks = task_manager.list_tasks(TaskFilter::default()).await?;
        let forest = build_forest(&all_tasks);

        let target_id = task_id.to_string();
        if let Some(node) = find_node_in_forest(&forest, &target_id) {
            let output = render_single_tree(node, true);
            print!("{output}");
        } else {
            // Task exists but not in any tree - show as standalone
            match &task {
                Task::External(ext) => {
                    let node = crate::cli::tree::TreeNode::from_external(ext);
                    let output = render_single_tree(&node, true);
                    print!("{output}");
                }
                Task::Internal(int) => {
                    let node = crate::cli::tree::TreeNode::from_internal(int);
                    let output = render_single_tree(&node, true);
                    print!("{output}");
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

    let forest = build_forest(&tasks);
    let output = render_forest(&forest);
    print!("{output}");

    Ok(())
}

async fn dependency_tree(focus: Option<i64>) -> anyhow::Result<()> {
    use std::collections::HashMap;

    let store = crate::cli::init_store().await?;
    let focus_id = match focus {
        Some(id) => Some(crate::cli::task_ref::resolve_store_id(&store, &id.to_string()).await?),
        None => None,
    };
    let edges = store.list_dependency_edges().await?;
    let mut by_task: HashMap<i64, Vec<_>> = HashMap::new();
    for edge in edges {
        if let Some(focus_id) = focus_id {
            if edge.task_id != focus_id {
                continue;
            }
        }
        by_task.entry(edge.task_id).or_default().push(edge);
    }
    if by_task.is_empty() {
        println!("No dependency edges.");
        return Ok(());
    }
    let mut ids: Vec<i64> = by_task.keys().copied().collect();
    ids.sort_unstable();
    for id in ids {
        let group = &by_task[&id];
        let head = &group[0];
        let number = head
            .task_external_id
            .clone()
            .unwrap_or_else(|| format!("task:{id}"));
        let status = store.get(id).await.map_or_else(
            |_| "unknown".to_string(),
            |task| task.status.as_str().to_string(),
        );
        println!("{number} {} [{status}]", head.task_title);
        for edge in group {
            let label = dep_label(
                &edge.task_repo,
                &edge.depends_on_repo,
                edge.depends_on_number,
            );
            let done = store
                .blocker_snapshot(&edge.depends_on_repo, edge.depends_on_number)
                .await
                .ok()
                .flatten()
                .is_some_and(|snap| snap.is_satisfied());
            let state = if done { "done" } else { "open" };
            println!("  blocked by {label} [{state}]");
        }
    }
    Ok(())
}

/// List what a task waits on, and what waits on it.
pub async fn deps(id: &str) -> anyhow::Result<()> {
    let store = crate::cli::init_store().await?;
    let task_id = crate::cli::task_ref::resolve_store_id(&store, id).await?;
    let task = store.get(task_id).await?;
    let shown = task
        .external_id
        .clone()
        .unwrap_or_else(|| format!("internal:{}", task.id));
    println!("{shown} {} [{}]", task.title, task.status.as_str());
    println!("Waiting on:");
    let dependencies = store.list_dependencies(task_id).await?;
    if dependencies.is_empty() {
        println!("  (none)");
    }
    for dep in dependencies {
        let label = dep_label(&task.repo, &dep.depends_on_repo, dep.depends_on_number);
        let done = store
            .blocker_snapshot(&dep.depends_on_repo, dep.depends_on_number)
            .await?
            .is_some_and(|snap| snap.is_satisfied());
        let state = if done { "done" } else { "open" };
        let local = dep.blocker_status.as_deref().unwrap_or("untracked");
        println!("  {label}  {state}  local:{local}  source:{}", dep.source);
    }
    println!("Waiting on this:");
    let number = task
        .external_id
        .as_deref()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(0);
    let dependents = if number > 0 {
        store.list_dependents(&task.repo, number).await?
    } else {
        Vec::new()
    };
    if dependents.is_empty() {
        println!("  (none)");
    }
    for dep in dependents {
        let waiter = store.get(dep.task_id).await?;
        let label = waiter
            .external_id
            .clone()
            .unwrap_or_else(|| format!("internal:{}", waiter.id));
        println!("  {label}  {}  {}", waiter.status.as_str(), waiter.title);
    }
    Ok(())
}

/// Show logs / post-mortem for a task (internal or external).
pub async fn logs(id: &str) -> anyhow::Result<()> {
    let store_owned = crate::cli::init_store().await?;
    let resolved = crate::cli::task_ref::resolve(&store_owned, id).await?;
    let store: Option<Arc<TaskStore>> = Some(Arc::new(store_owned.clone()));
    let repo = resolved.repo.clone();
    let task_key = resolved.external_id.clone();

    if let Some(store_id) = resolved.store_id {
        if let Ok(task) = store_owned.get(store_id).await {
            if resolved.is_internal() {
                println!("ID: {} (internal)", task.id);
                println!("Title: {}", task.title);
                println!("Status: {}", task.status.as_str());
                if let Some(agent) = &task.agent {
                    println!("Agent: {agent}");
                }
                if let Some(reason) = &task.block_reason {
                    println!("Block reason: {reason}");
                }
                println!("Created: {}", task.created_at);
                println!("Updated: {}", task.updated_at);
            } else {
                println!("ID: {task_key} (external)");
                println!("Title: {}", task.title);
                println!("State: {}", task.status.as_str());
                println!("Labels: {}", task.labels.join(", "));
                println!("Author: {}", task.author);
                println!("URL: {}", task.url);
                println!("Created: {}", task.created_at);
                println!("Updated: {}", task.updated_at);
            }
        }
    } else {
        println!("ID: {}", resolved.display_spec());
    }

    // Task store fields
    {
        {
            // Agent, model, and attempts from store — single DB round-trip.
            if let Some(ref s) = store {
                if let Ok(Some(t)) = s.get_by_external_id(&repo, &task_key).await {
                    if let Some(ref agent) = t.agent {
                        println!("Agent: {agent}");
                    }
                    if let Some(ref model) = t.model {
                        println!("Model: {model}");
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
                println!("  total tokens:  {total_tokens}");
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
                            println!("    Error: {err}");
                        }
                        if !m.learnings.is_empty() {
                            println!("    Learnings:");
                            for l in &m.learnings {
                                println!("      - {l}");
                            }
                        }
                        if !m.files_modified.is_empty() {
                            println!("    Files modified:");
                            for f in &m.files_modified {
                                println!("      - {f}");
                            }
                        }
                    }
                }
            }
        }
    }

    // Show agent output from last attempt if available
    let attempt_n = store::opt_store_get_task(&store, &repo, &task_key)
        .await
        .map_or(0, |t| u32::try_from(t.attempts).unwrap_or(0));
    if attempt_n > 0 {
        match home::task_attempt_dir(&repo, &task_key, attempt_n) {
            Ok(attempt_dir) => {
                let output_file = attempt_dir.join("output.json");
                let exit_file = attempt_dir.join("exit.txt");

                if output_file.exists() {
                    if let Ok(content) = std::fs::read_to_string(&output_file) {
                        let tail =
                            crate::engine::runner::agents::patterns::safe_tail(&content, 2000);
                        let truncated = if content.len() > 2000 {
                            format!(
                                "...({} chars truncated)...\n{}",
                                content.len() - tail.len(),
                                tail
                            )
                        } else {
                            content.clone()
                        };
                        println!("\n--- Last attempt output (attempt {attempt_n}) ---");
                        println!("{truncated}");
                    }
                }

                if exit_file.exists() {
                    if let Ok(exit_code) = std::fs::read_to_string(&exit_file) {
                        println!("Exit code: {}", exit_code.trim());
                    }
                }
            }
            Err(e) => {
                println!("(could not resolve attempt dir: {e})");
            }
        }
    }

    // Show review agent output if available
    let review_task_id = format!("{task_key}-review");
    if let Ok(review_attempt_dir) = home::task_attempt_dir(&repo, &review_task_id, 1) {
        let review_output_file = review_attempt_dir.join("output.json");
        if review_output_file.exists() {
            if let Ok(content) = std::fs::read_to_string(&review_output_file) {
                println!("\n--- Review agent output ---");
                println!("{content}");
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
                            println!("    exit_code: {code}");
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
                            println!("    completed: {completed}");
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
        println!("\nLive tmux session detected: {session} — appending recent output:\n---");
        match tmux.capture_pane(&session, 200).await {
            Ok(output) => {
                println!("{output}");
            }
            Err(e) => {
                println!("(tmux capture failed: {e})");
            }
        }
    }

    Ok(())
}

/// Show run history for a task.
pub async fn runs(id: &str, verbose: bool) -> anyhow::Result<()> {
    let store = crate::cli::init_store().await?;
    let task_id = crate::cli::task_ref::resolve_store_id(&store, id).await?;

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
    let task_id = crate::cli::task_ref::resolve_store_id(&store, id).await?;

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
    let task_id = crate::cli::task_ref::resolve_store_id(&store, id).await?;

    let task = store.get(task_id).await?;
    let events = store.get_activity(task_id, None).await?;

    // Header
    println!("Routing History for Task {id}");
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
            println!("\nAttempt #{attempt_num}");
            println!("  Time:     {timestamp}");
            println!("  Agent:    {agent}");
            println!("  Model:    {model}");

            // Extract routing details
            if let Some(reason) = event.details.get("reason").and_then(|r| r.as_str()) {
                println!("  Reason:   {reason}");
            }
            if let Some(complexity) = event.details.get("complexity").and_then(|c| c.as_str()) {
                println!("  Complexity: {complexity}");
            }
        } else if event.event_type == "rerouted" {
            println!("  → Rerouted at {timestamp}");

            // Check for failure reason
            if let Some(failure) = event.details.get("failure_reason").and_then(|f| f.as_str()) {
                println!("    Failure: {failure}");
            } else if let Some(failure) = event.details.get("silence_duration_secs") {
                println!("    Failure: Agent silent for {failure}s");
            }

            // Show from/to agent if available
            if let Some(from) = event.details.get("from_agent").and_then(|a| a.as_str()) {
                if let Some(to) = event.details.get("to_agent").and_then(|a| a.as_str()) {
                    println!("    Change:   {from} → {to}");
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
        println!("  Current agent: {agent}");
    }
    if let Some(ref model) = task.model {
        println!("  Current model: {model}");
    }

    Ok(())
}

/// Unified diagnostic view for a task: status, agent, attempts, last error, PR, block reason.
pub async fn inspect(id: &str) -> anyhow::Result<()> {
    let store = crate::cli::init_store().await?;
    let task_id = crate::cli::task_ref::resolve_store_id(&store, id).await?;

    let task = store.get(task_id).await?;

    // ── Identity ─────────────────────────────────────────────────────────────
    println!("Task: {}", task.title);
    println!(
        "  ID      : {} (internal:{})",
        task.external_id.as_deref().unwrap_or("-"),
        task.id
    );
    println!("  Status  : {}", task.status.as_str());
    if task.status == crate::store::TaskStatus::Waiting {
        let blockers = store
            .unmet_blocker_labels(task.id)
            .await
            .unwrap_or_default();
        if !blockers.is_empty() {
            println!("  Waiting : {}", blockers.join(", "));
        }
    }
    println!(
        "  Created : {} ({})",
        task.created_at,
        format_age(&task.created_at)
    );
    println!(
        "  Updated : {} ({})",
        task.updated_at,
        format_age(&task.updated_at)
    );
    if !task.url.is_empty() {
        println!("  URL     : {}", task.url);
    }

    // ── Routing ───────────────────────────────────────────────────────────────
    println!();
    println!("Routing:");
    println!("  Agent      : {}", task.agent.as_deref().unwrap_or("-"));
    println!("  Model      : {}", task.model.as_deref().unwrap_or("-"));
    if !task.complexity.is_empty() {
        println!("  Complexity : {}", task.complexity);
    }
    if task.route_attempts > 0 {
        println!("  Route attempts : {}", task.route_attempts);
    }
    if !task.route_reason.is_empty() {
        println!("  Reason     : {}", truncate_err(&task.route_reason, 120));
    }

    // ── Execution ─────────────────────────────────────────────────────────────
    println!();
    println!("Execution:");
    println!("  Attempts : {}", task.attempts);
    if !task.branch.is_empty() {
        println!("  Branch   : {}", task.branch);
    }
    if !task.worktree.is_empty() {
        println!("  Worktree : {}", task.worktree);
    }
    if !task.last_error.is_empty() {
        println!("  Last error : {}", truncate_err(&task.last_error, 200));
    }
    if !task.summary.is_empty() {
        println!("  Summary  : {}", truncate_err(&task.summary, 200));
    }

    // ── PR / Review ───────────────────────────────────────────────────────────
    if task.pr_number.is_some() || task.review_cycles > 0 || !task.pr_review_context.is_empty() {
        println!();
        println!("PR / Review:");
        if let Some(pr) = task.pr_number {
            println!("  PR number     : {pr}");
        }
        if task.review_cycles > 0 {
            println!("  Review cycles : {}", task.review_cycles);
        }
        if !task.pr_review_context.is_empty() {
            println!(
                "  Review context: {}",
                truncate_err(&task.pr_review_context, 200)
            );
        }
    }

    // Each failed review attempt, so a rebroadcast escalation shows what kept failing
    // instead of only the final "escalated after N retries"
    let attempts: Vec<_> = store
        .get_activity(task.id, None)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|a| a.event_type == "review_attempt_failed")
        .collect();
    if !attempts.is_empty() {
        println!();
        println!("Review attempts ({} failed):", attempts.len());
        for activity in attempts.iter().rev().take(10).rev() {
            println!("  {}", review_attempt_line(activity));
        }
    }

    // ── Block reason ─────────────────────────────────────────────────────────
    if let Some(ref reason) = task.block_reason {
        println!();
        println!("Blocked:");
        println!("  Reason : {reason}");
        if task.auto_unblock_count > 0 {
            println!("  Auto-unblock attempts : {}", task.auto_unblock_count);
        }
    }

    // ── Cost ─────────────────────────────────────────────────────────────────
    let total_tokens = task.input_tokens + task.output_tokens;
    if total_tokens > 0 || task.total_cost_usd > 0.0 {
        println!();
        println!("Cost:");
        println!(
            "  Tokens : {}in / {}out",
            task.input_tokens, task.output_tokens
        );
        println!("  Total  : ${:.6}", task.total_cost_usd);
    }

    // ── Live session ─────────────────────────────────────────────────────────
    let task_key = task
        .external_id
        .clone()
        .unwrap_or_else(|| format!("internal:{}", task.id));
    let tmux = TmuxManager::new();
    let session = tmux.session_name(&task.repo, &task_key);
    if tmux.session_exists(&session).await {
        println!();
        println!("Live session: {session} (use `orch task attach {id}` to connect)");
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

    fn task_ref(status: &str) -> store::CliTaskRef {
        store::CliTaskRef {
            store_id: Some(7),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "t".to_string(),
            external_id: "58".to_string(),
            status: status.to_string(),
        }
    }

    #[test]
    fn refuse_if_done_rejects_done_tasks() {
        for action in ["retry", "unblock"] {
            let err = refuse_if_done(&task_ref("done"), action)
                .expect_err("done task must not be revived");
            let msg = err.to_string();
            assert!(msg.contains("is done"), "got: {msg}");
            assert!(msg.contains(action), "got: {msg}");
        }
    }

    #[test]
    fn refuse_if_done_allows_other_statuses() {
        for status in ["blocked", "needs_review", "new", "in_review", ""] {
            assert!(refuse_if_done(&task_ref(status), "unblock").is_ok());
        }
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

    /// Verify the forced-routing field construction for agent-only case.
    #[test]
    fn retry_force_route_fields_agent_only() {
        let forced_agent = "claude";
        let model: Option<String> = None;
        let fields: Vec<(&str, serde_json::Value)> = {
            let mut f = vec![
                ("agent", serde_json::json!(forced_agent)),
                (
                    "route_reason",
                    serde_json::json!(format!("forced via retry --agent {forced_agent}")),
                ),
            ];
            if let Some(ref m) = model {
                f.push(("model", serde_json::json!(m)));
            }
            f
        };
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].1, serde_json::json!("claude"));
        assert_eq!(
            fields[1].1,
            serde_json::json!("forced via retry --agent claude")
        );
    }

    /// Verify the forced-routing field construction for agent+model case.
    #[test]
    fn retry_force_route_fields_agent_and_model() {
        let fields = retry_force_fields("claude", Some("opus"));
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[0].1, serde_json::json!("claude"));
        assert_eq!(fields[2].0, "model");
        assert_eq!(fields[2].1, serde_json::json!("opus"));
    }

    /// Verify that the model hint display is correct in both cases.
    #[test]
    fn retry_model_hint_display() {
        let model_some: Option<String> = Some("opus".to_string());
        let model_none: Option<String> = None;
        assert_eq!(model_some.as_deref().unwrap_or("(router default)"), "opus");
        assert_eq!(
            model_none.as_deref().unwrap_or("(router default)"),
            "(router default)"
        );
    }

    #[test]
    fn review_attempt_line_shows_cause_and_reason() {
        let activity = store::TaskActivity {
            id: 1,
            task_id: 7,
            timestamp: "2026-10-25T22:40:00Z".to_string(),
            event_type: "review_attempt_failed".to_string(),
            from_status: None,
            to_status: None,
            agent: None,
            model: None,
            details: serde_json::json!({
                "cause": "rate_limit",
                "external": true,
                "reason": "claude failed: rate limit reached",
            }),
        };
        let line = review_attempt_line(&activity);
        assert_eq!(
            line,
            "2026-10-25T22:40:00Z  rate_limit (external): claude failed: rate limit reached"
        );
    }

    #[test]
    fn truncate_err_under_limit() {
        let s = "short error";
        assert_eq!(truncate_err(s, 50), "short error");
    }

    #[test]
    fn truncate_err_exact_limit() {
        let s = "short error";
        assert_eq!(truncate_err(s, 11), "short error");
    }

    #[test]
    fn truncate_err_truncates() {
        let s = "this is a long error message that should be cut";
        let result = truncate_err(s, 10);
        assert!(result.ends_with("..."));
        assert!(result.len() <= 13); // 10 chars + "..."
    }

    #[test]
    fn truncate_err_multi_byte_char_boundary() {
        // "héllo world" bytes: h(0)=1, é(1-2)=2, l(3)=1, l(4)=1, o(5)=1, sp(6)=1, w(7)=1...
        // byte 3 = 'l' (valid boundary); byte 4 = 'l' (valid); byte 5 = 'o' (valid)
        // No panic = success; exact result varies by where the boundary lands
        let s = "héllo world";
        let result = truncate_err(s, 3);
        assert!(result.ends_with("..."));
        assert!(!result.contains("panic"));
    }

    #[test]
    fn truncate_err_emoji() {
        // "🔥 error message": 🔥=4 bytes, space=1, e=1, r=1, r=1, o=1, sp=1, m=1...
        // byte 5 = 'e' (valid boundary); walk-back not needed
        let s = "🔥 error message";
        let result = truncate_err(s, 5);
        assert_eq!(result, "🔥 ...");
    }

    #[test]
    fn truncate_err_cjk() {
        // "你好世界": 你=3 bytes(0-2), 好=3 bytes(3-5), 世=3 bytes(6-8), 界=3 bytes(9-11)
        // byte 4 falls inside 好; walk back to byte 3 (= end of 你)
        let s = "你好世界";
        let result = truncate_err(s, 4);
        assert_eq!(result, "你...");
    }
}
