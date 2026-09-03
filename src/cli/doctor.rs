use crate::backends::github::GitHubBackend;
use crate::backends::{ExternalBackend, ExternalId};
use crate::config;
use crate::engine::cleanup::{
    cleanup_orphaned_worktree, remove_worktree_and_branch, resolve_repo_root,
};
use crate::github::http::GhHttp;
use crate::store::{Task, TaskStatus, TaskStore};
use crate::tmux::TmuxManager;
use anyhow::Context;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// What kind of automatic repair can be applied to a finding.
enum FixAction {
    /// Commit dirty worktree, push, create PR, reopen issue, set needs_review.
    CommitPushCreatePr { store_id: i64 },
    /// Push unpushed commits, create PR, reopen issue, set needs_review.
    PushCreatePr { store_id: i64 },
    /// Branch is pushed but no PR — create PR, reopen issue, set needs_review.
    CreatePr { store_id: i64 },
    /// PR exists but not merged — link PR, reopen issue, set needs_review.
    LinkPrAndReopen { store_id: i64, pr_number: u64 },
    /// PR exists but pr_number not recorded — link it in the store.
    LinkPr { store_id: i64, pr_number: u64 },
    /// GitHub issue is closed but task is not done — reopen issue, sync labels.
    ReopenIssue { store_id: i64 },
    /// Task is done but GitHub issue still open — close issue.
    CloseIssue { store_id: i64 },
    /// GitHub label doesn't match SQLite status — sync labels.
    SyncLabels { store_id: i64 },
    /// Stale in_progress — no tmux session — reset to routed.
    ResetToRouted { store_id: i64 },
    /// Done task with uncleaned worktree that has no uncommitted/unpushed work.
    CleanWorktree { store_id: i64 },
    /// Orphaned worktree with no owning task — remove directory.
    RemoveOrphanedWorktree { path: String },
    /// PR merged but task not done — set done, close issue, clean worktree.
    MarkDoneFromMergedPr { store_id: i64 },
    /// Dead review session — reset to needs_review.
    ResetDeadReview { store_id: i64 },
    /// No automatic fix possible.
    None,
}

