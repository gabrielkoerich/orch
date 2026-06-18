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
use std::path::{Path, PathBuf};
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
    /// Path to the `prompts/jobs/*.md` file this job was loaded from, if any.
    ///
    /// `None` means the job was defined inline in `.orch.yml` / `config.yml`.
    /// Set by `scan_prompt_jobs`; skipped during serialization so it never
    /// leaks into config files.
    #[serde(skip)]
    pub prompt_file: Option<PathBuf>,
}

/// Template for creating a task from a job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskTemplate {
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// Path to a file (typically markdown) whose contents are used as the
    /// task body. Resolved relative to the directory of the config file
    /// that defines this job. Mutually exclusive with `body`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub agent: Option<String>,
}

/// Resolve the body for a task template, reading from `prompt` if set.
///
/// `base_dir` is the directory containing the jobs config file (used to
/// resolve relative `prompt` paths).
pub fn resolve_template_body(template: &TaskTemplate, base_dir: &Path) -> anyhow::Result<String> {
    match template.prompt.as_deref() {
        Some(rel) => {
            let path = Path::new(rel);
            let full = if path.is_absolute() {
                path.to_path_buf()
            } else {
                base_dir.join(path)
            };
            std::fs::read_to_string(&full)
                .with_context(|| format!("reading prompt file {}", full.display()))
        }
        None => Ok(template.body.clone()),
    }
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
    other: serde_norway::Mapping,
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

/// Load jobs from the orch config file plus any markdown prompt files
/// discovered under `<base_dir>/prompts/jobs/*.md`.
///
/// Inline `jobs:` in `.orch.yml` and discovered prompt files are merged;
/// duplicate ids across the two sources are rejected.
pub fn load_jobs(path: &PathBuf) -> anyhow::Result<Vec<Job>> {
    let base_dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut jobs: Vec<Job> = if path.exists() {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let file: ConfigFile = serde_norway::from_str(&content)
            .with_context(|| format!("parsing {}", path.display()))?;
        file.jobs
    } else {
        vec![]
    };

    let scanned = scan_prompt_jobs(&base_dir)?;
    let mut seen: std::collections::HashSet<String> = jobs.iter().map(|j| j.id.clone()).collect();
    for job in scanned {
        if !seen.insert(job.id.clone()) {
            anyhow::bail!(
                "duplicate job id '{}' — defined in both '{}' and prompts/jobs/",
                job.id,
                path.display()
            );
        }
        jobs.push(job);
    }

    for job in &jobs {
        validate_job(job, &base_dir)?;
    }
    Ok(jobs)
}

fn validate_job(job: &Job, base_dir: &Path) -> anyhow::Result<()> {
    let expanded = crate::cron::expand_alias(&job.schedule)
        .with_context(|| format!("job '{}': invalid cron schedule '{}'", job.id, job.schedule))?;
    let normalized = crate::cron::normalize_dow(&expanded);
    let full_expr = format!("0 {normalized} *");
    Schedule::from_str(&full_expr)
        .with_context(|| format!("job '{}': invalid cron schedule '{}'", job.id, job.schedule))?;
    if let Some(template) = job.task.as_ref() {
        if template.prompt.is_some() && !template.body.is_empty() {
            anyhow::bail!(
                "job '{}': set either 'task.body' or 'task.prompt', not both",
                job.id
            );
        }
        if let Some(rel) = template.prompt.as_deref() {
            let candidate = Path::new(rel);
            let full = if candidate.is_absolute() {
                candidate.to_path_buf()
            } else {
                base_dir.join(candidate)
            };
            if !full.exists() {
                anyhow::bail!(
                    "job '{}': prompt file not found: {}",
                    job.id,
                    full.display()
                );
            }
        }
    }
    Ok(())
}

/// Frontmatter schema for `prompts/jobs/*.md` files.
#[derive(Debug, Deserialize)]
struct JobFrontmatter {
    id: String,
    schedule: String,
    title: String,
    #[serde(default = "default_job_type")]
    r#type: String,
    #[serde(default = "default_enabled")]
    enabled: bool,
    #[serde(default = "default_external")]
    external: bool,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    notify: bool,
    #[serde(default)]
    notify_target: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    dir: Option<String>,
}

/// Split a markdown file into (frontmatter, body). Returns `None` if the file
/// does not start with a `---` fence.
fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let rest = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))?;
    let end = rest.find("\n---\n").or_else(|| rest.find("\n---\r\n"))?;
    let fm = &rest[..end];
    let after = &rest[end..];
    let body = after
        .strip_prefix("\n---\n")
        .or_else(|| after.strip_prefix("\n---\r\n"))
        .unwrap_or(after);
    Some((fm, body.trim_start_matches('\n')))
}

