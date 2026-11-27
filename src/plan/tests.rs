use super::*;

const SAMPLE: &str = r"
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
";

fn roadmap() -> Roadmap {
    parse_roadmap(SAMPLE).unwrap()
}

fn task<'a>(roadmap: &'a Roadmap, id: &str) -> &'a Task {
    roadmap.tasks.iter().find(|task| task.id == id).unwrap()
}

const SUFFIXED: &str = "## M2S. API security\n\n### M2S-01 Rate limits\n\n- Goal: limit.\n\n## M3. Billing\n\n### M3-01 Invoices\n\n- Goal: bill.\n- Depends on: M9-01\n";

#[test]
fn milestone_ids_may_end_in_letters() {
    let (roadmap, _) =
        parse_roadmap_for(&SUFFIXED.replace("M9-01", "M2S-01"), Some("M2S")).unwrap();
    let selected = select(&roadmap, Some("M2S")).unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].id, "M2S-01");
}

#[test]
fn other_milestone_errors_are_warnings_when_one_is_selected() {
    let err = parse_roadmap_for(SUFFIXED, Some("M3")).unwrap_err();
    assert!(err.message.contains("M3-01 depends on unknown task M9-01"));
    let broken = SUFFIXED
        .replace("- Goal: limit.\n", "- Depends on: nothing\n")
        .replace("- Depends on: M9-01\n", "");
    assert!(parse_roadmap(&broken).is_err());
    let (roadmap, warnings) = parse_roadmap_for(&broken, Some("M3")).unwrap();
    assert_eq!(select(&roadmap, Some("M3")).unwrap().len(), 1);
    assert_eq!(warnings.len(), 1);
}

#[test]
fn milestone_heading_needs_an_id_and_a_title() {
    assert!(is_milestone_heading("## M1. Workspace"));
    assert!(is_milestone_heading("  ## M2S. API"));
    assert!(!is_milestone_heading("## Notes"));
    assert!(!is_milestone_heading("### M1-01 Task"));
    assert!(!is_milestone_heading("## M1"));
}

#[test]
fn outline_lists_milestones_tasks_and_dependencies() {
    let outline = render_outline(&roadmap());
    assert!(
        outline.contains("M1 Content model and pack format"),
        "{outline}"
    );
    assert!(
        outline.contains("  M1-01 Set up the workspace\n    depends on: none"),
        "{outline}"
    );
    assert!(
        outline.contains("  M1-02 Content model types\n    depends on: M1-01"),
        "{outline}"
    );
    assert!(
        outline.contains("  M5-02 Privacy policy [human]\n    depends on: M1-05"),
        "{outline}"
    );
    assert!(
        outline.contains("M6 Full year and plan editor"),
        "{outline}"
    );
    assert!(
        outline.contains("  M6-01 Units 4 to 10\n    depends on: M1-05, M1-01"),
        "{outline}"
    );
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
    assert!(first.depends_on.is_empty(), "{:?}", first.depends_on);
    assert_eq!(first.goal, "a Rust workspace that builds.");
    assert_eq!(first.scope, "the `api/` workspace.");

    let scheduler = task(&roadmap, "M1-05");
    assert!(
        scheduler.depends_on.is_empty(),
        "{:?}",
        scheduler.depends_on
    );
    assert_eq!(
        scheduler.acceptance,
        vec!["- At least 15 fixtures.".to_string()]
    );
}

