//! Job scheduler — replaces `jobs_tick.sh`.
//!
//! Reads job definitions from `jobs.yml`, checks cron schedules against
//! the current time, and creates tasks for due jobs. Handles catch-up
//! for missed schedules (capped at 24h).
//!
//! Job types:
//! - `task`: creates a task (GitHub Issue or internal SQLite) and lets the engine dispatch it
//! - `bash`: runs a shell command directly (no LLM)
//!
//! For task jobs, the `external` field controls where the task is created:
//! - `external: true` (default): Creates a GitHub Issue
//! - `external: false`: Creates an internal SQLite task

use crate::backends::{ExternalBackend, ExternalId};
use crate::db::Db;
use crate::engine::internal_tasks::{create_internal_task_with_source, get_internal_task};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

/// A scheduled job definition (from jobs.yml).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    #[serde(default = "default_job_type")]
    pub r#type: String,
    pub schedule: String,
    #[serde(default)]
    pub task: Option<TaskTemplate>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub dir: Option<String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_external")]
    pub external: bool, // NEW: true = GitHub Issue, false = internal SQLite task
    #[serde(default)]
    pub last_run: Option<String>,
    #[serde(default)]
    pub last_task_status: Option<String>,
    #[serde(default)]
    pub active_task_id: Option<String>,
}

/// Template for creating a task from a job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskTemplate {
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub agent: Option<String>,
}

fn default_job_type() -> String {
    "task".to_string()
}

fn default_enabled() -> bool {
    true
}

fn default_external() -> bool {
    true // Default to external (GitHub) for backward compatibility
}

/// Top-level config structure (for reading jobs from .orch.yml / config.yml).
#[derive(Debug, Serialize, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    jobs: Vec<Job>,
    // Capture all other fields so we can round-trip them
    #[serde(flatten)]
    other: serde_yml::Mapping,
}

/// Resolve the config file that contains jobs.
///
/// Priority:
/// 1. `.orch.yml` in the current directory (project config)
/// 2. `~/.orch/config.yml` (global config)
pub fn resolve_jobs_path() -> PathBuf {
    let project = PathBuf::from(".orch.yml");
    if project.exists() {
        return project;
    }
    crate::home::config_path().unwrap_or_else(|_| PathBuf::from(".orch/config.yml"))
}

/// Load jobs from the orchestrator config file.
///
/// Reads the `jobs` key from `.orch.yml` (project) or
/// `~/.orch/config.yml` (global).
pub fn load_jobs(path: &PathBuf) -> anyhow::Result<Vec<Job>> {
    if !path.exists() {
        return Ok(vec![]);
    }
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let file: ConfigFile =
        serde_yml::from_str(&content).with_context(|| format!("parsing {}", path.display()))?;
    Ok(file.jobs)
}

