use crate::config;
use crate::engine::jobs::{self, Job, TaskTemplate};
use anyhow::Context;
use std::path::PathBuf;
use std::sync::Arc;

/// List scheduled jobs (with runtime state from SQLite).
pub async fn list() -> anyhow::Result<()> {
    let path = jobs::resolve_jobs_path();
    let jobs = jobs::load_jobs(&path)?;

    if jobs.is_empty() {
        println!("No jobs configured.");
        println!("Add one with: orch job add \"0 9 * * *\" \"Daily review\"");
        return Ok(());
    }

    // Load runtime state from SQLite
    let store = crate::cli::init_store().await.ok().map(Arc::new);
    let repo = config::get_current_repo().unwrap_or_default();
    let states: std::collections::HashMap<String, crate::store::JobState> =
        if let Some(ref s) = store {
            s.list_job_states(&repo)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|st| (st.job_id.clone(), st))
                .collect()
        } else {
            std::collections::HashMap::new()
        };

    println!(
        "{:<20} {:<8} {:<20} {:<10} TITLE/COMMAND",
        "ID", "TYPE", "SCHEDULE", "ENABLED"
    );
    println!("{}", "-".repeat(80));

    for job in &jobs {
        let desc = match job.r#type.as_str() {
            "bash" => job.command.as_deref().unwrap_or(""),
            _ => job.task.as_ref().map(|t| t.title.as_str()).unwrap_or(""),
        };

        println!(
            "{:<20} {:<8} {:<20} {:<10} {}",
            job.id,
            job.r#type,
            job.schedule,
            if job.enabled { "yes" } else { "no" },
            desc,
        );

        if let Some(state) = states.get(&job.id) {
            if let Some(ref last_run) = state.last_run {
                println!(
                    "  Last run: {} (status: {})",
                    last_run,
                    state.last_task_status.as_deref().unwrap_or("unknown")
                );
            }
        }
    }

    Ok(())
}

/// Add a scheduled job.
pub fn add(
    schedule: &str,
    title: &str,
    body: Option<&str>,
    job_type: &str,
    command: Option<&str>,
) -> anyhow::Result<()> {
    let path = jobs::resolve_jobs_path();
    let mut jobs = jobs::load_jobs(&path)?;

    // Generate ID from title
    let id = title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string();

    // Check for duplicate ID
    if jobs.iter().any(|j| j.id == id) {
        anyhow::bail!("job with id '{}' already exists", id);
    }

    let job = Job {
        id: id.clone(),
        r#type: job_type.to_string(),
        schedule: schedule.to_string(),
        task: if job_type == "task" {
            Some(TaskTemplate {
                title: title.to_string(),
                body: body.unwrap_or("").to_string(),
                prompt: None,
                labels: vec![],
                agent: None,
            })
        } else {
            None
        },
        command: command.map(String::from),
        dir: None,
        enabled: true,
        external: true,
        notify: false,
        notify_target: None,
    };

    jobs.push(job);
    jobs::save_jobs(&path, &jobs)?;

    println!("Added job '{}' (schedule: {})", id, schedule);
    Ok(())
}

/// Remove a job.
pub fn remove(id: &str) -> anyhow::Result<()> {
    let path = jobs::resolve_jobs_path();
    let mut jobs = jobs::load_jobs(&path)?;

    let initial_len = jobs.len();
    jobs.retain(|j| j.id != id);

    if jobs.len() == initial_len {
        anyhow::bail!("job '{}' not found", id);
    }

    jobs::save_jobs(&path, &jobs)?;
    println!("Removed job '{}'", id);
    Ok(())
}

/// Enable a job.
pub fn enable(id: &str) -> anyhow::Result<()> {
    toggle_job(id, true)
}

/// Disable a job.
pub fn disable(id: &str) -> anyhow::Result<()> {
    toggle_job(id, false)
}

fn toggle_job(id: &str, enabled: bool) -> anyhow::Result<()> {
    let path = jobs::resolve_jobs_path();
    let mut jobs = jobs::load_jobs(&path)?;

    let job = jobs
        .iter_mut()
        .find(|j| j.id == id)
        .with_context(|| format!("job '{}' not found", id))?;

    job.enabled = enabled;
    jobs::save_jobs(&path, &jobs)?;

    println!(
        "Job '{}' {}",
        id,
        if enabled { "enabled" } else { "disabled" }
    );
    Ok(())
}

