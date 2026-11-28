//! Roadmap files and the issue plan `orch plan import` applies.
//!
//! A milestone is `## M1. Title`. A task is `### M1-03 Title` with Goal, Scope,
//! Acceptance, Depends on and Owner lines. Later milestones may be a table with
//! those columns. An unknown table column or a task with no title is an error.
//! `none` is no dependency.

use std::collections::{HashMap, HashSet};
use std::error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::fmt::Write as _;
use std::hash::BuildHasher;

pub const PLAN_LABEL: &str = "orch-plan";
pub const HUMAN_LABEL: &str = "owner:human";
pub const NO_AGENT_LABEL: &str = "no-agent";
/// Marks the `no-agent` that an import adds while it links blocked-by, so a later import releases
/// that hold and never a `no-agent` the owner set by hand
pub const HOLD_LABEL: &str = "orch-plan-hold";
pub const CI_BOOTSTRAP_ID: &str = "CI-01";
pub const CI_BOOTSTRAP_MARKER: &str = "<!-- orch-ci-bootstrap -->";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanError {
    pub message: String,
}

impl Display for PlanError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl error::Error for PlanError {}

impl PlanError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Milestone {
    pub id: String,
    pub title: String,
    /// Text after `## `, used for the GitHub heading anchor
    pub heading: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub milestone_id: String,
    pub milestone_title: String,
    pub milestone_heading: String,
    /// Full `###` heading. Empty for a table row, which links to the milestone heading
    pub anchor_heading: String,
    pub goal: String,
    pub scope: String,
    pub acceptance: Vec<String>,
    pub depends_on: Vec<String>,
    pub owner_human: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Roadmap {
    pub milestones: Vec<Milestone>,
    pub tasks: Vec<Task>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Create,
    Update,
    Unchanged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingIssue {
    pub task_id: String,
    pub number: u64,
    pub database_id: u64,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskPlan {
    pub task: Task,
    pub kind: ChangeKind,
    pub number: Option<u64>,
    pub database_id: Option<u64>,
    pub existing_labels: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovedTask {
    pub task_id: String,
    pub number: u64,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanDiff {
    pub tasks: Vec<TaskPlan>,
    pub removed: Vec<RemovedTask>,
}

/// A selected task depends on a roadmap id outside the import that has no issue yet
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnmetExternal {
    pub task_id: String,
    pub blocking_task_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub task_id: String,
    pub number: Option<u64>,
    pub database_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepRef {
    pub number: u64,
    pub database_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkEdit {
    Add {
        task_id: String,
        issue_number: Option<u64>,
        blocking_task_id: String,
        blocking_number: Option<u64>,
        blocking_database_id: Option<u64>,
    },
    Remove {
        task_id: String,
        issue_number: u64,
        blocking_number: u64,
        blocking_database_id: u64,
    },
}

struct Draft {
    id: String,
    title: String,
    milestone_id: String,
    milestone_title: String,
    milestone_heading: String,
    anchor_heading: String,
    goal: String,
    scope: String,
    acceptance: Vec<String>,
    depends_raw: Option<String>,
    owner_human: bool,
    in_acceptance: bool,
    line: usize,
}

#[derive(Clone, Copy)]
struct TableCols {
    task: usize,
    goal: Option<usize>,
    scope: Option<usize>,
    acceptance: Option<usize>,
    depends: Option<usize>,
    owner: Option<usize>,
}

pub fn parse_roadmap(markdown: &str) -> Result<Roadmap, PlanError> {
    parse_roadmap_for(markdown, None).map(|(roadmap, _)| roadmap)
}

/// With a milestone, errors that belong to other milestones come back as warnings
pub fn parse_roadmap_for(
    markdown: &str,
    milestone: Option<&str>,
) -> Result<(Roadmap, Vec<String>), PlanError> {
    let mut milestones = Vec::new();
    let mut tasks = Vec::new();
    let mut errors = Vec::new();
    let mut current: Option<Milestone> = None;
    let mut draft: Option<Draft> = None;
    let mut table: Option<TableCols> = None;
    let mut seen = HashSet::new();
    let mut tags: Vec<Option<String>> = Vec::new();
    let mut tag: Option<String> = None;

    for (idx, raw) in markdown.lines().enumerate() {
        tags.resize(errors.len(), tag.clone());
        tag = current.as_ref().map(|item| item.id.clone());
        let line = raw.trim_end_matches('\r');
        let line_no = idx + 1;

        if let Some(cols) = table {
            if line.trim().starts_with('|') {
                if !is_separator_row(line)
                    && let Some(milestone) = current.as_ref()
                {
                    match task_from_row(line, &cols, milestone, line_no) {
                        Ok(Some(task)) => {
                            push_task(task, &mut tasks, &mut seen, &mut errors);
                        }
                        Ok(None) => {}
                        Err(err) => errors.push(err),
                    }
                }
                continue;
            }
            table = None;
        }

        if let Some(rest) = line.strip_prefix("### ") {
            flush_draft(&mut draft, &mut tasks, &mut seen, &mut errors);
            let Some(milestone) = current.as_ref() else {
                errors.push(format!(
                    "line {line_no}: task heading is outside a milestone"
                ));
                continue;
            };
            if let Some((id, title)) = parse_task_heading(rest) {
                if title.is_empty() {
                    errors.push(format!("line {line_no}: {id} has no title"));
                } else {
                    draft = Some(Draft::new(id, title, milestone, rest.trim(), line_no));
                }
            }
            continue;
        }

        if let Some(rest) = line.strip_prefix("## ") {
            flush_draft(&mut draft, &mut tasks, &mut seen, &mut errors);
            if let Some((id, title)) = parse_milestone_heading(rest) {
                let milestone = Milestone {
                    id,
                    title,
                    heading: rest.trim().to_string(),
                };
                current = Some(milestone.clone());
                milestones.push(milestone);
            } else {
                current = None;
            }
            continue;
        }

        if let Some((cols, unknown)) = table_cols(line) {
            flush_draft(&mut draft, &mut tasks, &mut seen, &mut errors);
            if current.is_some() {
                if !unknown.is_empty() {
                    errors.push(format!(
                        "line {line_no}: unknown table column {}",
                        unknown.join(", ")
                    ));
                }
                table = Some(cols);
            }
            continue;
        }

        if let Some(draft) = draft.as_mut() {
            if let Some((name, value)) = top_level_field(line) {
                draft.apply_field(name, value);
                continue;
            }
            draft.take_acceptance_line(line);
        }
    }

    flush_draft(&mut draft, &mut tasks, &mut seen, &mut errors);
    tags.resize(errors.len(), tag);
    check_dependencies(&tasks, &mut errors);
    for message in &errors[tags.len()..] {
        let id = message
            .split_whitespace()
            .next()
            .and_then(task_milestone_id);
        tags.push(id.map(str::to_string));
    }

    let (mut fatal, mut warnings) = (Vec::new(), Vec::new());
    for (message, tag) in errors.into_iter().zip(tags) {
        let elsewhere = match (milestone, tag) {
            (Some(wanted), Some(id)) => !id.eq_ignore_ascii_case(wanted),
            _ => false,
        };
        if elsewhere {
            warnings.push(message);
        } else {
            fatal.push(message);
        }
    }
    if fatal.is_empty() {
        Ok((Roadmap { milestones, tasks }, warnings))
    } else {
        Err(PlanError::new(fatal.join("\n")))
    }
}

pub fn select<'a>(
    roadmap: &'a Roadmap,
    milestone: Option<&str>,
) -> Result<Vec<&'a Task>, PlanError> {
    let Some(wanted) = milestone else {
        return Ok(roadmap.tasks.iter().collect());
    };
    if !roadmap
        .milestones
        .iter()
        .any(|item| item.id.eq_ignore_ascii_case(wanted))
    {
        return Err(PlanError::new(format!("no milestone {wanted}")));
    }
    Ok(roadmap
        .tasks
        .iter()
        .filter(|task| {
            task.milestone_id.eq_ignore_ascii_case(wanted) || is_ci_bootstrap_id(&task.id)
        })
        .collect())
}

pub fn issue_title(task: &Task) -> String {
    if task.title.is_empty() {
        task.id.clone()
    } else {
        format!("{} {}", task.id, task.title)
    }
}

pub fn task_anchor(task: &Task) -> String {
    if task.anchor_heading.is_empty() {
        github_anchor(&task.milestone_heading)
    } else {
        github_anchor(&task.anchor_heading)
    }
}

pub fn issue_body(task: &Task, repo: &str, branch: &str, rel_path: &str) -> String {
    issue_body_with(task, repo, branch, rel_path, &[])
}

pub fn issue_body_with(
    task: &Task,
    repo: &str,
    branch: &str,
    rel_path: &str,
    blocked_external: &[String],
) -> String {
    let mut out = String::new();
    if is_ci_bootstrap_id(&task.id) {
        out.push_str(CI_BOOTSTRAP_MARKER);
        out.push('\n');
    }
    if !task.goal.is_empty() {
        out.push_str("## Goal\n\n");
        out.push_str(task.goal.trim());
        out.push_str("\n\n");
    }
    if !task.scope.is_empty() {
        out.push_str("## Scope\n\n");
        out.push_str(task.scope.trim());
        out.push_str("\n\n");
    }
    if !task.acceptance.is_empty() {
        out.push_str("## Acceptance\n\n");
        for item in &task.acceptance {
            let item = item.trim_end();
            if item.starts_with('-') || item.starts_with('*') || item.starts_with(' ') {
                out.push_str(item);
            } else {
                out.push_str("- ");
                out.push_str(item);
            }
            out.push('\n');
        }
        out.push('\n');
    }
    if !blocked_external.is_empty() {
        out.push_str("blocked-external: ");
        out.push_str(&blocked_external.join(", "));
        out.push_str(" (no issue yet)\n\n");
    }
    if !is_ci_bootstrap_id(&task.id) {
        let url = format!(
            "https://github.com/{repo}/blob/{branch}/{path}#{anchor}",
            branch = encode_git_path(branch),
            path = encode_git_path(rel_path),
            anchor = urlencoding::encode(&task_anchor(task)),
        );
        let _ = write!(out, "[{rel_path}]({url})\n\n");
    }
    if !blocked_external.is_empty() {
        out.push_str("<!-- orch-blocked-external: ");
        out.push_str(&blocked_external.join(", "));
        out.push_str(" -->\n");
    }
    out.push_str(&marker_for(&task.id));
    out.push('\n');
    out
}

pub fn marker_for(id: &str) -> String {
    format!("<!-- orch-plan: {id} -->")
}

pub fn marker_id(body: &str) -> Option<String> {
    let start = body.find("<!-- orch-plan:")?;
    let rest = &body[start + "<!-- orch-plan:".len()..];
    let end = rest.find("-->")?;
    let id = rest[..end].trim();
    is_task_id(id).then(|| id.to_string())
}

pub fn is_ci_bootstrap_id(id: &str) -> bool {
    id.eq_ignore_ascii_case(CI_BOOTSTRAP_ID)
}

pub fn is_ci_bootstrap_body(body: &str) -> bool {
    body.contains(CI_BOOTSTRAP_MARKER)
}

/// True when import should add CI-01, because the default branch has no workflow or CI-01 is still open
pub fn needs_ci_bootstrap(has_workflow: bool, ci_issue_open: bool) -> bool {
    !has_workflow || ci_issue_open
}

/// Prepend the CI bootstrap task and make every other task wait on it
pub fn inject_ci_bootstrap(roadmap: &mut Roadmap, include_shipcheck: bool) {
    if !roadmap
        .tasks
        .iter()
        .any(|task| is_ci_bootstrap_id(&task.id))
    {
        if !roadmap
            .milestones
            .iter()
            .any(|milestone| milestone.id.eq_ignore_ascii_case("CI"))
        {
            roadmap.milestones.insert(
                0,
                Milestone {
                    id: "CI".to_string(),
                    title: "CI bootstrap".to_string(),
                    heading: "CI. CI bootstrap".to_string(),
                },
            );
        }
        roadmap
            .tasks
            .insert(0, ci_bootstrap_task(include_shipcheck));
    }
    for task in &mut roadmap.tasks {
        if is_ci_bootstrap_id(&task.id) {
            continue;
        }
        if !task.depends_on.iter().any(|dep| is_ci_bootstrap_id(dep)) {
            task.depends_on.insert(0, CI_BOOTSTRAP_ID.to_string());
        }
    }
}

pub(crate) fn ci_bootstrap_task(include_shipcheck: bool) -> Task {
    let mut scope = String::from(
        "Add the smallest project layout that can run tests for this repository's language.\n\
         Add at least one test that fails if that layout is broken.\n\
         Add a GitHub Actions workflow under `.github/workflows/` that runs those tests on every push to the default branch.\n\
         Do not add product features.",
    );
    let mut acceptance = vec![
        "A workflow file exists on the default branch after merge.".to_string(),
        "Pushing to the default branch runs the workflow.".to_string(),
        "The workflow reports a passing check.".to_string(),
        "Tests fail when the skeleton is broken.".to_string(),
    ];
    if include_shipcheck {
        scope.push_str("\nAdd or keep `shipcheck.toml` so local checks match the workflow jobs.");
        acceptance.push("`shipcheck.toml` lists the same checks the workflow runs.".to_string());
    }
    Task {
        id: CI_BOOTSTRAP_ID.to_string(),
        title: "Set up CI and tests".to_string(),
        milestone_id: "CI".to_string(),
        milestone_title: "CI bootstrap".to_string(),
        milestone_heading: "CI. CI bootstrap".to_string(),
        anchor_heading: "CI-01 Set up CI and tests".to_string(),
        goal: "Give this repository a project skeleton, tests, and a GitHub Actions workflow that runs on every push to the default branch, so later tasks can rely on required checks.".to_string(),
        scope,
        acceptance,
        depends_on: Vec::new(),
        owner_human: false,
    }
}

/// Labels for a plan issue. Human tasks also get `no-agent`, which the dispatcher already skips
pub fn labels_for(existing: &[String], human: bool) -> Vec<String> {
    let drop_no_agent = existing.iter().any(|label| label == HUMAN_LABEL);
    let mut labels = vec![PLAN_LABEL.to_string()];
    if human {
        labels.push(HUMAN_LABEL.to_string());
        labels.push(NO_AGENT_LABEL.to_string());
    }
    for label in existing {
        if label == PLAN_LABEL || label == HUMAN_LABEL {
            continue;
        }
        if label == NO_AGENT_LABEL && (human || drop_no_agent) {
            continue;
        }
        if !labels.iter().any(|have| have == label) {
            labels.push(label.clone());
        }
    }
    labels
}

pub fn diff_import(
    roadmap: &Roadmap,
    existing: &[ExistingIssue],
    milestone: Option<&str>,
    mut body_for: impl FnMut(&Task) -> String,
) -> Result<PlanDiff, PlanError> {
    let selected = select(roadmap, milestone)?;
    let mut by_id: HashMap<&str, &ExistingIssue> = HashMap::new();
    let mut errors = Vec::new();
    for issue in existing {
        if by_id.insert(issue.task_id.as_str(), issue).is_some() {
            errors.push(format!(
                "more than one issue is marked as {}",
                issue.task_id
            ));
        }
    }
    if !errors.is_empty() {
        return Err(PlanError::new(errors.join("\n")));
    }

    let mut tasks = Vec::new();
    let mut selected_ids = HashSet::new();
    for task in selected {
        selected_ids.insert(task.id.as_str());
        let title = issue_title(task);
        let body = body_for(task);
        let kind = if let Some(issue) = by_id.get(task.id.as_str()) {
            let same = issue.title == title
                && normalize_body(&issue.body) == normalize_body(&body)
                && same_labels(&issue.labels, &labels_for(&issue.labels, task.owner_human));
            if same {
                ChangeKind::Unchanged
            } else {
                ChangeKind::Update
            }
        } else {
            ChangeKind::Create
        };
        let issue = by_id.get(task.id.as_str()).copied();
        tasks.push(TaskPlan {
            task: task.clone(),
            kind,
            number: issue.map(|item| item.number),
            database_id: issue.map(|item| item.database_id),
            existing_labels: issue.map(|item| item.labels.clone()).unwrap_or_default(),
        });
    }

    let mut removed = Vec::new();
    for issue in existing {
        if selected_ids.contains(issue.task_id.as_str()) || is_ci_bootstrap_id(&issue.task_id) {
            continue;
        }
        if let Some(wanted) = milestone {
            let Some(prefix) = task_milestone_id(&issue.task_id) else {
                continue;
            };
            if !prefix.eq_ignore_ascii_case(wanted) {
                continue;
            }
        }
        removed.push(RemovedTask {
            task_id: issue.task_id.clone(),
            number: issue.number,
            state: issue.state.clone(),
        });
    }

    Ok(PlanDiff { tasks, removed })
}

/// Dependencies outside the selected set that have no GitHub issue yet.
///
/// An id that is not in the roadmap is an error. An id in the roadmap is reported
pub fn unmet_externals<S: BuildHasher>(
    tasks: &[Task],
    roadmap: &Roadmap,
    known_ids: &HashSet<String, S>,
) -> Result<Vec<UnmetExternal>, PlanError> {
    let selected: HashSet<&str> = tasks.iter().map(|task| task.id.as_str()).collect();
    let roadmap_ids: HashSet<&str> = roadmap.tasks.iter().map(|task| task.id.as_str()).collect();
    let mut unmet = Vec::new();
    let mut errors = Vec::new();
    for task in tasks {
        for dep in &task.depends_on {
            if selected.contains(dep.as_str()) || known_ids.contains(dep) {
                continue;
            }
            if roadmap_ids.contains(dep.as_str()) {
                unmet.push(UnmetExternal {
                    task_id: task.id.clone(),
                    blocking_task_id: dep.clone(),
                });
            } else {
                errors.push(format!("{} depends on unknown task {dep}", task.id));
            }
        }
    }
    if errors.is_empty() {
        Ok(unmet)
    } else {
        Err(PlanError::new(errors.join("\n")))
    }
}

/// Task ids to omit when `--skip-unmet` is set, including tasks that depend on a skipped task
pub fn skip_unmet_ids(tasks: &[Task], unmet: &[UnmetExternal]) -> HashSet<String> {
    let mut skip: HashSet<String> = unmet.iter().map(|item| item.task_id.clone()).collect();
    loop {
        let mut added = false;
        for task in tasks {
            if skip.contains(&task.id) {
                continue;
            }
            if task.depends_on.iter().any(|dep| skip.contains(dep)) {
                skip.insert(task.id.clone());
                added = true;
            }
        }
        if !added {
            break;
        }
    }
    skip
}

pub fn blocked_external_ids<'a>(unmet: &'a [UnmetExternal], task_id: &str) -> Vec<&'a str> {
    unmet
        .iter()
        .filter(|item| item.task_id == task_id)
        .map(|item| item.blocking_task_id.as_str())
        .collect()
}

pub fn diff_links<S: BuildHasher>(
    tasks: &[Task],
    bindings: &[Binding],
    current: &HashMap<u64, Vec<DepRef>, S>,
) -> Result<Vec<LinkEdit>, PlanError> {
    let mut edits = Vec::new();
    let mut errors = Vec::new();
    let managed: HashSet<u64> = bindings.iter().filter_map(|item| item.number).collect();
    for task in tasks {
        let Some(binding) = bindings.iter().find(|item| item.task_id == task.id) else {
            errors.push(format!("{} has no issue binding", task.id));
            continue;
        };
        let blocked = binding
            .number
            .and_then(|number| current.get(&number))
            .cloned()
            .unwrap_or_default();
        let mut desired_numbers = Vec::new();
        for dep_id in &task.depends_on {
            let Some(dep) = bindings.iter().find(|item| item.task_id == dep_id.as_str()) else {
                continue;
            };
            if let Some(number) = dep.number {
                desired_numbers.push(number);
            }
            let already = dep
                .number
                .is_some_and(|number| blocked.iter().any(|item| item.number == number));
            if !already {
                edits.push(LinkEdit::Add {
                    task_id: task.id.clone(),
                    issue_number: binding.number,
                    blocking_task_id: dep_id.clone(),
                    blocking_number: dep.number,
                    blocking_database_id: dep.database_id,
                });
            }
        }
        if let Some(issue_number) = binding.number {
            for item in &blocked {
                // Only drop links to other plan issues, a link a person added by hand stays
                if desired_numbers.contains(&item.number) || !managed.contains(&item.number) {
                    continue;
                }
                edits.push(LinkEdit::Remove {
                    task_id: task.id.clone(),
                    issue_number,
                    blocking_number: item.number,
                    blocking_database_id: item.database_id,
                });
            }
        }
    }
    if errors.is_empty() {
        Ok(edits)
    } else {
        Err(PlanError::new(errors.join("\n")))
    }
}

pub fn render(diff: &PlanDiff, links: &[LinkEdit], dry_run: bool) -> String {
    render_report(diff, links, &[], &[], dry_run)
}

pub fn render_report(
    diff: &PlanDiff,
    links: &[LinkEdit],
    unmet: &[UnmetExternal],
    skipped: &[String],
    dry_run: bool,
) -> String {
    let mut out = render_tasks(diff);
    out.push_str(&render_unmet(unmet, skipped));
    out.push_str(&render_links(links));
    out.push_str(&render_summary(diff, links, dry_run));
    out
}

fn render_unmet(unmet: &[UnmetExternal], skipped: &[String]) -> String {
    let skip: HashSet<&str> = skipped.iter().map(String::as_str).collect();
    let mut out = String::new();
    for id in skipped {
        let deps = blocked_external_ids(unmet, id);
        if deps.is_empty() {
            let _ = writeln!(out, "skip {id}");
        } else {
            let _ = writeln!(
                out,
                "skip {id} depends on {}, which has no issue yet",
                deps.join(", ")
            );
        }
    }
    let mut seen = HashSet::new();
    for item in unmet {
        if skip.contains(item.task_id.as_str()) || !seen.insert(item.task_id.as_str()) {
            continue;
        }
        let deps = blocked_external_ids(unmet, &item.task_id);
        let _ = writeln!(
            out,
            "blocked-external {} depends on {}, which has no issue yet",
            item.task_id,
            deps.join(", ")
        );
    }
    out
}

pub fn render_outline(roadmap: &Roadmap) -> String {
    let mut out = String::new();
    for milestone in &roadmap.milestones {
        let _ = writeln!(out, "{} {}", milestone.id, milestone.title);
        for task in roadmap
            .tasks
            .iter()
            .filter(|task| task.milestone_id == milestone.id)
        {
            let human = if task.owner_human { " [human]" } else { "" };
            let deps = if task.depends_on.is_empty() {
                "none".to_string()
            } else {
                task.depends_on.join(", ")
            };
            let _ = writeln!(out, "  {} {}{human}", task.id, task.title);
            let _ = writeln!(out, "    depends on: {deps}");
        }
        out.push('\n');
    }
    out
}

pub fn render_tasks(diff: &PlanDiff) -> String {
    let mut out = String::new();
    for plan in &diff.tasks {
        let human = if plan.task.owner_human {
            " [human]"
        } else {
            ""
        };
        let deps = if plan.task.depends_on.is_empty() {
            "none".to_string()
        } else {
            plan.task.depends_on.join(", ")
        };
        let _ = writeln!(
            out,
            "{kind} {title}{human}\n  depends on: {deps}",
            kind = kind_word(plan.kind),
            title = issue_title(&plan.task),
        );
    }
    for removed in &diff.removed {
        let _ = writeln!(
            out,
            "removed {} issue #{} ({}), not closed",
            removed.task_id, removed.number, removed.state
        );
    }
    out
}

pub fn render_links(links: &[LinkEdit]) -> String {
    let mut out = String::new();
    for link in links {
        match link {
            LinkEdit::Add {
                task_id,
                issue_number,
                blocking_task_id,
                blocking_number,
                ..
            } => {
                let _ = writeln!(
                    out,
                    "link {task_id}{issue} blocked by {blocking_task_id}{blocking}",
                    issue = number_suffix(*issue_number),
                    blocking = number_suffix(*blocking_number),
                );
            }
            LinkEdit::Remove {
                task_id,
                blocking_number,
                ..
            } => {
                let _ = writeln!(out, "unlink {task_id} blocked by #{blocking_number}");
            }
        }
    }
    out
}

pub fn render_summary(diff: &PlanDiff, links: &[LinkEdit], dry_run: bool) -> String {
    let creates = diff
        .tasks
        .iter()
        .filter(|plan| plan.kind == ChangeKind::Create)
        .count();
    let updates = diff
        .tasks
        .iter()
        .filter(|plan| plan.kind == ChangeKind::Update)
        .count();
    let unchanged = diff
        .tasks
        .iter()
        .filter(|plan| plan.kind == ChangeKind::Unchanged)
        .count();
    let prefix = if dry_run { "dry run: " } else { "" };
    format!(
        "{prefix}create {creates}, update {updates}, unchanged {unchanged}, link {}, removed {}\n",
        links.len(),
        diff.removed.len()
    )
}

pub fn github_anchor(heading: &str) -> String {
    let mut out = String::new();
    let mut prev_hyphen = false;
    for ch in heading.chars() {
        if ch.is_whitespace() {
            if !out.is_empty() && !prev_hyphen {
                out.push('-');
                prev_hyphen = true;
            }
            continue;
        }
        if ch == '-' || ch == '_' {
            if !out.is_empty() && !prev_hyphen {
                out.push(ch);
                prev_hyphen = true;
            }
            continue;
        }
        if ch.is_alphanumeric() {
            for lower in ch.to_lowercase() {
                out.push(lower);
            }
            prev_hyphen = false;
        }
    }
    while out.ends_with('-') || out.ends_with('_') {
        out.pop();
    }
    out
}

fn encode_git_path(path: &str) -> String {
    path.split('/')
        .map(|segment| urlencoding::encode(segment).into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn kind_word(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Create => "create",
        ChangeKind::Update => "update",
        ChangeKind::Unchanged => "unchanged",
    }
}

fn number_suffix(number: Option<u64>) -> String {
    match number {
        Some(number) if number > 0 => format!(" (#{number})"),
        _ => String::new(),
    }
}

fn normalize_body(body: &str) -> String {
    body.replace("\r\n", "\n").trim().to_string()
}

fn same_labels(left: &[String], right: &[String]) -> bool {
    let left: HashSet<&str> = left.iter().map(String::as_str).collect();
    let right: HashSet<&str> = right.iter().map(String::as_str).collect();
    left == right
}

fn task_milestone_id(task_id: &str) -> Option<&str> {
    if !is_task_id(task_id) {
        return None;
    }
    task_id.split_once('-').map(|(prefix, _)| prefix)
}

fn push_task(
    task: Task,
    tasks: &mut Vec<Task>,
    seen: &mut HashSet<String>,
    errors: &mut Vec<String>,
) {
    if !seen.insert(task.id.clone()) {
        errors.push(format!("duplicate task id {}", task.id));
        return;
    }
    tasks.push(task);
}

fn flush_draft(
    draft: &mut Option<Draft>,
    tasks: &mut Vec<Task>,
    seen: &mut HashSet<String>,
    errors: &mut Vec<String>,
) {
    let Some(draft) = draft.take() else {
        return;
    };
    match draft.into_task() {
        Ok(task) => push_task(task, tasks, seen, errors),
        Err(err) => errors.push(err),
    }
}

fn check_dependencies(tasks: &[Task], errors: &mut Vec<String>) {
    let ids: HashSet<&str> = tasks.iter().map(|task| task.id.as_str()).collect();
    for task in tasks {
        for dep in &task.depends_on {
            if dep == &task.id {
                errors.push(format!("{} depends on itself", task.id));
            } else if !ids.contains(dep.as_str()) {
                errors.push(format!("{} depends on unknown task {dep}", task.id));
            }
        }
    }
}

impl Draft {
    /// Add `line` to the acceptance list while an `Acceptance:` field is open
    fn take_acceptance_line(&mut self, line: &str) {
        if self.in_acceptance && is_acceptance_line(line) {
            self.acceptance.push(unindent(line));
        }
    }

    fn apply_field(&mut self, name: &str, value: &str) {
        if name.eq_ignore_ascii_case("goal") {
            self.in_acceptance = false;
            self.goal = value.to_string();
        } else if name.eq_ignore_ascii_case("scope") {
            self.in_acceptance = false;
            self.scope = value.to_string();
        } else if name.eq_ignore_ascii_case("acceptance") {
            self.in_acceptance = true;
            if !value.is_empty() {
                self.acceptance.push(acceptance_item(value));
            }
        } else if name.eq_ignore_ascii_case("depends on") {
            self.in_acceptance = false;
            self.depends_raw = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("owner") {
            self.in_acceptance = false;
            self.owner_human = is_human_owner(value);
        }
    }
}

fn acceptance_item(value: &str) -> String {
    if value.starts_with('-') || value.starts_with('*') {
        value.to_string()
    } else {
        format!("- {value}")
    }
}

fn is_acceptance_line(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty()
        && (line.starts_with(' ')
            || line.starts_with('\t')
            || trimmed.starts_with("- ")
            || trimmed.starts_with("* "))
}

fn unindent(line: &str) -> String {
    let line = line.trim_end();
    line.strip_prefix("  ")
        .or_else(|| line.strip_prefix('\t'))
        .unwrap_or(line)
        .to_string()
}

fn top_level_field(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix("- ")?;
    let (name, value) = rest.split_once(':')?;
    let name = name.trim();
    is_known_field(name).then_some((name, value.trim()))
}

fn is_known_field(name: &str) -> bool {
    name.eq_ignore_ascii_case("goal")
        || name.eq_ignore_ascii_case("scope")
        || name.eq_ignore_ascii_case("acceptance")
        || name.eq_ignore_ascii_case("depends on")
        || name.eq_ignore_ascii_case("owner")
}

fn is_human_owner(value: &str) -> bool {
    let Some(word) = value.split_whitespace().next() else {
        return false;
    };
    word.trim_matches(|ch: char| matches!(ch, '.' | ',' | ';' | ':'))
        .eq_ignore_ascii_case("human")
}

fn parse_depends(raw: &str) -> Result<Vec<String>, String> {
    let trimmed = raw.trim().trim_end_matches('.').trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let first = trimmed.split_whitespace().next().unwrap_or("");
    let first = first.trim_matches(|ch: char| matches!(ch, '.' | ',' | ';' | ':'));
    // A leading none is no dependency, even when the rest of the line names a task
    if first.eq_ignore_ascii_case("none") {
        return Ok(Vec::new());
    }
    let ids = extract_ids(trimmed);
    if ids.is_empty() {
        Err(format!("depends on value \"{trimmed}\" has no task id"))
    } else {
        Ok(ids)
    }
}

fn extract_ids(raw: &str) -> Vec<String> {
    let bytes = raw.as_bytes();
    let mut ids = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_alphabetic() {
            let start = index;
            index += 1;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'-')
            {
                index += 1;
            }
            let token = &raw[start..index];
            if is_task_id(token) && !ids.iter().any(|id| id == token) {
                ids.push(token.to_string());
            }
            continue;
        }
        index += 1;
    }
    ids
}

fn is_task_id(token: &str) -> bool {
    if is_ci_bootstrap_id(token) {
        return true;
    }
    let Some((milestone, rest)) = token.split_once('-') else {
        return false;
    };
    if !is_milestone_id(milestone) || rest.is_empty() || !rest.as_bytes()[0].is_ascii_digit() {
        return false;
    }
    rest.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn is_milestone_id(token: &str) -> bool {
    let mut chars = token.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    let mut saw_digit = false;
    for ch in chars {
        if ch.is_ascii_digit() {
            saw_digit = true;
        } else if !ch.is_ascii_alphabetic() {
            return false;
        }
    }
    saw_digit
}

pub(crate) fn is_milestone_heading(line: &str) -> bool {
    line.trim_start()
        .strip_prefix("## ")
        .and_then(parse_milestone_heading)
        .is_some()
}

fn parse_milestone_heading(rest: &str) -> Option<(String, String)> {
    let rest = rest.trim();
    let (first, title) = rest.split_once(char::is_whitespace)?;
    let id = first.trim_end_matches('.');
    is_milestone_id(id).then(|| (id.to_string(), title.trim().to_string()))
}

fn parse_task_heading(rest: &str) -> Option<(String, String)> {
    let rest = rest.trim();
    let (id, title) = match rest.split_once(char::is_whitespace) {
        Some((id, title)) => (id, title.trim()),
        None => (rest, ""),
    };
    is_task_id(id).then(|| (id.to_string(), title.to_string()))
}

fn table_cols(line: &str) -> Option<(TableCols, Vec<String>)> {
    if !line.trim().starts_with('|') {
        return None;
    }
    let cells = split_cells(line);
    let names: Vec<String> = cells.iter().map(|cell| cell.to_ascii_lowercase()).collect();
    let task = names.iter().position(|name| name == "task")?;
    if !names.iter().any(|name| name == "depends on") {
        return None;
    }
    let unknown = cells
        .iter()
        .zip(names.iter())
        .filter(|(_, name)| !name.is_empty() && !is_known_table_column(name))
        .map(|(cell, _)| cell.clone())
        .collect();
    let pos = |label: &str| names.iter().position(|name| name == label);
    Some((
        TableCols {
            task,
            goal: pos("goal"),
            scope: pos("scope"),
            acceptance: pos("acceptance"),
            depends: pos("depends on"),
            owner: pos("owner"),
        },
        unknown,
    ))
}

fn is_known_table_column(name: &str) -> bool {
    name.eq_ignore_ascii_case("task") || is_known_field(name)
}

fn is_separator_row(line: &str) -> bool {
    let cells = split_cells(line);
    !cells.is_empty()
        && cells.iter().all(|cell| {
            let token = cell.trim_matches(':');
            !token.is_empty() && token.chars().all(|ch| ch == '-')
        })
}

fn split_cells(line: &str) -> Vec<String> {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(|cell| cell.trim().to_string())
        .collect()
}

fn cell(cells: &[String], index: Option<usize>) -> &str {
    index
        .and_then(|index| cells.get(index))
        .map_or("", String::as_str)
}

fn task_from_row(
    line: &str,
    cols: &TableCols,
    milestone: &Milestone,
    line_no: usize,
) -> Result<Option<Task>, String> {
    let cells = split_cells(line);
    let task_cell = cell(&cells, Some(cols.task));
    let Some((id, title)) = parse_task_heading(task_cell) else {
        return Ok(None);
    };
    if title.is_empty() {
        return Err(format!("line {line_no}: {id} has no title"));
    }
    let depends_raw = cell(&cells, cols.depends);
    let depends_on =
        parse_depends(depends_raw).map_err(|err| format!("line {line_no}: {id} {err}"))?;
    let owner = cell(&cells, cols.owner);
    Ok(Some(Task {
        id,
        title,
        milestone_id: milestone.id.clone(),
        milestone_title: milestone.title.clone(),
        milestone_heading: milestone.heading.clone(),
        anchor_heading: String::new(),
        goal: cell(&cells, cols.goal).to_string(),
        scope: cell(&cells, cols.scope).to_string(),
        acceptance: {
            let value = cell(&cells, cols.acceptance);
            if value.is_empty() {
                Vec::new()
            } else {
                vec![acceptance_item(value)]
            }
        },
        depends_on,
        owner_human: is_human_owner(owner),
    }))
}

impl Draft {
    fn new(
        id: String,
        title: String,
        milestone: &Milestone,
        anchor_heading: &str,
        line: usize,
    ) -> Self {
        Self {
            id,
            title,
            milestone_id: milestone.id.clone(),
            milestone_title: milestone.title.clone(),
            milestone_heading: milestone.heading.clone(),
            anchor_heading: anchor_heading.to_string(),
            goal: String::new(),
            scope: String::new(),
            acceptance: Vec::new(),
            depends_raw: None,
            owner_human: false,
            in_acceptance: false,
            line,
        }
    }

    fn into_task(self) -> Result<Task, String> {
        let depends_on = match self.depends_raw {
            Some(raw) => parse_depends(&raw)
                .map_err(|err| format!("line {}: {} {err}", self.line, self.id))?,
            None => Vec::new(),
        };
        Ok(Task {
            id: self.id,
            title: self.title,
            milestone_id: self.milestone_id,
            milestone_title: self.milestone_title,
            milestone_heading: self.milestone_heading,
            anchor_heading: self.anchor_heading,
            goal: self.goal,
            scope: self.scope,
            acceptance: self.acceptance,
            depends_on,
            owner_human: self.owner_human,
        })
    }
}

#[cfg(test)]
mod tests;