/// A single diagnostic finding from `orch service doctor`.
struct Finding {
    severity: Severity,
    task_id: String,
    message: String,
    fix: FixAction,
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
///
/// When called outside a project directory (no `.orch.yml`), runs across all
/// repos found in the store. When called inside a project, scopes to that repo.
pub async fn run(full: bool, fix: bool, dry_run: bool) -> anyhow::Result<()> {
    let store = Arc::new(crate::cli::init_store().await?);
    let gh = GhHttp::new()?;
    let tmux = TmuxManager::new();

    // Resolve repos to check: current project if available, otherwise all repos in store.
    let repos: Vec<String> = match config::get_current_repo() {
        Ok(repo) => vec![repo],
        Err(_) => {
            let all = store.distinct_repos().await?;
            if all.is_empty() {
                println!("No tasks found in store.");
                return Ok(());
            }
            all
        }
    };

    // Spinner: show progress on stderr while checks run (erase with \r on done).
    let spinner_running = Arc::new(AtomicBool::new(true));
    let spinner_flag = spinner_running.clone();
    let spinner_task = tokio::spawn(async move {
        let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let mut i = 0usize;
        while spinner_flag.load(Ordering::Relaxed) {
            eprint!(
                "\r{} Running orch service doctor…",
                frames[i % frames.len()]
            );
            let _ = std::io::stderr().flush();
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            i += 1;
        }
        eprint!("\r");
        let _ = std::io::stderr().flush();
    });

    let result = run_checks(full, fix, dry_run, &repos, &store, &gh, &tmux).await;

    spinner_running.store(false, Ordering::Relaxed);
    let _ = spinner_task.await;

    result
}

async fn run_checks(
    full: bool,
    fix: bool,
    dry_run: bool,
    repos: &[String],
    store: &Arc<TaskStore>,
    gh: &GhHttp,
    tmux: &TmuxManager,
) -> anyhow::Result<()> {
    let mut all_findings: Vec<Finding> = Vec::new();

    for repo in repos {
        let backend: Arc<dyn ExternalBackend> = Arc::new(GitHubBackend::new(repo.clone())?);
        let mut findings: Vec<Finding> = Vec::new();
        if full {
            // Existing expensive path: load active + recent done tasks and batch GraphQL queries.
            // Only load active tasks + done tasks from the last 7 days — avoids
            // fetching hundreds of historical done tasks on every run.
            let cutoff = chrono::Utc::now() - chrono::Duration::days(7);
            let cutoff_str = cutoff.format("%Y-%m-%dT%H:%M:%SZ").to_string();
            let all_tasks = store.list_for_doctor(repo, &cutoff_str).await?;

            let recent_done_tasks: Vec<_> = all_tasks
                .iter()
                .filter(|t| t.status == TaskStatus::Done)
                .collect();
            let active_tasks: Vec<_> = all_tasks
                .iter()
                .filter(|t| t.status != TaskStatus::Done)
                .collect();
            let in_progress_tasks: Vec<_> = all_tasks
                .iter()
                .filter(|t| t.status == TaskStatus::InProgress)
                .collect();
            let in_review_tasks: Vec<_> = all_tasks
                .iter()
                .filter(|t| t.status == TaskStatus::InReview)
                .collect();

            // Batch-fetch all PR states needed for checks 1 and 8 in a single GraphQL call.
            let pr_numbers_to_fetch: Vec<u64> = {
                let mut nums: Vec<u64> = recent_done_tasks
                    .iter()
                    .filter(|t| !(t.origin == "internal" && t.external_id.is_none()))
                    .filter_map(|t| t.pr_number.map(|n| n as u64))
                    .chain(
                        active_tasks
                            .iter()
                            .filter_map(|t| t.pr_number.map(|n| n as u64)),
                    )
                    .collect();
                nums.sort_unstable();
                nums.dedup();
                nums
            };
            let pr_states = gh
                .batch_get_pr_states(repo, &pr_numbers_to_fetch)
                .await
                .unwrap_or_default();

            // 1. Done tasks with no merged PR (recent only — skip internal no-PR tasks)
            for task in &recent_done_tasks {
                if task.origin == "internal" && task.external_id.is_none() {
                    continue;
                }
                if task.pr_number.is_none() && task.external_id.is_none() {
                    continue;
                }
                check_done_no_merged_pr(task, gh, &pr_states, &mut findings, repo).await;
            }

            // 2. Done tasks with dirty worktrees (recent only — filesystem check, fast)
            for task in &recent_done_tasks {
                check_dirty_worktree(task, &mut findings).await;
            }

            // 3. Done tasks with unpushed commits (recent only — filesystem check, fast)
            for task in &recent_done_tasks {
                check_unpushed_commits(task, &mut findings).await;
            }

            // 4+5. Issue status and label checks (active external tasks only).
            // Batch-fetch all issue states and labels in a single GraphQL call.
            let external_active: Vec<_> = active_tasks
                .iter()
                .copied()
                .filter(|t| t.origin != "internal")
                .collect();
            let issue_numbers = collect_valid_external_issue_numbers(
                external_active.iter().copied(),
                repo,
                "cli::doctor::run_checks(full)",
            );
            let issue_states = gh
                .batch_get_issue_states(repo, &issue_numbers, "cli::doctor::run_checks(full)")
                .await
                .unwrap_or_default();
            for task in &external_active {
                check_issue_status_mismatch(task, &issue_states, &mut findings);
                check_label_mismatch(task, &issue_states, &mut findings);
            }

            // 6. Stale in_progress tasks (no tmux session)
            for task in &in_progress_tasks {
                check_stale_in_progress(task, repo, tmux, &mut findings).await;
            }

            // 7. Orphaned worktrees (needs all tasks to match worktree paths to owners)
            check_orphaned_worktrees(&all_tasks, &mut findings).await;

            // 8. PR merged but task not done (active tasks only — uses batched pr_states)
            for task in &active_tasks {
                check_pr_merged_not_done(task, &pr_states, &mut findings);
            }

            // 9. Dead review sessions
            for task in &in_review_tasks {
                check_dead_review_session(task, repo, tmux, &mut findings).await;
            }
        } else {
            // Fast path: only consider active (non-done) tasks and open PRs via REST.
            let active_tasks = store.list_active(repo).await?;

            // Fetch open PRs for the repo (paginated REST call)
            let open_prs: Vec<serde_json::Value> =
                gh.list_open_pulls(repo).await.unwrap_or_default();

            // Build lookup maps: pr_number -> pr, head_ref -> pr
            let mut pr_by_number: std::collections::HashMap<u64, serde_json::Value> =
                std::collections::HashMap::new();
            let mut pr_by_head: std::collections::HashMap<String, serde_json::Value> =
                std::collections::HashMap::new();
            for pr in &open_prs {
                if let Some(n) = pr.get("number").and_then(|v| v.as_u64()) {
                    pr_by_number.insert(n, pr.clone());
                }
                if let Some(head) = pr.pointer("/head/ref").and_then(|v| v.as_str()) {
                    pr_by_head.insert(head.to_string(), pr.clone());
                }
            }

            // Active tasks: perform lightweight checks
            let external_active: Vec<_> = active_tasks
                .iter()
                .filter(|t| t.origin != "internal")
                .collect();
            for task in &active_tasks {
                // Stale in_progress
                if task.status == TaskStatus::InProgress {
                    check_stale_in_progress(task, repo, tmux, &mut findings).await;
                }

                // Dead review sessions
                if task.status == TaskStatus::InReview {
                    check_dead_review_session(task, repo, tmux, &mut findings).await;
                }

                // PR merged check for tasks that have a PR recorded — make one REST call per active task (few)
                if let Some(pr_num) = task.pr_number {
                    match gh.get_pr(repo, pr_num as u64).await {
                        Ok(pr) => {
                            if pr.merged.unwrap_or(false) {
                                findings.push(Finding {
                                    severity: Severity::Error,
                                    task_id: task_label(task),
                                    message: format!(
                                        "PR #{} is merged but task status is '{}'",
                                        pr_num,
                                        task.status.as_str()
                                    ),
                                    fix: FixAction::MarkDoneFromMergedPr { store_id: task.id },
                                });
                            }
                        }
                        Err(_) => {
                            // Ignore transient failures in fast path; the full path will surface them.
                        }
                    }
                } else if !task.branch.is_empty() {
                    // If task has a branch, check if there's an open PR for it
                    if let Some(pr) = pr_by_head.get(&task.branch) {
                        // PR exists and is open — link PR (warning) unless already linked
                        if task.pr_number.is_none() {
                            if let Some(n) = pr.get("number").and_then(|v| v.as_u64()) {
                                findings.push(Finding {
                                    severity: Severity::Warning,
                                    task_id: task_label(task),
                                    message: format!(
                                        "branch '{}' has open PR #{} not linked in store",
                                        task.branch, n
                                    ),
                                    fix: FixAction::LinkPr {
                                        store_id: task.id,
                                        pr_number: n,
                                    },
                                });
                            }
                        }
                    }
                }
            }

            // Batch issue state/label checks for external active tasks
            let external_issue_numbers = collect_valid_external_issue_numbers(
                external_active.iter().copied(),
                repo,
                "cli::doctor::run_checks(fast)",
            );
            let issue_states = gh
                .batch_get_issue_states(
                    repo,
                    &external_issue_numbers,
                    "cli::doctor::run_checks(fast)",
                )
                .await
                .unwrap_or_default();
            for task in &external_active {
                check_issue_status_mismatch(task, &issue_states, &mut findings);
                check_label_mismatch(task, &issue_states, &mut findings);
            }

            // Orphaned PRs: open PRs that don't match any active task by number or head ref
            for pr in &open_prs {
                let pr_num = pr.get("number").and_then(|v| v.as_u64());
                let head = pr
                    .pointer("/head/ref")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let mut owned = false;
                if let Some(n) = pr_num {
                    for t in &active_tasks {
                        if let Some(pn) = t.pr_number {
                            if pn as u64 == n {
                                owned = true;
                                break;
                            }
                        }
                        if !t.branch.is_empty() && Some(t.branch.clone()) == head {
                            owned = true;
                            break;
                        }
                    }
                }
                if !owned {
                    let num_str = pr_num
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "?".to_string());
                    findings.push(Finding {
                        severity: Severity::Warning,
                        task_id: format!("pr:{}", num_str),
                        message: format!("open PR with no active task: #{}", num_str),
                        fix: FixAction::None,
                    });
                }
            }

            // Orphaned worktrees relative to active tasks only
            check_orphaned_worktrees(&active_tasks, &mut findings).await;
        }

        if repos.len() > 1 && !findings.is_empty() {
            println!("\n=== {} ===", repo);
        }

        // Apply per-repo fixes immediately so store_id lookups are in scope.
        if (fix || dry_run) && !findings.is_empty() {
            let errors = findings
                .iter()
                .filter(|f| matches!(f.severity, Severity::Error))
                .count();
            let warnings = findings.len() - errors;
            for f in &findings {
                println!("[{}] #{}: {}", f.severity.symbol(), f.task_id, f.message);
            }
            println!(
                "\nFound {} issue(s): {} error(s), {} warning(s)",
                findings.len(),
                errors,
                warnings,
            );
            apply_fixes(&findings, store, repo, &backend, gh, dry_run).await?;
        }

        all_findings.extend(findings);
    }

