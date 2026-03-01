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
//! - `external: true` (default): Creates a GitHub Issue
//! - `external: false`: Creates an internal SQLite task

use crate::backends::{ExternalBackend, ExternalId};
use crate::cmd::CommandErrorContext;
use crate::db::{Db, ErrorStat, MetricsSummary, SlowTaskInfo};
use crate::engine::internal_tasks::{create_internal_task_with_source, get_internal_task};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

/// Maximum self-improvement issues that can be created per week.
const MAX_SELF_IMPROVEMENT_ISSUES_PER_WEEK: i64 = 3;

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
                        .output_with_context()
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
            "self-review" => {
                // Analyze metrics and create self-improvement issues
                match run_self_review(db, backend).await {
                    Ok(issues_created) => {
                        tracing::info!(job_id = job.id, issues_created, "self-review completed");
                        job.last_task_status = Some(if issues_created > 0 {
                            "done".to_string()
                        } else {
                            "no_issues".to_string()
                        });
                    }
                    Err(e) => {
                        tracing::error!(job_id = job.id, ?e, "self-review failed");
                        job.last_task_status = Some("failed".to_string());
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

/// Run the self-review job: analyze metrics and create improvement issues.
async fn run_self_review(db: &Arc<Db>, backend: &Arc<dyn ExternalBackend>) -> anyhow::Result<i64> {
    let mut issues_created: i64 = 0;

    // Check rate limit
    let current_count = db.count_self_improvement_issues_7d().await?;
    if current_count >= MAX_SELF_IMPROVEMENT_ISSUES_PER_WEEK {
        tracing::info!(
            current_count,
            max = MAX_SELF_IMPROVEMENT_ISSUES_PER_WEEK,
            "rate limited: max self-improvement issues per week reached"
        );
        return Ok(0);
    }

    // Gather metrics data
    let summary = db.get_metrics_summary_24h().await?;
    let slow_tasks = db.get_slow_tasks_7d().await?;
    let error_distribution = db.get_error_distribution_7d().await?;

    // Analyze and create issues for each pattern
    let remaining_slots = MAX_SELF_IMPROVEMENT_ISSUES_PER_WEEK - current_count;

    // Issue 1: High failure rate agent
    if issues_created < remaining_slots {
        if let Some(issue) = detect_high_failure_agent(&summary.agent_stats) {
            if create_self_improvement_issue(backend, &issue.title, &issue.body)
                .await
                .is_ok()
            {
                db.increment_self_improvement_counter().await?;
                issues_created += 1;
            }
        }
    }

    // Issue 2: Common error patterns
    if issues_created < remaining_slots {
        if let Some(issue) = detect_common_errors(&error_distribution) {
            if create_self_improvement_issue(backend, &issue.title, &issue.body)
                .await
                .is_ok()
            {
                db.increment_self_improvement_counter().await?;
                issues_created += 1;
            }
        }
    }

    // Issue 3: Slow tasks pattern
    if issues_created < remaining_slots {
        if let Some(issue) = detect_slow_tasks(&slow_tasks, &summary) {
            if create_self_improvement_issue(backend, &issue.title, &issue.body)
                .await
                .is_ok()
            {
                db.increment_self_improvement_counter().await?;
                issues_created += 1;
            }
        }
    }

    Ok(issues_created)
}

/// Create a self-improvement issue in GitHub, with title-based deduplication.
async fn create_self_improvement_issue(
    backend: &Arc<dyn ExternalBackend>,
    title: &str,
    body: &str,
) -> anyhow::Result<ExternalId> {
    // Deduplicate: skip if an open issue with the same title already exists
    match backend
        .has_open_issue_with_title(title, "self-improvement")
        .await
    {
        Ok(true) => {
            tracing::info!(title, "skipping duplicate self-improvement issue");
            return Err(anyhow::anyhow!("duplicate issue: {}", title));
        }
        Err(e) => {
            tracing::warn!(?e, "dedup check failed, proceeding with issue creation");
        }
        Ok(false) => {}
    }

    let labels = vec![
        "self-improvement".to_string(),
        "scheduled".to_string(),
        "automation".to_string(),
    ];
    backend.create_task(title, body, &labels).await
}

/// Detect if any agent has a high failure rate (>50% failures).
fn detect_high_failure_agent(agent_stats: &[crate::db::AgentStat]) -> Option<ImprovementIssue> {
    for stat in agent_stats {
        if stat.total_runs >= 3 && stat.success_rate < 50.0 {
            return Some(ImprovementIssue {
                title: format!(
                    "[Self-Improvement] High failure rate for {} agent",
                    stat.agent
                ),
                body: format!(
                    r#"## Analysis

The **{}** agent has a high failure rate based on recent metrics:

- **Total runs (24h):** {}
- **Successful:** {}
- **Failure rate:** {:.1}%

## Recommendation

Consider investigating the {} agent for:
- Task type mismatch (routing to wrong complexity level)
- Missing required skills or tools
- Model capacity issues

## Evidence

This issue was automatically created by the orchestrator's self-review job."#,
                    stat.agent,
                    stat.total_runs,
                    stat.success_count,
                    100.0 - stat.success_rate,
                    stat.agent
                ),
            });
        }
    }
    None
}

/// Detect common error patterns.
fn detect_common_errors(errors: &[ErrorStat]) -> Option<ImprovementIssue> {
    if errors.is_empty() {
        return None;
    }

    // Find error types with 2+ occurrences
    let significant_errors: Vec<_> = errors.iter().filter(|e| e.count >= 2).collect();

    if significant_errors.is_empty() {
        return None;
    }

    let mut body = String::from("## Error Distribution (Last 7 Days)\n\n");
    body.push_str("| Error Type | Count |\n|------------|-------|\n");
    for err in &significant_errors {
        body.push_str(&format!(
            "| {} | {} |\n",
            err.error_type.as_deref().unwrap_or("unknown"),
            err.count
        ));
    }
    body.push_str("\n## Recommendation\n\n");
    body.push_str("Consider addressing these recurring error patterns:\n");
    for err in &significant_errors {
        if let Some(ref error_type) = err.error_type {
            body.push_str(&format!(
                "- **{}**: {} occurrences\n",
                error_type, err.count
            ));
        }
    }
    body.push_str("\n## Evidence\n\nThis issue was automatically created by the orchestrator's self-review job.\n");

    Some(ImprovementIssue {
        title: "[Self-Improvement] Recurring error patterns detected".to_string(),
        body,
    })
}

/// Detect slow tasks pattern.
fn detect_slow_tasks(
    slow_tasks: &[SlowTaskInfo],
    summary: &MetricsSummary,
) -> Option<ImprovementIssue> {
    if slow_tasks.is_empty() {
        return None;
    }

    // Only create issue if there are genuinely slow tasks (complex tasks > 10 min)
    let slow_complex_tasks: Vec<_> = slow_tasks
        .iter()
        .filter(|t| t.complexity.as_deref() == Some("complex") && t.duration_seconds > 600.0)
        .collect();

    if slow_complex_tasks.is_empty() {
        return None;
    }

    let mut body = String::from("## Slowest Completed Tasks (Last 7 Days)\n\n");
    body.push_str("| Task ID | Agent | Complexity | Duration |\n|---------|-------|------------|----------|\n");
    for task in slow_tasks.iter().take(5) {
        body.push_str(&format!(
            "| {} | {} | {} | {:.1}m |\n",
            task.task_id,
            task.agent,
            task.complexity.as_deref().unwrap_or("unknown"),
            task.duration_seconds / 60.0
        ));
    }

    body.push_str("\n## Duration by Complexity\n\n");
    if let Some(avg) = summary.avg_duration_simple {
        body.push_str(&format!("- **Simple:** {:.1}m\n", avg / 60.0));
    }
    if let Some(avg) = summary.avg_duration_medium {
        body.push_str(&format!("- **Medium:** {:.1}m\n", avg / 60.0));
    }
    if let Some(avg) = summary.avg_duration_complex {
        body.push_str(&format!("- **Complex:** {:.1}m\n", avg / 60.0));
    }

    body.push_str("\n## Recommendation\n\n");
    body.push_str("Consider the following improvements:\n");
    body.push_str("- Review routing logic for complex tasks\n");
    body.push_str("- Add more specific skills or tools for complex work\n");
    body.push_str("- Consider using a more capable model for complex tasks\n");
    body.push_str("\n## Evidence\n\n");
    body.push_str("This issue was automatically created by the orchestrator's self-review job.\n");

    Some(ImprovementIssue {
        title: "[Self-Improvement] Slow task execution patterns detected".to_string(),
        body,
    })
}

/// A self-improvement issue to be created.
struct ImprovementIssue {
    title: String,
    body: String,
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

    #[test]
    fn detect_high_failure_agent_with_high_failure() {
        let agent_stats = vec![
            crate::db::AgentStat {
                agent: "claude".to_string(),
                total_runs: 10,
                success_count: 3,
                success_rate: 30.0,
            },
            crate::db::AgentStat {
                agent: "codex".to_string(),
                total_runs: 5,
                success_count: 4,
                success_rate: 80.0,
            },
        ];

        let issue = detect_high_failure_agent(&agent_stats);
        assert!(issue.is_some());
        let issue = issue.unwrap();
        assert!(issue.title.contains("claude"));
        assert!(issue.body.contains("70.0%")); // failure rate
    }

    #[test]
    fn detect_high_failure_agent_ignores_low_runs() {
        // Agent with high failure rate but only 2 runs should be ignored
        let agent_stats = vec![crate::db::AgentStat {
            agent: "claude".to_string(),
            total_runs: 2,
            success_count: 0,
            success_rate: 0.0,
        }];

        let issue = detect_high_failure_agent(&agent_stats);
        assert!(issue.is_none());
    }

    #[test]
    fn detect_high_failure_agent_ignores_good_success_rate() {
        let agent_stats = vec![crate::db::AgentStat {
            agent: "claude".to_string(),
            total_runs: 10,
            success_count: 9,
            success_rate: 90.0,
        }];

        let issue = detect_high_failure_agent(&agent_stats);
        assert!(issue.is_none());
    }

    #[test]
    fn detect_common_errors_with_significant_errors() {
        let errors = vec![
            ErrorStat {
                error_type: Some("timeout".to_string()),
                count: 5,
            },
            ErrorStat {
                error_type: Some("rate_limit".to_string()),
                count: 3,
            },
            ErrorStat {
                error_type: Some("auth_error".to_string()),
                count: 1,
            },
        ];

        let issue = detect_common_errors(&errors);
        assert!(issue.is_some());
        let issue = issue.unwrap();
        assert!(issue.title.contains("Recurring error patterns"));
        assert!(issue.body.contains("timeout"));
        assert!(issue.body.contains("rate_limit"));
        // auth_error has count 1, should not be included
        assert!(!issue.body.contains("auth_error"));
    }

    #[test]
    fn detect_common_errors_ignores_low_count() {
        // All errors have count < 2
        let errors = vec![ErrorStat {
            error_type: Some("timeout".to_string()),
            count: 1,
        }];

        let issue = detect_common_errors(&errors);
        assert!(issue.is_none());
    }

    #[test]
    fn detect_common_errors_empty_errors() {
        let errors: Vec<ErrorStat> = vec![];
        let issue = detect_common_errors(&errors);
        assert!(issue.is_none());
    }

    #[test]
    fn detect_slow_tasks_with_slow_complex_tasks() {
        let slow_tasks = vec![
            SlowTaskInfo {
                task_id: "task-123".to_string(),
                agent: "claude".to_string(),
                complexity: Some("complex".to_string()),
                duration_seconds: 900.0, // 15 minutes
            },
            SlowTaskInfo {
                task_id: "task-456".to_string(),
                agent: "codex".to_string(),
                complexity: Some("medium".to_string()),
                duration_seconds: 300.0, // 5 minutes
            },
        ];
        let summary = MetricsSummary {
            tasks_completed_24h: 10,
            tasks_failed_24h: 2,
            avg_duration_simple: Some(60.0),
            avg_duration_medium: Some(300.0),
            avg_duration_complex: Some(600.0),
            agent_stats: vec![],
            rate_limits_24h: 0,
        };

        let issue = detect_slow_tasks(&slow_tasks, &summary);
        assert!(issue.is_some());
        let issue = issue.unwrap();
        assert!(issue.title.contains("Slow task"));
        assert!(issue.body.contains("15.0m")); // 900s = 15m
    }

    #[test]
    fn detect_slow_tasks_ignores_fast_tasks() {
        let slow_tasks = vec![SlowTaskInfo {
            task_id: "task-123".to_string(),
            agent: "claude".to_string(),
            complexity: Some("complex".to_string()),
            duration_seconds: 300.0, // Only 5 minutes - not slow enough
        }];
        let summary = MetricsSummary {
            tasks_completed_24h: 10,
            tasks_failed_24h: 2,
            avg_duration_simple: Some(60.0),
            avg_duration_medium: Some(300.0),
            avg_duration_complex: Some(600.0),
            agent_stats: vec![],
            rate_limits_24h: 0,
        };

        let issue = detect_slow_tasks(&slow_tasks, &summary);
        assert!(issue.is_none());
    }

    #[test]
    fn detect_slow_tasks_empty_list() {
        let slow_tasks: Vec<SlowTaskInfo> = vec![];
        let summary = MetricsSummary {
            tasks_completed_24h: 10,
            tasks_failed_24h: 2,
            avg_duration_simple: Some(60.0),
            avg_duration_medium: Some(300.0),
            avg_duration_complex: Some(600.0),
            agent_stats: vec![],
            rate_limits_24h: 0,
        };

        let issue = detect_slow_tasks(&slow_tasks, &summary);
        assert!(issue.is_none());
    }
}
