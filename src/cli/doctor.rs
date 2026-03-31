use crate::backends::github::GitHubBackend;
use crate::backends::{ExternalBackend, ExternalId};
use crate::config;
use crate::github::http::GhHttp;
use crate::store::{Task, TaskStatus};
use crate::store::TaskStore;
use crate::tmux::TmuxManager;
use anyhow::Context;
use std::path::Path;
use std::sync::Arc;

/// A single diagnostic finding from `orch doctor`.
struct Finding {
    severity: Severity,
    task_id: String,
    message: String,
}

#[derive(Clone, Copy)]
enum Severity {
    Error,
    Warning,
}

impl Severity {
    fn symbol(self) -> &'static str {
        match self {
            Self::Error => "ERR",
            Self::Warning => "WARN",
        }
    }
}

/// Run all diagnostic checks and print findings.
pub async fn run(fix: bool) -> anyhow::Result<()> {
    let repo =
        config::get_current_repo().context("'repo' not set — ensure .orch.yml has gh.repo")?;
    let store = Arc::new(crate::cli::init_store().await?);
    let gh = GhHttp::new()?;
    let backend: Arc<dyn ExternalBackend> = Arc::new(GitHubBackend::new(repo.clone())?);
    let tmux = TmuxManager::new();

    let mut findings: Vec<Finding> = Vec::new();

    // Load all tasks from the store for the current repo.
    let all_tasks = store.list_all(&repo).await?;

    let done_tasks: Vec<_> = all_tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Done)
        .collect();
    let in_progress_tasks: Vec<_> = all_tasks
        .iter()
        .filter(|t| t.status == TaskStatus::InProgress)
        .collect();

    // 1. Done tasks with no merged PR
    for task in &done_tasks {
        if task.origin == "internal" && task.external_id.is_none() {
            continue; // pure internal tasks don't need PRs
        }
        check_done_no_merged_pr(task, &gh, &repo, &mut findings).await;
    }

    // 2. Done tasks with dirty worktrees (uncommitted changes)
    for task in &done_tasks {
        check_dirty_worktree(task, &mut findings);
    }

    // 3. Done tasks with unpushed commits
    for task in &done_tasks {
        check_unpushed_commits(task, &mut findings);
    }

    // 4. Closed GitHub issues with non-done SQLite status (and vice versa)
    for task in &all_tasks {
        if task.origin == "internal" {
            continue;
        }
        check_issue_status_mismatch(task, &backend, &mut findings).await;
    }

    // 5. Label/status mismatch
    for task in &all_tasks {
        if task.origin == "internal" {
            continue;
        }
        check_label_mismatch(task, &backend, &mut findings).await;
    }

    // 6. Stale in_progress tasks (no tmux session)
    for task in &in_progress_tasks {
        check_stale_in_progress(task, &repo, &tmux, &mut findings).await;
    }

    // 7. Orphaned worktrees
    check_orphaned_worktrees(&all_tasks, &mut findings);

    // Print results
    if findings.is_empty() {
        println!("No issues found. All task state is consistent.");
        return Ok(());
    }

    let errors = findings
        .iter()
        .filter(|f| matches!(f.severity, Severity::Error))
        .count();
    let warnings = findings
        .iter()
        .filter(|f| matches!(f.severity, Severity::Warning))
        .count();

    for f in &findings {
        println!("[{}] #{}: {}", f.severity.symbol(), f.task_id, f.message);
    }

    println!(
        "\nFound {} issue(s): {} error(s), {} warning(s)",
        findings.len(),
        errors,
        warnings,
    );

    if !fix && errors > 0 {
        println!("\nRun `orch doctor --fix` to attempt automatic repairs.");
    }

    if fix {
        apply_fixes(&findings, &store, &repo, &backend, &gh).await?;
    }

    Ok(())
}

/// Check 1: Done task with no merged PR.
async fn check_done_no_merged_pr(
    task: &Task,
    gh: &GhHttp,
    repo: &str,
    findings: &mut Vec<Finding>,
) {
    let task_label = task_label(task);

    match task.pr_number {
        Some(pr_num) => {
            // PR exists — check if it's merged
            match gh.get_pr(repo, pr_num as u64).await {
                Ok(pr) => {
                    if pr.merged != Some(true) {
                        findings.push(Finding {
                            severity: Severity::Error,
                            task_id: task_label,
                            message: format!(
                                "done but PR #{} is {} (not merged)",
                                pr_num, pr.state
                            ),
                        });
                    }
                }
                Err(e) => {
                    findings.push(Finding {
                        severity: Severity::Warning,
                        task_id: task_label,
                        message: format!("done with PR #{} but failed to fetch PR: {}", pr_num, e),
                    });
                }
            }
        }
        None => {
            // No PR recorded — check if branch has a PR
            if !task.branch.is_empty() {
                match gh.get_pr_number(repo, &task.branch).await {
                    Ok(Some(pr_num)) => {
                        findings.push(Finding {
                            severity: Severity::Warning,
                            task_id: task_label,
                            message: format!(
                                "done with no pr_number in store but branch has PR #{}",
                                pr_num
                            ),
                        });
                    }
                    Ok(None) => {
                        findings.push(Finding {
                            severity: Severity::Error,
                            task_id: task_label,
                            message: "done but no PR was ever created".to_string(),
                        });
                    }
                    Err(_) => {
                        findings.push(Finding {
                            severity: Severity::Error,
                            task_id: task_label,
                            message: "done but no PR number recorded and branch lookup failed"
                                .to_string(),
                        });
                    }
                }
            } else {
                findings.push(Finding {
                    severity: Severity::Error,
                    task_id: task_label,
                    message: "done but no branch or PR recorded".to_string(),
                });
            }
        }
    }
}