    if all_findings.is_empty() {
        println!("No issues found. All task state is consistent.");
        return Ok(());
    }

    // If we didn't already print+fix per-repo above (non-fix mode), print summary now.
    if !fix && !dry_run {
        let errors = all_findings
            .iter()
            .filter(|f| matches!(f.severity, Severity::Error))
            .count();
        let warnings = all_findings.len() - errors;

        for f in &all_findings {
            println!("[{}] #{}: {}", f.severity.symbol(), f.task_id, f.message);
        }

        println!(
            "\nFound {} issue(s): {} error(s), {} warning(s)",
            all_findings.len(),
            errors,
            warnings,
        );

        if errors > 0 {
            println!("\nRun `orch service doctor --fix` to attempt automatic repairs.");
            println!("Run `orch service doctor --dry-run` to preview what --fix would do.");
        }
    }

    Ok(())
}

/// Check 1: Done task with no merged PR.
/// `pr_states` is a pre-fetched cache of pr_number → (merged, state).
async fn check_done_no_merged_pr(
    task: &Task,
    gh: &GhHttp,
    pr_states: &std::collections::HashMap<u64, (bool, String)>,
    findings: &mut Vec<Finding>,
    repo: &str,
) {
    let task_label = task_label(task);
    let store_id = task.id;

    match task.pr_number {
        Some(pr_num) => {
            // Use cached state; fall back to live fetch if missing (e.g. new PR since batch)
            let (merged, state) = if let Some(s) = pr_states.get(&(pr_num as u64)) {
                s.clone()
            } else {
                match gh.get_pr(repo, pr_num as u64).await {
                    Ok(pr) => (pr.merged.unwrap_or(false), pr.state),
                    Err(e) => {
                        findings.push(Finding {
                            severity: Severity::Warning,
                            task_id: task_label,
                            message: format!(
                                "done with PR #{} but failed to fetch PR: {}",
                                pr_num, e
                            ),
                            fix: FixAction::None,
                        });
                        return;
                    }
                }
            };
            if !merged {
                findings.push(Finding {
                    severity: Severity::Error,
                    task_id: task_label,
                    message: format!("done but PR #{} is {} (not merged)", pr_num, state),
                    fix: FixAction::LinkPrAndReopen {
                        store_id,
                        pr_number: pr_num as u64,
                    },
                });
            }
        }
        None => {
            // No PR recorded — check if branch has a PR
            if !task.branch.is_empty() {
                match gh.get_pr_number(repo, &task.branch).await {
                    Ok(Some(pr_num)) => {
                        // PR exists on GitHub but not linked — check merge state
                        let merged = gh
                            .get_pr(repo, pr_num)
                            .await
                            .map(|pr| pr.merged == Some(true))
                            .unwrap_or(false);
                        if merged {
                            // PR is merged — just link it
                            findings.push(Finding {
                                severity: Severity::Warning,
                                task_id: task_label,
                                message: format!(
                                    "done with no pr_number but branch has merged PR #{}",
                                    pr_num
                                ),
                                fix: FixAction::LinkPr {
                                    store_id,
                                    pr_number: pr_num,
                                },
                            });
                        } else {
                            // PR not merged — shouldn't be done
                            findings.push(Finding {
                                severity: Severity::Error,
                                task_id: task_label,
                                message: format!(
                                    "done with no pr_number but branch has unmerged PR #{}",
                                    pr_num
                                ),
                                fix: FixAction::LinkPrAndReopen {
                                    store_id,
                                    pr_number: pr_num,
                                },
                            });
                        }
                    }
                    Ok(None) => {
                        // No PR at all — classify based on worktree state
                        let fix = classify_no_pr_fix(task, store_id).await;
                        findings.push(Finding {
                            severity: Severity::Error,
                            task_id: task_label,
                            message: "done but no PR was ever created".to_string(),
                            fix,
                        });
                    }
                    Err(_) => {
                        findings.push(Finding {
                            severity: Severity::Error,
                            task_id: task_label,
                            message: "done but no PR number recorded and branch lookup failed"
                                .to_string(),
                            fix: FixAction::None,
                        });
                    }
                }
            } else {
                findings.push(Finding {
                    severity: Severity::Error,
                    task_id: task_label,
                    message: "done but no branch or PR recorded".to_string(),
                    fix: FixAction::None,
                });
            }
        }
    }
}

/// Classify the appropriate fix when a done task has no PR.
async fn classify_no_pr_fix(task: &Task, store_id: i64) -> FixAction {
    if task.worktree.is_empty() || task.worktree_cleaned {
        // Work was lost — worktree already cleaned
        return FixAction::None;
    }
    let wt = Path::new(&task.worktree);
    if !wt.exists() || !wt.join(".git").exists() {
        return FixAction::None;
    }

    // Check for dirty files
    let has_dirty = tokio::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(wt)
        .output()
        .await
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    if has_dirty {
        return FixAction::CommitPushCreatePr { store_id };
    }

    // Check for unpushed commits
    let has_unpushed = tokio::process::Command::new("git")
        .args(["log", &format!("origin/{}..HEAD", task.branch), "--oneline"])
        .current_dir(wt)
        .output()
        .await
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    if has_unpushed {
        return FixAction::PushCreatePr { store_id };
    }

    // Branch pushed, no PR
    let branch_on_remote = tokio::process::Command::new("git")
        .args(["ls-remote", "--heads", "origin", &task.branch])
        .current_dir(wt)
        .output()
        .await
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    if branch_on_remote {
        return FixAction::CreatePr { store_id };
    }

    FixAction::None
}

/// Check 2: Done task with dirty worktree (uncommitted changes).
async fn check_dirty_worktree(task: &Task, findings: &mut Vec<Finding>) {
    if task.worktree.is_empty() || task.worktree_cleaned {
        return;
    }
    let wt = Path::new(&task.worktree);
    if !wt.exists() || !wt.join(".git").exists() {
        return;
    }
    let output = tokio::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(wt)
        .output()
        .await;
    if let Ok(out) = output {
        if !out.stdout.is_empty() {
            findings.push(Finding {
                severity: Severity::Error,
                task_id: task_label(task),
                message: format!(
                    "done but worktree has uncommitted changes: {}",
                    task.worktree
                ),
                fix: FixAction::CommitPushCreatePr { store_id: task.id },
            });
        }
    }
}

