use super::super::*;

#[tokio::test]
async fn resolve_task_id_external() {
    let store = TaskStore::open_memory().await.unwrap();

    store
        .create(&NewTask {
            external_id: Some("42".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Issue 42".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let resolved = store.resolve_task_id("owner/repo", "42").await.unwrap();
    assert_eq!(resolved, Some(1));

    let missing = store.resolve_task_id("owner/repo", "999").await.unwrap();
    assert_eq!(missing, None);
}

#[tokio::test]
async fn resolve_task_id_internal_returns_none() {
    let store = TaskStore::open_memory().await.unwrap();
    let resolved = store
        .resolve_task_id("owner/repo", "internal:5")
        .await
        .unwrap();
    assert_eq!(
        resolved, None,
        "internal tasks not yet supported in resolve_task_id"
    );
}

#[tokio::test]
async fn resolve_task_id_for_external_and_internal() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create(&NewTask {
            external_id: Some("123".to_string()),
            repo: "owner/repo".to_string(),
            origin: "github".to_string(),
            title: "Resolve test".to_string(),
            body: String::new(),
            source: String::new(),
            source_id: String::new(),
            author: String::new(),
            url: String::new(),
            labels: vec![],
            parent_id: None,
        })
        .await
        .unwrap();

    // External task resolves
    let resolved = store.resolve_task_id("owner/repo", "123").await.unwrap();
    assert_eq!(resolved, Some(id));

    // Wrong repo returns None
    let resolved = store.resolve_task_id("other/repo", "123").await.unwrap();
    assert_eq!(resolved, None);

    // Internal task ID format returns None (not migrated yet)
    let resolved = store
        .resolve_task_id("owner/repo", "internal:5")
        .await
        .unwrap();
    assert_eq!(resolved, None);

    // Non-existent external ID returns None
    let resolved = store.resolve_task_id("owner/repo", "999").await.unwrap();
    assert_eq!(resolved, None);
}

#[tokio::test]
async fn resolve_task_id_finds_internal_task() {
    let store = TaskStore::open_memory().await.unwrap();

    let id = store
        .create(&NewTask {
            external_id: Some("internal:5".to_string()),
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Internal task 5".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let resolved = store
        .resolve_task_id("owner/repo", "internal:5")
        .await
        .unwrap();
    assert_eq!(resolved, Some(id));
}

#[tokio::test]
async fn resolve_task_id_returns_none_for_unknown_internal() {
    let store = TaskStore::open_memory().await.unwrap();

    // Create a different internal task to make sure the store isn't empty
    store
        .create(&NewTask {
            external_id: Some("internal:1".to_string()),
            repo: "owner/repo".to_string(),
            origin: "internal".to_string(),
            title: "Internal task 1".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let resolved = store
        .resolve_task_id("owner/repo", "internal:999")
        .await
        .unwrap();
    assert_eq!(resolved, None);
}

#[tokio::test]
async fn create_internal_sets_external_id() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create_internal("owner/repo", "Test task", "body", "cron", "job:1", None)
        .await
        .unwrap();

    let task = store.get(id).await.unwrap();
    assert_eq!(task.origin, "internal");
    assert_eq!(task.external_id, Some(format!("internal:{id}")));
    assert_eq!(task.title, "Test task");
    assert_eq!(task.source, "cron");
    assert_eq!(task.source_id, "job:1");
}

#[tokio::test]
async fn create_internal_resolvable_by_task_id() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create_internal("owner/repo", "Resolvable", "body", "cron", "job:2", None)
        .await
        .unwrap();

    let resolved = store
        .resolve_task_id("owner/repo", &format!("internal:{id}"))
        .await
        .unwrap();
    assert_eq!(resolved, Some(id));
}

#[tokio::test]
async fn resolve_task_id_globally_finds_internal_across_repos() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create_internal("other/repo", "Cross-repo task", "body", "cron", "j:1", None)
        .await
        .unwrap();

    // Current-repo scoped lookup misses it.
    let scoped = store
        .resolve_task_id("my/repo", &format!("internal:{id}"))
        .await
        .unwrap();
    assert_eq!(scoped, None);

    // Global lookup finds it.
    let global = store
        .resolve_task_id_globally(&format!("internal:{id}"))
        .await
        .unwrap();
    assert_eq!(global, Some((id, "other/repo".to_string())));
}

#[tokio::test]
async fn resolve_task_id_globally_returns_none_for_unknown() {
    let store = TaskStore::open_memory().await.unwrap();
    let result = store
        .resolve_task_id_globally("internal:999")
        .await
        .unwrap();
    assert_eq!(result, None);
}

#[tokio::test]
async fn resolve_task_id_globally_unique_external_resolves() {
    let store = TaskStore::open_memory().await.unwrap();
    let id = store
        .create(&NewTask {
            external_id: Some("77".to_string()),
            repo: "owner/repo-a".to_string(),
            origin: "github".to_string(),
            title: "Issue 77".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let global = store.resolve_task_id_globally("77").await.unwrap();
    assert_eq!(global, Some((id, "owner/repo-a".to_string())));
}

#[tokio::test]
async fn resolve_task_id_globally_ambiguous_external_errors() {
    let store = TaskStore::open_memory().await.unwrap();
    store
        .create(&NewTask {
            external_id: Some("42".to_string()),
            repo: "owner/repo-a".to_string(),
            origin: "github".to_string(),
            title: "Issue 42 in A".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    store
        .create(&NewTask {
            external_id: Some("42".to_string()),
            repo: "owner/repo-b".to_string(),
            origin: "github".to_string(),
            title: "Issue 42 in B".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();

    let result = store.resolve_task_id_globally("42").await;
    assert!(result.is_err(), "ambiguous ID should return an error");
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("ambiguous"), "error should mention ambiguity");
}

fn issue_task(repo: &str, number: &str, title: &str) -> NewTask {
    NewTask {
        external_id: Some(number.to_string()),
        repo: repo.to_string(),
        origin: "github".to_string(),
        title: title.to_string(),
        ..Default::default()
    }
}

#[test]
fn parse_cli_task_spec_accepts_explicit_and_bare_forms() {
    assert_eq!(
        parse_cli_task_spec("internal:58").unwrap(),
        CliTaskSpec::Internal(58)
    );
    assert_eq!(
        parse_cli_task_spec("owner/repo#58").unwrap(),
        CliTaskSpec::Issue {
            repo: "owner/repo".to_string(),
            number: 58,
        }
    );
    assert_eq!(parse_cli_task_spec("58").unwrap(), CliTaskSpec::Bare(58));
    assert_eq!(parse_cli_task_spec("#58").unwrap(), CliTaskSpec::Bare(58));
    assert!(parse_cli_task_spec("").is_err());
    assert!(parse_cli_task_spec("internal:").is_err());
    assert!(parse_cli_task_spec("0").is_err());
    assert!(parse_cli_task_spec("teach#58").is_err());
}

#[tokio::test]
async fn resolve_cli_id_prefers_current_repo_issue_over_internal_store_id() {
    let store = TaskStore::open_memory().await.unwrap();
    let internal_id = store
        .create_internal(
            "owner/project-a",
            "Code development: orch",
            "body",
            "job",
            "j:1",
            None,
        )
        .await
        .unwrap();
    store
        .update_status(internal_id, TaskStatus::Done)
        .await
        .unwrap();

    let issue_id = store
        .create(&issue_task(
            "owner/project-b",
            &internal_id.to_string(),
            "Teach issue",
        ))
        .await
        .unwrap();
    store
        .update_status(issue_id, TaskStatus::Blocked)
        .await
        .unwrap();

    let resolved = store
        .resolve_cli_id(&internal_id.to_string(), Some("owner/project-b"))
        .await
        .unwrap()
        .expect("current-repo issue should resolve");
    assert_eq!(resolved.store_id, Some(issue_id));
    assert_eq!(resolved.repo, "owner/project-b");
    assert_eq!(resolved.external_id, internal_id.to_string());
    assert!(!resolved.is_internal());

    store
        .update_status(resolved.store_id.unwrap(), TaskStatus::New)
        .await
        .unwrap();
    assert_eq!(store.get(issue_id).await.unwrap().status, TaskStatus::New);
    assert_eq!(
        store.get(internal_id).await.unwrap().status,
        TaskStatus::Done
    );
}

#[tokio::test]
async fn resolve_cli_id_bare_number_outside_a_project_is_ambiguous() {
    let store = TaskStore::open_memory().await.unwrap();
    let internal_id = store
        .create_internal(
            "owner/project-a",
            "Code development: orch",
            "body",
            "job",
            "j:1",
            None,
        )
        .await
        .unwrap();
    store
        .create(&issue_task(
            "owner/project-b",
            &internal_id.to_string(),
            "Teach issue",
        ))
        .await
        .unwrap();

    let result = store.resolve_cli_id(&internal_id.to_string(), None).await;
    assert!(result.is_err(), "two matches outside a project must refuse");
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("matches more than one task"), "{msg}");
    assert!(msg.contains(&format!("internal:{internal_id}")), "{msg}");
    assert!(
        msg.contains(&format!("owner/project-b#{internal_id}")),
        "{msg}"
    );
    assert!(
        msg.contains(&format!(
            "Use owner/project-b#{internal_id} for the GitHub task"
        )),
        "{msg}"
    );
    assert!(
        msg.contains(&format!("or internal:{internal_id} for the internal one")),
        "{msg}"
    );
}

#[tokio::test]
async fn resolve_cli_id_owner_repo_hash_works_from_any_directory() {
    let store = TaskStore::open_memory().await.unwrap();
    let internal_id = store
        .create_internal(
            "owner/project-a",
            "Code development: orch",
            "body",
            "job",
            "j:1",
            None,
        )
        .await
        .unwrap();
    let issue_id = store
        .create(&issue_task(
            "owner/project-b",
            &internal_id.to_string(),
            "Teach issue",
        ))
        .await
        .unwrap();

    let spec = format!("owner/project-b#{internal_id}");
    let resolved = store
        .resolve_cli_id(&spec, Some("owner/other"))
        .await
        .unwrap()
        .expect("owner/repo#N should resolve from any directory");
    assert_eq!(resolved.store_id, Some(issue_id));
    assert_eq!(resolved.repo, "owner/project-b");

    let internal = store
        .resolve_cli_id(&format!("internal:{internal_id}"), Some("owner/project-b"))
        .await
        .unwrap()
        .expect("internal:N should resolve from any directory");
    assert_eq!(internal.store_id, Some(internal_id));
    assert!(internal.is_internal());
}

#[tokio::test]
async fn resolve_cli_id_current_repo_internal_when_no_issue() {
    let store = TaskStore::open_memory().await.unwrap();
    let internal_id = store
        .create_internal(
            "owner/project-a",
            "Internal only",
            "body",
            "job",
            "j:1",
            None,
        )
        .await
        .unwrap();

    let resolved = store
        .resolve_cli_id(&internal_id.to_string(), Some("owner/project-a"))
        .await
        .unwrap()
        .expect("bare number should hit the current project's internal task");
    assert_eq!(resolved.store_id, Some(internal_id));
    assert!(resolved.is_internal());
}

#[tokio::test]
async fn resolve_cli_id_unique_global_match_outside_a_project() {
    let store = TaskStore::open_memory().await.unwrap();
    let issue_id = store
        .create(&issue_task("owner/project-b", "77", "Only issue 77"))
        .await
        .unwrap();

    let resolved = store
        .resolve_cli_id("77", None)
        .await
        .unwrap()
        .expect("unique global match should resolve");
    assert_eq!(resolved.store_id, Some(issue_id));
    assert_eq!(resolved.display_spec(), "owner/project-b#77");
}

#[tokio::test]
async fn resolve_cli_id_in_project_does_not_pick_other_repo_internal() {
    let store = TaskStore::open_memory().await.unwrap();
    let internal_id = store
        .create_internal(
            "owner/project-a",
            "Code development: orch",
            "body",
            "job",
            "j:1",
            None,
        )
        .await
        .unwrap();

    let resolved = store
        .resolve_cli_id(&internal_id.to_string(), Some("owner/project-b"))
        .await
        .unwrap();
    assert!(
        resolved.is_none(),
        "a bare number in another project must not pick this internal task"
    );
}

/// Regression test: resolve_task_id fallback must not cross repo boundaries.
///
/// When the primary external_id lookup fails, the numeric-suffix fallback used
/// to query `SELECT id FROM tasks WHERE id = ?` with no repo filter.  In a
/// multi-repo setup this could resolve a task that belongs to a different repo.
#[tokio::test]
async fn resolve_task_id_fallback_respects_repo() {
    let store = TaskStore::open_memory().await.unwrap();

    // Create an internal task for repo-a.  The store assigns id=1 and sets
    // external_id = "internal:1".
    let id_a = store
        .create_internal("owner/repo-a", "Task A", "body", "test", "src-a", None)
        .await
        .unwrap();

    // Corrupt: remove the external_id so the primary lookup fails and the
    // code falls through to the numeric-suffix fallback path.
    sqlx::query("UPDATE tasks SET external_id = NULL WHERE id = ?")
        .bind(id_a)
        .execute(store.pool())
        .await
        .unwrap();

    // repo-b should NOT resolve task "internal:{id_a}" — the numeric id
    // belongs to repo-a, not repo-b.
    let resolved = store
        .resolve_task_id("owner/repo-b", &format!("internal:{id_a}"))
        .await
        .unwrap();

    assert!(
        resolved.is_none(),
        "fallback must not resolve a task from a different repo (got {resolved:?})"
    );
}
