//! `orch plan` turns a free-form markdown plan into issues.

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;

use crate::backends::BlockedByFetch;
use crate::cli;
use crate::config;
use crate::control;
use crate::github::http::GhHttp;
use crate::github::types::GitHubIssue;
use crate::plan::{
    self, Binding, ChangeKind, DepRef, ExistingIssue, HUMAN_LABEL, LinkEdit, NO_AGENT_LABEL,
    PLAN_LABEL, PlanDiff, Roadmap, Task, UnmetExternal, diff_import, diff_links, issue_body_with,
    issue_title, labels_for, marker_id, render_report, skip_unmet_ids, unmet_externals,
};
use crate::store::TaskStore;

/// Draft a roadmap from a free-form plan, print it, and import after a yes.
pub async fn from_plan(file: &str, yes: bool) -> anyhow::Result<()> {
    let path = Path::new(file);
    let markdown =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    if markdown.trim().is_empty() {
        anyhow::bail!("{} is empty", path.display());
    }
    let dest = roadmap_path(path);
    let existing = fs::read_to_string(&dest).ok();
    let (roadmap_md, roadmap) = draft_roadmap(&markdown, existing.as_deref()).await?;
    let dest_str = dest
        .to_str()
        .with_context(|| format!("roadmap path {} is not utf-8", dest.display()))?;
    // Save the draft before asking, a later run reuses the ids
    fs::write(&dest, with_trailing_newline(&roadmap_md))
        .with_context(|| format!("failed to write {dest_str}"))?;
    print!("{}", plan::render_outline(&roadmap));
    println!("Wrote {dest_str}");
    if !yes && !prompt_import()? {
        println!("aborted");
        return Ok(());
    }
    import(dest_str, false, None, false).await
}

