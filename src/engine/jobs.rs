//! Job scheduler — replaces `jobs_tick.sh`.
//!
//! Reads job definitions from `jobs.yml`, checks cron schedules against
//! the current time, and creates tasks for due jobs. Handles catch-up
//! for missed schedules (capped at 24h).
//!
//! Job types:
//! - `task`: creates a task (GitHub Issue or internal SQLite) and lets the engine dispatch it
//! - `bash`: runs a shell command directly (no LLM)
//! - `self-review`: analyzes task metrics and creates self-improvement issues
//!
//! For task jobs, the `external` field controls where the task is created:
//! - `external: false` (default): Creates an internal SQLite task
//! - `external: true`: Creates a GitHub Issue

use crate::backends::{ExternalBackend, ExternalId};
use crate::channels::notification::TaskNotification;
use crate::channels::transport::Transport;
use crate::cmd::CommandErrorContext;
use crate::store::{JobState, TaskStatus};
use anyhow::Context;
use cron::Schedule;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

// Note: self-review analysis is now performed by scheduled tasks configured
// in `.orch.yml` and executed as regular `task` jobs. Hardcoded self-review
// logic was removed to keep this module a generic job dispatcher.

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
    pub external: bool, // true = GitHub Issue, false (default) = internal SQLite task
    /// Send a Telegram notification when this job completes or fails.
    /// Uses `channels.telegram.chat_id` from config unless `notify_target` overrides it.
    #[serde(default)]
    pub notify: bool,
    /// Override the Telegram chat_id for this job's completion notification.
    /// Only used when `notify: true`. Falls back to `channels.telegram.chat_id`.
    #[serde(default)]
    pub notify_target: Option<String>,
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
    false // Default to internal (SQLite) tasks
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
/// 2. `.orch.yml` in any registered project directory (from global config)
/// 3. `~/.orch/config.yml` (global config)
pub fn resolve_jobs_path() -> PathBuf {
    // 1. Check cwd
    let project = PathBuf::from(".orch.yml");
    if project.exists() {
        return project;
    }

    // 2. Check registered project directories (handles brew service running from /)
    if let Ok(paths) = crate::config::get_project_paths() {
        for path_str in &paths {
            let candidate = PathBuf::from(path_str).join(".orch.yml");
            if candidate.exists() {
                return candidate;
            }
        }
    }

    // 3. Fall back to global config
    crate::home::config_path().unwrap_or_else(|_| PathBuf::from(".orch/config.yml"))
}

/// Load jobs from the orch config file.
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
    for job in &file.jobs {
        let expanded = crate::cron::expand_alias(&job.schedule).with_context(|| {
            format!("job '{}': invalid cron schedule '{}'", job.id, job.schedule)
        })?;
        let normalized = crate::cron::normalize_dow(&expanded);
        let full_expr = format!("0 {normalized} *");
        Schedule::from_str(&full_expr).with_context(|| {
            format!("job '{}': invalid cron schedule '{}'", job.id, job.schedule)
        })?;
    }
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

/// Returns `true` if the error indicates a "not found" (404) response.
///
/// Used to distinguish permanent task deletion from transient API errors.
/// A 404 should clear `active_task_id`; other errors (rate limit, 5xx,
/// network timeout) should be retried next tick without losing the reference.
fn is_not_found_error(e: &anyhow::Error) -> bool {
    // Walk the full error chain. Both the HTTP backend ("GitHub API GET {url}
    // failed (404): ...") and the CLI backend ("... 404 ...") include "404"
    // in the error string, matching the convention used throughout this codebase.
    e.chain().any(|cause| cause.to_string().contains("404"))
}

