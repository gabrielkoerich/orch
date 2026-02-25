use crate::config;
use crate::engine::jobs::{self, Job, TaskTemplate};
use anyhow::Context;
use std::path::PathBuf;
use std::sync::Arc;

fn jobs_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".orchestrator")
        .join("jobs.yml")
}

/// List scheduled jobs.
pub fn list() -> anyhow::Result<()> {
    let path = jobs_path();
    let jobs = jobs::load_jobs(&path)?;

    if jobs.is_empty() {
        println!("No jobs configured.");
        println!("Add one with: orch job add \"0 9 * * *\" \"Daily review\"");
        return Ok(());
    }

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

        if let Some(ref last_run) = job.last_run {
            println!(
                "  Last run: {} (status: {})",
                last_run,
                job.last_task_status.as_deref().unwrap_or("unknown")
            );
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
    let path = jobs_path();
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
        last_run: None,
        last_task_status: None,
        active_task_id: None,
    };

    jobs.push(job);
    jobs::save_jobs(&path, &jobs)?;

    println!("Added job '{}' (schedule: {})", id, schedule);
    Ok(())
}

/// Remove a job.
pub fn remove(id: &str) -> anyhow::Result<()> {
    let path = jobs_path();
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
    let path = jobs_path();
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

/// Run one job scheduler tick.
pub async fn tick() -> anyhow::Result<()> {
    use crate::backends::github::GitHubBackend;
    use crate::backends::ExternalBackend;
    use crate::db::Db;

    let repo = config::get("repo").context("'repo' not set in config")?;
    let backend: Arc<dyn ExternalBackend> = Arc::new(GitHubBackend::new(repo));
    let db = Arc::new(Db::open(&crate::db::default_path()?)?);
    db.migrate().await?;

    let path = jobs_path();
    jobs::tick(&path, &backend, &db).await?;

    println!("Job tick completed");
    Ok(())
}