/// Save jobs back to the config file, preserving all other keys.
pub fn save_jobs(path: &PathBuf, jobs: &[Job]) -> anyhow::Result<()> {
    // Read the existing file to preserve non-jobs keys
    let mut file: ConfigFile = if path.exists() {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        serde_yml::from_str(&content).with_context(|| format!("parsing {}", path.display()))?
    } else {
        ConfigFile {
            jobs: vec![],
            other: serde_yml::Mapping::new(),
        }
    };

    file.jobs = jobs.to_vec();
    let content = serde_yml::to_string(&file)?;
    std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Check all jobs and execute due ones.
pub async fn tick(
    jobs_path: &PathBuf,
    backend: &Arc<dyn ExternalBackend>,
    db: &Arc<Db>,
) -> anyhow::Result<()> {
    let mut jobs = load_jobs(jobs_path)?;
    let mut changed = false;
    let now = chrono::Utc::now();

    for job in &mut jobs {
        if !job.enabled {
            continue;
        }

        // Check if schedule matches
        let is_due = match &job.last_run {
            Some(last) => crate::cron::check(&job.schedule, Some(last))?,
            None => crate::cron::check(&job.schedule, None)?,
        };

        if !is_due {
            continue;
        }

        // Check if previous task is still active
        let mut should_clear_task_id = false;
        let mut should_skip = false;

        if let Some(ref task_id) = job.active_task_id {
            let task_id_clone = task_id.clone();
            let is_active = if job.external {
                // Check external (GitHub) task
                match backend.get_task(&ExternalId(task_id_clone.clone())).await {
                    Ok(task) => {
                        let status = task.labels.iter().find(|l| l.starts_with("status:"));
                        match status.map(|s| s.as_str()) {
                            Some("status:in_progress")
                            | Some("status:routed")
                            | Some("status:new") => true,
                            None => true,     // No status label — treat as active
                            Some(_) => false, // Terminal state
                        }
                    }
                    Err(e) => {
                        // Task lookup failed (deleted, API error, rate limit).
                        // Clear active_task_id so the job isn't permanently blocked.
                        tracing::warn!(
                            job_id = job.id,
                            task_id = task_id_clone,
                            ?e,
                            "cannot fetch active task, clearing active_task_id"
                        );
                        should_clear_task_id = true;
                        job.last_task_status = Some("error".to_string());
                        false
                    }
                }
            } else {
                // Check internal (SQLite) task
                // Parse "internal:{id}" format
                if let Some(internal_id_str) = task_id_clone.strip_prefix("internal:") {
                    if let Ok(internal_id) = internal_id_str.parse::<i64>() {
                        match get_internal_task(db, internal_id).await {
                            Ok(Some(task)) => {
                                match task.status.as_str() {
                                    "new" | "routed" | "in_progress" => true,
                                    _ => false, // Terminal state (done, blocked, needs_review, etc.)
                                }
                            }
                            Ok(None) => {
                                // Task not found — clear active_task_id
                                tracing::warn!(
                                    job_id = job.id,
                                    task_id = task_id_clone,
                                    "internal task not found, clearing active_task_id"
                                );
                                should_clear_task_id = true;
                                false
                            }
                            Err(e) => {
                                tracing::warn!(
                                    job_id = job.id,
                                    task_id = task_id_clone,
                                    ?e,
                                    "cannot fetch internal task, clearing active_task_id"
                                );
                                should_clear_task_id = true;
                                false
                            }
                        }
                    } else {
                        // Invalid format — clear it
                        should_clear_task_id = true;
                        false
                    }
                } else {
                    // Legacy format without prefix — clear it
                    should_clear_task_id = true;
                    false
                }
            };

            if is_active {
                tracing::debug!(
                    job_id = job.id,
                    task_id = task_id_clone,
                    external = job.external,
                    "skipping: previous task still active"
                );
                should_skip = true;
            }
        }

        // Apply deferred mutations
        if should_clear_task_id {
            job.active_task_id = None;
        }

        if should_skip {
            continue;
        }

        tracing::info!(job_id = job.id, r#type = job.r#type, "job due, executing");

        // Set last_run BEFORE execution (prevents catch-up loops on restart)
        job.last_run = Some(now.format("%Y-%m-%dT%H:%M:%SZ").to_string());
        changed = true;

        match job.r#type.as_str() {
            "task" => {
                if let Some(ref template) = job.task {
                    if job.external {
                        // Create external (GitHub) task
                        let mut labels = template.labels.clone();
                        labels.push("scheduled".to_string());
                        labels.push(format!("job:{}", job.id));

                        if let Some(ref agent) = template.agent {
                            if !agent.is_empty() {
                                labels.push(format!("agent:{agent}"));
                            }
                        }

                        match backend
                            .create_task(&template.title, &template.body, &labels)
                            .await
                        {
                            Ok(ext_id) => {
                                tracing::info!(
                                    job_id = job.id,
                                    task_id = ext_id.0,
                                    "created external task"
                                );
                                job.active_task_id = Some(ext_id.0);
                                job.last_task_status = Some("new".to_string());
                            }
                            Err(e) => {
                                tracing::error!(
                                    job_id = job.id,
                                    ?e,
                                    "failed to create external task"
                                );
                                job.last_task_status = Some("failed".to_string());
                            }
                        }
                    } else {
                        // Create internal (SQLite) task
                        match create_internal_task_with_source(
                            db,
                            &template.title,
                            &template.body,
                            "cron",
                            &job.id,
                        )
                        .await
                        {
                            Ok(internal_id) => {
                                let task_id = format!("internal:{}", internal_id);
                                tracing::info!(job_id = job.id, task_id, "created internal task");
                                job.active_task_id = Some(task_id);
                                job.last_task_status = Some("new".to_string());
                            }
                            Err(e) => {
                                tracing::error!(
                                    job_id = job.id,
                                    ?e,
                                    "failed to create internal task"
                                );
                                job.last_task_status = Some("failed".to_string());
                            }
                        }
                    }
                }
            }
            "bash" => {
                if let Some(ref cmd) = job.command {
                    let dir = job.dir.as_deref().unwrap_or(".");
                    tracing::info!(job_id = job.id, cmd, dir, "running bash command");

                    let output = tokio::process::Command::new("bash")
                        .arg("-c")
                        .arg(cmd)
                        .current_dir(dir)
                        .output()
                        .await;

                    match output {
                        Ok(o) if o.status.success() => {
                            job.last_task_status = Some("done".to_string());
                        }
                        Ok(o) => {
                            let stderr = String::from_utf8_lossy(&o.stderr);
                            tracing::warn!(
                                job_id = job.id,
                                code = o.status.code(),
                                %stderr,
                                "bash command failed"
                            );
                            job.last_task_status = Some("failed".to_string());
                        }
                        Err(e) => {
                            tracing::error!(job_id = job.id, ?e, "bash command error");
                            job.last_task_status = Some("failed".to_string());
                        }
                    }
                }
            }
            other => {
                tracing::warn!(job_id = job.id, r#type = other, "unknown job type");
            }
        }
    }

    if changed {
        save_jobs(jobs_path, &jobs)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.yml");
        std::fs::write(&path, "jobs: []\n").unwrap();
        let jobs = load_jobs(&path).unwrap();
        assert!(jobs.is_empty());
    }

    #[test]
    fn load_missing_file() {
        let path = PathBuf::from("/nonexistent/jobs.yml");
        let jobs = load_jobs(&path).unwrap();
        assert!(jobs.is_empty());
    }

    #[test]
    fn load_job_with_task_template() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.yml");
        std::fs::write(
            &path,
            r#"jobs:
  - id: morning
    schedule: "0 8 * * *"
    task:
      title: Morning review
      body: Do the review
      labels: [maintenance]
      agent: claude
"#,
        )
        .unwrap();

        let jobs = load_jobs(&path).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, "morning");
        assert_eq!(jobs[0].schedule, "0 8 * * *");
        assert_eq!(jobs[0].r#type, "task");
        assert!(jobs[0].enabled);
        let tmpl = jobs[0].task.as_ref().unwrap();
        assert_eq!(tmpl.title, "Morning review");
        assert_eq!(tmpl.agent, Some("claude".to_string()));
    }

    #[test]
    fn load_bash_job() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.yml");
        std::fs::write(
            &path,
            r#"jobs:
  - id: cleanup
    type: bash
    schedule: "0 * * * *"
    command: echo hello
    dir: /tmp
"#,
        )
        .unwrap();

        let jobs = load_jobs(&path).unwrap();
        assert_eq!(jobs[0].r#type, "bash");
        assert_eq!(jobs[0].command, Some("echo hello".to_string()));
        assert_eq!(jobs[0].dir, Some("/tmp".to_string()));
    }

    #[test]
    fn save_and_reload_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.yml");

        let jobs = vec![Job {
            id: "test".to_string(),
            r#type: "task".to_string(),
            schedule: "0 9 * * 1".to_string(),
            task: Some(TaskTemplate {
                title: "Weekly review".to_string(),
                body: "Do it".to_string(),
                labels: vec!["review".to_string()],
                agent: None,
            }),
            command: None,
            dir: None,
            enabled: true,
            external: true,
            last_run: Some("2026-02-22T10:00:00Z".to_string()),
            last_task_status: Some("done".to_string()),
            active_task_id: None,
        }];

        save_jobs(&path, &jobs).unwrap();
        let reloaded = load_jobs(&path).unwrap();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0].id, "test");
        assert_eq!(
            reloaded[0].last_run,
            Some("2026-02-22T10:00:00Z".to_string())
        );
    }

    #[test]
    fn disabled_job_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.yml");
        std::fs::write(
            &path,
            r#"jobs:
  - id: disabled-job
    schedule: "0 0 * * *"
    enabled: false
    task:
      title: Never runs
      body: ""
"#,
        )
        .unwrap();

        let jobs = load_jobs(&path).unwrap();
        assert!(!jobs[0].enabled);
    }

    #[test]
    fn default_type_is_task() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.yml");
        std::fs::write(
            &path,
            r#"jobs:
  - id: no-type
    schedule: "0 0 * * *"
    task:
      title: Test
      body: ""
"#,
        )
        .unwrap();

        let jobs = load_jobs(&path).unwrap();
        assert_eq!(jobs[0].r#type, "task");
    }
}