/// Check 3: Done task with unpushed commits.
async fn check_unpushed_commits(task: &Task, findings: &mut Vec<Finding>) {
    if task.worktree.is_empty() || task.worktree_cleaned || task.branch.is_empty() {
        return;
    }
    let wt = Path::new(&task.worktree);
    if !wt.exists() || !wt.join(".git").exists() {
        return;
    }
    let output = tokio::process::Command::new("git")
        .args(["log", &format!("origin/{}..HEAD", task.branch), "--oneline"])
        .current_dir(wt)
        .output()
        .await;
    if let Ok(out) = output {
        if !out.stdout.is_empty() {
            let count = out.stdout.iter().filter(|&&b| b == b'\n').count();
            findings.push(Finding {
                severity: Severity::Error,
                task_id: task_label(task),
                message: format!(
                    "done but {} unpushed commit(s) in worktree: {}",
                    count, task.worktree
                ),
                fix: FixAction::PushCreatePr { store_id: task.id },
            });
        }
    }
}

/// Check 4: Closed GitHub issue with non-done SQLite status (and vice versa).
fn check_issue_status_mismatch(
    task: &Task,
    issue_states: &std::collections::HashMap<String, (String, Vec<String>)>,
    findings: &mut Vec<Finding>,
) {
    let ext_id = match &task.external_id {
        Some(id) => id.clone(),
        None => return,
    };

    let (state, _labels) = match issue_states.get(&ext_id) {
        Some(v) => v,
        None => return,
    };

    let issue_closed = state == "closed";
    let task_done = task.status == TaskStatus::Done;
    let task_blocked = task.status == TaskStatus::Blocked;

    if issue_closed && !task_done && !task_blocked {
        findings.push(Finding {
            severity: Severity::Error,
            task_id: task_label(task),
            message: format!(
                "GitHub issue is closed but SQLite status is '{}'",
                task.status.as_str()
            ),
            fix: FixAction::ReopenIssue { store_id: task.id },
        });
    } else if !issue_closed && task_done {
        findings.push(Finding {
            severity: Severity::Warning,
            task_id: task_label(task),
            message: "SQLite status is 'done' but GitHub issue is still open".to_string(),
            fix: FixAction::CloseIssue { store_id: task.id },
        });
    }
}

/// Check 5: Label/status mismatch.
fn check_label_mismatch(
    task: &Task,
    issue_states: &std::collections::HashMap<String, (String, Vec<String>)>,
    findings: &mut Vec<Finding>,
) {
    let ext_id = match &task.external_id {
        Some(id) => id.clone(),
        None => return,
    };

    let (_state, labels) = match issue_states.get(&ext_id) {
        Some(v) => v,
        None => return,
    };

    let expected_label = format!("status:{}", task.status.as_str());
    let has_expected = labels.iter().any(|l| l == &expected_label);
    let status_labels: Vec<_> = labels.iter().filter(|l| l.starts_with("status:")).collect();

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
            fix: FixAction::SyncLabels { store_id: task.id },
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
            fix: FixAction::ResetToRouted { store_id: task.id },
        });
    }
}

/// Check 7: Orphaned worktrees.
async fn check_orphaned_worktrees(all_tasks: &[Task], findings: &mut Vec<Finding>) {
    let worktrees_dir = match crate::home::worktrees_dir() {
        Ok(d) => d,
        Err(_) => return,
    };

    let project_dirs = match std::fs::read_dir(&worktrees_dir) {
        Ok(d) => d,
        Err(_) => return,
    };

    // Build a HashMap for O(1) worktree-path → task lookups instead of O(T) linear scan.
    let task_by_worktree: std::collections::HashMap<&str, &Task> = all_tasks
        .iter()
        .filter(|t| !t.worktree.is_empty())
        .map(|t| (t.worktree.as_str(), t))
        .collect();

    for project_entry in project_dirs {
        let project_entry = match project_entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(err = %e, "error reading worktrees directory entry");
                continue;
            }
        };
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
        for branch_entry in branch_dirs {
            let branch_entry = match branch_entry {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(err = %e, "error reading project directory entry");
                    continue;
                }
            };
            if !branch_entry
                .file_type()
                .map(|t| t.is_dir())
                .unwrap_or(false)
            {
                continue;
            }
            let wt_path = branch_entry.path().to_string_lossy().to_string();

            let owning_task = task_by_worktree.get(wt_path.as_str()).copied();

            match owning_task {
                Some(task) => {
                    if task.status == TaskStatus::Done && !task.worktree_cleaned {
                        // Check if worktree has uncommitted/unpushed work
                        let has_work = worktree_has_work(Path::new(&wt_path), &task.branch).await;
                        if has_work {
                            // Don't auto-clean — there's work to save
                            findings.push(Finding {
                                severity: Severity::Warning,
                                task_id: task_label(task),
                                message: format!("done but worktree has unsaved work: {}", wt_path),
                                fix: FixAction::None,
                            });
                        } else {
                            findings.push(Finding {
                                severity: Severity::Warning,
                                task_id: task_label(task),
                                message: format!("done but worktree not cleaned up: {}", wt_path),
                                fix: FixAction::CleanWorktree { store_id: task.id },
                            });
                        }
                    }
                }
                None => {
                    findings.push(Finding {
                        severity: Severity::Warning,
                        task_id: "?".to_string(),
                        message: format!("orphaned worktree (no task owns it): {}", wt_path),
                        fix: FixAction::RemoveOrphanedWorktree {
                            path: wt_path.clone(),
                        },
                    });
                }
            }
        }
    }
}

/// Check 8: PR merged but task not done (uses pre-fetched pr_states cache).
fn check_pr_merged_not_done(
    task: &Task,
    pr_states: &std::collections::HashMap<u64, (bool, String)>,
    findings: &mut Vec<Finding>,
) {
    let pr_num = match task.pr_number {
        Some(n) => n as u64,
        None => return,
    };

    if let Some((merged, _)) = pr_states.get(&pr_num) {
        if *merged {
            findings.push(Finding {
                severity: Severity::Error,
                task_id: task_label(task),
                message: format!(
                    "PR #{} is merged but task status is '{}'",
                    pr_num,
                    task.status.as_str()
                ),
                fix: FixAction::MarkDoneFromMergedPr { store_id: task.id },
            });
        }
    }
}