pub async fn import(
    file: &str,
    dry_run: bool,
    milestone: Option<&str>,
    skip_unmet: bool,
) -> anyhow::Result<()> {
    let path = Path::new(file);
    let markdown =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let repo = config::get_current_repo()
        .context("'repo' not set — run `orch init` or set gh.repo in .orch.yml")?;
    let rel_path = repo_relative(path).unwrap_or_else(|| {
        let name = path.file_name().map_or_else(
            || file.to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        eprintln!("warning: {file} is outside the git checkout, the issue link uses {name}");
        name
    });
    let outcome =
        import_roadmap(&repo, &markdown, &rel_path, milestone, skip_unmet, dry_run).await?;
    for warning in outcome.warnings {
        eprintln!("warning: {warning}");
    }
    print!("{}", outcome.report);
    Ok(())
}

/// What an import did, or would do on a dry run, as the text `orch plan import` prints
#[derive(Debug, Clone)]
pub(crate) struct ImportOutcome {
    pub(crate) report: String,
    pub(crate) warnings: Vec<String>,
}

/// Import a roadmap into `repo`. `rel_path` is where the roadmap lives in the repo, issues link to it
pub(crate) async fn import_roadmap(
    repo: &str,
    markdown: &str,
    rel_path: &str,
    milestone: Option<&str>,
    skip_unmet: bool,
    dry_run: bool,
) -> anyhow::Result<ImportOutcome> {
    let repo = repo.to_string();
    let rel_path = rel_path.to_string();
    let (mut roadmap, warnings) = plan::parse_roadmap_for(markdown, milestone)?;
    if let Some(id) = milestone {
        plan::select(&roadmap, Some(id))?;
    }

    let http = GhHttp::new()?;
    let branch = http.get_default_branch(&repo).await?;
    let existing = load_existing(&http, &repo).await?;
    let has_workflow = http.has_workflow_on_branch(&repo, &branch).await?;
    let ci_open = existing.iter().any(|issue| {
        plan::is_ci_bootstrap_id(&issue.task_id) && issue.state.eq_ignore_ascii_case("open")
    });
    if plan::needs_ci_bootstrap(has_workflow, ci_open) {
        let shipcheck = http
            .has_file_on_branch(&repo, "shipcheck.toml", &branch)
            .await?;
        plan::inject_ci_bootstrap(&mut roadmap, shipcheck);
    }
    let known: HashSet<String> = existing.iter().map(|issue| issue.task_id.clone()).collect();
    let selected_all: Vec<Task> = plan::select(&roadmap, milestone)?
        .into_iter()
        .cloned()
        .collect();
    let unmet = unmet_externals(&selected_all, &roadmap, &known)?;
    let extras = extras_by_task(&unmet);
    let body_for = |task: &Task| {
        let extra = extras.get(&task.id).map_or(&[][..], Vec::as_slice);
        issue_body_with(task, &repo, &branch, &rel_path, extra)
    };
    let mut diff = diff_import(&roadmap, &existing, milestone, body_for)?;
    let skipped = if skip_unmet {
        let skip = skip_unmet_ids(&selected_all, &unmet);
        let mut skipped = Vec::new();
        diff.tasks.retain(|plan| {
            if plan.kind == ChangeKind::Create && skip.contains(&plan.task.id) {
                skipped.push(plan.task.id.clone());
                false
            } else {
                true
            }
        });
        skipped
    } else {
        Vec::new()
    };

    let selected: Vec<Task> = diff.tasks.iter().map(|plan| plan.task.clone()).collect();
    let bindings = bindings_for(&diff, &existing);
    let current = load_blocked_by(&http, &repo, &diff).await?;
    let links = diff_links(&selected, &bindings, &current)?;
    if dry_run {
        return Ok(ImportOutcome {
            report: render_report(&diff, &links, &unmet, &skipped, true),
            warnings,
        });
    }

    apply(&http, &repo, &diff, &links, &rel_path, &branch, &extras).await?;
    Ok(ImportOutcome {
        report: render_report(&diff, &links, &unmet, &skipped, false),
        warnings,
    })
}

fn extras_by_task(unmet: &[UnmetExternal]) -> HashMap<String, Vec<String>> {
    let mut extras: HashMap<String, Vec<String>> = HashMap::new();
    for item in unmet {
        extras
            .entry(item.task_id.clone())
            .or_default()
            .push(item.blocking_task_id.clone());
    }
    extras
}

async fn load_existing(http: &GhHttp, repo: &str) -> anyhow::Result<Vec<ExistingIssue>> {
    // No label filter, so an issue with the marker and no label still matches
    let issues = match http.list_issues_with_state(repo, "", "all").await {
        Ok(issues) => issues,
        Err(err) if err.to_string().contains("404") => Vec::new(),
        Err(err) => return Err(err),
    };
    existing_from_issues(issues)
}

fn existing_from_issues(issues: Vec<GitHubIssue>) -> anyhow::Result<Vec<ExistingIssue>> {
    let mut existing = Vec::new();
    for issue in issues {
        let labeled = issue.labels.iter().any(|label| label.name == PLAN_LABEL);
        // Only a label or a repo writer vouches for an unlabeled marker, so a stranger cannot bind an issue
        let trusted = labeled
            || matches!(
                issue.author_association.as_deref(),
                Some("OWNER" | "MEMBER" | "COLLABORATOR")
            );
        let body = issue.body.unwrap_or_default();
        let Some(task_id) = marker_id(&body).filter(|_| trusted) else {
            if labeled {
                eprintln!(
                    "warning: #{} has the {PLAN_LABEL} label and no plan marker",
                    issue.number
                );
            }
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
            BlockedByFetch::Native(items) => items,
            BlockedByFetch::Unavailable => {
                anyhow::bail!("blocked-by API unavailable for #{number}");
            }
            BlockedByFetch::Failed(err) | BlockedByFetch::Unreachable(err) => {
                anyhow::bail!("blocked-by API failed for #{number}: {err}");
            }
        };
        current.insert(
            number,
            blocked
                .into_iter()
                .map(|item| DepRef {
                    number: u64::try_from(item.number).unwrap_or(0),
                    database_id: u64::try_from(item.github_id).unwrap_or(0),
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
    blocked_external: &HashMap<String, Vec<String>>,
) -> anyhow::Result<()> {
    http.ensure_label(repo, PLAN_LABEL, "6f42c1", "Imported from a roadmap file")
        .await?;
    http.ensure_label(repo, HUMAN_LABEL, "fbca04", "A person does this task")
        .await?;
    http.ensure_label(repo, NO_AGENT_LABEL, "ededed", "Skip agent dispatch")
        .await?;

    let mut numbers: HashMap<String, (u64, u64)> = HashMap::new();
    let mut held: Vec<u64> = Vec::new();
    for plan in &diff.tasks {
        if let (Some(number), Some(database_id)) = (plan.number, plan.database_id) {
            numbers.insert(plan.task.id.clone(), (number, database_id));
        }
    }

    for plan in &diff.tasks {
        match plan.kind {
            ChangeKind::Unchanged => {}
            ChangeKind::Create => {
                let title = issue_title(&plan.task);
                let extra = blocked_external
                    .get(&plan.task.id)
                    .map_or(&[][..], Vec::as_slice);
                let body = issue_body_with(&plan.task, repo, branch, rel_path, extra);
                let mut labels = labels_for(&[], plan.task.owner_human);
                // Held until its blocked-by links exist, so the engine cannot route it ahead of them
                if !plan.task.owner_human {
                    labels.push(NO_AGENT_LABEL.to_string());
                }
                let issue = http.create_issue(repo, &title, &body, &labels).await?;
                if !plan.task.owner_human {
                    held.push(issue.number);
                }
                if issue.id == 0 {
                    anyhow::bail!(
                        "created issue #{} did not include a database id",
                        issue.number
                    );
                }
                numbers.insert(plan.task.id.clone(), (issue.number, issue.id));
            }
            ChangeKind::Update => {
                let number = plan.number.context("update is missing an issue number")?;
                let title = issue_title(&plan.task);
                let extra = blocked_external
                    .get(&plan.task.id)
                    .map_or(&[][..], Vec::as_slice);
                let body = issue_body_with(&plan.task, repo, branch, rel_path, extra);
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
                let blocking_id =
                    i64::try_from(blocking_id).context("issue database id is out of range")?;
                http.add_blocked_by(repo, *number, blocking_id).await?;
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

    // Every link is in, so release the new issues. A failure above leaves them held
    for number in held {
        http.remove_label(repo, &number.to_string(), NO_AGENT_LABEL)
            .await?;
    }
    Ok(())
}

fn repo_relative(path: &Path) -> Option<String> {
    let output = Command::new("git")
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
        env::current_dir().ok()?.join(path)
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

async fn draft_roadmap(plan: &str, existing: Option<&str>) -> anyhow::Result<(String, Roadmap)> {
    let store = cli::init_store().await?;
    draft_roadmap_with(&store, plan, existing).await
}

/// A roadmap drafted from a free-form plan by the control session's agent, retried once
pub(crate) async fn draft_roadmap_with(
    store: &TaskStore,
    plan: &str,
    existing: Option<&str>,
) -> anyhow::Result<(String, Roadmap)> {
    let agent = control::get_agent(store).await?;
    let model = control::get_model(store).await?;
    if model.is_empty() {
        eprintln!("Drafting a roadmap with {agent}…");
    } else {
        eprintln!("Drafting a roadmap with {agent}:{model}…");
    }
    let prompt = build_prompt(plan, existing);
    let result = control::invoke_agent(&agent, &model, "", &prompt).await?;
    match accept_roadmap(&result.text) {
        Ok(ok) => Ok(ok),
        Err(first) => {
            tracing::warn!(error = %first, "plan draft did not parse, retrying once");
            let retry = retry_prompt(plan, existing, &result.text, &first);
            let second = control::invoke_agent(&agent, &model, "", &retry).await?;
            accept_roadmap(&second.text)
        }
    }
}

fn build_prompt(plan: &str, existing: Option<&str>) -> String {
    let existing = match existing.map(str::trim).filter(|text| !text.is_empty()) {
        Some(text) => text.to_string(),
        None => "(empty)".to_string(),
    };
    include_str!("../../prompts/plan_roadmap.md")
        .replace("{{EXISTING}}", &existing)
        .replace("{{PLAN}}", plan)
}

fn retry_prompt(
    plan: &str,
    existing: Option<&str>,
    previous: &str,
    error: &anyhow::Error,
) -> String {
    let mut prompt = build_prompt(plan, existing);
    prompt.push_str("\n\nThe previous attempt was not a valid roadmap:\n\n");
    prompt.push_str(&error.to_string());
    prompt.push_str("\n\nPrevious output:\n\n");
    prompt.push_str(previous.trim());
    prompt.push_str("\n\nWrite a corrected roadmap. Output only the markdown.\n");
    prompt
}

fn accept_roadmap(raw: &str) -> anyhow::Result<(String, Roadmap)> {
    let mut last = anyhow::anyhow!("agent returned no roadmap");
    for candidate in roadmap_candidates(raw) {
        let slices = milestone_slices(&candidate);
        let preferred: Vec<&str> = slices
            .iter()
            .copied()
            .filter(|slice| starts_with_milestone(slice))
            .collect();
        let fallback = if preferred.is_empty() {
            if slices.is_empty() {
                vec![candidate.as_str()]
            } else {
                slices
            }
        } else {
            preferred
        };
        for slice in fallback {
            match try_parse(slice) {
                Ok(ok) => return Ok(ok),
                Err(err) => last = err,
            }
        }
    }
    Err(last)
}

fn starts_with_milestone(text: &str) -> bool {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .is_some_and(plan::is_milestone_heading)
}

fn milestone_slices(text: &str) -> Vec<&str> {
    let mut slices = Vec::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if line.trim_start().starts_with("## ") {
            slices.push(text[offset..].trim());
        }
        offset += line.len();
    }
    slices
}

fn try_parse(markdown: &str) -> anyhow::Result<(String, Roadmap)> {
    let markdown = markdown.trim();
    let roadmap = plan::parse_roadmap(markdown)?;
    if roadmap.tasks.is_empty() {
        anyhow::bail!("roadmap has no tasks");
    }
    Ok((markdown.to_string(), roadmap))
}

fn roadmap_candidates(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for block in fenced_blocks(raw) {
        push_unique(&mut out, &block);
    }
    let stripped = strip_outer_fence(raw);
    push_unique(&mut out, &stripped);
    out
}

fn push_unique(out: &mut Vec<String>, text: &str) {
    let trimmed = text.trim();
    if trimmed.is_empty() || out.iter().any(|have| have == trimmed) {
        return;
    }
    out.push(trimmed.to_string());
}

fn strip_outer_fence(raw: &str) -> String {
    let trimmed = raw.trim();
    let without_open = if let Some(rest) = trimmed.strip_prefix("```") {
        match rest.find('\n') {
            Some(i) => &rest[i + 1..],
            None => rest,
        }
    } else {
        trimmed
    };
    without_open
        .trim_end()
        .strip_suffix("```")
        .unwrap_or(without_open)
        .trim()
        .to_string()
}

fn fenced_blocks(raw: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut index = 0;
    while let Some(rel) = raw[index..].find("```") {
        let start = index + rel + 3;
        let after_open = &raw[start..];
        let content_start = match after_open.find('\n') {
            Some(i) => start + i + 1,
            None => break,
        };
        if let Some(rel_end) = raw[content_start..].find("```") {
            let end = content_start + rel_end;
            blocks.push(raw[content_start..end].trim().to_string());
            index = end + 3;
        } else {
            blocks.push(raw[content_start..].trim().to_string());
            break;
        }
    }
    blocks
}

pub(crate) fn roadmap_path(plan: &Path) -> PathBuf {
    let name = plan.file_name().map_or_else(
        || "plan.md".to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    let file_name = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem}.roadmap.{ext}"),
        _ => format!("{name}.roadmap.md"),
    };
    match plan.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(file_name),
        _ => PathBuf::from(file_name),
    }
}

fn with_trailing_newline(text: &str) -> String {
    if text.ends_with('\n') {
        text.to_string()
    } else {
        format!("{text}\n")
    }
}

fn prompt_import() -> anyhow::Result<bool> {
    print!("\nImport these issues? [y/n] ");
    io::stdout().flush().ok();
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(is_yes(&input))
}

fn is_yes(input: &str) -> bool {
    matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::types::{GitHubLabel, GitHubUser};

    fn issue(number: u64, body: &str, labels: &[&str]) -> GitHubIssue {
        issue_by(number, body, labels, "OWNER")
    }

    fn issue_by(number: u64, body: &str, labels: &[&str], association: &str) -> GitHubIssue {
        GitHubIssue {
            number,
            id: number * 10,
            title: format!("issue {number}"),
            body: Some(body.to_string()),
            state: "open".to_string(),
            labels: labels
                .iter()
                .map(|name| GitHubLabel {
                    name: name.to_string(),
                    color: None,
                })
                .collect(),
            user: GitHubUser {
                login: "u".to_string(),
            },
            created_at: String::new(),
            updated_at: String::new(),
            html_url: String::new(),
            node_id: None,
            pull_request: None,
            author_association: Some(association.to_string()),
            state_reason: None,
        }
    }

    #[test]
    fn marker_matches_with_or_without_the_label_and_no_marker_is_skipped() {
        let found = existing_from_issues(vec![
            issue(1, "<!-- orch-plan: M1-01 -->", &[]),
            issue(2, "<!-- orch-plan: M1-02 -->", &[PLAN_LABEL]),
            issue(3, "no marker", &[PLAN_LABEL]),
            issue(4, "no marker", &[]),
            issue_by(5, "<!-- orch-plan: M1-03 -->", &[], "NONE"),
        ])
        .unwrap();
        let ids: Vec<&str> = found.iter().map(|item| item.task_id.as_str()).collect();
        assert_eq!(ids, ["M1-01", "M1-02"]);
    }

    const VALID_ROADMAP: &str = "\
## M1. Workspace

### M1-01 Create the crate

- Goal: a crate that builds.
- Scope: the library crate.
- Acceptance:
  - `cargo test` passes.
- Depends on: none.

### M1-02 Add a parser

- Goal: parse the config file.
- Acceptance:
  - A sample file round-trips.
- Depends on: M1-01.
";

    #[test]
    fn accept_roadmap_reads_a_fenced_response_with_preamble() {
        let raw = format!(
            "Here is the roadmap:\n\n```markdown\n{VALID_ROADMAP}```\n\nLet me know if you want edits.\n"
        );
        let (markdown, roadmap) = accept_roadmap(&raw).unwrap();
        assert_eq!(markdown, VALID_ROADMAP.trim());
        assert_eq!(
            roadmap
                .tasks
                .iter()
                .map(|task| task.id.as_str())
                .collect::<Vec<_>>(),
            ["M1-01", "M1-02"]
        );
        assert_eq!(roadmap.tasks[1].depends_on, ["M1-01".to_string()]);
    }

    #[test]
    fn accept_roadmap_reads_plain_markdown() {
        let (markdown, roadmap) = accept_roadmap(VALID_ROADMAP).unwrap();
        assert_eq!(markdown, VALID_ROADMAP.trim());
        assert_eq!(roadmap.milestones[0].id, "M1");
    }

    #[test]
    fn accept_roadmap_starts_at_the_first_milestone() {
        let raw = format!("Notes from the meeting.\n\n{VALID_ROADMAP}");
        let (markdown, roadmap) = accept_roadmap(&raw).unwrap();
        assert!(markdown.starts_with("## M1. Workspace"), "{markdown}");
        assert_eq!(roadmap.tasks.len(), 2);
    }

    #[test]
    fn accept_roadmap_skips_a_leading_notes_heading() {
        let raw = format!("## Notes\n\nFrom the meeting.\n\n{VALID_ROADMAP}");
        let (markdown, roadmap) = accept_roadmap(&raw).unwrap();
        assert!(markdown.starts_with("## M1. Workspace"), "{markdown}");
        assert_eq!(roadmap.tasks.len(), 2);
    }

    #[test]
    fn accept_roadmap_reads_an_unclosed_fence() {
        let raw = format!("```markdown\n{VALID_ROADMAP}");
        let (markdown, roadmap) = accept_roadmap(&raw).unwrap();
        assert!(markdown.starts_with("## M1. Workspace"), "{markdown}");
        assert_eq!(roadmap.tasks.len(), 2);
    }

    #[test]
    fn accept_roadmap_rejects_a_plan_with_no_ids() {
        let err = accept_roadmap("# Ship it\n\nBuild the app and write tests.\n").unwrap_err();
        assert!(!err.to_string().is_empty(), "{err}");
    }

    #[test]
    fn one_page_plan_without_ids_has_a_roadmap_import_accepts() {
        let plan = "\
# Notes app

Build a local notes app.

First, set up a Rust crate that builds and tests on a clean checkout.

Then add a notes file format and a command that lists notes.

A person has to create the GitHub repo.
";
        assert!(plan::parse_roadmap(plan).unwrap().tasks.is_empty());
        assert!(accept_roadmap(plan).is_err());
        let dir = tempfile::tempdir().unwrap();
        let plan_file = dir.path().join("notes.md");
        fs::write(&plan_file, plan).unwrap();
        let dest = roadmap_path(&plan_file);
        let (markdown, imported) = accept_roadmap(VALID_ROADMAP).unwrap();
        fs::write(&dest, with_trailing_newline(&markdown)).unwrap();
        let saved = fs::read_to_string(&dest).unwrap();
        let (roadmap, warnings) = plan::parse_roadmap_for(&saved, None).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(!imported.tasks.is_empty());
        assert_eq!(roadmap.tasks.len(), imported.tasks.len());
        assert!(roadmap.tasks.iter().all(|task| !task.id.is_empty()));
        assert_eq!(dest.file_name().unwrap(), "notes.roadmap.md");
    }

    #[test]
    fn retry_prompt_includes_the_error_and_previous_output() {
        let prompt = retry_prompt(
            "# Build it\n",
            None,
            "not a roadmap",
            &anyhow::anyhow!("roadmap has no tasks"),
        );
        assert!(prompt.contains("# Build it"));
        assert!(prompt.contains("not a roadmap"));
        assert!(prompt.contains("roadmap has no tasks"));
        assert!(prompt.contains("Write a corrected roadmap"));
    }

    #[test]
    fn roadmap_path_sits_next_to_the_plan() {
        assert_eq!(
            roadmap_path(Path::new("docs/PLAN.md")),
            PathBuf::from("docs/PLAN.roadmap.md")
        );
        assert_eq!(
            roadmap_path(Path::new("plan.md")),
            PathBuf::from("plan.roadmap.md")
        );
        assert_eq!(
            roadmap_path(Path::new("/tmp/idea")),
            PathBuf::from("/tmp/idea.roadmap.md")
        );
    }

    #[test]
    fn prompt_keeps_existing_ids_and_the_plan_body() {
        let prompt = build_prompt("# Build it\n", Some(VALID_ROADMAP));
        assert!(prompt.contains("# Build it"));
        assert!(prompt.contains("### M1-01 Create the crate"));
        assert!(prompt.contains("keep every task id"));
    }

    #[test]
    fn yes_accepts_y_and_yes() {
        assert!(is_yes("y\n"));
        assert!(is_yes(" Yes "));
        assert!(!is_yes("n"));
        assert!(!is_yes(""));
    }

    #[test]
    fn trailing_newline_is_added_once() {
        assert_eq!(with_trailing_newline("a"), "a\n");
        assert_eq!(with_trailing_newline("a\n"), "a\n");
    }
}
