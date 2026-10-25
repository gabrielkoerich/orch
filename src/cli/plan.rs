//! `orch plan import` creates and updates issues from a roadmap file.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Context;

use crate::config;
use crate::github::http::GhHttp;
use crate::plan::{
    diff_import, diff_links, issue_body, issue_title, labels_for, marker_id, missing_blockers,
    render, Binding, DepRef, ExistingIssue, LinkEdit, PlanDiff, Task, HUMAN_LABEL, NO_AGENT_LABEL,
    PLAN_LABEL,
};

pub async fn import(file: &str, dry_run: bool, milestone: Option<&str>) -> anyhow::Result<()> {
    let path = Path::new(file);
    let markdown = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let roadmap = crate::plan::parse_roadmap(&markdown)?;
    if let Some(id) = milestone {
        crate::plan::select(&roadmap, Some(id))?;
    }
    let repo = config::get_current_repo()
        .context("'repo' not set — run `orch init` or set gh.repo in .orch.yml")?;
    let rel_path = repo_relative(path).unwrap_or_else(|| {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| file.to_string());
        eprintln!("warning: {file} is outside the git checkout, the issue link uses {name}");
        name
    });

    let http = GhHttp::new()?;
    let branch = http.get_default_branch(&repo).await?;
    let existing = load_existing(&http, &repo).await?;
    let body_for = |task: &Task| issue_body(task, &repo, &branch, &rel_path);
    let diff = diff_import(&roadmap, &existing, milestone, body_for)?;

    let known: HashSet<String> = existing.iter().map(|issue| issue.task_id.clone()).collect();
    let selected: Vec<Task> = diff.tasks.iter().map(|plan| plan.task.clone()).collect();
    let missing = missing_blockers(&selected, &known);
    if !missing.is_empty() {
        print!("{}", crate::plan::render_tasks(&diff));
        anyhow::bail!("{}", missing.join("\n"));
    }

    let bindings = bindings_for(&diff, &existing);
    let current = load_blocked_by(&http, &repo, &diff).await?;
    let links = diff_links(&selected, &bindings, &current)?;
    if dry_run {
        print!("{}", render(&diff, &links, true));
        return Ok(());
    }

    apply(&http, &repo, &diff, &links, &rel_path, &branch).await?;
    print!("{}", render(&diff, &links, false));
    Ok(())
}

async fn load_existing(http: &GhHttp, repo: &str) -> anyhow::Result<Vec<ExistingIssue>> {
    let issues = match http.list_issues_with_state(repo, PLAN_LABEL, "all").await {
        Ok(issues) => issues,
        Err(err) if err.to_string().contains("404") => Vec::new(),
        Err(err) => return Err(err),
    };
    let mut existing = Vec::new();
    for issue in issues {
        let body = issue.body.unwrap_or_default();
        let Some(task_id) = marker_id(&body) else {
            eprintln!(
                "warning: #{} has the {PLAN_LABEL} label and no plan marker",
                issue.number
            );
            continue;
        };
        if issue.id == 0 {
            anyhow::bail!("issue #{} did not include a database id", issue.number);
        }
        existing.push(ExistingIssue {
            task_id,
            number: issue.number,
            database_id: issue.id,
            title: issue.title,
            body,
            labels: issue.labels.into_iter().map(|label| label.name).collect(),
            state: issue.state,
        });
    }
    Ok(existing)
}

fn bindings_for(diff: &PlanDiff, existing: &[ExistingIssue]) -> Vec<Binding> {
    let mut bindings: Vec<Binding> = diff
        .tasks
        .iter()
        .map(|plan| Binding {
            task_id: plan.task.id.clone(),
            number: plan.number,
            database_id: plan.database_id,
        })
        .collect();
    for issue in existing {
        if bindings
            .iter()
            .any(|binding| binding.task_id == issue.task_id)
        {
            continue;
        }
        bindings.push(Binding {
            task_id: issue.task_id.clone(),
            number: Some(issue.number),
            database_id: Some(issue.database_id),
        });
    }
    bindings
}