/// Run a single job immediately, ignoring its cron schedule.
///
/// Searches all projects for the job ID. If the ID exists in multiple projects,
/// requires `--project` to disambiguate.
pub async fn run(job_id: &str, project: Option<&str>) -> anyhow::Result<()> {
    use crate::backends::github::GitHubBackend;
    use crate::backends::ExternalBackend;

    // 1. If inside an orch project (or --project given), try that first
    let mut resolved: Option<(String, jobs::Job, PathBuf)> = None;

    if project.is_none() {
        // Check if CWD is inside an orch project
        if let Ok(cwd_repo) = config::get_current_repo() {
            let jobs_path = jobs::resolve_jobs_path();
            if let Ok(all_jobs) = jobs::load_jobs(&jobs_path) {
                if let Some(job) = all_jobs.into_iter().find(|j| j.id == job_id) {
                    let base_dir = jobs_path
                        .parent()
                        .map(PathBuf::from)
                        .unwrap_or_else(|| PathBuf::from("."));
                    resolved = Some((cwd_repo, job, base_dir));
                }
            }
        }
    }

    // 2. If not resolved locally, search all projects
    if resolved.is_none() {
        let mut matches: Vec<(String, jobs::Job, PathBuf)> = Vec::new();

        let projects = crate::config::get_projects_with_paths().unwrap_or_default();
        for (repo, dir) in &projects {
            if let Some(filter) = project {
                if !repo.contains(filter) {
                    continue;
                }
            }
            let orch_yml = dir.join(".orch.yml");
            if let Ok(all_jobs) = jobs::load_jobs(&orch_yml) {
                for job in all_jobs {
                    if job.id == job_id {
                        matches.push((repo.clone(), job, dir.clone()));
                    }
                }
            }
        }

        match matches.len() {
            0 => anyhow::bail!("job '{}' not found in any project", job_id),
            1 => resolved = matches.pop(),
            _ => {
                let repos: Vec<&str> = matches.iter().map(|(r, _, _)| r.as_str()).collect();
                anyhow::bail!(
                    "job '{}' exists in multiple projects: {}\nUse --project to specify which one, e.g.: orch job run {} --project {}",
                    job_id,
                    repos.join(", "),
                    job_id,
                    repos[0],
                );
            }
        }
    }

    let (repo, job, base_dir) = match resolved {
        Some(r) => r,
        None => anyhow::bail!("job '{}' not found", job_id),
    };

    if !job.enabled {
        println!("Warning: job '{}' is disabled, running anyway", job_id);
    }

    let backend: Arc<dyn ExternalBackend> = Arc::new(GitHubBackend::new(repo.clone())?);
    let store = crate::cli::init_store().await.ok().map(Arc::new);

    // Load or create job state
    let mut state = if let Some(ref s) = store {
        s.get_job_state(&repo, job_id)
            .await?
            .unwrap_or_else(|| crate::store::JobState {
                repo: repo.clone(),
                job_id: job_id.to_string(),
                last_run: None,
                last_task_status: None,
                active_task_id: None,
            })
    } else {
        crate::store::JobState {
            repo: repo.clone(),
            job_id: job_id.to_string(),
            last_run: None,
            last_task_status: None,
            active_task_id: None,
        }
    };

    println!("Running job '{}' ({}) in {}", job_id, job.r#type, repo);

    // Persist last_run BEFORE execution to avoid duplicate runs if the
    // process crashes or is restarted during execution. The job runtime
    // state is loaded from SQLite on restart, so writing last_run first
    // prevents an immediate catch-up run.
    state.last_run = Some(chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string());
    if let Some(ref s) = store {
        if let Err(e) = s.upsert_job_state(&state).await {
            tracing::error!(
                job_id = job_id,
                ?e,
                "failed to persist job state before execution",
            );
        }
    }

    // Execute the job (may mutate state.active_task_id / last_task_status).
    jobs::execute_job(
        &job,
        &mut state,
        &backend,
        store.as_ref(),
        &repo,
        None,
        &base_dir,
    )
    .await;

    let status = state.last_task_status.as_deref().unwrap_or("unknown");
    if let Some(ref task_id) = state.active_task_id {
        println!("Done (status: {}, task: {})", status, task_id);
    } else {
        println!("Done (status: {})", status);
    }

    Ok(())
}

/// Run one job scheduler tick.
pub async fn tick() -> anyhow::Result<()> {
    use crate::backends::github::GitHubBackend;
    use crate::backends::ExternalBackend;

    let repo =
        config::get_current_repo().context("'repo' not set — ensure .orch.yml has gh.repo")?;
    let backend: Arc<dyn ExternalBackend> = Arc::new(GitHubBackend::new(repo.clone())?);
    let store = crate::cli::init_store().await.ok().map(std::sync::Arc::new);

    let path = jobs::resolve_jobs_path();
    jobs::tick(&path, &backend, store.as_ref(), &repo, None).await?;

    println!("Job tick completed");
    Ok(())
}