#[test]
fn none_with_trailing_prose_adds_no_dependency() {
    let roadmap = roadmap();
    assert_eq!(task(&roadmap, "M1-04").depends_on, [] as [String; 0]);
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
    assert_eq!(task(&roadmap, "M6-01").anchor_heading, "");
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
fn unknown_table_column_is_an_error() {
    let markdown = r"
## M3. Session

| Task | Title | Depends on |
|---|---|---|
| M3-01 Session backfill | ignored | none |
";
    let err = parse_roadmap(markdown).unwrap_err();
    assert!(err.message.contains("Title"), "{}", err.message);
    assert!(err.message.contains("line 4"), "{}", err.message);
    assert!(
        err.message.contains("unknown table column"),
        "{}",
        err.message
    );
}

#[test]
fn table_row_with_only_an_id_has_no_title() {
    let markdown = r"
## M3. Session

| Task | Depends on |
|---|---|
| M3-01 | none |
";
    let err = parse_roadmap(markdown).unwrap_err();
    assert!(err.message.contains("M3-01"), "{}", err.message);
    assert!(err.message.contains("no title"), "{}", err.message);
    assert!(err.message.contains("line 6"), "{}", err.message);
}

#[test]
fn heading_task_with_no_title_is_an_error() {
    let markdown = r"
## M3. Session

### M3-01

- Goal: backfill.
- Depends on: none.
";
    let err = parse_roadmap(markdown).unwrap_err();
    assert!(err.message.contains("M3-01"), "{}", err.message);
    assert!(err.message.contains("no title"), "{}", err.message);
    assert!(err.message.contains("line 4"), "{}", err.message);
}

#[test]
fn table_owner_column_keeps_title_in_the_task_cell() {
    let markdown = r"
## M3. Session

| Task | Owner | Depends on |
|---|---|---|
| M3-01 Session backfill | human | none |
| M3-02 Follow up | | M3-01 |
";
    let roadmap = parse_roadmap(markdown).unwrap();
    let first = task(&roadmap, "M3-01");
    assert_eq!(first.title, "Session backfill");
    assert!(first.owner_human);
    assert!(first.depends_on.is_empty(), "{:?}", first.depends_on);
    let second = task(&roadmap, "M3-02");
    assert_eq!(second.title, "Follow up");
    assert!(!second.owner_human);
    assert_eq!(second.depends_on, vec!["M3-01".to_string()]);
}

#[test]
fn depends_on_without_a_task_id_is_an_error() {
    let markdown = r"
## M1. Content model

### M1-01 First

- Goal: one.
- Depends on: later today.
";
    let err = parse_roadmap(markdown).unwrap_err();
    assert!(err.message.contains("no task id"), "{}", err.message);
}

#[test]
fn unknown_dependency_is_an_error() {
    let markdown = r"
## M1. Content model

### M1-01 First

- Goal: one.
- Acceptance:
  - done.
- Depends on: M1-99.
";
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
    assert!(
        diff.tasks
            .iter()
            .all(|plan| plan.kind == ChangeKind::Unchanged)
    );
    assert!(diff.removed.is_empty(), "{:?}", diff.removed);
    let current = current_links(&roadmap);
    let links = diff_links(&roadmap.tasks, &bindings_from_existing(&existing), &current).unwrap();
    assert!(links.is_empty(), "{links:?}");
    let text = render(&diff, &links, false);
    assert!(text.contains("unchanged M1-02 Content model types"));
    assert_eq!(
        text.lines()
            .filter(|line| line.starts_with("unchanged "))
            .count(),
        8
    );
    assert!(
        !text
            .lines()
            .any(|line| line.starts_with("link ") || line.starts_with("unlink "))
    );
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
    assert!(
        diff.tasks[1..]
            .iter()
            .all(|plan| plan.kind == ChangeKind::Unchanged)
    );
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
fn milestone_import_links_an_existing_external_issue() {
    let roadmap = roadmap();
    let existing: Vec<ExistingIssue> = existing_for(&roadmap)
        .into_iter()
        .filter(|issue| issue.task_id.starts_with("M1-"))
        .collect();
    let known: HashSet<String> = existing.iter().map(|issue| issue.task_id.clone()).collect();
    let selected: Vec<Task> = select(&roadmap, Some("M6"))
        .unwrap()
        .into_iter()
        .cloned()
        .collect();
    assert_eq!(
        unmet_externals(&selected, &roadmap, &known).unwrap(),
        [] as [UnmetExternal; 0]
    );

    let diff = diff_import(&roadmap, &existing, Some("M6"), |task| {
        issue_body(task, "acme/app", "main", "docs/ROADMAP.md")
    })
    .unwrap();
    let links = diff_links(
        &selected,
        &bindings_with_existing(&diff, &existing),
        &HashMap::new(),
    )
    .unwrap();
    assert!(links.iter().any(|edit| matches!(
        edit,
        LinkEdit::Add { task_id, blocking_task_id, blocking_number: Some(5), .. }
            if task_id == "M6-01" && blocking_task_id == "M1-05"
    )));
    assert!(links.iter().any(|edit| matches!(
        edit,
        LinkEdit::Add { task_id, blocking_task_id, .. }
            if task_id == "M6-01" && blocking_task_id == "M1-01"
    )));
    assert!(links.iter().any(|edit| matches!(
        edit,
        LinkEdit::Add { task_id, blocking_task_id, .. }
            if task_id == "M6-02" && blocking_task_id == "M6-01"
    )));
}

#[test]
fn milestone_import_keeps_going_when_an_external_blocker_has_no_issue() {
    let roadmap = roadmap();
    let selected: Vec<Task> = select(&roadmap, Some("M6"))
        .unwrap()
        .into_iter()
        .cloned()
        .collect();
    let unmet = unmet_externals(&selected, &roadmap, &HashSet::new()).unwrap();
    assert!(
        unmet
            .iter()
            .any(|item| item.task_id == "M6-01" && item.blocking_task_id == "M1-05"),
        "{unmet:?}"
    );
    assert!(
        unmet
            .iter()
            .any(|item| item.task_id == "M6-01" && item.blocking_task_id == "M1-01"),
        "{unmet:?}"
    );
    let extras: Vec<String> = blocked_external_ids(&unmet, "M6-01")
        .into_iter()
        .map(str::to_string)
        .collect();
    let body = issue_body_with(
        task(&roadmap, "M6-01"),
        "acme/app",
        "main",
        "docs/ROADMAP.md",
        &extras,
    );
    assert!(body.contains("blocked-external: M1-05, M1-01 (no issue yet)"));
    assert!(body.contains("<!-- orch-blocked-external: M1-05, M1-01 -->"));
    let diff = diff_import(&roadmap, &[], Some("M6"), |task| {
        let extra: Vec<String> = blocked_external_ids(&unmet, &task.id)
            .into_iter()
            .map(str::to_string)
            .collect();
        issue_body_with(task, "acme/app", "main", "docs/ROADMAP.md", &extra)
    })
    .unwrap();
    assert_eq!(diff.tasks.len(), 2);
    assert!(
        diff.tasks
            .iter()
            .all(|plan| plan.kind == ChangeKind::Create)
    );
    let links = diff_links(&selected, &bindings_for(&diff), &HashMap::new()).unwrap();
    assert!(
        !links.iter().any(|edit| matches!(
            edit,
            LinkEdit::Add { blocking_task_id, .. }
                if blocking_task_id == "M1-05" || blocking_task_id == "M1-01"
        )),
        "an unmet external blocker must not be linked: {links:?}"
    );
    assert!(links.iter().any(|edit| matches!(
        edit,
        LinkEdit::Add { task_id, blocking_task_id, .. }
            if task_id == "M6-02" && blocking_task_id == "M6-01"
    )));
    let text = render_report(&diff, &links, &unmet, &[], true);
    assert!(
        text.contains("blocked-external M6-01 depends on M1-05, M1-01, which has no issue yet")
    );
    let mut known = HashSet::new();
    known.insert("M1-05".to_string());
    known.insert("M1-01".to_string());
    assert_eq!(
        unmet_externals(&selected, &roadmap, &known).unwrap(),
        [] as [UnmetExternal; 0]
    );
}

#[test]
fn skip_unmet_omits_the_dependent_and_tasks_that_wait_on_it() {
    let roadmap = roadmap();
    let selected: Vec<Task> = select(&roadmap, Some("M6"))
        .unwrap()
        .into_iter()
        .cloned()
        .collect();
    let unmet = unmet_externals(&selected, &roadmap, &HashSet::new()).unwrap();
    let skip = skip_unmet_ids(&selected, &unmet);
    assert!(skip.contains("M6-01"));
    assert!(skip.contains("M6-02"));
    let text = render_report(
        &PlanDiff {
            tasks: Vec::new(),
            removed: Vec::new(),
        },
        &[],
        &unmet,
        &["M6-01".to_string(), "M6-02".to_string()],
        true,
    );
    assert!(text.contains("skip M6-01 depends on M1-05, M1-01, which has no issue yet"));
    assert!(text.contains("skip M6-02\n"));
    assert!(!text.contains("blocked-external M6-01"));
}

#[test]
fn unmet_externals_rejects_an_unknown_id() {
    let roadmap = roadmap();
    let mut task = task(&roadmap, "M6-01").clone();
    task.depends_on = vec!["M9-99".to_string()];
    let err = unmet_externals(&[task], &roadmap, &HashSet::new()).unwrap_err();
    assert!(
        err.message.contains("unknown task M9-99"),
        "{}",
        err.message
    );
    assert!(err.message.contains("M6-01"), "{}", err.message);
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
fn empty_repo_import_creates_ci_and_blocks_every_other_task() {
    let mut roadmap = roadmap();
    inject_ci_bootstrap(&mut roadmap, false);
    assert_eq!(roadmap.tasks[0].id, CI_BOOTSTRAP_ID);
    assert!(roadmap.tasks[0].depends_on.is_empty());
    for task in &roadmap.tasks[1..] {
        assert_eq!(task.depends_on[0], CI_BOOTSTRAP_ID, "{}", task.id);
    }
    let diff = diff_import(&roadmap, &[], None, |task| {
        issue_body(task, "acme/app", "main", "docs/ROADMAP.md")
    })
    .unwrap();
    assert_eq!(diff.tasks[0].task.id, CI_BOOTSTRAP_ID);
    assert_eq!(diff.tasks[0].kind, ChangeKind::Create);
    let selected: Vec<Task> = diff.tasks.iter().map(|plan| plan.task.clone()).collect();
    let links = diff_links(&selected, &bindings_for(&diff), &HashMap::new()).unwrap();
    for plan in &diff.tasks[1..] {
        assert!(
            links.iter().any(|edit| matches!(
                edit,
                LinkEdit::Add { task_id, blocking_task_id, .. }
                    if task_id == &plan.task.id && blocking_task_id == CI_BOOTSTRAP_ID
            )),
            "{} should be blocked by {CI_BOOTSTRAP_ID}: {links:?}",
            plan.task.id
        );
    }
    let body = issue_body(&diff.tasks[0].task, "acme/app", "main", "docs/ROADMAP.md");
    assert!(is_ci_bootstrap_body(&body));
    assert_eq!(marker_id(&body).as_deref(), Some(CI_BOOTSTRAP_ID));
    assert!(body.contains("GitHub Actions workflow"));
    assert!(!body.contains("shipcheck.toml"));
    assert!(!body.contains("https://github.com/acme/app/blob"));
}

#[test]
fn ci_bootstrap_includes_shipcheck_when_requested() {
    let task = ci_bootstrap_task(true);
    let body = issue_body(&task, "acme/app", "main", "docs/ROADMAP.md");
    assert!(body.contains("shipcheck.toml"));
}

#[test]
fn inject_ci_bootstrap_is_idempotent_and_survives_a_milestone_filter() {
    let mut roadmap = roadmap();
    inject_ci_bootstrap(&mut roadmap, false);
    inject_ci_bootstrap(&mut roadmap, false);
    assert_eq!(
        roadmap
            .tasks
            .iter()
            .filter(|task| is_ci_bootstrap_id(&task.id))
            .count(),
        1
    );
    let selected = select(&roadmap, Some("M1")).unwrap();
    assert_eq!(selected[0].id, CI_BOOTSTRAP_ID);
    assert!(selected.iter().any(|task| task.id == "M1-01"));
    let existing = vec![ExistingIssue {
        task_id: CI_BOOTSTRAP_ID.to_string(),
        number: 9,
        database_id: 90,
        title: issue_title(&roadmap.tasks[0]),
        body: issue_body(&roadmap.tasks[0], "acme/app", "main", "docs/ROADMAP.md"),
        labels: labels_for(&[], false),
        state: "open".to_string(),
    }];
    let diff = diff_import(&roadmap, &existing, Some("M1"), |task| {
        issue_body(task, "acme/app", "main", "docs/ROADMAP.md")
    })
    .unwrap();
    assert!(
        diff.removed.is_empty(),
        "CI-01 must stay when importing another milestone: {:?}",
        diff.removed
    );
    assert_eq!(diff.tasks[0].task.id, CI_BOOTSTRAP_ID);
    assert_eq!(diff.tasks[0].kind, ChangeKind::Unchanged);
}

#[test]
fn ci_01_is_a_task_id() {
    assert!(is_task_id(CI_BOOTSTRAP_ID));
    assert!(is_ci_bootstrap_id("ci-01"));
    assert!(!is_task_id("CI"));
    assert!(needs_ci_bootstrap(false, false));
    assert!(needs_ci_bootstrap(false, true));
    assert!(needs_ci_bootstrap(true, true));
    assert!(!needs_ci_bootstrap(true, false));
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

fn bindings_with_existing(diff: &PlanDiff, existing: &[ExistingIssue]) -> Vec<Binding> {
    let mut bindings = bindings_for(diff);
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
