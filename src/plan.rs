//! Roadmap files and the issue plan `orch plan import` applies.
//!
//! A milestone is `## M1. Title`. A task is `### M1-03 Title` with Goal, Scope,
//! Acceptance, Depends on and Owner lines. Later milestones may be a table with
//! those columns. `none` is no dependency.

use std::collections::{HashMap, HashSet};

pub const PLAN_LABEL: &str = "orch-plan";
pub const HUMAN_LABEL: &str = "owner:human";
pub const NO_AGENT_LABEL: &str = "no-agent";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanError {
    pub message: String,
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PlanError {}

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
    let mut milestones = Vec::new();
    let mut tasks = Vec::new();
    let mut errors = Vec::new();
    let mut current: Option<Milestone> = None;
    let mut draft: Option<Draft> = None;
    let mut table: Option<TableCols> = None;
    let mut seen = HashSet::new();

    for (idx, raw) in markdown.lines().enumerate() {
        let line = raw.trim_end_matches('\r');
        let line_no = idx + 1;

        if let Some(cols) = table {
            if line.trim().starts_with('|') {
                if !is_separator_row(line) {
                    if let Some(milestone) = current.as_ref() {
                        match task_from_row(line, &cols, milestone, line_no) {
                            Ok(Some(task)) => {
                                push_task(task, &mut tasks, &mut seen, &mut errors);
                            }
                            Ok(None) => {}
                            Err(err) => errors.push(err),
                        }
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
                draft = Some(Draft::new(id, title, milestone, rest.trim(), line_no));
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

        if let Some(cols) = table_cols(line) {
            flush_draft(&mut draft, &mut tasks, &mut seen, &mut errors);
            if current.is_some() {
                table = Some(cols);
            }
            continue;
        }

        if let Some(draft) = draft.as_mut() {
            if let Some((name, value)) = top_level_field(line) {
                apply_field(draft, name, value);
                continue;
            }
            if draft.in_acceptance && is_acceptance_line(line) {
                draft.acceptance.push(unindent(line));
            }
        }
    }

    flush_draft(&mut draft, &mut tasks, &mut seen, &mut errors);
    check_dependencies(&tasks, &mut errors);

    if errors.is_empty() {
        Ok(Roadmap { milestones, tasks })
    } else {
        Err(PlanError::new(errors.join("\n")))
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
        .filter(|task| task.milestone_id.eq_ignore_ascii_case(wanted))
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
    let mut out = String::new();
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
    let url = format!(
        "https://github.com/{repo}/blob/{branch}/{path}#{anchor}",
        branch = encode_git_path(branch),
        path = encode_git_path(rel_path),
        anchor = urlencoding::encode(&task_anchor(task)),
    );
    out.push_str(&format!("[{rel_path}]({url})\n\n"));
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
        if selected_ids.contains(issue.task_id.as_str()) {
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

pub fn missing_blockers(tasks: &[Task], known_ids: &HashSet<String>) -> Vec<String> {
    let selected: HashSet<&str> = tasks.iter().map(|task| task.id.as_str()).collect();
    let mut missing = Vec::new();
    for task in tasks {
        for dep in &task.depends_on {
            if selected.contains(dep.as_str()) || known_ids.contains(dep) {
                continue;
            }
            missing.push(format!(
                "{} depends on {dep}, which has no issue yet",
                task.id
            ));
        }
    }
    missing
}

pub fn diff_links(
    tasks: &[Task],
    bindings: &[Binding],
    current: &HashMap<u64, Vec<DepRef>>,
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
                errors.push(format!(
                    "{} depends on {dep_id}, which has no issue yet",
                    task.id
                ));
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
    let mut out = render_tasks(diff);
    out.push_str(&render_links(links));
    out.push_str(&render_summary(diff, links, dry_run));
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
        out.push_str(&format!(
            "{kind} {title}{human}\n  depends on: {deps}\n",
            kind = kind_word(plan.kind),
            title = issue_title(&plan.task),
        ));
    }
    for removed in &diff.removed {
        out.push_str(&format!(
            "removed {} issue #{} ({}), not closed\n",
            removed.task_id, removed.number, removed.state
        ));
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
                out.push_str(&format!(
                    "link {task_id}{issue} blocked by {blocking_task_id}{blocking}\n",
                    issue = number_suffix(*issue_number),
                    blocking = number_suffix(*blocking_number),
                ));
            }
            LinkEdit::Remove {
                task_id,
                blocking_number,
                ..
            } => {
                out.push_str(&format!("unlink {task_id} blocked by #{blocking_number}\n"));
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

fn apply_field(draft: &mut Draft, name: &str, value: &str) {
    if name.eq_ignore_ascii_case("goal") {
        draft.in_acceptance = false;
        draft.goal = value.to_string();
    } else if name.eq_ignore_ascii_case("scope") {
        draft.in_acceptance = false;
        draft.scope = value.to_string();
    } else if name.eq_ignore_ascii_case("acceptance") {
        draft.in_acceptance = true;
        if !value.is_empty() {
            draft.acceptance.push(acceptance_item(value));
        }
    } else if name.eq_ignore_ascii_case("depends on") {
        draft.in_acceptance = false;
        draft.depends_raw = Some(value.to_string());
    } else if name.eq_ignore_ascii_case("owner") {
        draft.in_acceptance = false;
        draft.owner_human = is_human_owner(value);
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
        } else if ch.is_ascii_alphabetic() && !saw_digit {
            continue;
        } else {
            return false;
        }
    }
    saw_digit
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

fn table_cols(line: &str) -> Option<TableCols> {
    if !line.trim().starts_with('|') {
        return None;
    }
    let cells = split_cells(line);
    let names: Vec<String> = cells.iter().map(|cell| cell.to_ascii_lowercase()).collect();
    let task = names.iter().position(|name| name == "task")?;
    if !names.iter().any(|name| name == "depends on") {
        return None;
    }
    let pos = |label: &str| names.iter().position(|name| name == label);
    Some(TableCols {
        task,
        goal: pos("goal"),
        scope: pos("scope"),
        acceptance: pos("acceptance"),
        depends: pos("depends on"),
        owner: pos("owner"),
    })
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
        .map(String::as_str)
        .unwrap_or("")
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
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
## M1. Content model and pack format

### M1-01 Set up the workspace

- Goal: a Rust workspace that builds.
- Scope: the `api/` workspace.
- Acceptance:
  - `cargo test` passes on a clean checkout.
    - including the ignored tests
  - CI fails on a formatting error.
- Depends on: none.

### M1-02 Content model types

- Goal: serde types.
- Scope: crates/content.
- Acceptance:
  - A sample file round-trips.
- Depends on: M1-01.

### M1-03 Content validation

- Owner: human.
- Goal: catch broken content.
- Scope: content-cli validate.
- Acceptance:
  - One test per check.
- Depends on: M1-02.

### M1-04 Festival dates

- Goal: resolve dates.
- Acceptance:
  - Tests for 2026.
- Depends on: none. Do it before M1-05.

### M1-05 Scheduler

- Goal: the guardrail.
- Scope: resolve().
- Acceptance:
  - At least 15 fixtures.

## M5. Release

### M5-02 Privacy policy

- Owner: human (counsel), with an agent draft.
- Goal: legal pages.
- Acceptance:
  - Counsel signs off.
- Depends on: M1-05.

## M6. Full year and plan editor

| Task | Goal | Acceptance | Depends on |
|---|---|---|---|
| M6-01 Units 4 to 10 | Write the rest of the year | Each pack validates | M1-05, M1-01 |
| M6-02 Festival modules | Attach modules | Each shows in Hoje | M6-01 |
"#;

    fn roadmap() -> Roadmap {
        parse_roadmap(SAMPLE).unwrap()
    }

    fn task<'a>(roadmap: &'a Roadmap, id: &str) -> &'a Task {
        roadmap.tasks.iter().find(|task| task.id == id).unwrap()
    }

    #[test]
    fn parses_nested_acceptance_and_missing_depends_on() {
        let roadmap = roadmap();
        let first = task(&roadmap, "M1-01");
        assert_eq!(
            first.acceptance,
            vec![
                "- `cargo test` passes on a clean checkout.".to_string(),
                "  - including the ignored tests".to_string(),
                "- CI fails on a formatting error.".to_string(),
            ]
        );
        assert!(first.depends_on.is_empty());
        assert_eq!(first.goal, "a Rust workspace that builds.");
        assert_eq!(first.scope, "the `api/` workspace.");

        let scheduler = task(&roadmap, "M1-05");
        assert!(scheduler.depends_on.is_empty());
        assert_eq!(
            scheduler.acceptance,
            vec!["- At least 15 fixtures.".to_string()]
        );
    }

    #[test]
    fn none_with_trailing_prose_adds_no_dependency() {
        let roadmap = roadmap();
        assert!(task(&roadmap, "M1-04").depends_on.is_empty());
    }

    #[test]
    fn parses_owner_human_and_table_dependencies() {
        let roadmap = roadmap();
        assert!(task(&roadmap, "M1-03").owner_human);
        assert!(task(&roadmap, "M5-02").owner_human);
        assert!(!task(&roadmap, "M1-01").owner_human);
        assert_eq!(
            task(&roadmap, "M1-02").depends_on,
            vec!["M1-01".to_string()]
        );
        assert_eq!(
            task(&roadmap, "M6-01").depends_on,
            vec!["M1-05".to_string(), "M1-01".to_string()]
        );
        assert_eq!(
            task(&roadmap, "M6-02").depends_on,
            vec!["M6-01".to_string()]
        );
        assert!(task(&roadmap, "M6-01").anchor_heading.is_empty());
        assert_eq!(task(&roadmap, "M6-01").milestone_id, "M6");
    }

    #[test]
    fn labels_keep_status_and_drop_no_agent_when_a_task_is_no_longer_human() {
        let kept = labels_for(
            &[
                "orch-plan".to_string(),
                "owner:human".to_string(),
                "no-agent".to_string(),
                "status:new".to_string(),
            ],
            false,
        );
        assert_eq!(
            kept,
            vec!["orch-plan".to_string(), "status:new".to_string()]
        );

        let human = labels_for(&["status:new".to_string(), "no-agent".to_string()], true);
        assert_eq!(
            human,
            vec![
                "orch-plan".to_string(),
                "owner:human".to_string(),
                "no-agent".to_string(),
                "status:new".to_string(),
            ]
        );
    }

    #[test]
    fn depends_on_without_a_task_id_is_an_error() {
        let markdown = r#"
## M1. Content model

### M1-01 First

- Goal: one.
- Depends on: later today.
"#;
        let err = parse_roadmap(markdown).unwrap_err();
        assert!(err.message.contains("no task id"), "{}", err.message);
    }

    #[test]
    fn unknown_dependency_is_an_error() {
        let markdown = r#"
## M1. Content model

### M1-01 First

- Goal: one.
- Acceptance:
  - done.
- Depends on: M1-99.
"#;
        let err = parse_roadmap(markdown).unwrap_err();
        assert!(
            err.message.contains("unknown task M1-99"),
            "{}",
            err.message
        );
        assert!(err.message.contains("M1-01"), "{}", err.message);
    }

    #[test]
    fn issue_body_links_the_heading_and_carries_the_marker() {
        let roadmap = roadmap();
        let task = task(&roadmap, "M1-01");
        let body = issue_body(task, "acme/app", "main", "docs/ROADMAP.md");
        assert!(body.contains("## Goal\n\na Rust workspace that builds."));
        assert!(body.contains("## Scope\n"));
        assert!(body.contains("- `cargo test` passes on a clean checkout."));
        assert!(body.contains("  - including the ignored tests"));
        assert!(body.contains("<!-- orch-plan: M1-01 -->"));
        assert!(body.contains(
            "https://github.com/acme/app/blob/main/docs/ROADMAP.md#m1-01-set-up-the-workspace"
        ));
        assert_eq!(marker_id(&body).as_deref(), Some("M1-01"));
        assert_eq!(issue_title(task), "M1-01 Set up the workspace");
    }

    #[test]
    fn table_task_links_to_the_milestone_heading() {
        let roadmap = roadmap();
        let body = issue_body(
            task(&roadmap, "M6-01"),
            "acme/app",
            "main",
            "docs/ROADMAP.md",
        );
        assert!(body.contains("#m6-full-year-and-plan-editor"));
        assert!(!body.contains("## Scope"));
    }

    #[test]
    fn report_lists_dependencies_and_marks_human_tasks() {
        let roadmap = roadmap();
        let diff = diff_import(&roadmap, &[], None, |task| {
            issue_body(task, "acme/app", "main", "docs/ROADMAP.md")
        })
        .unwrap();
        let links = diff_links(&roadmap.tasks, &bindings_for(&diff), &HashMap::new()).unwrap();
        let text = render(&diff, &links, true);
        assert!(text.contains("create M1-01 Set up the workspace\n  depends on: none\n"));
        assert!(text.contains("create M1-03 Content validation [human]\n  depends on: M1-02\n"));
        assert!(text.contains("create M5-02 Privacy policy [human]\n  depends on: M1-05\n"));
        assert!(text.contains("link M6-01 blocked by M1-05\n"));
        assert!(text.contains("link M6-01 blocked by M1-01\n"));
        assert!(text.starts_with("dry run:") || text.contains("\ndry run:"));
        assert!(text.contains("create 8,"));
    }

    #[test]
    fn second_import_with_no_file_change_edits_nothing() {
        let roadmap = roadmap();
        let existing = existing_for(&roadmap);
        let diff = diff_import(&roadmap, &existing, None, |task| {
            issue_body(task, "acme/app", "main", "docs/ROADMAP.md")
        })
        .unwrap();
        assert!(diff
            .tasks
            .iter()
            .all(|plan| plan.kind == ChangeKind::Unchanged));
        assert!(diff.removed.is_empty());
        let current = current_links(&roadmap);
        let links =
            diff_links(&roadmap.tasks, &bindings_from_existing(&existing), &current).unwrap();
        assert!(links.is_empty(), "{links:?}");
        let text = render(&diff, &links, false);
        assert!(text.contains("unchanged M1-02 Content model types"));
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("unchanged "))
                .count(),
            8
        );
        assert!(!text
            .lines()
            .any(|line| line.starts_with("link ") || line.starts_with("unlink ")));
        assert!(text.contains("create 0, update 0, unchanged 8, link 0, removed 0\n"));
    }

    #[test]
    fn changed_body_is_an_update_and_a_dropped_task_is_reported() {
        let roadmap = roadmap();
        let mut existing = existing_for(&roadmap);
        existing[0].body = "stale\n<!-- orch-plan: M1-01 -->\n".to_string();
        existing.push(ExistingIssue {
            task_id: "M1-99".to_string(),
            number: 99,
            database_id: 990,
            title: "M1-99 Gone".to_string(),
            body: marker_for("M1-99"),
            labels: labels_for(&[], false),
            state: "open".to_string(),
        });
        let diff = diff_import(&roadmap, &existing, None, |task| {
            issue_body(task, "acme/app", "main", "docs/ROADMAP.md")
        })
        .unwrap();
        assert_eq!(diff.tasks[0].kind, ChangeKind::Update);
        assert!(diff.tasks[1..]
            .iter()
            .all(|plan| plan.kind == ChangeKind::Unchanged));
        assert_eq!(diff.removed.len(), 1);
        assert_eq!(diff.removed[0].task_id, "M1-99");
        assert!(render_tasks(&diff).contains("removed M1-99 issue #99 (open), not closed"));
    }

    #[test]
    fn marker_without_label_is_an_update_that_adds_the_label() {
        let roadmap = roadmap();
        let mut existing = existing_for(&roadmap);
        existing[0].labels = vec!["status:new".to_string()];
        let diff = diff_import(&roadmap, &existing, None, |task| {
            issue_body(task, "acme/app", "main", "docs/ROADMAP.md")
        })
        .unwrap();
        assert_eq!(diff.tasks[0].kind, ChangeKind::Update);
        assert_eq!(diff.tasks[0].number, Some(1));
        assert!(labels_for(&diff.tasks[0].existing_labels, false).contains(&PLAN_LABEL.to_string()));
    }

    #[test]
    fn milestone_filter_limits_tasks_and_removed() {
        let roadmap = roadmap();
        let mut existing = existing_for(&roadmap);
        existing.push(ExistingIssue {
            task_id: "M6-09".to_string(),
            number: 70,
            database_id: 700,
            title: "M6-09 Gone".to_string(),
            body: marker_for("M6-09"),
            labels: labels_for(&[], false),
            state: "closed".to_string(),
        });
        let diff = diff_import(&roadmap, &existing, Some("M6"), |task| {
            issue_body(task, "acme/app", "main", "docs/ROADMAP.md")
        })
        .unwrap();
        let ids: Vec<&str> = diff
            .tasks
            .iter()
            .map(|plan| plan.task.id.as_str())
            .collect();
        assert_eq!(ids, vec!["M6-01", "M6-02"]);
        assert_eq!(diff.removed.len(), 1);
        assert_eq!(diff.removed[0].task_id, "M6-09");
        let err = select(&roadmap, Some("M9")).unwrap_err();
        assert!(err.message.contains("no milestone M9"));
    }

    #[test]
    fn milestone_import_reports_a_blocker_with_no_issue() {
        let roadmap = roadmap();
        let selected: Vec<Task> = select(&roadmap, Some("M6"))
            .unwrap()
            .into_iter()
            .cloned()
            .collect();
        let missing = missing_blockers(&selected, &HashSet::new());
        assert!(
            missing
                .iter()
                .any(|line| line.contains("M6-01 depends on M1-05")),
            "{missing:?}"
        );
        let mut known = HashSet::new();
        known.insert("M1-05".to_string());
        known.insert("M1-01".to_string());
        assert!(missing_blockers(&selected, &known).is_empty());
    }

    #[test]
    fn link_diff_adds_missing_and_removes_extras_then_is_empty() {
        let roadmap = roadmap();
        let existing = existing_for(&roadmap);
        let bindings = bindings_from_existing(&existing);
        let stale = number_of(&existing, "M1-05");
        let mut current = HashMap::new();
        current.insert(
            number_of(&existing, "M1-02"),
            vec![
                DepRef {
                    number: stale,
                    database_id: stale * 10,
                },
                DepRef {
                    number: 50,
                    database_id: 500,
                },
            ],
        );
        let edits = diff_links(&roadmap.tasks, &bindings, &current).unwrap();
        assert!(edits.iter().any(|edit| matches!(
            edit,
            LinkEdit::Add { task_id, blocking_task_id, .. }
                if task_id == "M1-02" && blocking_task_id == "M1-01"
        )));
        assert!(edits.iter().any(|edit| matches!(
            edit,
            LinkEdit::Remove { task_id, blocking_number, .. }
                if task_id == "M1-02" && *blocking_number == stale
        )));
        assert!(
            !edits.iter().any(|edit| matches!(
                edit,
                LinkEdit::Remove {
                    blocking_number: 50,
                    ..
                }
            )),
            "a blocker outside the plan must stay: {edits:?}"
        );

        let synced = current_links(&roadmap);
        let again = diff_links(&roadmap.tasks, &bindings, &synced).unwrap();
        assert!(again.is_empty(), "{again:?}");
    }

    #[test]
    fn anchor_keeps_letters_and_drops_punctuation() {
        assert_eq!(
            github_anchor("M1. Content model and pack format"),
            "m1-content-model-and-pack-format"
        );
        assert_eq!(
            github_anchor("M3-09 Class screen and Modo áudio"),
            "m3-09-class-screen-and-modo-áudio"
        );
    }

    fn existing_for(roadmap: &Roadmap) -> Vec<ExistingIssue> {
        roadmap
            .tasks
            .iter()
            .enumerate()
            .map(|(index, task)| {
                let number = (index + 1) as u64;
                ExistingIssue {
                    task_id: task.id.clone(),
                    number,
                    database_id: number * 10,
                    title: issue_title(task),
                    body: issue_body(task, "acme/app", "main", "docs/ROADMAP.md"),
                    labels: labels_for(&["status:new".to_string()], task.owner_human),
                    state: "open".to_string(),
                }
            })
            .collect()
    }

    fn bindings_for(diff: &PlanDiff) -> Vec<Binding> {
        diff.tasks
            .iter()
            .map(|plan| Binding {
                task_id: plan.task.id.clone(),
                number: plan.number,
                database_id: plan.database_id,
            })
            .collect()
    }

    fn bindings_from_existing(existing: &[ExistingIssue]) -> Vec<Binding> {
        existing
            .iter()
            .map(|issue| Binding {
                task_id: issue.task_id.clone(),
                number: Some(issue.number),
                database_id: Some(issue.database_id),
            })
            .collect()
    }

    fn number_of(existing: &[ExistingIssue], id: &str) -> u64 {
        existing
            .iter()
            .find(|issue| issue.task_id == id)
            .unwrap()
            .number
    }

    fn current_links(roadmap: &Roadmap) -> HashMap<u64, Vec<DepRef>> {
        let existing = existing_for(roadmap);
        let mut current = HashMap::new();
        for task in &roadmap.tasks {
            let blocked = task
                .depends_on
                .iter()
                .map(|dep| DepRef {
                    number: number_of(&existing, dep),
                    database_id: number_of(&existing, dep) * 10,
                })
                .collect();
            current.insert(number_of(&existing, &task.id), blocked);
        }
        current
    }
}