/// Check 2: Done task with dirty worktree (uncommitted changes).
fn check_dirty_worktree(task: &Task, findings: &mut Vec<Finding>) {
    if task.worktree.is_empty() || task.worktree_cleaned {
        return;
    }
    let wt = Path::new(&task.worktree);
    if !wt.exists() {
        return;
    }
    // Check for uncommitted changes using git status
    let output = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(wt)
        .output();
    match output {
        Ok(out) if !out.stdout.is_empty() => {
            findings.push(Finding {
                severity: Severity::Error,
                task_id: task_label(task),
                message: format!(
                    "done but worktree has uncommitted changes: {}",
                    task.worktree
                ),
            });
        }
        _ => {}
    }
}

/// Check 3: Done task with unpushed commits.
fn check_unpushed_commits(task: &Task, findings: &mut Vec<Finding>) {
    if task.worktree.is_empty() || task.worktree_cleaned || task.branch.is_empty() {
        return;
    }
    let wt = Path::new(&task.worktree);
    if !wt.exists() {
        return;
    }
    // Check for commits not pushed to origin
    let output = std::process::Command::new("git")
        .args(["log", &format!("origin/{}..HEAD", task.branch), "--oneline"])
        .current_dir(wt)
        .output();
    match output {
        Ok(out) if !out.stdout.is_empty() => {
            let count = out.stdout.iter().filter(|&&b| b == b'\n').count();
            findings.push(Finding {
                severity: Severity::Error,
                task_id: task_label(task),
                message: format!(
                    "done but {} unpushed commit(s) in worktree: {}",
                    count, task.worktree
                ),
            });
        }
        _ => {}
    }
}

/// Check 4: Closed GitHub issue with non-done SQLite status (and vice versa).
async fn check_issue_status_mismatch(
    task: &Task,
    backend: &Arc<dyn ExternalBackend>,
    findings: &mut Vec<Finding>,
) {
    let ext_id = match &task.external_id {
        Some(id) => id.clone(),
        None => return,
    };

    let ext_task = match backend.get_task(&ExternalId(ext_id)).await {
        Ok(t) => t,
        Err(_) => return, // can't verify, skip
    };

    let issue_closed = ext_task.state == "closed";
    let task_done = task.status == TaskStatus::Done;

    if issue_closed && !task_done {
        findings.push(Finding {
            severity: Severity::Error,
            task_id: task_label(task),
            message: format!(
                "GitHub issue is closed but SQLite status is '{}'",
                task.status.as_str()
            ),
        });
    } else if !issue_closed && task_done {
        findings.push(Finding {
            severity: Severity::Warning,
            task_id: task_label(task),
            message: "SQLite status is 'done' but GitHub issue is still open".to_string(),
        });
    }
}