async fn load_blocked_by(
    http: &GhHttp,
    repo: &str,
    diff: &PlanDiff,
) -> anyhow::Result<HashMap<u64, Vec<DepRef>>> {
    let mut current = HashMap::new();
    for plan in &diff.tasks {
        let Some(number) = plan.number else {
            continue;
        };
        if current.contains_key(&number) {
            continue;
        }
        let blocked = match http.list_blocked_by(repo, number).await? {
            crate::backends::BlockedByFetch::Native(items) => items,
            crate::backends::BlockedByFetch::Unavailable => {
                anyhow::bail!("blocked-by API unavailable for #{number}");
            }
            crate::backends::BlockedByFetch::Failed(err) => {
                anyhow::bail!("blocked-by API failed for #{number}: {err}");
            }
        };
        current.insert(
            number,
            blocked
                .into_iter()
                .map(|item| DepRef {
                    number: item.number as u64,
                    database_id: item.github_id as u64,
                })
                .collect(),
        );
    }
    Ok(current)
}

async fn apply(
    http: &GhHttp,
    repo: &str,
    diff: &PlanDiff,
    links: &[LinkEdit],
    rel_path: &str,
    branch: &str,
) -> anyhow::Result<()> {
    http.ensure_label(repo, PLAN_LABEL, "6f42c1", "Imported from a roadmap file")
        .await?;
    http.ensure_label(repo, HUMAN_LABEL, "fbca04", "A person does this task")
        .await?;
    http.ensure_label(repo, NO_AGENT_LABEL, "ededed", "Skip agent dispatch")
        .await?;

    let mut numbers: HashMap<String, (u64, u64)> = HashMap::new();
    for plan in &diff.tasks {
        if let (Some(number), Some(database_id)) = (plan.number, plan.database_id) {
            numbers.insert(plan.task.id.clone(), (number, database_id));
        }
    }

    for plan in &diff.tasks {
        match plan.kind {
            crate::plan::ChangeKind::Unchanged => {}
            crate::plan::ChangeKind::Create => {
                let title = issue_title(&plan.task);
                let body = issue_body(&plan.task, repo, branch, rel_path);
                let labels = labels_for(&[], plan.task.owner_human);
                let issue = http.create_issue(repo, &title, &body, &labels).await?;
                if issue.id == 0 {
                    anyhow::bail!(
                        "created issue #{} did not include a database id",
                        issue.number
                    );
                }
                numbers.insert(plan.task.id.clone(), (issue.number, issue.id));
            }
            crate::plan::ChangeKind::Update => {
                let number = plan.number.context("update is missing an issue number")?;
                let title = issue_title(&plan.task);
                let body = issue_body(&plan.task, repo, branch, rel_path);
                let labels = labels_for(&plan.existing_labels, plan.task.owner_human);
                let issue = http
                    .update_issue(repo, number, &title, &body, &labels)
                    .await?;
                let database_id = if issue.id == 0 {
                    plan.database_id.unwrap_or(0)
                } else {
                    issue.id
                };
                if database_id == 0 {
                    anyhow::bail!("issue #{number} did not include a database id");
                }
                numbers.insert(plan.task.id.clone(), (issue.number, database_id));
            }
        }
    }

    for link in links {
        match link {
            LinkEdit::Add {
                task_id,
                blocking_task_id,
                blocking_database_id,
                ..
            } => {
                let (number, _) = numbers
                    .get(task_id)
                    .with_context(|| format!("{task_id} has no issue number"))?;
                let blocking_id = blocking_database_id.or_else(|| {
                    numbers
                        .get(blocking_task_id)
                        .map(|(_, database_id)| *database_id)
                });
                let Some(blocking_id) = blocking_id else {
                    anyhow::bail!("{task_id} depends on {blocking_task_id}, which has no issue id");
                };
                http.add_blocked_by(repo, *number, blocking_id as i64)
                    .await?;
            }
            LinkEdit::Remove {
                issue_number,
                blocking_database_id,
                ..
            } => {
                http.remove_blocked_by(repo, *issue_number, *blocking_database_id)
                    .await?;
            }
        }
    }
    Ok(())
}

fn repo_relative(path: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let top = String::from_utf8(output.stdout).ok()?;
    let top = Path::new(top.trim()).to_path_buf();
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    let abs = abs.canonicalize().unwrap_or(abs);
    let top = top.canonicalize().unwrap_or(top);
    let rel = abs.strip_prefix(&top).ok()?;
    Some(
        rel.components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
    )
}