/// Scan `<base_dir>/prompts/jobs/*.md` for job-defining markdown files.
pub fn scan_prompt_jobs(base_dir: &Path) -> anyhow::Result<Vec<Job>> {
    let dir = base_dir.join("prompts").join("jobs");
    if !dir.is_dir() {
        return Ok(vec![]);
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("md"))
        .collect();
    entries.sort();

    let mut jobs = Vec::with_capacity(entries.len());
    for path in entries {
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let Some((fm, body)) = split_frontmatter(&content) else {
            continue; // No frontmatter → treat as a plain prompt referenced from .orch.yml
        };
        let meta: JobFrontmatter = serde_norway::from_str(fm)
            .with_context(|| format!("parsing frontmatter in {}", path.display()))?;
        jobs.push(Job {
            id: meta.id,
            r#type: meta.r#type,
            schedule: meta.schedule,
            task: Some(TaskTemplate {
                title: meta.title,
                body: body.to_string(),
                prompt: None,
                labels: meta.labels,
                agent: meta.agent,
            }),
            command: meta.command,
            dir: meta.dir,
            enabled: meta.enabled,
            external: meta.external,
            notify: meta.notify,
            notify_target: meta.notify_target,
            prompt_file: Some(path.clone()),
        });
    }
    Ok(jobs)
}