/// Check 5: Label/status mismatch.
async fn check_label_mismatch(
    task: &Task,
    backend: &Arc<dyn ExternalBackend>,
    findings: &mut Vec<Finding>,
) {
    let ext_id = match &task.external_id {
        Some(id) => id.clone(),
        None => return,
    };

    let ext_task = match backend.get_task(&ExternalId(ext_id)).await {
        Ok(t) => t,
        Err(_) => return,
    };

    let expected_label = format!("status:{}", task.status.as_str());
    let has_expected = ext_task.labels.iter().any(|l| l == &expected_label);
    let status_labels: Vec<_> = ext_task
        .labels
        .iter()
        .filter(|l| l.starts_with("status:"))
        .collect();

    if !has_expected {
        let actual = if status_labels.is_empty() {
            "none".to_string()
        } else {
            status_labels
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        findings.push(Finding {
            severity: Severity::Warning,
            task_id: task_label(task),
            message: format!(
                "label mismatch: SQLite='{}' but GitHub labels=[{}]",
                task.status.as_str(),
                actual
            ),
        });
    }
}

/// Check 6: Stale in_progress task (no tmux session).
async fn check_stale_in_progress(
    task: &Task,
    repo: &str,
    tmux: &TmuxManager,
    findings: &mut Vec<Finding>,
) {
    let task_id_str = match &task.external_id {
        Some(id) => id.clone(),
        None => format!("internal:{}", task.id),
    };
    let session = tmux.session_name(repo, &task_id_str);
    if !tmux.session_exists(&session).await {
        findings.push(Finding {
            severity: Severity::Error,
            task_id: task_label(task),
            message: format!(
                "status is 'in_progress' but no tmux session '{}' found",
                session
            ),
        });
    }
}

/// Check 7: Orphaned worktrees.
fn check_orphaned_worktrees(all_tasks: &[Task], findings: &mut Vec<Finding>) {
    let worktrees_dir = match crate::home::worktrees_dir() {
        Ok(d) => d,
        Err(_) => return,
    };

    // Walk worktrees_dir/<project>/<branch>/ and check against task records
    let project_dirs = match std::fs::read_dir(&worktrees_dir) {
        Ok(d) => d,
        Err(_) => return,
    };

    for project_entry in project_dirs.flatten() {
        if !project_entry
            .file_type()
            .map(|t| t.is_dir())
            .unwrap_or(false)
        {
            continue;
        }
        let branch_dirs = match std::fs::read_dir(project_entry.path()) {
            Ok(d) => d,
            Err(_) => continue,
        };
        for branch_entry in branch_dirs.flatten() {
            if !branch_entry
                .file_type()
                .map(|t| t.is_dir())
                .unwrap_or(false)
            {
                continue;
            }
            let wt_path = branch_entry.path().to_string_lossy().to_string();

            // Find if any task owns this worktree
            let owning_task = all_tasks.iter().find(|t| t.worktree == wt_path);

            match owning_task {
                Some(task) => {
                    if task.status == TaskStatus::Done && !task.worktree_cleaned {
                        findings.push(Finding {
                            severity: Severity::Warning,
                            task_id: task_label(task),
                            message: format!("done but worktree not cleaned up: {}", wt_path),
                        });
                    }
                }
                None => {
                    findings.push(Finding {
                        severity: Severity::Warning,
                        task_id: "?".to_string(),
                        message: format!("orphaned worktree (no task owns it): {}", wt_path),
                    });
                }
            }
        }
    }
}

/// Attempt automatic fixes for known issues.
async fn apply_fixes(
    findings: &[Finding],
    store: &Arc<TaskStore>,
    repo: &str,
    backend: &Arc<dyn ExternalBackend>,
    _gh: &GhHttp,
) -> anyhow::Result<()> {
    let mut fixed = 0;

    for f in findings {
        // Fix label mismatches by re-syncing labels
        if f.message.starts_with("label mismatch:") {
            if let Some(store_id) = resolve_finding_task_id(store, repo, &f.task_id).await {
                if let Ok(task) = store.get(store_id).await {
                    let ext_id = task.external_id.as_deref().unwrap_or(&f.task_id);
                    let status = task_status_to_backend_status(task.status);
                    if let Err(e) = backend
                        .update_status(&ExternalId(ext_id.to_string()), status)
                        .await
                    {
                        eprintln!("  fix failed for #{}: {}", f.task_id, e);
                    } else {
                        println!("  fixed label mismatch for #{}", f.task_id);
                        fixed += 1;
                    }
                }
            }
        }

        // Fix: SQLite done but GitHub issue still open → close the issue
        if f.message == "SQLite status is 'done' but GitHub issue is still open" {
            let status = crate::backends::Status::Done;
            if let Err(e) = backend
                .update_status(&ExternalId(f.task_id.clone()), status)
                .await
            {
                eprintln!("  fix failed for #{}: {}", f.task_id, e);
            } else {
                println!("  closed GitHub issue #{}", f.task_id);
                fixed += 1;
            }
        }
    }

    if fixed > 0 {
        println!("\nApplied {} fix(es).", fixed);
    } else {
        println!("\nNo automatic fixes available for the remaining issues.");
        println!("Use `orch task reopen <id>` to recover tasks with orphaned work.");
    }

    Ok(())
}

fn task_label(task: &Task) -> String {
    match &task.external_id {
        Some(id) => id.clone(),
        None => format!("internal:{}", task.id),
    }
}

fn task_status_to_backend_status(status: TaskStatus) -> crate::backends::Status {
    match status {
        TaskStatus::New => crate::backends::Status::New,
        TaskStatus::Routed => crate::backends::Status::Routed,
        TaskStatus::InProgress => crate::backends::Status::InProgress,
        TaskStatus::Done => crate::backends::Status::Done,
        TaskStatus::Blocked => crate::backends::Status::Blocked,
        TaskStatus::InReview => crate::backends::Status::InReview,
        TaskStatus::NeedsReview => crate::backends::Status::NeedsReview,
    }
}

async fn resolve_finding_task_id(store: &Arc<TaskStore>, repo: &str, task_id: &str) -> Option<i64> {
    store.resolve_task_id(repo, task_id).await.ok().flatten()
}