/// Check 9: Dead review sessions.
async fn check_dead_review_session(
    task: &Task,
    repo: &str,
    tmux: &TmuxManager,
    findings: &mut Vec<Finding>,
) {
    if !task.review_session_expected {
        return;
    }
    let task_id_str = match &task.external_id {
        Some(id) => id.clone(),
        None => format!("internal:{}", task.id),
    };
    let review_task_id = format!("{}-review", task_id_str);
    let session = tmux.session_name(repo, &review_task_id);
    if !tmux.session_exists(&session).await {
        findings.push(Finding {
            severity: Severity::Error,
            task_id: task_label(task),
            message: format!(
                "status is 'in_review' with review_session_expected but no review session '{}' found",
                session
            ),
            fix: FixAction::ResetDeadReview { store_id: task.id },
        });
    }
}

/// Check if a worktree has uncommitted changes or unpushed commits.
async fn worktree_has_work(wt: &Path, branch: &str) -> bool {
    if !wt.join(".git").exists() {
        return false;
    }
    let has_dirty = tokio::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(wt)
        .output()
        .await
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    if has_dirty {
        return true;
    }
    if !branch.is_empty() {
        let has_unpushed = tokio::process::Command::new("git")
            .args(["log", &format!("origin/{}..HEAD", branch), "--oneline"])
            .current_dir(wt)
            .output()
            .await
            .map(|o| !o.stdout.is_empty())
            .unwrap_or(false);
        if has_unpushed {
            return true;
        }
    }
    false
}