/// Save jobs back to the config file, preserving all other keys.
pub fn save_jobs(path: &PathBuf, jobs: &[Job]) -> anyhow::Result<()> {
    // Read the existing file to preserve non-jobs keys
    let mut file: ConfigFile = if path.exists() {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        serde_norway::from_str(&content).with_context(|| format!("parsing {}", path.display()))?
    } else {
        ConfigFile {
            jobs: vec![],
            other: serde_norway::Mapping::new(),
        }
    };

    // Only persist inline jobs (those without a prompt_file source).
    // Prompt-based jobs live in prompts/jobs/*.md and must not be duplicated
    // into .orch.yml — doing so causes a "duplicate job id" error on the next
    // load_jobs call.
    file.jobs = jobs
        .iter()
        .filter(|j| j.prompt_file.is_none())
        .cloned()
        .collect();
    let content = serde_norway::to_string(&file)?;
    std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Update the `enabled:` field in the YAML frontmatter of a prompt-based job
/// file, preserving all other content unchanged.
///
/// The function rewrites only the `enabled:` line inside the opening `---`
/// fence.  If the field is absent it is inserted before the closing `---`.
pub fn set_frontmatter_enabled(path: &Path, enabled: bool) -> anyhow::Result<()> {
    let original =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

    let value = if enabled { "true" } else { "false" };

    // Replace an existing `enabled: <anything>` line inside the frontmatter.
    // We only touch the first YAML block (between the opening and first closing
    // `---` fences) to avoid accidentally rewriting the body.
    let rewritten = rewrite_frontmatter_enabled(&original, value);
    std::fs::write(path, rewritten).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Rewrite the `enabled:` key inside the first `---` fence block of `content`.
///
/// If the key is present, its value is replaced.  If absent, it is appended
/// just before the closing `---` line.  The rest of the document is unchanged.
fn rewrite_frontmatter_enabled(content: &str, value: &str) -> String {
    // Locate the opening fence.
    let Some(rest) = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
    else {
        // No frontmatter: return unchanged.
        return content.to_string();
    };

    // Find the closing fence.
    let Some(close_pos) = rest.find("\n---\n").or_else(|| rest.find("\n---\r\n")) else {
        return content.to_string();
    };

    let fm = &rest[..close_pos];
    let after_close = &rest[close_pos..]; // starts with "\n---\n" or "\n---\r\n"

    // Rebuild the frontmatter, replacing or inserting `enabled:`.
    let new_line = format!("enabled: {value}");
    let mut found = false;
    let new_fm: String = fm
        .lines()
        .map(|line| {
            if line == "enabled: true" || line == "enabled: false" || line.starts_with("enabled: ")
            {
                found = true;
                new_line.clone()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    let new_fm = if found {
        new_fm
    } else {
        format!("{new_fm}\n{new_line}")
    };

    format!("---\n{new_fm}{after_close}")
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
    jobs_path: &Path,
    backend: &Arc<dyn ExternalBackend>,
    store: Option<&Arc<crate::store::TaskStore>>,
    repo: &str,
    transport: Option<&Arc<Transport>>,
) -> anyhow::Result<()> {
    let jobs_path_clone = jobs_path.to_path_buf();
    let jobs = match tokio::task::spawn_blocking(move || load_jobs(&jobs_path_clone))
        .await
        .map_err(|e| anyhow::anyhow!("load_jobs panicked: {e}"))
        .and_then(|r| r)
    {
        Ok(j) => j,
        Err(e) => {
            tracing::error!(
                path = %jobs_path.display(),
                repo = %repo,
                ?e,
                "job config invalid — skipping scheduler tick for this project"
            );
            return Ok(());
        }
    };
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
        let base_dir = jobs_path.parent().unwrap_or_else(|| Path::new("."));
        execute_job(job, &mut state, backend, store, repo, transport, base_dir).await;

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
    base_dir: &Path,
) {
    match job.r#type.as_str() {
        "task" => {
            if let Some(ref template) = job.task {
                let body = match resolve_template_body(template, base_dir) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::error!(job_id = job.id, ?e, "failed to resolve task body");
                        state.last_task_status = Some("failed".to_string());
                        return;
                    }
                };

                if job.external {
                    let mut labels = template.labels.clone();
                    labels.push("scheduled".to_string());
                    labels.push(format!("job:{}", job.id));

                    if let Some(ref agent) = template.agent {
                        if !agent.is_empty() {
                            labels.push(format!("agent:{agent}"));
                        }
                    }

                    match backend.create_task(&template.title, &body, &labels).await {
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
                        .create_internal(repo, &template.title, &body, "cron", &job.id, None)
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

                // Configurable timeout with 5-minute default. A bash job that hangs
                // (network operation, blocking subprocess) would otherwise park a Tokio
                // worker for its entire duration, blocking the inline tick loop.
                let timeout_secs: u64 = crate::config::get("jobs.bash_timeout_seconds")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(300);
                let timeout = std::time::Duration::from_secs(timeout_secs);

                let output = tokio::time::timeout(
                    timeout,
                    tokio::process::Command::new("bash")
                        .arg("-c")
                        .arg(cmd)
                        .current_dir(dir)
                        .output_with_context(),
                )
                .await;

                match output {
                    Ok(Ok(o)) if o.status.success() => {
                        state.last_task_status = Some("done".to_string());
                    }
                    Ok(Ok(o)) => {
                        let stderr = String::from_utf8_lossy(&o.stderr);
                        tracing::warn!(
                            job_id = job.id,
                            code = o.status.code(),
                            %stderr,
                            "bash command failed"
                        );
                        state.last_task_status = Some("failed".to_string());
                    }
                    Ok(Err(e)) => {
                        tracing::error!(job_id = job.id, ?e, "bash command error");
                        state.last_task_status = Some("failed".to_string());
                    }
                    Err(_) => {
                        tracing::error!(
                            job_id = job.id,
                            secs = timeout_secs,
                            "bash job timed out after {}s — killing to unblock tick loop",
                            timeout_secs
                        );
                        state.last_task_status = Some("timeout".to_string());
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
                pr_number: None,
            };
            t.push_notification(notification);
        }
    }
}

// NOTE: self-review analysis is handled by scheduled `task` jobs configured
// in `.orch.yml` and executed by agents. There is no embedded Rust logic
// here — this module remains a generic job dispatcher.

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write_config(dir: &Path, content: &str) -> PathBuf {
        let path = dir.join(".orch.yml");
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn load_jobs_reads_prompt_from_file() {
        let tmp = tempdir().unwrap();
        fs::create_dir(tmp.path().join("prompts")).unwrap();
        fs::write(tmp.path().join("prompts/hello.md"), "hello from file").unwrap();
        let cfg = write_config(
            tmp.path(),
            r#"
jobs:
  - id: with-prompt
    schedule: "0 9 * * *"
    task:
      title: "Daily"
      prompt: prompts/hello.md
"#,
        );

        let jobs = load_jobs(&cfg).unwrap();
        assert_eq!(jobs.len(), 1);
        let body = resolve_template_body(jobs[0].task.as_ref().unwrap(), tmp.path()).unwrap();
        assert_eq!(body, "hello from file");
    }

    #[test]
    fn load_jobs_rejects_both_body_and_prompt() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("p.md"), "x").unwrap();
        let cfg = write_config(
            tmp.path(),
            r#"
jobs:
  - id: conflict
    schedule: "0 9 * * *"
    task:
      title: "x"
      body: "inline"
      prompt: p.md
"#,
        );

        let err = load_jobs(&cfg).unwrap_err().to_string();
        assert!(err.contains("not both"), "unexpected error: {err}");
    }

    #[test]
    fn load_jobs_rejects_missing_prompt_file() {
        let tmp = tempdir().unwrap();
        let cfg = write_config(
            tmp.path(),
            r#"
jobs:
  - id: missing
    schedule: "0 9 * * *"
    task:
      title: "x"
      prompt: does-not-exist.md
"#,
        );

        let err = load_jobs(&cfg).unwrap_err().to_string();
        assert!(
            err.contains("prompt file not found"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_template_body_falls_back_to_inline_body() {
        let tmp = tempdir().unwrap();
        let template = TaskTemplate {
            title: "t".to_string(),
            body: "inline body".to_string(),
            prompt: None,
            labels: vec![],
            agent: None,
        };
        assert_eq!(
            resolve_template_body(&template, tmp.path()).unwrap(),
            "inline body"
        );
    }

    fn write_prompt_job(dir: &Path, name: &str, content: &str) {
        let jobs_dir = dir.join("prompts").join("jobs");
        fs::create_dir_all(&jobs_dir).unwrap();
        fs::write(jobs_dir.join(name), content).unwrap();
    }

    #[test]
    fn scan_prompt_jobs_picks_up_frontmatter_files() {
        let tmp = tempdir().unwrap();
        write_prompt_job(
            tmp.path(),
            "morning.md",
            "---\nid: morning\nschedule: '0 9 * * *'\ntitle: Morning\nlabels: [daily]\nagent: claude\n---\n\nDo morning stuff.\n",
        );
        let jobs = scan_prompt_jobs(tmp.path()).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, "morning");
        assert_eq!(jobs[0].schedule, "0 9 * * *");
        let task = jobs[0].task.as_ref().unwrap();
        assert_eq!(task.title, "Morning");
        assert_eq!(task.body, "Do morning stuff.\n");
        assert_eq!(task.labels, vec!["daily".to_string()]);
        assert_eq!(task.agent.as_deref(), Some("claude"));
    }

    #[test]
    fn load_jobs_merges_inline_and_scanned() {
        let tmp = tempdir().unwrap();
        write_prompt_job(
            tmp.path(),
            "scanned.md",
            "---\nid: scanned\nschedule: '0 9 * * *'\ntitle: Scanned\n---\n\nbody\n",
        );
        let cfg = write_config(
            tmp.path(),
            r#"
jobs:
  - id: inline
    schedule: "0 10 * * *"
    task:
      title: "Inline"
      body: "x"
"#,
        );
        let jobs = load_jobs(&cfg).unwrap();
        let ids: Vec<&str> = jobs.iter().map(|j| j.id.as_str()).collect();
        assert!(ids.contains(&"inline"));
        assert!(ids.contains(&"scanned"));
        assert_eq!(jobs.len(), 2);
    }

    #[test]
    fn load_jobs_rejects_duplicate_id_across_sources() {
        let tmp = tempdir().unwrap();
        write_prompt_job(
            tmp.path(),
            "dup.md",
            "---\nid: dup\nschedule: '0 9 * * *'\ntitle: Dup\n---\n\nbody\n",
        );
        let cfg = write_config(
            tmp.path(),
            r#"
jobs:
  - id: dup
    schedule: "0 10 * * *"
    task:
      title: "Dup inline"
      body: "x"
"#,
        );
        let err = load_jobs(&cfg).unwrap_err().to_string();
        assert!(err.contains("duplicate job id"), "unexpected error: {err}");
    }

    #[test]
    fn scan_prompt_jobs_ignores_files_without_frontmatter() {
        let tmp = tempdir().unwrap();
        write_prompt_job(
            tmp.path(),
            "plain.md",
            "just a prompt body, no frontmatter\n",
        );
        let jobs = scan_prompt_jobs(tmp.path()).unwrap();
        assert_eq!(jobs.len(), 0);
    }

    #[test]
    fn load_jobs_works_without_config_file_when_directory_present() {
        let tmp = tempdir().unwrap();
        write_prompt_job(
            tmp.path(),
            "solo.md",
            "---\nid: solo\nschedule: '0 9 * * *'\ntitle: Solo\n---\n\nbody\n",
        );
        let missing_cfg = tmp.path().join(".orch.yml");
        let jobs = load_jobs(&missing_cfg).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, "solo");
    }

    // ──── Issue #3193: frontmatter `enabled: false` must be respected ────────

    #[test]
    fn scan_prompt_jobs_respects_enabled_false() {
        let tmp = tempdir().unwrap();
        write_prompt_job(
            tmp.path(),
            "disabled.md",
            "---\nid: disabled\nschedule: '0 9 * * *'\ntitle: Disabled\nenabled: false\n---\n\nbody\n",
        );
        let jobs = scan_prompt_jobs(tmp.path()).unwrap();
        assert_eq!(jobs.len(), 1);
        assert!(!jobs[0].enabled, "job should be disabled via frontmatter");
    }

    #[test]
    fn scan_prompt_jobs_sets_prompt_file() {
        let tmp = tempdir().unwrap();
        write_prompt_job(
            tmp.path(),
            "morning.md",
            "---\nid: morning\nschedule: '0 9 * * *'\ntitle: Morning\n---\n\nbody\n",
        );
        let jobs = scan_prompt_jobs(tmp.path()).unwrap();
        assert_eq!(jobs.len(), 1);
        assert!(
            jobs[0].prompt_file.is_some(),
            "prompt_file should be set for frontmatter jobs"
        );
    }

    #[test]
    fn save_jobs_does_not_persist_prompt_based_jobs() {
        let tmp = tempdir().unwrap();
        // A prompt-based job
        write_prompt_job(
            tmp.path(),
            "prompt.md",
            "---\nid: from-prompt\nschedule: '0 9 * * *'\ntitle: Prompt Job\n---\n\nbody\n",
        );
        // An inline job in .orch.yml
        let cfg = write_config(
            tmp.path(),
            "jobs:\n  - id: inline\n    schedule: \"0 10 * * *\"\n    task:\n      title: Inline\n      body: x\n",
        );
        let jobs = load_jobs(&cfg).unwrap();
        assert_eq!(jobs.len(), 2);

        // save_jobs should only write the inline job, not the prompt-based one
        save_jobs(&cfg, &jobs).unwrap();

        let saved = load_jobs(&cfg).unwrap();
        assert_eq!(saved.len(), 2, "both jobs should still be loadable");
        let ids: Vec<&str> = saved.iter().map(|j| j.id.as_str()).collect();
        assert!(ids.contains(&"inline"));
        assert!(ids.contains(&"from-prompt"));
    }

    #[test]
    fn rewrite_frontmatter_enabled_replaces_existing_value() {
        let input = "---\nid: foo\nschedule: daily\nenabled: true\ntitle: Foo\n---\n\nbody\n";
        let out = rewrite_frontmatter_enabled(input, "false");
        assert!(
            out.contains("enabled: false"),
            "should replace enabled: true"
        );
        assert!(!out.contains("enabled: true"), "old value must be gone");
        assert!(out.contains("body"), "body must be preserved");
    }

    #[test]
    fn rewrite_frontmatter_enabled_inserts_when_absent() {
        let input = "---\nid: foo\nschedule: daily\ntitle: Foo\n---\n\nbody\n";
        let out = rewrite_frontmatter_enabled(input, "false");
        assert!(
            out.contains("enabled: false"),
            "should insert enabled: false"
        );
        assert!(out.contains("body"), "body must be preserved");
    }

    #[test]
    fn set_frontmatter_enabled_writes_file() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("job.md");
        fs::write(
            &path,
            "---\nid: foo\nschedule: daily\ntitle: Foo\nenabled: true\n---\n\nbody\n",
        )
        .unwrap();
        set_frontmatter_enabled(&path, false).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("enabled: false"));
        assert!(!content.contains("enabled: true"));
    }
}