/// Check all jobs and execute due ones.
///
/// Runtime state (last_run, active_task_id, last_task_status) is stored in
/// SQLite via the `job_state` table, not in the YAML config file.
pub async fn tick(
    jobs_path: &PathBuf,
    backend: &Arc<dyn ExternalBackend>,
    store: Option<&Arc<crate::store::TaskStore>>,
    repo: &str,
    transport: Option<&Arc<Transport>>,
) -> anyhow::Result<()> {
    let jobs = load_jobs(jobs_path)?;
    let now = chrono::Utc::now();

    for job in &jobs {
        if !job.enabled {
            continue;
        }

        // Load runtime state from SQLite
        let mut state = if let Some(s) = store {
            match s.get_job_state(repo, &job.id).await {
                Ok(Some(st)) => st,
                Ok(None) => JobState {
                    repo: repo.to_string(),
                    job_id: job.id.clone(),
                    last_run: None,
                    last_task_status: None,
                    active_task_id: None,
                },
                Err(e) => {
                    tracing::error!(job_id = job.id, ?e, "failed to load job state, skipping");
                    continue;
                }
            }
        } else {
            JobState {
                repo: repo.to_string(),
                job_id: job.id.clone(),
                last_run: None,
                last_task_status: None,
                active_task_id: None,
            }
        };

        // Check if schedule matches
        let is_due = match &state.last_run {
            Some(last) => match crate::cron::check(&job.schedule, Some(last)) {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!(
                        job_id = job.id,
                        schedule = job.schedule,
                        ?e,
                        "invalid cron expression, skipping"
                    );
                    continue;
                }
            },
            None => match crate::cron::check(&job.schedule, None) {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!(
                        job_id = job.id,
                        schedule = job.schedule,
                        ?e,
                        "invalid cron expression, skipping"
                    );
                    continue;
                }
            },
        };

        if !is_due {
            continue;
        }

        // Check if previous task is still active
        let mut should_clear_task_id = false;
        let mut should_skip = false;

        if let Some(ref task_id) = state.active_task_id {
            if task_id.is_empty() {
                state.active_task_id = None;
            }
        }

        if let Some(ref task_id) = state.active_task_id {
            let task_id_clone = task_id.clone();
            let is_active = if job.external {
                // Check external (GitHub) task
                match backend.get_task(&ExternalId(task_id_clone.clone())).await {
                    Ok(task) => {
                        let status = task.labels.iter().find(|l| l.starts_with("status:"));
                        match status.map(|s| s.as_str()) {
                            Some("status:in_progress")
                            | Some("status:routed")
                            | Some("status:new")
                            | Some("status:needs_review")
                            | Some("status:in_review") => true,
                            None => true,     // No status label — treat as active
                            Some(_) => false, // Terminal state
                        }
                    }
                    Err(e) => {
                        if is_not_found_error(&e) {
                            tracing::warn!(
                                job_id = job.id,
                                task_id = task_id_clone,
                                "active task not found (404), clearing active_task_id"
                            );
                            should_clear_task_id = true;
                            state.last_task_status = Some("error".to_string());
                        } else {
                            tracing::warn!(
                                job_id = job.id,
                                task_id = task_id_clone,
                                ?e,
                                "transient error fetching active task, skipping tick (active_task_id preserved)"
                            );
                            should_skip = true;
                        }
                        false
                    }
                }
            } else {
                // Check internal task via store
                if let Some(s) = store {
                    if let Ok(Some(store_id)) = s.resolve_task_id(repo, &task_id_clone).await {
                        match s.get(store_id).await {
                            Ok(task) => match task.status {
                                TaskStatus::New
                                | TaskStatus::Routed
                                | TaskStatus::InProgress
                                | TaskStatus::NeedsReview
                                | TaskStatus::InReview => true,
                                _ => false, // Terminal state (Done, Blocked, Cancelled)
                            },
                            Err(e) => {
                                tracing::warn!(
                                    job_id = job.id,
                                    task_id = task_id_clone,
                                    ?e,
                                    "cannot fetch internal task from store, clearing active_task_id"
                                );
                                should_clear_task_id = true;
                                false
                            }
                        }
                    } else {
                        tracing::warn!(
                            job_id = job.id,
                            task_id = task_id_clone,
                            "internal task not found in store, clearing active_task_id"
                        );
                        should_clear_task_id = true;
                        false
                    }
                } else {
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
            state.active_task_id = None;
        }

        if should_skip {
            continue;
        }

        tracing::info!(job_id = job.id, r#type = job.r#type, "job due, executing");

        // Persist last_run BEFORE execution to prevent catch-up on restart.
        // On restart the runtime state is loaded from SQLite, so writing
        // last_run here ensures a crash during execution won't cause the
        // job to be considered due again immediately.
        state.last_run = Some(now.format("%Y-%m-%dT%H:%M:%SZ").to_string());
        if let Some(s) = store {
            if let Err(e) = s.upsert_job_state(&state).await {
                tracing::error!(
                    job_id = job.id,
                    ?e,
                    "failed to persist job state before execution — skipping this tick to preserve restart safety"
                );
                continue; // retry next tick instead of risking duplicate execution
            }
        }

        // Execute the job (may mutate state.active_task_id / last_task_status).
        execute_job(job, &mut state, backend, store, repo, transport).await;

        // Persist updated state (active_task_id, last_task_status) after execution.
        if let Some(s) = store {
            if let Err(e) = s.upsert_job_state(&state).await {
                tracing::error!(
                    job_id = job.id,
                    ?e,
                    "failed to persist job state after execution"
                );
            }
        }
    }

    Ok(())
}

/// Execute a single job immediately (ignoring schedule and active-task guard).
///
/// Updates `state` with the result but does NOT persist to SQLite — caller is
/// responsible for that.
pub async fn execute_job(
    job: &Job,
    state: &mut JobState,
    backend: &Arc<dyn ExternalBackend>,
    store: Option<&Arc<crate::store::TaskStore>>,
    repo: &str,
    transport: Option<&Arc<Transport>>,
) {
    match job.r#type.as_str() {
        "task" => {
            if let Some(ref template) = job.task {
                if job.external {
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
                            state.active_task_id = Some(ext_id.0);
                            state.last_task_status = Some("new".to_string());
                        }
                        Err(e) => {
                            tracing::error!(job_id = job.id, ?e, "failed to create external task");
                            state.last_task_status = Some("failed".to_string());
                        }
                    }
                } else if let Some(s) = store {
                    match s
                        .create_internal(
                            repo,
                            &template.title,
                            &template.body,
                            "cron",
                            &job.id,
                            None,
                        )
                        .await
                    {
                        Ok(internal_id) => {
                            let task_id = format!("internal:{}", internal_id);
                            tracing::info!(job_id = job.id, task_id, "created internal task");
                            state.active_task_id = Some(task_id);
                            state.last_task_status = Some("new".to_string());
                        }
                        Err(e) => {
                            tracing::error!(job_id = job.id, ?e, "failed to create internal task");
                            state.last_task_status = Some("failed".to_string());
                        }
                    }
                } else {
                    tracing::error!(
                        job_id = job.id,
                        "no store available for internal task creation"
                    );
                    state.last_task_status = Some("failed".to_string());
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
                    .output_with_context()
                    .await;

                match output {
                    Ok(o) if o.status.success() => {
                        state.last_task_status = Some("done".to_string());
                    }
                    Ok(o) => {
                        let stderr = String::from_utf8_lossy(&o.stderr);
                        tracing::warn!(
                            job_id = job.id,
                            code = o.status.code(),
                            %stderr,
                            "bash command failed"
                        );
                        state.last_task_status = Some("failed".to_string());
                    }
                    Err(e) => {
                        tracing::error!(job_id = job.id, ?e, "bash command error");
                        state.last_task_status = Some("failed".to_string());
                    }
                }
            }
        }
        // Note: previous versions supported a special-case "self-review"
        // job type with embedded analysis logic. That logic has been removed
        // — self-review work should be configured as a regular `task` job in
        // `.orch.yml` and executed by agents. Unknown job types fall through
        // to the warning handler below.
        other => {
            tracing::warn!(job_id = job.id, r#type = other, "unknown job type");
        }
    }

    // Send Telegram notification if requested for this job.
    if job.notify {
        if let Some(t) = transport {
            let status = state
                .last_task_status
                .as_deref()
                .unwrap_or("unknown")
                .to_string();
            let summary = match status.as_str() {
                "done" => format!("Job `{}` completed successfully.", job.id),
                "failed" => format!("Job `{}` failed.", job.id),
                "new" => format!("Job `{}` created a new task.", job.id),
                other => format!("Job `{}` finished with status: {other}.", job.id),
            };
            // Resolve target chat_id: job override → config default
            let target = job
                .notify_target
                .clone()
                .or_else(|| crate::config::get("channels.telegram.chat_id").ok());
            let notification = TaskNotification {
                task_id: job.id.clone(),
                title: format!("Job: {}", job.id),
                status,
                agent: "scheduler".to_string(),
                duration_seconds: 0.0,
                summary,
                repo: Some(repo.to_string()),
                notify_target: target,
            };
            t.push_notification(notification);
        }
    }
}

// NOTE: self-review analysis is handled by scheduled `task` jobs configured
// in `.orch.yml` and executed by agents. There is no embedded Rust logic
// here — this module remains a generic job dispatcher.