/// Attempt automatic fixes for known issues.
async fn apply_fixes(
    findings: &[Finding],
    store: &Arc<TaskStore>,
    repo: &str,
    backend: &Arc<dyn ExternalBackend>,
    gh: &GhHttp,
    dry_run: bool,
) -> anyhow::Result<()> {
    let prefix = if dry_run { "[dry-run] " } else { "" };
    let mut fixed = 0;
    let mut skipped = 0;

    for f in findings {
        match &f.fix {
            FixAction::None => {
                skipped += 1;
                continue;
            }
            FixAction::CommitPushCreatePr { store_id } => {
                let task = store.get(*store_id).await?;
                let wt = Path::new(&task.worktree);
                if !wt.exists() {
                    eprintln!("  skip #{}: worktree no longer exists", f.task_id);
                    skipped += 1;
                    continue;
                }

                if dry_run {
                    println!(
                        "  {}would commit + push + create PR + reopen issue + set needs_review for #{}",
                        prefix, f.task_id
                    );
                    fixed += 1;
                    continue;
                }

                // Commit all changes
                let commit_ok = git_cmd(wt, &["add", "-A"]).await
                    && git_cmd(
                        wt,
                        &[
                            "commit",
                            "-m",
                            &format!("fix: recover orphaned work for #{}", f.task_id),
                        ],
                    )
                    .await;
                if !commit_ok {
                    eprintln!("  fix failed for #{}: git commit failed", f.task_id);
                    skipped += 1;
                    continue;
                }

                // Push
                if !git_cmd(
                    wt,
                    &[
                        "-c",
                        "lfs.locksverify=false",
                        "push",
                        "-u",
                        "origin",
                        &task.branch,
                    ],
                )
                .await
                {
                    eprintln!("  fix failed for #{}: git push failed", f.task_id);
                    skipped += 1;
                    continue;
                }

                // Create PR
                match create_pr_for_task(gh, repo, &task).await {
                    Ok(pr_num) => {
                        let _ = store
                            .set_fields(
                                *store_id,
                                &[("pr_number", serde_json::json!(pr_num as i64))],
                            )
                            .await;
                        // Set needs_review + reopen issue + sync labels
                        match reopen_and_set_needs_review(
                            store, *store_id, &task, backend, gh, repo,
                        )
                        .await
                        {
                            Ok(()) => {
                                println!(
                                    "  fixed #{}: committed + pushed + created PR #{} + reopened + needs_review",
                                    f.task_id, pr_num
                                );
                                fixed += 1;
                            }
                            Err(e) => {
                                eprintln!(
                                    "  fix failed for #{}: PR #{} created but status/reopen failed: {e}",
                                    f.task_id, pr_num
                                );
                                skipped += 1;
                            }
                        }
                    }
                    Err(e) => {
                        // PR creation failed — it may be a transient server error. Re-check GitHub
                        // for an existing PR for the branch and link it if found.
                        eprintln!(
                            "  PR creation error for #{}: {}. Re-checking for existing PR...",
                            f.task_id, e
                        );
                        match gh.get_pr_number(repo, &task.branch).await {
                            Ok(Some(pr_num)) => {
                                let _ = store
                                    .set_fields(
                                        *store_id,
                                        &[("pr_number", serde_json::json!(pr_num as i64))],
                                    )
                                    .await;
                                match reopen_and_set_needs_review(
                                    store, *store_id, &task, backend, gh, repo,
                                )
                                .await
                                {
                                    Ok(()) => {
                                        println!(
                                            "  fixed #{}: PR was present on GitHub as #{} (linked) + reopened + needs_review",
                                            f.task_id, pr_num
                                        );
                                        fixed += 1;
                                    }
                                    Err(e2) => {
                                        eprintln!(
                                            "  fix failed for #{}: PR #{} linked but status/reopen failed: {e2}",
                                            f.task_id, pr_num
                                        );
                                        skipped += 1;
                                    }
                                }
                            }
                            Ok(None) => {
                                eprintln!("  fix failed for #{}: PR creation failed and no PR found for branch", f.task_id);
                                skipped += 1;
                            }
                            Err(err2) => {
                                eprintln!("  fix failed for #{}: PR creation failed: {}; and PR lookup failed: {}", f.task_id, e, err2);
                                skipped += 1;
                            }
                        }
                    }
                }
            }
            FixAction::PushCreatePr { store_id } => {
                let task = store.get(*store_id).await?;
                let wt = Path::new(&task.worktree);
                if !wt.exists() {
                    eprintln!("  skip #{}: worktree no longer exists", f.task_id);
                    skipped += 1;
                    continue;
                }

                if dry_run {
                    println!(
                        "  {}would push + create PR + reopen issue + set needs_review for #{}",
                        prefix, f.task_id
                    );
                    fixed += 1;
                    continue;
                }

                if !git_cmd(wt, &["push", "-u", "origin", &task.branch]).await {
                    eprintln!("  fix failed for #{}: git push failed", f.task_id);
                    skipped += 1;
                    continue;
                }

                match create_pr_for_task(gh, repo, &task).await {
                    Ok(pr_num) => {
                        let _ = store
                            .set_fields(
                                *store_id,
                                &[("pr_number", serde_json::json!(pr_num as i64))],
                            )
                            .await;
                        match reopen_and_set_needs_review(
                            store, *store_id, &task, backend, gh, repo,
                        )
                        .await
                        {
                            Ok(()) => {
                                println!(
                                    "  fixed #{}: pushed + created PR #{} + reopened + needs_review",
                                    f.task_id, pr_num
                                );
                                fixed += 1;
                            }
                            Err(e) => {
                                eprintln!(
                                    "  fix failed for #{}: PR #{} created but status/reopen failed: {e}",
                                    f.task_id, pr_num
                                );
                                skipped += 1;
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "  PR creation error for #{}: {}. Re-checking for existing PR...",
                            f.task_id, e
                        );
                        match gh.get_pr_number(repo, &task.branch).await {
                            Ok(Some(pr_num)) => {
                                let _ = store
                                    .set_fields(
                                        *store_id,
                                        &[("pr_number", serde_json::json!(pr_num as i64))],
                                    )
                                    .await;
                                match reopen_and_set_needs_review(
                                    store, *store_id, &task, backend, gh, repo,
                                )
                                .await
                                {
                                    Ok(()) => {
                                        println!(
                                            "  fixed #{}: PR was present on GitHub as #{} (linked) + reopened + needs_review",
                                            f.task_id, pr_num
                                        );
                                        fixed += 1;
                                    }
                                    Err(e2) => {
                                        eprintln!(
                                            "  fix failed for #{}: PR #{} linked but status/reopen failed: {e2}",
                                            f.task_id, pr_num
                                        );
                                        skipped += 1;
                                    }
                                }
                            }
                            Ok(None) => {
                                eprintln!("  fix failed for #{}: PR creation failed and no PR found for branch", f.task_id);
                                skipped += 1;
                            }
                            Err(err2) => {
                                eprintln!("  fix failed for #{}: PR creation failed: {}; and PR lookup failed: {}", f.task_id, e, err2);
                                skipped += 1;
                            }
                        }
                    }
                }
            }
            FixAction::CreatePr { store_id } => {
                let task = store.get(*store_id).await?;

                if dry_run {
                    println!(
                        "  {}would create PR + reopen issue + set needs_review for #{}",
                        prefix, f.task_id
                    );
                    fixed += 1;
                    continue;
                }

                match create_pr_for_task(gh, repo, &task).await {
                    Ok(pr_num) => {
                        let _ = store
                            .set_fields(
                                *store_id,
                                &[("pr_number", serde_json::json!(pr_num as i64))],
                            )
                            .await;
                        match reopen_and_set_needs_review(
                            store, *store_id, &task, backend, gh, repo,
                        )
                        .await
                        {
                            Ok(()) => {
                                println!(
                                    "  fixed #{}: created PR #{} + reopened + needs_review",
                                    f.task_id, pr_num
                                );
                                fixed += 1;
                            }
                            Err(e) => {
                                eprintln!(
                                    "  fix failed for #{}: PR #{} created but status/reopen failed: {e}",
                                    f.task_id, pr_num
                                );
                                skipped += 1;
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "  PR creation error for #{}: {}. Re-checking for existing PR...",
                            f.task_id, e
                        );
                        match gh.get_pr_number(repo, &task.branch).await {
                            Ok(Some(pr_num)) => {
                                let _ = store
                                    .set_fields(
                                        *store_id,
                                        &[("pr_number", serde_json::json!(pr_num as i64))],
                                    )
                                    .await;
                                match reopen_and_set_needs_review(
                                    store, *store_id, &task, backend, gh, repo,
                                )
                                .await
                                {
                                    Ok(()) => {
                                        println!(
                                            "  fixed #{}: PR was present on GitHub as #{} (linked) + reopened + needs_review",
                                            f.task_id, pr_num
                                        );
                                        fixed += 1;
                                    }
                                    Err(e2) => {
                                        eprintln!(
                                            "  fix failed for #{}: PR #{} linked but status/reopen failed: {e2}",
                                            f.task_id, pr_num
                                        );
                                        skipped += 1;
                                    }
                                }
                            }
                            Ok(None) => {
                                eprintln!("  fix failed for #{}: PR creation failed and no PR found for branch", f.task_id);
                                skipped += 1;
                            }
                            Err(err2) => {
                                eprintln!("  fix failed for #{}: PR creation failed: {}; and PR lookup failed: {}", f.task_id, e, err2);
                                skipped += 1;
                            }
                        }
                    }
                }
            }
            FixAction::LinkPrAndReopen {
                store_id,
                pr_number,
            } => {
                let task = store.get(*store_id).await?;

                if dry_run {
                    println!(
                        "  {}would link PR #{} + reopen issue + set needs_review for #{}",
                        prefix, pr_number, f.task_id
                    );
                    fixed += 1;
                    continue;
                }

                let _ = store
                    .set_fields(
                        *store_id,
                        &[("pr_number", serde_json::json!(*pr_number as i64))],
                    )
                    .await;
                match reopen_and_set_needs_review(store, *store_id, &task, backend, gh, repo).await
                {
                    Ok(()) => {
                        println!(
                            "  fixed #{}: linked PR #{} + reopened + needs_review",
                            f.task_id, pr_number
                        );
                        fixed += 1;
                    }
                    Err(e) => {
                        eprintln!(
                            "  fix failed for #{}: PR #{} linked but status/reopen failed: {e}",
                            f.task_id, pr_number
                        );
                        skipped += 1;
                    }
                }
            }
            FixAction::LinkPr {
                store_id,
                pr_number,
            } => {
                if dry_run {
                    println!(
                        "  {}would link PR #{} to task #{}",
                        prefix, pr_number, f.task_id
                    );
                    fixed += 1;
                    continue;
                }

                let _ = store
                    .set_fields(
                        *store_id,
                        &[("pr_number", serde_json::json!(*pr_number as i64))],
                    )
                    .await;
                println!("  fixed #{}: linked PR #{}", f.task_id, pr_number);
                fixed += 1;
            }
            FixAction::ReopenIssue { store_id } => {
                let task = store.get(*store_id).await?;

                if dry_run {
                    println!(
                        "  {}would reopen GitHub issue + sync labels for #{}",
                        prefix, f.task_id
                    );
                    fixed += 1;
                    continue;
                }

                if let Some(ref ext_id) = task.external_id {
                    if let Err(e) = gh.reopen_issue(repo, ext_id).await {
                        eprintln!("  fix failed for #{}: reopen failed: {}", f.task_id, e);
                        skipped += 1;
                        continue;
                    }
                    let status = task_status_to_backend_status(task.status);
                    let _ = backend
                        .update_status(&ExternalId(ext_id.clone()), status)
                        .await;
                    println!("  fixed #{}: reopened issue + synced labels", f.task_id);
                    fixed += 1;
                } else {
                    skipped += 1;
                }
            }
            FixAction::CloseIssue { store_id } => {
                let task = store.get(*store_id).await?;

                if dry_run {
                    println!("  {}would close GitHub issue #{}", prefix, f.task_id);
                    fixed += 1;
                    continue;
                }

                if let Some(ref ext_id) = task.external_id {
                    let status = crate::backends::Status::Done;
                    if let Err(e) = backend
                        .update_status(&ExternalId(ext_id.clone()), status)
                        .await
                    {
                        eprintln!("  fix failed for #{}: {}", f.task_id, e);
                        skipped += 1;
                    } else {
                        println!("  fixed #{}: closed GitHub issue", f.task_id);
                        fixed += 1;
                    }
                } else {
                    skipped += 1;
                }
            }
            FixAction::SyncLabels { store_id } => {
                let task = store.get(*store_id).await?;

                if dry_run {
                    println!(
                        "  {}would sync labels to status:{} for #{}",
                        prefix,
                        task.status.as_str(),
                        f.task_id
                    );
                    fixed += 1;
                    continue;
                }

                if let Some(ref ext_id) = task.external_id {
                    let status = task_status_to_backend_status(task.status);
                    if let Err(e) = backend
                        .update_status(&ExternalId(ext_id.clone()), status)
                        .await
                    {
                        eprintln!("  fix failed for #{}: {}", f.task_id, e);
                        skipped += 1;
                    } else {
                        println!("  fixed #{}: synced labels", f.task_id);
                        fixed += 1;
                    }
                } else {
                    skipped += 1;
                }
            }
            FixAction::ResetToRouted { store_id } => {
                if dry_run {
                    println!(
                        "  {}would reset #{} to 'routed' (re-dispatch on next tick)",
                        prefix, f.task_id
                    );
                    fixed += 1;
                    continue;
                }

                if let Err(e) = store.update_status(*store_id, TaskStatus::Routed).await {
                    eprintln!("  fix failed for #{}: {}", f.task_id, e);
                    skipped += 1;
                    continue;
                }

                // Sync labels if external
                let task = store.get(*store_id).await?;
                if let Some(ref ext_id) = task.external_id {
                    let _ = backend
                        .update_status(&ExternalId(ext_id.clone()), crate::backends::Status::Routed)
                        .await;
                }
                println!("  fixed #{}: reset to routed", f.task_id);
                fixed += 1;
            }
            FixAction::CleanWorktree { store_id } => {
                let task = store.get(*store_id).await?;

                if dry_run {
                    println!(
                        "  {}would clean worktree + mark cleaned for #{}",
                        prefix, f.task_id
                    );
                    fixed += 1;
                    continue;
                }

                let wt = Path::new(&task.worktree);
                if wt.exists() {
                    match resolve_repo_root(repo).await {
                        Ok(root) => {
                            let branch = if task.branch.is_empty() {
                                None
                            } else {
                                Some(task.branch.as_str())
                            };
                            let removed = remove_worktree_and_branch(
                                &f.task_id,
                                wt,
                                branch,
                                Path::new(&root),
                                task.pr_number.is_some(), // keep remote branch if PR exists
                            )
                            .await;
                            if removed {
                                let _ = store.mark_cleaned(*store_id).await;
                                println!("  fixed #{}: cleaned worktree", f.task_id);
                                fixed += 1;
                            } else {
                                eprintln!(
                                    "  fix failed for #{}: worktree removal failed",
                                    f.task_id
                                );
                                skipped += 1;
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "  fix failed for #{}: cannot resolve repo root: {}",
                                f.task_id, e
                            );
                            skipped += 1;
                        }
                    }
                } else {
                    // Directory already gone — just mark cleaned
                    let _ = store.mark_cleaned(*store_id).await;
                    println!(
                        "  fixed #{}: marked worktree cleaned (already gone)",
                        f.task_id
                    );
                    fixed += 1;
                }
            }
            FixAction::RemoveOrphanedWorktree { path } => {
                if dry_run {
                    println!("  {}would remove orphaned worktree: {}", prefix, path);
                    fixed += 1;
                    continue;
                }

                let removed = cleanup_orphaned_worktree(&f.task_id, Path::new(path)).await;
                if removed {
                    println!("  fixed: removed orphaned worktree {}", path);
                    fixed += 1;
                } else {
                    eprintln!("  fix failed: could not remove orphaned worktree {}", path);
                    skipped += 1;
                }
            }
            FixAction::MarkDoneFromMergedPr { store_id } => {
                let task = store.get(*store_id).await?;

                if dry_run {
                    println!(
                        "  {}would mark #{} done + close issue + clean worktree",
                        prefix, f.task_id
                    );
                    fixed += 1;
                    continue;
                }

                // Set status to done
                if let Err(e) = store.update_status(*store_id, TaskStatus::Done).await {
                    eprintln!("  fix failed for #{}: {}", f.task_id, e);
                    skipped += 1;
                    continue;
                }

                // Close GitHub issue + sync labels
                if let Some(ref ext_id) = task.external_id {
                    let _ = backend
                        .update_status(&ExternalId(ext_id.clone()), crate::backends::Status::Done)
                        .await;
                }

                // Clean worktree if present
                if !task.worktree.is_empty() && !task.worktree_cleaned {
                    let wt = Path::new(&task.worktree);
                    if wt.exists() {
                        if let Ok(root) = resolve_repo_root(repo).await {
                            let branch = if task.branch.is_empty() {
                                None
                            } else {
                                Some(task.branch.as_str())
                            };
                            let removed = remove_worktree_and_branch(
                                &f.task_id,
                                wt,
                                branch,
                                Path::new(&root),
                                true, // keep remote — PR is merged
                            )
                            .await;
                            if removed {
                                let _ = store.mark_cleaned(*store_id).await;
                            }
                        }
                    } else {
                        let _ = store.mark_cleaned(*store_id).await;
                    }
                }

                println!("  fixed #{}: marked done + closed issue", f.task_id);
                fixed += 1;
            }
            FixAction::ResetDeadReview { store_id } => {
                if dry_run {
                    println!(
                        "  {}would reset #{} review_session_expected=0 + set needs_review",
                        prefix, f.task_id
                    );
                    fixed += 1;
                    continue;
                }

                // Reset review_session_expected and set status to needs_review
                let _ = store
                    .set_fields(
                        *store_id,
                        &[("review_session_expected", serde_json::json!(false))],
                    )
                    .await;
                if let Err(e) = store
                    .update_status(*store_id, TaskStatus::NeedsReview)
                    .await
                {
                    eprintln!("  fix failed for #{}: {}", f.task_id, e);
                    skipped += 1;
                    continue;
                }

                // Sync labels
                let task = store.get(*store_id).await?;
                if let Some(ref ext_id) = task.external_id {
                    let _ = backend
                        .update_status(
                            &ExternalId(ext_id.clone()),
                            crate::backends::Status::NeedsReview,
                        )
                        .await;
                }
                println!(
                    "  fixed #{}: reset review session + set needs_review",
                    f.task_id
                );
                fixed += 1;
            }
        }
    }

    let action = if dry_run { "would fix" } else { "fixed" };
    println!("\n{} {}, skipped {}.", action, fixed, skipped);

    if !dry_run && skipped > 0 {
        println!("Use `orch task reopen <id>` to manually recover skipped tasks.");
    }

    Ok(())
}

/// Helper: reopen issue, set status to needs_review, sync labels.
///
/// Returns an error if the SQLite status update or the GitHub issue reopen
/// fails. Backend label sync failures are logged as warnings but do not cause
/// the function to fail, since they are secondary/cosmetic.
async fn reopen_and_set_needs_review(
    store: &Arc<TaskStore>,
    store_id: i64,
    task: &Task,
    backend: &Arc<dyn ExternalBackend>,
    gh: &GhHttp,
    repo: &str,
) -> anyhow::Result<()> {
    // Update SQLite status — hard failure: without this the task remains in
    // the wrong state and the next doctor run will attempt the same fix again.
    store
        .update_status(store_id, TaskStatus::NeedsReview)
        .await
        .context("failed to update task status to NeedsReview in database")?;

    // Reopen issue + sync labels for external tasks
    if let Some(ref ext_id) = task.external_id {
        // Hard failure: if the issue stays closed the engine will never pick it up.
        gh.reopen_issue(repo, ext_id)
            .await
            .with_context(|| format!("failed to reopen GitHub issue {ext_id}"))?;

        // Soft failure: label sync is cosmetic; log a warning but don't block.
        if let Err(e) = backend
            .update_status(
                &ExternalId(ext_id.clone()),
                crate::backends::Status::NeedsReview,
            )
            .await
        {
            eprintln!(
                "  warning: backend label sync failed for #{}: {e}",
                task_label(task)
            );
        }
    }

    Ok(())
}

/// Create a PR for a task using `gh` CLI (more reliable for auth than API).
async fn create_pr_for_task(gh: &GhHttp, repo: &str, task: &Task) -> anyhow::Result<u64> {
    let title = format!("fix: recover work for #{}", task_label(task));
    let body = format!(
        "Recovered by `orch service doctor --fix`.\n\nOriginal task: #{}",
        task_label(task)
    );
    let pr_url = gh
        .create_pr(repo, &title, &body, &task.branch, "main")
        .await?;
    // Extract PR number from URL (e.g., ".../pull/123")
    let pr_num = pr_url
        .rsplit('/')
        .next()
        .and_then(|s| s.parse::<u64>().ok())
        .context("could not parse PR number from URL")?;
    Ok(pr_num)
}

/// Run a git command in a worktree directory, returning success/failure.
async fn git_cmd(wt: &Path, args: &[&str]) -> bool {
    tokio::process::Command::new("git")
        .args(args)
        .current_dir(wt)
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
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

fn collect_valid_external_issue_numbers<'a>(
    tasks: impl IntoIterator<Item = &'a Task>,
    repo: &str,
    caller_context: &str,
) -> Vec<u64> {
    let mut issue_numbers = Vec::new();

    for task in tasks {
        let Some(external_id) = task.external_id.as_deref() else {
            continue;
        };

        match external_id.parse::<u64>() {
            Ok(0) => {
                tracing::warn!(
                    repo,
                    caller_context,
                    store_id = task.id,
                    external_id,
                    "dropping invalid external_id before batch issue-state query"
                );
            }
            Ok(issue_number) => issue_numbers.push(issue_number),
            Err(error) => {
                tracing::warn!(
                    repo,
                    caller_context,
                    store_id = task.id,
                    external_id,
                    error = %error,
                    "skipping non-numeric external_id in batch issue-state query"
                );
            }
        }
    }

    issue_numbers.sort_unstable();
    issue_numbers.dedup();
    issue_numbers
}

#[cfg(test)]
mod tests {
    use super::collect_valid_external_issue_numbers;
    use crate::store::{Task, TaskStatus};

    fn task(id: i64, external_id: Option<&str>, origin: &str) -> Task {
        Task {
            id,
            external_id: external_id.map(ToOwned::to_owned),
            repo: "owner/repo".to_string(),
            origin: origin.to_string(),
            title: String::new(),
            body: String::new(),
            status: TaskStatus::New,
            source: String::new(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
            labels: Vec::new(),
            agent: None,
            model: None,
            complexity: String::new(),
            estimate: 0,
            route_reason: String::new(),
            agent_profile: String::new(),
            selected_skills: String::new(),
            route_attempts: 0,
            attempts: 0,
            branch: String::new(),
            worktree: String::new(),
            worktree_cleaned: false,
            summary: String::new(),
            last_error: String::new(),
            parent_id: None,
            block_reason: None,
            pr_number: None,
            pr_review_context: String::new(),
            last_review_ts: String::new(),
            review_ts_map: String::new(),
            last_comment_review_ts: String::new(),
            merge_conflict_retries: 0,
            ci_merge_failures: 0,
            pr_create_failures: 0,
            push_failures: 0,
            network_retries: 0,
            review_agent_failures: 0,
            review_cycles: 0,
            review_invocations: 0,
            review_session_expected: false,
            needs_review_refires: 0,
            input_tokens: 0,
            output_tokens: 0,
            input_cost_usd: 0.0,
            output_cost_usd: 0.0,
            total_cost_usd: 0.0,
            model_reroute_chain: String::new(),
            limit_reroute_chain: String::new(),
            memory: Vec::new(),
            delegations: Vec::new(),
            auto_unblock_count: 0,
            auto_unblock_last_at: String::new(),
            auto_unblock_last_reason: String::new(),
            ci_recovery_count: 0,
            no_code_reroutes: 0,
            no_code_last_agent: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn collect_valid_external_issue_numbers_filters_zero_and_non_numeric_ids() {
        let tasks = [
            task(1, Some("42"), "github"),
            task(2, Some("0"), "github"),
            task(3, Some("not-a-number"), "github"),
            task(4, Some("42"), "github"),
        ];

        let issue_numbers =
            collect_valid_external_issue_numbers(tasks.iter(), "owner/repo", "test");

        assert_eq!(issue_numbers, vec![42]);
    }
}
